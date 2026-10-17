//! `application/x-dvzip`, the upload body of iOS 18 and later.
//!
//! A sequence of framed blocks, each `u32` big-endian descriptor + payload:
//! the low 31 bits are the payload length, the high bit marks a stored
//! (uncompressed) block, otherwise the payload is a zlib stream. A zero
//! length or a clean end of input ends the body. Inflated and concatenated,
//! the blocks are an odc cpio archive (see `archive`).
//!
//! Decoded as a stream so a large video never sits in memory.

use std::io::{self, Read, Write};

use flate2::Compression;
use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;

const STORED: u32 = 0x8000_0000;
/// Uncompressed bytes per block when we send, as Apple does.
const BLOCK: usize = 128 << 10;

enum Block<R: Read> {
    Stored(io::Take<R>),
    Zlib(ZlibDecoder<io::Take<R>>),
}

pub struct Reader<R: Read> {
    // `None` between blocks; `Some` while one is being read.
    cur: Option<Block<R>>,
    src: Option<R>,
    done: bool,
}

impl<R: Read> Reader<R> {
    pub fn new(src: R) -> Self {
        Self { cur: None, src: Some(src), done: false }
    }

    /// Opens the next block; `false` at the end of the body.
    fn open(&mut self) -> io::Result<bool> {
        let mut src = self.src.take().expect("source present between blocks");
        let mut d = [0u8; 4];
        let mut got = 0;
        while got < 4 {
            match src.read(&mut d[got..])? {
                0 if got == 0 => return Ok(false),
                0 => return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "dvzip: truncated block header")),
                n => got += n,
            }
        }
        let d = u32::from_be_bytes(d);
        let len = (d & !STORED) as u64;
        if len == 0 {
            return Ok(false);
        }
        let body = src.take(len);
        self.cur = Some(if d & STORED != 0 { Block::Stored(body) } else { Block::Zlib(ZlibDecoder::new(body)) });
        Ok(true)
    }

    /// Finishes the current block: drains what zlib left and checks the frame was whole.
    fn close(&mut self) -> io::Result<()> {
        let mut take = match self.cur.take().expect("open block") {
            Block::Stored(t) => t,
            Block::Zlib(z) => z.into_inner(),
        };
        io::copy(&mut take, &mut io::sink())?;
        if take.limit() != 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "dvzip: truncated block"));
        }
        self.src = Some(take.into_inner());
        Ok(())
    }
}

impl<R: Read> Read for Reader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            if self.done || buf.is_empty() {
                return Ok(0);
            }
            if self.cur.is_none() && !self.open()? {
                self.done = true;
                return Ok(0);
            }
            let n = match self.cur.as_mut().unwrap() {
                Block::Stored(t) => t.read(buf)?,
                Block::Zlib(z) => z.read(buf)?,
            };
            if n > 0 {
                return Ok(n);
            }
            self.close()?;
        }
    }
}

/// Writes a dvzip body: buffers `BLOCK` bytes, emits one zlib block each.
pub struct Writer<W: Write> {
    out: W,
    buf: Vec<u8>,
}

impl<W: Write> Writer<W> {
    pub fn new(out: W) -> Self {
        Self { out, buf: Vec::with_capacity(BLOCK) }
    }

    fn emit(&mut self) -> io::Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        let mut z = ZlibEncoder::new(Vec::with_capacity(self.buf.len() / 2), Compression::fast());
        z.write_all(&self.buf)?;
        let packed = z.finish()?;
        // Already-compressed data (JPEG, HEIC, video) does not shrink: store it.
        let (desc, payload) = if packed.len() < self.buf.len() {
            (packed.len() as u32, &packed[..])
        } else {
            (self.buf.len() as u32 | STORED, &self.buf[..])
        };
        self.out.write_all(&desc.to_be_bytes())?;
        self.out.write_all(payload)?;
        self.buf.clear();
        Ok(())
    }

    pub fn finish(mut self) -> io::Result<W> {
        self.emit()?;
        self.out.flush()?;
        Ok(self.out)
    }
}

impl<W: Write> Write for Writer<W> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        let n = data.len().min(BLOCK - self.buf.len());
        self.buf.extend_from_slice(&data[..n]);
        if self.buf.len() == BLOCK {
            self.emit()?;
        }
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.out.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn zblock(data: &[u8]) -> Vec<u8> {
        let mut z = ZlibEncoder::new(Vec::new(), Compression::default());
        z.write_all(data).unwrap();
        let p = z.finish().unwrap();
        let mut v = (p.len() as u32).to_be_bytes().to_vec();
        v.extend(p);
        v
    }

    fn decode(raw: &[u8]) -> io::Result<Vec<u8>> {
        let mut out = Vec::new();
        Reader::new(raw).read_to_end(&mut out)?;
        Ok(out)
    }

    #[test]
    fn mixed_blocks() {
        let mut raw = zblock(b"0707");
        raw.extend((3u32 | STORED).to_be_bytes());
        raw.extend(b"07 ");
        raw.extend(zblock(b"fine"));
        assert_eq!(decode(&raw).unwrap(), b"070707 fine");
        // A zero descriptor ends the body even with bytes after it.
        raw.extend([0, 0, 0, 0, 9, 9]);
        assert_eq!(decode(&raw).unwrap(), b"070707 fine");
    }

    #[test]
    fn truncated() {
        let raw = zblock(b"abcdefgh");
        assert!(decode(&raw[..raw.len() - 2]).is_err());
        assert!(decode(&raw[..2]).is_err());
    }

    #[test]
    fn roundtrip_big() {
        let data: Vec<u8> = (0..(BLOCK * 2 + 777)).map(|i| (i % 251) as u8).collect();
        let mut noise = crate::ident::random::<4096>().to_vec();
        noise.extend(&data);
        let mut w = Writer::new(Vec::new());
        w.write_all(&noise).unwrap();
        let raw = w.finish().unwrap();
        assert_eq!(decode(&raw).unwrap(), noise);
    }
}
