//! Wire format between the privileged sampler (stdout) and the GUI.
//!
//! Every message is a one-byte tag followed by little-endian fields.
//! Paths are interned: a `Path` message assigns an id once, `Sample`
//! messages then refer to paths by id. The first path of a sample is the
//! one the sample is attributed to ("represented").
//!
//! GUI → sampler (stdin): `p` pauses, `r` resumes, EOF terminates.

use std::io::{self, Read, Write};

#[derive(Debug, Clone)]
pub struct Header {
    pub total_bytes: u64,
    pub nodesize: u32,
    pub devices: u32,
    pub threads: u32,
}

#[derive(Debug)]
pub enum Message {
    Header(Header),
    Path { id: u32, path: String },
    Sample { paths: Vec<u32> },
    Fatal(String),
}

const TAG_HEADER: u8 = b'H';
const TAG_PATH: u8 = b'P';
const TAG_SAMPLE: u8 = b'S';
const TAG_FATAL: u8 = b'F';

fn put_str(w: &mut impl Write, s: &str) -> io::Result<()> {
    w.write_all(&(s.len() as u32).to_le_bytes())?;
    w.write_all(s.as_bytes())
}

pub fn write(w: &mut impl Write, msg: &Message) -> io::Result<()> {
    match msg {
        Message::Header(h) => {
            w.write_all(&[TAG_HEADER])?;
            w.write_all(&h.total_bytes.to_le_bytes())?;
            w.write_all(&h.nodesize.to_le_bytes())?;
            w.write_all(&h.devices.to_le_bytes())?;
            w.write_all(&h.threads.to_le_bytes())
        }
        Message::Path { id, path } => {
            w.write_all(&[TAG_PATH])?;
            w.write_all(&id.to_le_bytes())?;
            put_str(w, path)
        }
        Message::Sample { paths } => {
            w.write_all(&[TAG_SAMPLE])?;
            w.write_all(&(paths.len() as u32).to_le_bytes())?;
            for id in paths {
                w.write_all(&id.to_le_bytes())?;
            }
            Ok(())
        }
        Message::Fatal(text) => {
            w.write_all(&[TAG_FATAL])?;
            put_str(w, text)
        }
    }
}

fn u32_(r: &mut impl Read) -> io::Result<u32> {
    let mut b = [0; 4];
    r.read_exact(&mut b)?;
    Ok(u32::from_le_bytes(b))
}
fn u64_(r: &mut impl Read) -> io::Result<u64> {
    let mut b = [0; 8];
    r.read_exact(&mut b)?;
    Ok(u64::from_le_bytes(b))
}
fn str_(r: &mut impl Read) -> io::Result<String> {
    let len = u32_(r)? as usize;
    let mut b = vec![0; len];
    r.read_exact(&mut b)?;
    Ok(String::from_utf8_lossy(&b).into_owned())
}

/// Reads the next message, or `Ok(None)` on a clean end of stream.
pub fn read(r: &mut impl Read) -> io::Result<Option<Message>> {
    let mut tag = [0u8];
    if r.read(&mut tag)? == 0 {
        return Ok(None);
    }
    let msg = match tag[0] {
        TAG_HEADER => Message::Header(Header {
            total_bytes: u64_(r)?,
            nodesize: u32_(r)?,
            devices: u32_(r)?,
            threads: u32_(r)?,
        }),
        TAG_PATH => Message::Path {
            id: u32_(r)?,
            path: str_(r)?,
        },
        TAG_SAMPLE => {
            let n = u32_(r)? as usize;
            let mut paths = Vec::with_capacity(n.min(1 << 16));
            for _ in 0..n {
                paths.push(u32_(r)?);
            }
            Message::Sample { paths }
        }
        TAG_FATAL => Message::Fatal(str_(r)?),
        other => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unknown message tag {other:#x}"),
            ));
        }
    };
    Ok(Some(msg))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let mut buf = Vec::new();
        write(
            &mut buf,
            &Message::Header(Header {
                total_bytes: 42,
                nodesize: 16384,
                devices: 1,
                threads: 4,
            }),
        )
        .unwrap();
        write(
            &mut buf,
            &Message::Path {
                id: 7,
                path: "@home/é/x".into(),
            },
        )
        .unwrap();
        write(&mut buf, &Message::Sample { paths: vec![7, 3] }).unwrap();
        let mut r = &buf[..];
        assert!(matches!(
            read(&mut r).unwrap(),
            Some(Message::Header(Header {
                total_bytes: 42,
                ..
            }))
        ));
        match read(&mut r).unwrap() {
            Some(Message::Path { id: 7, path }) => assert_eq!(path, "@home/é/x"),
            m => panic!("{m:?}"),
        }
        match read(&mut r).unwrap() {
            Some(Message::Sample { paths }) => assert_eq!(paths, vec![7, 3]),
            m => panic!("{m:?}"),
        }
        assert!(read(&mut r).unwrap().is_none());
    }
}
