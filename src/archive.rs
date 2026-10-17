//! cpio archives, the payload of an AirDrop `/Upload` (gzip-compressed).
//!
//! We write the POSIX "odc" format (magic `070707`), the one Apple's own
//! sender uses, and read both odc and SVR4 "newc" (`070701`/`070702`), so a
//! change of format on the phone side does not break receiving.
//!
//! Extraction never trusts the names in the archive: absolute paths, `..`
//! and anything that is not a plain file or directory are refused or skipped.

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};

const TRAILER: &str = "TRAILER!!!";
const S_IFMT: u32 = 0o170000;
const S_IFDIR: u32 = 0o040000;
const S_IFREG: u32 = 0o100000;

/// Streaming odc writer.
pub struct Writer<W: Write> {
    out: W,
    ino: u32,
}

impl<W: Write> Writer<W> {
    pub fn new(out: W) -> Self {
        Self { out, ino: 1 }
    }

    pub fn dir(&mut self, name: &str, mtime: u64) -> io::Result<()> {
        self.header(name, S_IFDIR | 0o755, 0, mtime)
    }

    /// Appends a file of exactly `size` bytes read from `data`.
    pub fn file(&mut self, name: &str, size: u64, mtime: u64, data: &mut impl Read) -> io::Result<()> {
        self.header(name, S_IFREG | 0o644, size, mtime)?;
        let copied = io::copy(&mut data.take(size), &mut self.out)?;
        if copied != size {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, format!("{name}: file shrank while sending")));
        }
        Ok(())
    }

    /// Writes the trailer and returns the inner writer.
    pub fn finish(mut self) -> io::Result<W> {
        self.ino = 0;
        self.header(TRAILER, 0, 0, 0)?;
        Ok(self.out)
    }

    fn header(&mut self, name: &str, mode: u32, size: u64, mtime: u64) -> io::Result<()> {
        let nlink = if mode & S_IFMT == S_IFDIR { 2 } else { 1 };
        let h = format!(
            "070707{:06o}{:06o}{:06o}{:06o}{:06o}{:06o}{:06o}{:011o}{:06o}{:011o}",
            0, self.ino, mode, 0, 0, if name == TRAILER { 1 } else { nlink }, 0, mtime, name.len() + 1, size
        );
        self.ino += 1;
        self.out.write_all(h.as_bytes())?;
        self.out.write_all(name.as_bytes())?;
        self.out.write_all(&[0])
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Format {
    Odc,
    Newc,
}

struct Entry {
    name: String,
    mode: u32,
    size: u64,
}

/// Streaming reader over any cpio source.
struct Reader<R: Read> {
    inp: R,
    pos: u64,
}

impl<R: Read> Reader<R> {
    fn next(&mut self) -> Result<Option<(Entry, Format)>> {
        let mut magic = [0u8; 6];
        self.read_exact(&mut magic)?;
        let fmt = match &magic {
            b"070707" => Format::Odc,
            b"070701" | b"070702" => Format::Newc,
            _ => bail!("not a cpio archive (magic {:?})", String::from_utf8_lossy(&magic)),
        };
        let (mode, namesize, size) = match fmt {
            Format::Odc => {
                let mut h = [0u8; 70];
                self.read_exact(&mut h)?;
                (octal(&h[12..18])? as u32, octal(&h[53..59])?, octal(&h[59..70])?)
            }
            Format::Newc => {
                let mut h = [0u8; 104];
                self.read_exact(&mut h)?;
                (hex(&h[8..16])? as u32, hex(&h[88..96])?, hex(&h[48..56])?)
            }
        };
        if namesize == 0 || namesize > 4096 {
            bail!("cpio name length {namesize} out of range");
        }
        let mut name = vec![0u8; namesize as usize];
        self.read_exact(&mut name)?;
        if fmt == Format::Newc {
            self.pad4()?;
        }
        name.pop(); // NUL
        let name = String::from_utf8(name).context("cpio name is not UTF-8")?;
        if name == TRAILER {
            return Ok(None);
        }
        Ok(Some((Entry { name, mode, size }, fmt)))
    }

    fn read_exact(&mut self, buf: &mut [u8]) -> Result<()> {
        self.inp.read_exact(buf).context("archive truncated")?;
        self.pos += buf.len() as u64;
        Ok(())
    }

    fn pad4(&mut self) -> Result<()> {
        let pad = (4 - self.pos % 4) % 4;
        let mut b = [0u8; 3];
        self.read_exact(&mut b[..pad as usize])
    }

    /// Copies the current entry's data into `out` (or discards it).
    fn data(&mut self, size: u64, fmt: Format, out: Option<&mut File>) -> Result<()> {
        let mut src = (&mut self.inp).take(size);
        let n = match out {
            Some(f) => io::copy(&mut src, f)?,
            None => io::copy(&mut src, &mut io::sink())?,
        };
        self.pos += n;
        if n != size {
            bail!("archive truncated inside a file");
        }
        if fmt == Format::Newc {
            self.pad4()?;
        }
        Ok(())
    }
}

/// What an extraction produced.
#[derive(Debug, Default)]
pub struct Extracted {
    /// Every regular file written, relative to the destination.
    pub files: Vec<PathBuf>,
    pub bytes: u64,
}

/// Extracts a cpio stream into `dest`, which must exist.
pub fn extract(inp: impl Read, dest: &Path) -> Result<Extracted> {
    let mut r = Reader { inp, pos: 0 };
    let mut done = Extracted::default();
    while let Some((e, fmt)) = r.next()? {
        let Some(rel) = safe_path(&e.name) else {
            // Apple's archives open with a "." entry for the root itself.
            if e.name.trim_end_matches('/') != "." {
                tracing::warn!("archivio: salto il nome non sicuro {:?}", e.name);
            }
            r.data(e.size, fmt, None)?;
            continue;
        };
        let path = dest.join(&rel);
        match e.mode & S_IFMT {
            S_IFDIR => {
                fs::create_dir_all(&path).with_context(|| format!("creating {}", path.display()))?;
                r.data(e.size, fmt, None)?;
            }
            S_IFREG => {
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent)?;
                }
                let mut f = File::create(&path).with_context(|| format!("creating {}", path.display()))?;
                r.data(e.size, fmt, Some(&mut f))?;
                done.bytes += e.size;
                done.files.push(rel);
            }
            _ => {
                tracing::warn!("archivio: salto {:?} (né file né cartella)", e.name);
                r.data(e.size, fmt, None)?;
            }
        }
    }
    Ok(done)
}

/// `./a/b.jpg` -> `a/b.jpg`; `None` for anything that could escape `dest`.
fn safe_path(name: &str) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for c in Path::new(name).components() {
        match c {
            Component::Normal(p) => out.push(p),
            Component::CurDir => {}
            _ => return None,
        }
    }
    (!out.as_os_str().is_empty()).then_some(out)
}

fn octal(b: &[u8]) -> Result<u64> {
    let s = std::str::from_utf8(b)?;
    u64::from_str_radix(s, 8).with_context(|| format!("bad octal field {s:?}"))
}

fn hex(b: &[u8]) -> Result<u64> {
    let s = std::str::from_utf8(b)?;
    u64::from_str_radix(s, 16).with_context(|| format!("bad hex field {s:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn odc(entries: &[(&str, Option<&[u8]>)]) -> Vec<u8> {
        let mut w = Writer::new(Vec::new());
        for (name, data) in entries {
            match data {
                None => w.dir(name, 0).unwrap(),
                Some(d) => w.file(name, d.len() as u64, 0, &mut &d[..]).unwrap(),
            }
        }
        w.finish().unwrap()
    }

    #[test]
    fn roundtrip_odc() {
        let a = odc(&[("./Foto", None), ("./Foto/a.jpg", Some(b"ciao")), ("./b.txt", Some(b""))]);
        let dir = tempfile::tempdir().unwrap();
        let x = extract(&a[..], dir.path()).unwrap();
        assert_eq!(x.files, vec![PathBuf::from("Foto/a.jpg"), PathBuf::from("b.txt")]);
        assert_eq!(fs::read(dir.path().join("Foto/a.jpg")).unwrap(), b"ciao");
        assert_eq!(x.bytes, 4);
    }

    #[test]
    fn refuses_escapes() {
        let a = odc(&[("../fuori", Some(b"x")), ("/etc/passwd", Some(b"x")), ("./ok", Some(b"y"))]);
        let dir = tempfile::tempdir().unwrap();
        let x = extract(&a[..], dir.path()).unwrap();
        assert_eq!(x.files, vec![PathBuf::from("ok")]);
    }

    #[test]
    fn reads_newc() {
        // `printf 'hi' > f; echo f | cpio -o -H newc`, trimmed to the header we need.
        fn ent(name: &str, mode: u32, data: &[u8]) -> Vec<u8> {
            let mut v = format!(
                "070701{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}",
                1, mode, 0, 0, 1, 0, data.len(), 0, 0, 0, 0, name.len() + 1, 0
            )
            .into_bytes();
            v.extend(name.as_bytes());
            v.push(0);
            while v.len() % 4 != 0 {
                v.push(0);
            }
            v.extend(data);
            while v.len() % 4 != 0 {
                v.push(0);
            }
            v
        }
        let mut a = ent("f", S_IFREG | 0o644, b"hi!");
        a.extend(ent(TRAILER, 0, b""));
        let dir = tempfile::tempdir().unwrap();
        let x = extract(&a[..], dir.path()).unwrap();
        assert_eq!(fs::read(dir.path().join("f")).unwrap(), b"hi!");
        assert_eq!(x.files.len(), 1);
    }

    #[test]
    fn truncated_is_an_error() {
        let a = odc(&[("./a", Some(b"0123456789"))]);
        let dir = tempfile::tempdir().unwrap();
        assert!(extract(&a[..80], dir.path()).is_err());
    }
}
