#![allow(dead_code)]

use std::io::Cursor;
use std::io::Error;
use std::io::ErrorKind;
use std::io::Read;
use std::io::Result;
use std::io::Seek;
use std::io::SeekFrom;
use std::io::Write;
use std::net::SocketAddr;
use std::net::IpAddr;
use std::net::Ipv4Addr;

use byteorder::ReadBytesExt;
use byteorder::WriteBytesExt;
use byteorder::NetworkEndian;
use ring::constant_time::verify_slices_are_equal;
use ring::digest;
use tokio_core::net::UdpCodec;

#[derive(Debug, Clone)]
pub enum Request {
    Bind(BindRequest),
    SharedSecret, //(SharedSecretRequestMsg),
}

#[derive(Debug, Clone)]
pub enum ChangeRequest {
    Ip,
    Port,
    IpAndPort,
}

#[derive(Debug, Default, Clone)]
pub struct BindRequest {
    pub response_address: Option<SocketAddr>,
    pub change_request: Option<ChangeRequest>,
    pub username: Option<Vec<u8>>,
}

impl BindRequest {
    fn encode(&self) -> Result<Vec<u8>> {
        let mut buf = Vec::new();

        if let Some(a) = self.response_address {
            Attribute::ResponseAddress(a).encode(&mut buf)?;
        }

        if let Some(ref r) = self.change_request {
            Attribute::ChangeRequest(r.clone()).encode(&mut buf)?;
        }

        if let Some(ref u) = self.username {
            Attribute::Username(u.clone()).encode(&mut buf)?;
        }

        Ok(buf)
    }
}

#[derive(Debug)]
pub enum Response {
    Bind(BindResponse),
//    'BindErrorResponseMsg': BindErrorResponseMsg,
//    'SharedSecretResponseMsg': SharedSecretResponseMsg,
//    'SharedSecretErrorResponseMsg': SharedSecretErrorResponseMsg}
}

#[derive(Debug)]
pub struct BindResponse {
    pub mapped_address: SocketAddr,
    pub source_address: SocketAddr,
    pub changed_address: SocketAddr,
    pub reflected_from: Option<SocketAddr>,
}

pub struct StunCodec;

pub enum Attribute {
    MappedAddress(SocketAddr),
    ResponseAddress(SocketAddr),
    ChangedAddress(SocketAddr),
    SourceAddress(SocketAddr),
    ReflectedFrom(SocketAddr),
    ChangeRequest(ChangeRequest),
    MessageIntegrity([u8; 20]),
    Username(Vec<u8>),
    UnknownOptional,
}

impl StunCodec {
    pub fn new() -> StunCodec {
        StunCodec {}
    }

    fn read_binding_response(msg: &[u8], mut c: &mut Cursor<&[u8]>) -> Result<BindResponse> {
        let mut mapped_address = None;
        let mut source_address = None;
        let mut changed_address = None;
        let mut message_integrity = None;
        let mut reflected_from = None;

        let error = |reason| Err(Error::new(ErrorKind::InvalidData, reason));

        loop {
            let attr = Attribute::read(c);
            match attr {
                Ok(Attribute::MappedAddress(s))  if mapped_address.is_none()  => mapped_address = Some(s),
                Ok(Attribute::SourceAddress(s))  if source_address.is_none()  => source_address = Some(s),
                Ok(Attribute::ChangedAddress(s)) if changed_address.is_none() => changed_address = Some(s),
                Ok(Attribute::ReflectedFrom(s))  if reflected_from.is_none()  => reflected_from = Some(s),
                Ok(Attribute::MessageIntegrity(h)) if message_integrity.is_none() => {
                    message_integrity = Some(h)
                },
                Ok(Attribute::UnknownOptional) => continue,
                Err(ref e) if e.kind() == ErrorKind::UnexpectedEof => break,
                _ => return error("Unknown mandatory attribute!"),
            }
        }

        if let Some(expected) = message_integrity {
            let actual = digest::digest(&digest::SHA1, &msg[..msg.len() - 24]);

            if verify_slices_are_equal(actual.as_ref(), &expected).is_err() {
                return error("Message integrity violated!");
            }
        }

        Ok(BindResponse {
            mapped_address:  if let Some(a) = mapped_address { a } else { return error("MappedAddress missing!") },
            source_address:  if let Some(a) = source_address { a } else { return error("SourceAddress missing!") },
            changed_address: if let Some(a) = changed_address { a } else { return error("ChangedAddress missing!") },
            reflected_from:  reflected_from,
        })
    }
}

impl Attribute {
    fn read(mut c: &mut Cursor<&[u8]>) -> Result<Attribute> {
        let typ = c.read_u16::<NetworkEndian>()?;
        let len = c.read_u16::<NetworkEndian>()?;

        match typ {
            MAPPED_ADDRESS    => Ok(Attribute::MappedAddress(Self::read_address(&mut c)?)),
            RESPONSE_ADDRESS  => Ok(Attribute::ResponseAddress(Self::read_address(&mut c)?)),
            CHANGED_ADDRESS   => Ok(Attribute::ChangedAddress(Self::read_address(&mut c)?)),
            SOURCE_ADDRESS    => Ok(Attribute::SourceAddress(Self::read_address(&mut c)?)),
            REFLECTED_FROM    => Ok(Attribute::ReflectedFrom(Self::read_address(&mut c)?)),
            MESSAGE_INTEGRITY => {
                let mut hash = [0; 20];
                c.read_exact(&mut hash)?;
                Ok(Attribute::MessageIntegrity(hash))
            },
            CHANGE_REQUEST    => {
                match c.read_u32::<NetworkEndian>()? {
                    CHANGE_REQUEST_IP          => Ok(Attribute::ChangeRequest(ChangeRequest::Ip)),
                    CHANGE_REQUEST_PORT        => Ok(Attribute::ChangeRequest(ChangeRequest::Port)),
                    CHANGE_REQUEST_IP_AND_PORT => Ok(Attribute::ChangeRequest(ChangeRequest::IpAndPort)),
                    _ => Err(Error::new(ErrorKind::InvalidData, "CHANGE_REQUEST not understood")),
                }
            },
            _ if typ <= 0x7fff => Err(Error::new(ErrorKind::InvalidData, "Unknown mandatory field")),
            _ => {
                c.seek(SeekFrom::Current(len as i64))?;
                Ok(Attribute::UnknownOptional)
            },
        }
    }

    fn read_address(c: &mut Cursor<&[u8]>) -> Result<SocketAddr> {
        let _ = c.read_u8()?; // ignored
        let typ = c.read_u8()?;
        let port = c.read_u16::<NetworkEndian>()?;
        let addr = c.read_u32::<NetworkEndian>()?;

        if typ != 0x01 {
            return Err(Error::new(ErrorKind::InvalidData, "Invalid address family"));
        }

        let b0 = ((addr & 0xff000000) >> 24) as u8;
        let b1 = ((addr & 0x00ff0000) >> 16) as u8;
        let b2 = ((addr & 0x0000ff00) >>  8) as u8;
        let b3 = ((addr & 0x000000ff) >>  0) as u8;
        let ip = IpAddr::V4(Ipv4Addr::new(b0, b1, b2, b3));

        Ok(SocketAddr::new(ip, port))
    }

    fn encode(&self, buf: &mut Vec<u8>) -> Result<()> {
        let (typ, opaque) = match *self {
            Attribute::MappedAddress(ref s)    => (MAPPED_ADDRESS,    Self::encode_address(s)?),
            Attribute::ResponseAddress(ref s)  => (RESPONSE_ADDRESS,  Self::encode_address(s)?),
            Attribute::ChangedAddress(ref s)   => (CHANGED_ADDRESS,   Self::encode_address(s)?),
            Attribute::SourceAddress(ref s)    => (SOURCE_ADDRESS,    Self::encode_address(s)?),
            Attribute::ReflectedFrom(ref s)    => (REFLECTED_FROM,    Self::encode_address(s)?),
            Attribute::MessageIntegrity(ref h) => (MESSAGE_INTEGRITY, h.to_vec()),
            Attribute::Username(ref u) => {
                let total_len = (4.0*(u.len() as f64 / 4.0).ceil()) as usize;
                let padding_len = total_len - u.len();

                let mut buf = Vec::with_capacity(total_len);
                buf.write_all(&u[..])?;
                for _ in 0..padding_len {
                    buf.write_u8(0x00)?;
                }
                assert_eq!(buf.len(), total_len);

                (USERNAME, buf.clone())
            },
            Attribute::ChangeRequest(ref c) => (CHANGE_REQUEST, Self::encode_change_request(c)?),
            Attribute::UnknownOptional => unreachable!(),
        };

        buf.write_u16::<NetworkEndian>(typ)?;
        buf.write_u16::<NetworkEndian>(opaque.len() as u16)?;
        buf.write_all(&opaque[..])?;

        Ok(())
    }

    fn encode_change_request(c: &ChangeRequest) -> Result<Vec<u8>> {
        let mut buf = Vec::with_capacity(4);

        match *c {
            ChangeRequest::Ip        => buf.write_u32::<NetworkEndian>(CHANGE_REQUEST_IP)?,
            ChangeRequest::Port      => buf.write_u32::<NetworkEndian>(CHANGE_REQUEST_PORT)?,
            ChangeRequest::IpAndPort => buf.write_u32::<NetworkEndian>(CHANGE_REQUEST_IP_AND_PORT)?,
        };

        Ok(buf)
    }

    fn encode_address(addr: &SocketAddr) -> Result<Vec<u8>> {
        let mut buf = Vec::with_capacity(8);
        buf.write_u8(0x00)?;
        buf.write_u8(0x01)?;

        if let SocketAddr::V4(ref addr) = *addr {
            buf.write_u16::<NetworkEndian>(addr.port())?;
            buf.write_all(&addr.ip().octets()[..])?;

            Ok(buf)
        } else {
            Err(Error::new(ErrorKind::InvalidInput, "STUN does not support IPv6"))
        }
    }
}

const BINDING_REQUEST:u16        = 0x0001;
const BINDING_RESPONSE:u16       = 0x0101;
const BINDING_ERROR:u16          = 0x0111;
const SHARED_SECRET_REQUEST:u16  = 0x0002;
const SHARED_SECRET_RESPONSE:u16 = 0x0102;
const SHARED_SECRET_ERROR:u16    = 0x0112;

const MAPPED_ADDRESS:u16     = 0x0001;
const RESPONSE_ADDRESS:u16   = 0x0002;
const CHANGE_REQUEST:u16     = 0x0003;
const SOURCE_ADDRESS:u16     = 0x0004;
const CHANGED_ADDRESS:u16    = 0x0005;
const USERNAME:u16           = 0x0006;
const PASSWORD:u16           = 0x0007;
const MESSAGE_INTEGRITY:u16  = 0x0008;
const ERROR_CODE:u16         = 0x0009;
const UNKNOWN_ATTRIBUTES:u16 = 0x000a;
const REFLECTED_FROM:u16     = 0x000b;

const CHANGE_REQUEST_IP:u32          = 0x20;
const CHANGE_REQUEST_PORT:u32        = 0x40;
const CHANGE_REQUEST_IP_AND_PORT:u32 = 0x60;

impl UdpCodec for StunCodec {
    type In = (u64, Response);
    type Out = (u64, SocketAddr, Request);

    fn decode(&mut self, _: &SocketAddr, msg: &[u8]) -> Result<Self::In> {
        let mut c = Cursor::new(msg);

        let msg_type = c.read_u16::<NetworkEndian>()?;
        let _ = c.read_u16::<NetworkEndian>()?; // msg_len
        let trans_id1 = c.read_u64::<NetworkEndian>()?;
        let trans_id2 = c.read_u64::<NetworkEndian>()?;

        if trans_id1 != 0 {
            return Err(Error::new(ErrorKind::InvalidData, "Invalid transaction ID!"));
        }

        let res = match msg_type {
            BINDING_RESPONSE => Self::read_binding_response(msg, &mut c).map(|r| Response::Bind(r)),
            BINDING_ERROR => unimplemented!(),
            SHARED_SECRET_RESPONSE => unimplemented!(),
            SHARED_SECRET_ERROR => unimplemented!(),
            _ => return Err(Error::new(ErrorKind::InvalidData, "Unknown message type!")),
        };

        res.map(|v| (trans_id2, v))
    }

    fn encode(&mut self, msg: Self::Out, buf: &mut Vec<u8>) -> SocketAddr {
        let (trans_id, dst, req) = msg;

        let (typ, m) = match req {
            Request::Bind(bind) => (BINDING_REQUEST, bind.encode().unwrap()),
            _ => unimplemented!(),
        };

        buf.write_u16::<NetworkEndian>(typ).unwrap();
//        buf.write_u16::<NetworkEndian>(m.len() as u16 + 24).unwrap();
        buf.write_u16::<NetworkEndian>(m.len() as u16).unwrap();
        buf.write_u64::<NetworkEndian>(0x0).unwrap();
        buf.write_u64::<NetworkEndian>(trans_id).unwrap();
        buf.write_all(&m[..]).unwrap();

        /*
            TODO
        let mut copy = buf.clone();
        while copy.len() % 64 != 0 {
            copy.write_u8(0).unwrap();
        }
        println!("{}", copy.len());

        let mut hash = [0; 20];
        let digest = digest::digest(&digest::SHA1, &copy[..]);
        hash.copy_from_slice(digest.as_ref());
        let message_integrity = Attribute::MessageIntegrity(hash);
        message_integrity.encode(buf).unwrap();
            */

        dst
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_address() {
        let mut buf = Vec::new();

        let attr = Attribute::ChangedAddress("127.0.1.2:54321".parse().unwrap());
        attr.encode(&mut buf).unwrap();

        let expected = vec![0x00, 0x05, 0x00, 0x08,
                            0x00, 0x01, 0xd4, 0x31,
                            0x7f, 0x00, 0x01, 0x02];

        assert_eq!(expected, buf);
    }

    #[test]
    fn encode_binding_request() {
        let req = BindRequest {
            response_address: None,
            change_request: Some(ChangeRequest::IpAndPort),
            username: Some(b"foo".to_vec()),
        };

        let addr = "0.0.0.0:0".parse().unwrap();
        let mut actual = Vec::new();
        let _ = StunCodec.encode((0x123456789, addr, Request::Bind(req)), &mut actual); // dst

        // TODO: sha1
        let expected = vec![
//            0x00, 0x01, 0x00, 0x14, // type, len
            0x00, 0x01, 0x00, 0x10, // type, len
            0x00, 0x00, 0x00, 0x00, // transaction id
            0x00, 0x00, 0x00, 0x00, //  ...
            0x00, 0x00, 0x00, 0x01, //  ...
            0x23, 0x45, 0x67, 0x89, //  ...
            0x00, 0x03, 0x00, 0x04, // changed_address, len
            0x00, 0x00, 0x00, 0x60, //  ip and port
            0x00, 0x06, 0x00, 0x04, // username
            0x66, 0x6f, 0x6f, 0x00, //  "foo"

            /*0x00, 0x08, 0x00, 0x14, // message integrity
            0x89, 0x4f, 0xef, 0x24, //  sha1
            0xd5, 0x81, 0x45, 0x66, //  ...
            0x8b, 0xa8, 0x27, 0xf0, //  ...
            0xf8, 0x1e, 0x54, 0x98, //  ...
            0xf7, 0x19, 0x52, 0x04, //  ...
            */];

            assert_eq!(expected, actual);
    }
}
