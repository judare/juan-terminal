//! Minimal RFC 6455 WebSocket server pieces: handshake and frame I/O (text, ping/pong, close,
//! fragmentation). Client frames are masked; server frames are not.

use std::io::{self, Read, Write};

use base64::Engine;

const GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
pub const MAX_MESSAGE: usize = 64 * 1024 * 1024;

pub fn accept_key(key: &str) -> String {
    let mut h = sha1_smol::Sha1::new();
    h.update(key.trim().as_bytes());
    h.update(GUID.as_bytes());
    base64::engine::general_purpose::STANDARD.encode(h.digest().bytes())
}

/// A parsed HTTP request head.
pub struct Request {
    pub method: String,
    pub path: String,
    /// Lower-cased header names.
    pub headers: Vec<(String, String)>,
    /// Bytes read past the head (start of the body).
    pub rest: Vec<u8>,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }
}

pub fn read_request(r: &mut impl Read) -> io::Result<Request> {
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    let end = loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i;
        }
        if buf.len() > 16 * 1024 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "request head too large"));
        }
        let n = r.read(&mut chunk)?;
        if n == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = String::from_utf8_lossy(&buf[..end]).into_owned();
    let mut lines = head.split("\r\n");
    let first = lines.next().unwrap_or_default();
    let mut parts = first.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let path = parts.next().unwrap_or_default().to_string();
    let headers = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    Ok(Request { method, path, headers, rest: buf[end + 4..].to_vec() })
}

#[derive(Debug, PartialEq, Eq)]
pub enum Message {
    Text(String),
    Binary(Vec<u8>),
    Ping(Vec<u8>),
    Pong,
    Close,
}

/// Read one complete message (joining fragments).
pub fn read_message(r: &mut impl Read) -> io::Result<Message> {
    let mut data = Vec::new();
    let mut first_op = None;
    loop {
        let mut h = [0u8; 2];
        r.read_exact(&mut h)?;
        let fin = h[0] & 0x80 != 0;
        let op = h[0] & 0x0f;
        let masked = h[1] & 0x80 != 0;
        let mut len = (h[1] & 0x7f) as u64;
        if len == 126 {
            let mut b = [0u8; 2];
            r.read_exact(&mut b)?;
            len = u16::from_be_bytes(b) as u64;
        } else if len == 127 {
            let mut b = [0u8; 8];
            r.read_exact(&mut b)?;
            len = u64::from_be_bytes(b);
        }
        if len as usize > MAX_MESSAGE || data.len() + len as usize > MAX_MESSAGE {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "message too large"));
        }
        let mut mask = [0u8; 4];
        if masked {
            r.read_exact(&mut mask)?;
        }
        let mut payload = vec![0u8; len as usize];
        r.read_exact(&mut payload)?;
        if masked {
            for (i, b) in payload.iter_mut().enumerate() {
                *b ^= mask[i % 4];
            }
        }
        match op {
            // Control frames may arrive between fragments; they're never fragmented.
            0x8 => return Ok(Message::Close),
            0x9 => return Ok(Message::Ping(payload)),
            0xA => return Ok(Message::Pong),
            0x0 => data.extend_from_slice(&payload),
            0x1 | 0x2 => {
                first_op = Some(op);
                data = payload;
            }
            _ => return Err(io::Error::new(io::ErrorKind::InvalidData, "bad opcode")),
        }
        if fin {
            return Ok(match first_op {
                Some(0x2) => Message::Binary(data),
                _ => Message::Text(String::from_utf8(data).map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid utf-8"))?),
            });
        }
    }
}

pub fn write_frame(w: &mut impl Write, op: u8, payload: &[u8]) -> io::Result<()> {
    let mut head = Vec::with_capacity(10);
    head.push(0x80 | op);
    let n = payload.len();
    if n < 126 {
        head.push(n as u8);
    } else if n <= u16::MAX as usize {
        head.push(126);
        head.extend_from_slice(&(n as u16).to_be_bytes());
    } else {
        head.push(127);
        head.extend_from_slice(&(n as u64).to_be_bytes());
    }
    w.write_all(&head)?;
    w.write_all(payload)?;
    w.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc_example_accept_key() {
        // RFC 6455 §1.3 example.
        assert_eq!(accept_key("dGhlIHNhbXBsZSBub25jZQ=="), "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=");
    }

    fn masked(op: u8, fin: bool, payload: &[u8]) -> Vec<u8> {
        let mask = [1u8, 2, 3, 4];
        let mut v = vec![(if fin { 0x80 } else { 0 }) | op];
        if payload.len() < 126 {
            v.push(0x80 | payload.len() as u8);
        } else {
            v.push(0x80 | 126);
            v.extend_from_slice(&(payload.len() as u16).to_be_bytes());
        }
        v.extend_from_slice(&mask);
        v.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
        v
    }

    #[test]
    fn reads_masked_fragmented_and_long_messages() {
        let mut bytes = masked(0x1, false, b"hel");
        bytes.extend(masked(0x9, true, b"p")); // ping in the middle is allowed
        let mut cur = std::io::Cursor::new(bytes.clone());
        assert_eq!(read_message(&mut cur).unwrap(), Message::Ping(b"p".to_vec()));
        let mut bytes2 = masked(0x1, false, b"hel");
        bytes2.extend(masked(0x0, true, b"lo"));
        assert_eq!(read_message(&mut std::io::Cursor::new(bytes2)).unwrap(), Message::Text("hello".into()));
        let long = "x".repeat(300);
        assert_eq!(read_message(&mut std::io::Cursor::new(masked(0x1, true, long.as_bytes()))).unwrap(), Message::Text(long));
        assert_eq!(read_message(&mut std::io::Cursor::new(masked(0x8, true, b""))).unwrap(), Message::Close);
    }

    #[test]
    fn writes_frames_with_extended_lengths() {
        let mut out = Vec::new();
        write_frame(&mut out, 0x1, &vec![b'a'; 70_000]).unwrap();
        assert_eq!(out[0], 0x81);
        assert_eq!(out[1], 127);
        assert_eq!(u64::from_be_bytes(out[2..10].try_into().unwrap()), 70_000);
    }
}
