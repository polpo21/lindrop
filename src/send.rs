//! The AirDrop sender: `/Discover`, `/Ask`, `/Upload` to a phone's receiver.
//!
//! Mirrors what an Apple sender puts on the wire, because iOS refuses an
//! upload that differs: `/Ask` announces a `TransferID`, and the `/Upload`
//! on the same TLS connection repeats it, with Apple's headers in Apple's
//! order and the dvzip body as a single chunk of `TotalBytes`.

use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Seek, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use base64::Engine;
use plist::{Dictionary, Value};

use crate::ident::{Ident, hex, random};
use crate::{archive, dvzip, http};

const CONNECT: Duration = Duration::from_secs(10);
/// A write that finds no buffer room for this long means the phone stopped
/// acknowledging: give up instead of hanging on TCP retransmits.
const STALL: Duration = Duration::from_secs(30);
const SLICE: usize = 64 << 10;
/// How long the person at the phone has to tap Accept.
const ASK_WAIT: Duration = Duration::from_secs(120);

pub enum Outcome {
    Sent,
    Declined,
}

type Tls = BufReader<rustls::StreamOwned<rustls::ClientConnection, TcpStream>>;

pub struct Sender {
    tls: Arc<rustls::ClientConfig>,
    name: String,
    service_id: String,
    pseudonym: String,
    push_token: String,
}

impl Sender {
    pub fn new(ident: &Ident, name: &str) -> Result<Self> {
        Ok(Self {
            tls: ident.client_config()?,
            name: name.to_string(),
            service_id: ident.service_id.clone(),
            // Not verified by the phone in Everyone mode; only their shape matters.
            pseudonym: format!("pseud:{}", base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(random::<16>())),
            push_token: hex(&random::<32>()).to_uppercase(),
        })
    }

    fn connect(&self, addr: SocketAddr) -> Result<Tls> {
        let tcp = TcpStream::connect_timeout(&addr, CONNECT).with_context(|| format!("connessione a {addr}"))?;
        tcp.set_nodelay(true)?;
        tcp.set_read_timeout(Some(CONNECT * 3))?;
        let name = rustls::pki_types::ServerName::IpAddress(addr.ip().into());
        let conn = rustls::ClientConnection::new(Arc::clone(&self.tls), name)?;
        Ok(BufReader::new(rustls::StreamOwned::new(conn, tcp)))
    }

    /// The receiver's name, or `None` if it does not want us.
    pub fn discover(&self, addr: SocketAddr) -> Result<Option<String>> {
        let mut s = self.connect(addr)?;
        let (code, body) = post_plist(&mut s, "/Discover", Dictionary::new(), "close")?;
        if code != 200 {
            return Ok(None);
        }
        let v = crate::airdrop::parse_plist(&body).context("/Discover: risposta non valida")?;
        Ok(v.as_dictionary().and_then(|d| d.get("ReceiverComputerName")).and_then(Value::as_string).map(str::to_string))
    }

    /// Asks, waits for the person at the phone, then uploads.
    pub fn send(&self, addr: SocketAddr, files: &[PathBuf], cancel: &AtomicBool, progress: &dyn Fn(u64, u64)) -> Result<Outcome> {
        if files.is_empty() {
            bail!("nessun file da inviare");
        }
        let transfer = uuid();
        let mut list = Vec::new();
        for f in files {
            let name = file_name(f)?;
            let mut e = Dictionary::new();
            e.insert("FileName".into(), name.clone().into());
            e.insert("FileType".into(), uti(f).into());
            e.insert("FileBomPath".into(), format!("./{name}").into());
            e.insert("FileIsDirectory".into(), false.into());
            e.insert("ConvertMediaFormats".into(), 0.into());
            list.push(Value::Dictionary(e));
        }
        let mut ask = Dictionary::new();
        ask.insert("SenderComputerName".into(), self.name.clone().into());
        ask.insert("BundleID".into(), "com.apple.finder".into());
        ask.insert("SenderModelName".into(), crate::airdrop::MODEL.into());
        ask.insert("SenderID".into(), self.service_id.clone().into());
        ask.insert("ConvertMediaFormats".into(), false.into());
        let mut id = Dictionary::new();
        id.insert("id".into(), transfer.clone().into());
        ask.insert("TransferID".into(), Value::Dictionary(id));
        let mut kind = Dictionary::new();
        kind.insert("files".into(), Value::Dictionary(Dictionary::new()));
        ask.insert("TransferType".into(), Value::Dictionary(kind));
        ask.insert("Files".into(), Value::Array(list));

        // Build the body first: TotalBytes must be known before it is sent.
        // Not /tmp: it is RAM (tmpfs) here, and a video can be gigabytes.
        let spool = crate::config::cache_dir();
        std::fs::create_dir_all(&spool)?;
        let mut body = tempfile::tempfile_in(&spool).context("file temporaneo")?;
        pack(files, &mut body)?;
        let total = body.stream_position()?;
        body.rewind()?;

        let mut s = self.connect(addr)?;
        // A person is deciding: wait up to ASK_WAIT, interruptible.
        let (code, _) = post_plist_wait(&mut s, "/Ask", ask, "keep-alive", Some((cancel, ASK_WAIT)))?;
        match code {
            200 => {}
            403 | 401 => return Ok(Outcome::Declined),
            c => bail!("/Ask rifiutato dal telefono (HTTP {c})"),
        }

        let headers = [
            ("User-Agent", "AirDrop/1.0".to_string()),
            ("TotalBytes", total.to_string()),
            ("Content-Type", "application/x-dvzip".to_string()),
            ("SenderPseudonym", self.pseudonym.clone()),
            ("SenderPushToken", self.push_token.clone()),
            ("TransferID", transfer),
            ("Connection", "keep-alive".to_string()),
            ("Transfer-Encoding", "chunked".to_string()),
        ];
        let w = s.get_mut();
        w.sock.set_write_timeout(Some(STALL))?;
        http::write_request_head(w, "/Upload", &headers)?;
        write!(w, "{total:x}\r\n")?;
        let mut buf = vec![0u8; SLICE];
        let mut sent = 0;
        loop {
            if cancel.load(Ordering::SeqCst) {
                bail!("interrotto");
            }
            let n = body.read(&mut buf)?;
            if n == 0 {
                break;
            }
            w.write_all(&buf[..n]).map_err(stalled)?;
            sent += n as u64;
            progress(sent, total);
        }
        w.write_all(b"\r\n0\r\n\r\n").map_err(stalled)?;
        w.flush().map_err(stalled)?;
        w.sock.set_read_timeout(Some(Duration::from_secs(120)))?;
        let head = http::read_head(&mut s)?.context("il telefono ha chiuso senza rispondere all'invio")?;
        match head.status() {
            200 => Ok(Outcome::Sent),
            c => bail!("invio rifiutato dal telefono (HTTP {c})"),
        }
    }
}

fn stalled(e: io::Error) -> anyhow::Error {
    if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) {
        anyhow::anyhow!("il telefono ha smesso di ricevere (nessuna conferma per {}s)", STALL.as_secs())
    } else {
        e.into()
    }
}

/// POSTs a binary plist the way iOS does (chunked, no Content-Type); returns status and body.
fn post_plist(s: &mut Tls, path: &str, d: Dictionary, connection: &str) -> Result<(u16, Vec<u8>)> {
    post_plist_wait(s, path, d, connection, None)
}

/// As [`post_plist`]; with `wait`, polls for the answer once a second until
/// `wait.0` is set or `wait.1` elapses (a person is deciding on the phone).
fn post_plist_wait(
    s: &mut Tls,
    path: &str,
    d: Dictionary,
    connection: &str,
    wait: Option<(&AtomicBool, Duration)>,
) -> Result<(u16, Vec<u8>)> {
    let mut body = Vec::new();
    Value::Dictionary(d).to_writer_binary(&mut body)?;
    let w = s.get_mut();
    let headers = [
        ("User-Agent", "AirDrop/1.0".to_string()),
        ("Connection", connection.to_string()),
        ("Transfer-Encoding", "chunked".to_string()),
    ];
    http::write_request_head(w, path, &headers)?;
    let mut c = http::ChunkedWriter::new(&mut *w);
    c.write_all(&body)?;
    c.finish()?;
    if let Some((cancel, limit)) = wait {
        // Wait for plaintext, not for socket bytes: a TLS 1.3 session ticket
        // makes the socket readable long before the answer. fill_buf with a
        // 1 s timeout returns WouldBlock without losing anything.
        s.get_ref().sock.set_read_timeout(Some(Duration::from_secs(1)))?;
        let t = std::time::Instant::now();
        loop {
            match s.fill_buf() {
                Ok([]) => bail!("{path}: connessione chiusa senza risposta"),
                Ok(_) => break,
                Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {}
                Err(e) => return Err(e.into()),
            }
            if cancel.load(Ordering::SeqCst) {
                bail!("interrotto");
            }
            if t.elapsed() > limit {
                bail!("nessuna risposta dall'iPhone in {} s", limit.as_secs());
            }
        }
        s.get_ref().sock.set_read_timeout(Some(CONNECT * 3))?;
    }
    let head = http::read_head(s)?.with_context(|| format!("{path}: connessione chiusa senza risposta"))?;
    let body = http::read_small(&head, s, 4 << 20)?;
    Ok((head.status(), body))
}

/// dvzip(cpio) of the files, as Apple lays it out: a `.` directory, then `./name`.
fn pack(files: &[PathBuf], out: &mut File) -> Result<()> {
    let mut w = archive::Writer::new(dvzip::Writer::new(io::BufWriter::new(&mut *out)));
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs();
    w.dir(".", now)?;
    for f in files {
        let meta = f.metadata().with_context(|| format!("{}", f.display()))?;
        if !meta.is_file() {
            bail!("{} non è un file normale", f.display());
        }
        let mtime = meta.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(now, |d| d.as_secs());
        let mut src = File::open(f).with_context(|| format!("apro {}", f.display()))?;
        w.file(&format!("./{}", file_name(f)?), meta.len(), mtime, &mut src)?;
    }
    w.finish()?.finish()?.flush()?;
    Ok(())
}

fn file_name(p: &Path) -> Result<String> {
    Ok(p.file_name().with_context(|| format!("{} non ha un nome", p.display()))?.to_string_lossy().into_owned())
}

fn uuid() -> String {
    let mut b = random::<16>();
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h = hex(&b).to_uppercase();
    format!("{}-{}-{}-{}-{}", &h[..8], &h[8..12], &h[12..16], &h[16..20], &h[20..])
}

/// Apple Uniform Type Identifier from the extension: the phone uses it to
/// pick where the file goes (Photos, Files, ...).
pub fn uti(p: &Path) -> &'static str {
    let ext = p.extension().map(|e| e.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
    match ext.as_str() {
        "jpg" | "jpeg" => "public.jpeg",
        "png" => "public.png",
        "heic" => "public.heic",
        "heif" => "public.heif",
        "gif" => "com.compuserve.gif",
        "tif" | "tiff" => "public.tiff",
        "webp" => "org.webmproject.webp",
        "mp4" | "m4v" => "public.mpeg-4",
        "mov" => "com.apple.quicktime-movie",
        "mp3" => "public.mp3",
        "m4a" => "com.apple.m4a-audio",
        "wav" => "com.microsoft.waveform-audio",
        "pdf" => "com.adobe.pdf",
        "txt" | "md" | "log" => "public.plain-text",
        "rtf" => "public.rtf",
        "html" | "htm" => "public.html",
        "zip" => "public.zip-archive",
        "gz" | "tgz" => "org.gnu.gnu-zip-archive",
        "doc" => "com.microsoft.word.doc",
        "docx" => "org.openxmlformats.wordprocessingml.document",
        "xls" => "com.microsoft.excel.xls",
        "xlsx" => "org.openxmlformats.spreadsheetml.sheet",
        "ppt" => "com.microsoft.powerpoint.ppt",
        "pptx" => "org.openxmlformats.presentationml.presentation",
        "vcf" => "public.vcard",
        "ics" => "com.apple.ical.ics",
        _ => "public.data",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::airdrop::{Ask, Receiver, Ui};
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};

    struct Auto {
        yes: bool,
        got: Mutex<Vec<PathBuf>>,
        asked: AtomicBool,
    }
    impl Ui for Arc<Auto> {
        fn ask(&self, a: &Ask) -> bool {
            assert_eq!(a.sender, "Prova");
            self.asked.store(true, Ordering::SeqCst);
            self.yes
        }
        fn received(&self, _: &Ask, paths: &[PathBuf], _: f64) {
            self.got.lock().unwrap().extend_from_slice(paths);
        }
        fn failed(&self, what: &str) {
            panic!("{what}");
        }
    }

    fn setup(yes: bool) -> (tempfile::TempDir, Arc<Auto>, Sender, SocketAddr) {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("Scaricati");
        std::fs::create_dir(&dest).unwrap();
        let ident = Ident::load_or_create(&dir.path().join("id"), "Prova").unwrap();
        let ui = Arc::new(Auto { yes, got: Mutex::new(Vec::new()), asked: AtomicBool::new(false) });
        let rx = Receiver::new("PC".into(), dest, ident.server_config().unwrap(), Box::new(Arc::clone(&ui)));
        let listener = crate::airdrop::listen("lo", 0).unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || rx.serve_on(listener));
        let tx = Sender::new(&ident, "Prova").unwrap();
        (dir, ui, tx, SocketAddr::from((std::net::Ipv6Addr::LOCALHOST, port)))
    }

    #[test]
    fn send_to_ourselves() {
        let (dir, ui, tx, addr) = setup(true);
        assert_eq!(tx.discover(addr).unwrap().as_deref(), Some("PC"));
        let a = dir.path().join("foto.jpg");
        let b = dir.path().join("note.txt");
        let big: Vec<u8> = (0..700_000u32).map(|i| (i * 7 % 256) as u8).collect();
        std::fs::write(&a, &big).unwrap();
        std::fs::write(&b, "ciao").unwrap();
        // A file with the same name already there: the new one gets "(1)".
        std::fs::write(dir.path().join("Scaricati/note.txt"), "vecchio").unwrap();
        let last = Mutex::new((0, 0));
        let out = tx.send(addr, &[a, b], &AtomicBool::new(false), &|s, t| *last.lock().unwrap() = (s, t)).unwrap();
        assert!(matches!(out, Outcome::Sent));
        let (s, t) = *last.lock().unwrap();
        assert_eq!(s, t);
        let got = ui.got.lock().unwrap().clone();
        assert_eq!(got.len(), 2);
        assert_eq!(std::fs::read(dir.path().join("Scaricati/foto.jpg")).unwrap(), big);
        assert_eq!(std::fs::read_to_string(dir.path().join("Scaricati/note (1).txt")).unwrap(), "ciao");
        assert!(!std::fs::read_dir(dir.path().join("Scaricati")).unwrap().any(|e| {
            e.unwrap().file_name().to_string_lossy().starts_with(".lindrop-")
        }));
    }

    #[test]
    fn declined() {
        let (dir, ui, tx, addr) = setup(false);
        let a = dir.path().join("x.txt");
        std::fs::write(&a, "x").unwrap();
        assert!(matches!(tx.send(addr, &[a], &AtomicBool::new(false), &|_, _| {}).unwrap(), Outcome::Declined));
        assert!(ui.asked.load(Ordering::SeqCst));
        assert!(ui.got.lock().unwrap().is_empty());
    }

    #[test]
    fn cancel_while_the_phone_decides() {
        struct Slow(Arc<AtomicBool>);
        impl Ui for Slow {
            fn ask(&self, _: &Ask) -> bool {
                std::thread::sleep(Duration::from_secs(4));
                true
            }
            fn received(&self, _: &Ask, _: &[PathBuf], _: f64) {
                self.0.store(true, Ordering::SeqCst);
            }
            fn failed(&self, _: &str) {}
        }
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("Scaricati");
        std::fs::create_dir(&dest).unwrap();
        let ident = Ident::load_or_create(&dir.path().join("id"), "Prova").unwrap();
        let uploaded = Arc::new(AtomicBool::new(false));
        let rx = Receiver::new("PC".into(), dest, ident.server_config().unwrap(), Box::new(Slow(Arc::clone(&uploaded))));
        let listener = crate::airdrop::listen("lo", 0).unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || rx.serve_on(listener));
        let tx = Sender::new(&ident, "Prova").unwrap();
        let a = dir.path().join("x.txt");
        std::fs::write(&a, "x").unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let c = Arc::clone(&cancel);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(500));
            c.store(true, Ordering::SeqCst);
        });
        let t = std::time::Instant::now();
        let addr = SocketAddr::from((std::net::Ipv6Addr::LOCALHOST, port));
        assert!(tx.send(addr, &[a], &cancel, &|_, _| {}).is_err());
        assert!(t.elapsed() < Duration::from_secs(3), "cancel took {:?}", t.elapsed());
        std::thread::sleep(Duration::from_secs(5));
        assert!(!uploaded.load(Ordering::SeqCst), "the file was sent after cancel");
    }

    #[test]
    fn utis() {
        assert_eq!(uti(Path::new("a.JPG")), "public.jpeg");
        assert_eq!(uti(Path::new("senza")), "public.data");
    }
}
