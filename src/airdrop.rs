//! The AirDrop receiver: HTTPS on port 8771 of `awdl0`.
//!
//! The phone posts `/Discover` (show us in its picker), `/Ask` (here is what
//! I want to send) and `/Upload` (the archive). `/Upload` is only taken from
//! an address whose `/Ask` was accepted in the last minutes, so nothing lands
//! on disk without the user saying yes.

use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{IpAddr, Ipv6Addr, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use plist::{Dictionary, Value};

use crate::{archive, dvzip, http};

pub const PORT: u16 = 8771;
/// Model shown on the phone; a Mac model makes iOS draw a laptop.
pub const MODEL: &str = "MacBookPro18,1";
const MAX_PLIST: u64 = 4 << 20;
/// How long an accepted `/Ask` keeps `/Upload` open.
const ASK_VALID: Duration = Duration::from_secs(300);
const IDLE: Duration = Duration::from_secs(60);

/// What the phone wants to send, from `/Ask`.
#[derive(Debug, Clone, Default)]
pub struct Ask {
    pub transfer_id: String,
    pub sender: String,
    pub files: Vec<AskFile>,
    /// Plain URLs (older senders put links here rather than in a .webloc).
    pub items: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct AskFile {
    pub name: String,
    pub kind: String,
    pub size: Option<u64>,
    pub dir: bool,
}

impl Ask {
    pub fn parse(body: &[u8]) -> Result<Self> {
        let v = Value::from_reader(std::io::Cursor::new(body)).context("/Ask: plist non valido")?;
        let d = v.as_dictionary().context("/Ask: non è un dizionario")?;
        let s = |k: &str| d.get(k).and_then(Value::as_string).unwrap_or_default().to_string();
        let files = d
            .get("Files")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_dictionary)
                    .map(|f| AskFile {
                        name: f.get("FileName").and_then(Value::as_string).unwrap_or_default().to_string(),
                        kind: f.get("FileType").and_then(Value::as_string).unwrap_or_default().to_string(),
                        size: f.get("FileSize").and_then(Value::as_unsigned_integer),
                        dir: f.get("FileIsDirectory").and_then(Value::as_boolean).unwrap_or(false),
                    })
                    .collect()
            })
            .unwrap_or_default();
        let items = d
            .get("Items")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_string).map(str::to_string).collect())
            .unwrap_or_default();
        // A string, or {"id": "<UUID>"} as Apple's own senders put it.
        let transfer_id = match d.get("TransferID") {
            Some(Value::Dictionary(t)) => t.get("id").and_then(Value::as_string).unwrap_or_default().to_string(),
            _ => s("TransferID"),
        };
        Ok(Self { transfer_id, sender: s("SenderComputerName"), files, items })
    }

    pub fn is_link(&self) -> bool {
        !self.items.is_empty()
            || (!self.files.is_empty() && self.files.iter().all(|f| f.kind == "com.apple.web-internet-location"))
    }

    /// One line for a notification: "3 foto (4,2 MB)", "un link", "documento.pdf".
    pub fn summary(&self) -> String {
        if self.is_link() {
            return "un link".into();
        }
        let size: u64 = self.files.iter().filter_map(|f| f.size).sum();
        let what = match self.files.as_slice() {
            [one] => one.name.clone(),
            many if many.iter().all(|f| f.kind.starts_with("public.") && is_image(&f.kind)) => {
                format!("{} foto", many.len())
            }
            many => format!("{} file", many.len()),
        };
        if size > 0 { format!("{what} ({})", human(size)) } else { what }
    }
}

fn is_image(uti: &str) -> bool {
    matches!(uti, "public.jpeg" | "public.heic" | "public.heif" | "public.png" | "public.image" | "com.compuserve.gif")
}

pub fn human(b: u64) -> String {
    let b = b as f64;
    let s = if b >= 1e9 {
        format!("{:.1} GB", b / 1e9)
    } else if b >= 1e6 {
        format!("{:.1} MB", b / 1e6)
    } else if b >= 1e3 {
        format!("{:.0} kB", b / 1e3)
    } else {
        format!("{b} B")
    };
    s.replace('.', ",")
}

/// The user-facing side: consent and results.
pub trait Ui: Send + Sync {
    /// Called on `/Ask`; `true` accepts.
    fn ask(&self, ask: &Ask) -> bool;
    /// Called once the files are in place.
    fn received(&self, ask: &Ask, paths: &[PathBuf], secs: f64);
    fn failed(&self, what: &str);
}

pub struct Receiver {
    pub name: String,
    pub dest: PathBuf,
    pub ui: Box<dyn Ui>,
    tls: Arc<rustls::ServerConfig>,
    accepted: Mutex<HashMap<IpAddr, (Ask, Instant)>>,
}

impl Receiver {
    pub fn new(name: String, dest: PathBuf, tls: Arc<rustls::ServerConfig>, ui: Box<dyn Ui>) -> Arc<Self> {
        Arc::new(Self { name, dest, ui, tls, accepted: Mutex::new(HashMap::new()) })
    }

    /// Serves `listener` (see [`listen`]); one thread per connection.
    pub fn serve_on(self: &Arc<Self>, listener: TcpListener) {
        for conn in listener.incoming() {
            let Ok(tcp) = conn else { continue };
            let me = Arc::clone(self);
            std::thread::spawn(move || {
                let peer = tcp.peer_addr().map(|a| a.to_string()).unwrap_or_default();
                if let Err(e) = me.connection(tcp) {
                    tracing::debug!("connessione {peer}: {e:#}");
                }
            });
        }
    }

    fn connection(&self, tcp: TcpStream) -> Result<()> {
        let peer = tcp.peer_addr()?.ip();
        tcp.set_read_timeout(Some(IDLE))?;
        tcp.set_nodelay(true)?;
        let tls = rustls::ServerConnection::new(Arc::clone(&self.tls))?;
        let mut s = BufReader::with_capacity(256 << 10, rustls::StreamOwned::new(tls, tcp));
        // iOS keeps the connection alive across requests.
        while let Some(head) = http::read_head(&mut s)? {
            tracing::debug!("{} {} da {peer}: {:?}", head.first.0, head.first.1, head.headers);
            let keep = match (head.first.0.as_str(), head.first.1.as_str()) {
                ("POST", "/Discover") => self.discover(&head, &mut s)?,
                ("POST", "/Ask") => self.on_ask(&head, &mut s, peer)?,
                ("POST", "/Upload") => self.upload(&head, &mut s, peer)?,
                _ => {
                    http::read_small(&head, &mut s, MAX_PLIST)?;
                    http::respond(s.get_mut(), 404, &[], b"")?;
                    true
                }
            };
            if !keep || head.is("connection", "close") {
                break;
            }
        }
        let tls = s.get_mut();
        tls.conn.send_close_notify();
        let _ = tls.flush();
        Ok(())
    }

    fn discover<S: BufRead + Write>(&self, head: &http::Head, s: &mut BufReader<S>) -> Result<bool>
    where
        BufReader<S>: BufRead,
    {
        http::read_small(head, s, MAX_PLIST)?;
        let mut d = Dictionary::new();
        d.insert("ReceiverComputerName".into(), self.name.clone().into());
        d.insert("ReceiverModelName".into(), MODEL.into());
        // No codecs advertised: the phone converts HEIC/HEVC to JPEG/H.264.
        d.insert("ReceiverMediaCapabilities".into(), Value::Data(br#"{"Version":1}"#.to_vec()));
        reply_plist(s.get_mut(), d)?;
        Ok(true)
    }

    fn on_ask<S: Read + Write>(&self, head: &http::Head, s: &mut BufReader<S>, peer: IpAddr) -> Result<bool> {
        let body = http::read_small(head, s, MAX_PLIST)?;
        let ask = match Ask::parse(&body) {
            Ok(a) => a,
            Err(e) => {
                tracing::warn!("{e:#}");
                http::respond(s.get_mut(), 400, &[], b"")?;
                return Ok(false);
            }
        };
        tracing::info!("richiesta da {:?}: {} (transfer {})", ask.sender, ask.summary(), ask.transfer_id);
        if !self.ui.ask(&ask) {
            http::respond(s.get_mut(), 403, &[], b"")?;
            return Ok(false);
        }
        if !ask.items.is_empty() {
            open_links(&ask.items);
        }
        self.accepted.lock().unwrap().insert(peer, (ask, Instant::now()));
        let mut d = Dictionary::new();
        d.insert("ReceiverComputerName".into(), self.name.clone().into());
        d.insert("ReceiverModelName".into(), MODEL.into());
        reply_plist(s.get_mut(), d)?;
        Ok(true)
    }

    fn upload<S: Read + Write>(&self, head: &http::Head, s: &mut BufReader<S>, peer: IpAddr) -> Result<bool> {
        let ask = {
            let mut acc = self.accepted.lock().unwrap();
            acc.retain(|_, (_, t)| t.elapsed() < ASK_VALID);
            acc.remove(&peer).map(|(a, _)| a)
        };
        let Some(ask) = ask else {
            tracing::warn!("/Upload da {peer} senza un /Ask accettato: rifiuto");
            http::respond(s.get_mut(), 403, &[("Connection", "close")], b"")?;
            return Ok(false);
        };
        let ctype = head.header("content-type").unwrap_or_default().to_ascii_lowercase();
        if ctype != "application/x-dvzip" && ctype != "application/x-cpio" {
            tracing::warn!("/Upload: formato {ctype:?} sconosciuto");
            http::respond(s.get_mut(), 406, &[("Content-Type", "application/x-cpio"), ("Connection", "close")], b"")?;
            return Ok(false);
        }
        if head.is("expect", "100-continue") {
            s.get_mut().write_all(b"HTTP/1.1 100 Continue\r\n\r\n")?;
            s.get_mut().flush()?;
        }

        let start = Instant::now();
        let staging = self.dest.join(format!(".lindrop-{}", crate::ident::hex(&crate::ident::random::<4>())));
        fs::create_dir_all(&staging)?;
        let result = (|| -> Result<Vec<PathBuf>> {
            let body = http::body(head, s);
            let x = if ctype == "application/x-dvzip" {
                archive::extract(BufReader::new(dvzip::Reader::new(body)), &staging)?
            } else {
                archive::extract(maybe_gzip(BufReader::new(body))?, &staging)?
            };
            tracing::info!("ricevuti {} file, {}", x.files.len(), human(x.bytes));
            place(&staging, &x.files, &ask, &self.dest)
        })();
        let _ = fs::remove_dir_all(&staging);

        match result {
            Ok(paths) => {
                http::respond(s.get_mut(), 200, &[("Connection", "close")], b"")?;
                let links = take_weblocs(&paths);
                if !links.is_empty() {
                    open_links(&links);
                }
                let files: Vec<_> = paths.into_iter().filter(|p| p.exists()).collect();
                if !files.is_empty() {
                    self.ui.received(&ask, &files, start.elapsed().as_secs_f64());
                }
            }
            Err(e) => {
                tracing::warn!("/Upload fallito: {e:#}");
                self.ui.failed(&format!("ricezione da {} interrotta", ask.sender));
                let _ = http::respond(s.get_mut(), 500, &[("Connection", "close")], b"");
            }
        }
        Ok(false)
    }
}

/// `application/x-cpio` may or may not be gzipped; look at the magic.
fn maybe_gzip<'a, R: BufRead + 'a>(mut r: R) -> Result<Box<dyn Read + 'a>> {
    let gz = r.fill_buf()?.starts_with(&[0x1f, 0x8b]);
    Ok(if gz { Box::new(flate2::bufread::MultiGzDecoder::new(r)) } else { Box::new(r) })
}

/// Moves what was extracted into `dest`. iOS wraps the payload in a folder of
/// its own; only folders the phone said were folders are kept, everything
/// else lands flat, renamed `foto (1).jpg` on a clash.
fn place(staging: &Path, files: &[PathBuf], ask: &Ask, dest: &Path) -> Result<Vec<PathBuf>> {
    let dirs: Vec<&str> = ask.files.iter().filter(|f| f.dir).map(|f| f.name.as_str()).collect();
    let mut out = Vec::new();
    let mut moved_dirs = Vec::new();
    for rel in files {
        // `._name` are AppleDouble metadata, not content.
        if rel.file_name().is_some_and(|n| n.to_string_lossy().starts_with("._")) {
            continue;
        }
        // A file inside a folder the phone sent as a folder: move the folder once.
        if let Some(d) = rel.ancestors().find(|a| a.file_name().is_some_and(|n| dirs.iter().any(|d| n == *d))) {
            if !moved_dirs.contains(&d.to_path_buf()) {
                let to = unique(dest, &d.file_name().unwrap().to_string_lossy());
                fs::rename(staging.join(d), &to)?;
                moved_dirs.push(d.to_path_buf());
                out.push(to);
            }
            continue;
        }
        let name = rel.file_name().context("file senza nome")?.to_string_lossy();
        let to = unique(dest, &name);
        fs::rename(staging.join(rel), &to)?;
        out.push(to);
    }
    if out.is_empty() {
        bail!("l'archivio non conteneva file");
    }
    Ok(out)
}

/// `dest/name`, or `dest/stem (n).ext` if taken.
fn unique(dest: &Path, name: &str) -> PathBuf {
    let p = dest.join(name);
    if !p.exists() {
        return p;
    }
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s, format!(".{e}")),
        _ => (name, String::new()),
    };
    (1..).map(|n| dest.join(format!("{stem} ({n}){ext}"))).find(|p| !p.exists()).unwrap()
}

/// Links arrive as `.webloc` files: read the URL, delete the file.
fn take_weblocs(paths: &[PathBuf]) -> Vec<String> {
    let mut urls = Vec::new();
    for p in paths.iter().filter(|p| p.extension().is_some_and(|e| e == "webloc")) {
        let url = Value::from_file(p)
            .ok()
            .and_then(|v| v.as_dictionary().and_then(|d| d.get("URL")).and_then(Value::as_string).map(str::to_string));
        if let Some(u) = url {
            urls.push(u);
            let _ = fs::remove_file(p);
        }
    }
    urls
}

fn open_links(urls: &[String]) {
    for u in urls {
        if u.starts_with("https://") || u.starts_with("http://") {
            tracing::info!("apro {u}");
            let _ = std::process::Command::new("xdg-open").arg(u).spawn();
        } else {
            tracing::warn!("link non aperto (schema non http): {u}");
        }
    }
}

fn reply_plist(w: &mut impl Write, d: Dictionary) -> Result<()> {
    let mut body = Vec::new();
    Value::Dictionary(d).to_writer_binary(&mut body)?;
    http::respond(w, 200, &[("Content-Type", "application/octet-stream")], &body)?;
    Ok(())
}

pub fn listen(iface: &str, port: u16) -> Result<TcpListener> {
    use socket2::{Domain, Socket, Type};
    let s = Socket::new(Domain::IPV6, Type::STREAM, None)?;
    s.set_only_v6(true)?;
    s.set_reuse_address(true)?;
    // Before listen(), so accepted sockets inherit it: AWDL delivers in bursts.
    s.set_recv_buffer_size(4 << 20)?;
    s.bind_device(Some(iface.as_bytes())).with_context(|| format!("non riesco a legarmi a {iface}"))?;
    s.bind(&SocketAddr::from((Ipv6Addr::UNSPECIFIED, port)).into())
        .with_context(|| format!("porta {port} occupata"))?;
    s.listen(16)?;
    Ok(s.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ask_summary() {
        let mut a = Ask::default();
        a.files = vec![
            AskFile { name: "IMG_1.jpg".into(), kind: "public.jpeg".into(), size: Some(2_100_000), dir: false },
            AskFile { name: "IMG_2.jpg".into(), kind: "public.jpeg".into(), size: Some(2_100_000), dir: false },
        ];
        assert_eq!(a.summary(), "2 foto (4,2 MB)");
        a.files.truncate(1);
        assert_eq!(a.summary(), "IMG_1.jpg (2,1 MB)");
    }

    #[test]
    fn unique_names() {
        let d = tempfile::tempdir().unwrap();
        fs::write(d.path().join("a.jpg"), "").unwrap();
        fs::write(d.path().join("a (1).jpg"), "").unwrap();
        assert_eq!(unique(d.path(), "a.jpg"), d.path().join("a (2).jpg"));
        assert_eq!(unique(d.path(), "nuovo"), d.path().join("nuovo"));
    }

    #[test]
    fn place_flattens_wrapper() {
        let st = tempfile::tempdir().unwrap();
        let dest = tempfile::tempdir().unwrap();
        fs::create_dir_all(st.path().join("wrap/Album")).unwrap();
        fs::write(st.path().join("wrap/IMG.jpg"), "x").unwrap();
        fs::write(st.path().join("wrap/._IMG.jpg"), "meta").unwrap();
        fs::write(st.path().join("wrap/Album/b.txt"), "y").unwrap();
        let ask = Ask {
            files: vec![
                AskFile { name: "IMG.jpg".into(), ..Default::default() },
                AskFile { name: "Album".into(), dir: true, ..Default::default() },
            ],
            ..Default::default()
        };
        let files = vec![PathBuf::from("wrap/._IMG.jpg"), PathBuf::from("wrap/IMG.jpg"), PathBuf::from("wrap/Album/b.txt")];
        let out = place(st.path(), &files, &ask, dest.path()).unwrap();
        assert_eq!(out, vec![dest.path().join("IMG.jpg"), dest.path().join("Album")]);
        assert!(dest.path().join("Album/b.txt").exists());
    }
}
