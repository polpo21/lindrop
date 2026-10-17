//! The small HTTP/1.1 subset AirDrop speaks, over any byte stream.
//!
//! Hand-written rather than a framework so the quirks stay in view: iOS
//! keeps connections alive between `/Discover`, `/Ask` and `/Upload`, sends
//! the upload chunked, may ask for `100-continue`, and does not always end
//! the final chunk with the CRLF the RFC requires.

use std::io::{self, BufRead, Read, Write};

const MAX_LINE: usize = 8192;
const MAX_HEADERS: usize = 64;

#[derive(Debug, Default, Clone)]
pub struct Head {
    /// Request: method and path. Response: "HTTP/1.1" and the status code.
    pub first: (String, String),
    pub headers: Vec<(String, String)>,
}

impl Head {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }

    pub fn is(&self, name: &str, value: &str) -> bool {
        self.header(name).is_some_and(|v| v.trim().eq_ignore_ascii_case(value))
    }

    pub fn content_length(&self) -> Option<u64> {
        self.header("content-length").and_then(|v| v.trim().parse().ok())
    }

    pub fn chunked(&self) -> bool {
        self.header("transfer-encoding").is_some_and(|v| v.to_ascii_lowercase().contains("chunked"))
    }

    pub fn status(&self) -> u16 {
        self.first.1.parse().unwrap_or(0)
    }
}

fn read_line(r: &mut impl BufRead) -> io::Result<Option<String>> {
    let mut buf = Vec::new();
    let n = r.take(MAX_LINE as u64).read_until(b'\n', &mut buf)?;
    if n == 0 {
        return Ok(None);
    }
    if buf.last() != Some(&b'\n') {
        return Err(bad("header line too long or truncated"));
    }
    while matches!(buf.last(), Some(b'\n' | b'\r')) {
        buf.pop();
    }
    String::from_utf8(buf).map(Some).map_err(|_| bad("header is not UTF-8"))
}

/// Reads a request or response head. `None` on a clean close between messages.
pub fn read_head(r: &mut impl BufRead) -> io::Result<Option<Head>> {
    let first = loop {
        match read_line(r)? {
            None => return Ok(None),
            Some(l) if l.is_empty() => continue, // stray CRLF after a body
            Some(l) => break l,
        }
    };
    let mut parts = first.splitn(3, ' ');
    let a = parts.next().unwrap_or_default().to_string();
    let b = parts.next().ok_or_else(|| bad("malformed first line"))?.to_string();
    let mut head = Head { first: (a, b), headers: Vec::new() };
    loop {
        let line = read_line(r)?.ok_or_else(|| bad("connection closed inside headers"))?;
        if line.is_empty() {
            return Ok(Some(head));
        }
        if head.headers.len() == MAX_HEADERS {
            return Err(bad("too many headers"));
        }
        let (k, v) = line.split_once(':').ok_or_else(|| bad("malformed header"))?;
        head.headers.push((k.trim().to_string(), v.trim().to_string()));
    }
}

/// The body of a message as a reader, bounded by its framing.
pub fn body<'a, R: BufRead>(head: &Head, r: &'a mut R) -> Box<dyn Read + 'a> {
    if head.chunked() {
        Box::new(ChunkedReader::new(r))
    } else {
        Box::new(r.take(head.content_length().unwrap_or(0)))
    }
}

/// Reads a whole small body (plists), refusing anything over `limit`.
pub fn read_small(head: &Head, r: &mut impl BufRead, limit: u64) -> io::Result<Vec<u8>> {
    let mut out = Vec::new();
    body(head, r).take(limit + 1).read_to_end(&mut out)?;
    if out.len() as u64 > limit {
        return Err(bad("body too large"));
    }
    Ok(out)
}

pub struct ChunkedReader<'a, R: BufRead> {
    inp: &'a mut R,
    left: u64,
    done: bool,
}

impl<'a, R: BufRead> ChunkedReader<'a, R> {
    pub fn new(inp: &'a mut R) -> Self {
        Self { inp, left: 0, done: false }
    }
}

impl<R: BufRead> Read for ChunkedReader<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.done || buf.is_empty() {
            return Ok(0);
        }
        if self.left == 0 {
            let line = read_line(self.inp)?.ok_or_else(|| eof("connection closed between chunks"))?;
            let size = line.split(';').next().unwrap_or_default().trim();
            self.left = u64::from_str_radix(size, 16).map_err(|_| bad("bad chunk size"))?;
            if self.left == 0 {
                // Trailers up to an empty line. A peer that closes instead of
                // sending that final CRLF has still sent the whole body.
                while let Some(l) = read_line(self.inp)? {
                    if l.is_empty() {
                        break;
                    }
                }
                self.done = true;
                return Ok(0);
            }
        }
        let want = buf.len().min(self.left as usize);
        let n = self.inp.read(&mut buf[..want])?;
        if n == 0 {
            return Err(eof("connection closed inside a chunk"));
        }
        self.left -= n as u64;
        if self.left == 0 {
            // The CRLF after the chunk data.
            read_line(self.inp)?;
        }
        Ok(n)
    }
}

/// Writes a chunked body; `finish` sends the terminating chunk.
pub struct ChunkedWriter<W: Write> {
    out: W,
}

impl<W: Write> ChunkedWriter<W> {
    pub fn new(out: W) -> Self {
        Self { out }
    }

    pub fn finish(mut self) -> io::Result<W> {
        self.out.write_all(b"0\r\n\r\n")?;
        self.out.flush()?;
        Ok(self.out)
    }
}

impl<W: Write> Write for ChunkedWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        write!(self.out, "{:x}\r\n", buf.len())?;
        self.out.write_all(buf)?;
        self.out.write_all(b"\r\n")?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.out.flush()
    }
}

pub fn reason(code: u16) -> &'static str {
    match code {
        100 => "Continue",
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        406 => "Not Acceptable",
        413 => "Payload Too Large",
        500 => "Internal Server Error",
        _ => "Unknown",
    }
}

/// Writes a complete response with a fixed-length body.
pub fn respond(w: &mut impl Write, code: u16, headers: &[(&str, &str)], body: &[u8]) -> io::Result<()> {
    let mut out = format!("HTTP/1.1 {code} {}\r\n", reason(code));
    for (k, v) in headers {
        out.push_str(&format!("{k}: {v}\r\n"));
    }
    out.push_str(&format!("Content-Length: {}\r\n\r\n", body.len()));
    w.write_all(out.as_bytes())?;
    w.write_all(body)?;
    w.flush()
}

pub fn write_request_head(w: &mut impl Write, path: &str, headers: &[(&str, String)]) -> io::Result<()> {
    let mut out = format!("POST {path} HTTP/1.1\r\n");
    for (k, v) in headers {
        out.push_str(&format!("{k}: {v}\r\n"));
    }
    out.push_str("\r\n");
    w.write_all(out.as_bytes())
}

fn bad(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.to_string())
}

fn eof(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::UnexpectedEof, msg.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use io::BufReader;

    #[test]
    fn two_requests_on_one_connection() {
        let raw = b"POST /Discover HTTP/1.1\r\nContent-Length: 3\r\nHost: x\r\n\r\nabc\
POST /Upload HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n4\r\nciao\r\n2\r\n!!\r\n0\r\n\r\n";
        let mut r = BufReader::new(&raw[..]);
        let h = read_head(&mut r).unwrap().unwrap();
        assert_eq!(h.first, ("POST".into(), "/Discover".into()));
        assert_eq!(read_small(&h, &mut r, 10).unwrap(), b"abc");
        let h = read_head(&mut r).unwrap().unwrap();
        assert!(h.chunked());
        let mut s = String::new();
        body(&h, &mut r).read_to_string(&mut s).unwrap();
        assert_eq!(s, "ciao!!");
        assert!(read_head(&mut r).unwrap().is_none());
    }

    #[test]
    fn final_chunk_without_crlf() {
        let raw = b"3\r\nabc\r\n0\r\n";
        let mut r = BufReader::new(&raw[..]);
        let mut s = String::new();
        ChunkedReader::new(&mut r).read_to_string(&mut s).unwrap();
        assert_eq!(s, "abc");
    }

    #[test]
    fn truncated_chunk_is_an_error() {
        let raw = b"10\r\nabc";
        let mut r = BufReader::new(&raw[..]);
        let mut s = Vec::new();
        assert!(ChunkedReader::new(&mut r).read_to_end(&mut s).is_err());
    }

    #[test]
    fn chunked_roundtrip() {
        let mut w = ChunkedWriter::new(Vec::new());
        w.write_all(b"hello ").unwrap();
        w.write_all(b"world").unwrap();
        let raw = w.finish().unwrap();
        let mut r = BufReader::new(&raw[..]);
        let mut s = String::new();
        ChunkedReader::new(&mut r).read_to_string(&mut s).unwrap();
        assert_eq!(s, "hello world");
    }

    #[test]
    fn small_body_limit() {
        let raw = b"POST / HTTP/1.1\r\nContent-Length: 20\r\n\r\n01234567890123456789";
        let mut r = BufReader::new(&raw[..]);
        let h = read_head(&mut r).unwrap().unwrap();
        assert!(read_small(&h, &mut r, 10).is_err());
    }
}
