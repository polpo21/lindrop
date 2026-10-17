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
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use plist::{Dictionary, Value};

use crate::{archive, dvzip, http};

pub const PORT: u16 = 8771;
/// Model shown on the phone; a Mac model makes iOS draw a laptop.
pub const MODEL: &str = "MacBookPro18,1";
/// Discover/Ask bodies are a few kB; a sender icon at most a few hundred.
const MAX_PLIST: u64 = 512 << 10;
/// Nesting a plist deeper than this is an attack (dropping a deep value
/// recurses and overflows the stack), not an AirDrop request.
const MAX_DEPTH: usize = 16;
/// Connections served at once; more are closed straight away.
const MAX_CONNECTIONS: usize = 8;
/// Slack over the sizes declared in /Ask: cpio headers, AppleDouble files.
const UPLOAD_SLACK: u64 = 16 << 20;
/// Without declared sizes (or for folders) at most this much.
const UPLOAD_UNDECLARED: u64 = 8 << 30;
const MAX_ENTRIES: usize = 10_000;
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
        let v = parse_plist(body).context("/Ask: plist non valido")?;
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
    connections: AtomicUsize,
    /// One consent prompt at a time: a second /Ask meanwhile is refused.
    asking: Mutex<()>,
}

impl Receiver {
    pub fn new(name: String, dest: PathBuf, tls: Arc<rustls::ServerConfig>, ui: Box<dyn Ui>) -> Arc<Self> {
        Arc::new(Self {
            name,
            dest,
            ui,
            tls,
            accepted: Mutex::new(HashMap::new()),
            connections: AtomicUsize::new(0),
            asking: Mutex::new(()),
        })
    }

    /// Serves `listener` (see [`listen`]); one thread per connection.
    pub fn serve_on(self: &Arc<Self>, listener: TcpListener) {
        for conn in listener.incoming() {
            let tcp = match conn {
                Ok(t) => t,
                Err(e) => {
                    // Out of file descriptors or similar: do not spin.
                    tracing::warn!("accept: {e}");
                    std::thread::sleep(Duration::from_millis(200));
                    continue;
                }
            };
            if self.connections.fetch_add(1, Ordering::SeqCst) >= MAX_CONNECTIONS {
                self.connections.fetch_sub(1, Ordering::SeqCst);
                tracing::warn!("troppe connessioni: chiudo quella da {:?}", tcp.peer_addr());
                continue;
            }
            let me = Arc::clone(self);
            std::thread::spawn(move || {
                let peer = tcp.peer_addr().map(|a| a.to_string()).unwrap_or_default();
                if let Err(e) = me.connection(tcp) {
                    tracing::debug!("connessione {peer}: {e:#}");
                }
                me.connections.fetch_sub(1, Ordering::SeqCst);
            });
        }
    }

    fn connection(&self, tcp: TcpStream) -> Result<()> {
        let peer = tcp.peer_addr()?.ip();
        tcp.set_read_timeout(Some(IDLE))?;
        // A peer that stops reading must not hold the thread forever.
        tcp.set_write_timeout(Some(IDLE))?;
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
        let Ok(_prompt) = self.asking.try_lock() else {
            tracing::warn!("un'altra richiesta è già in attesa di risposta: rifiuto");
            http::respond(s.get_mut(), 403, &[], b"")?;
            return Ok(false);
        };
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

        // Never more than the phone declared (plus headers), and only with room for it.
        let declared: Option<u64> = if ask.files.iter().any(|f| f.dir || f.size.is_none()) {
            None
        } else {
            Some(ask.files.iter().filter_map(|f| f.size).sum())
        };
        let limits = archive::Limits {
            max_bytes: declared.map_or(UPLOAD_UNDECLARED, |d| d + UPLOAD_SLACK),
            max_entries: MAX_ENTRIES,
        };
        if let Some(free) = free_bytes(&self.dest)
            && free < limits.max_bytes.min(declared.unwrap_or(0) + UPLOAD_SLACK)
        {
            tracing::warn!("spazio insufficiente in {}: {}", self.dest.display(), human(free));
            self.ui.failed(&format!("spazio insufficiente in {}", self.dest.display()));
            http::respond(s.get_mut(), 413, &[("Connection", "close")], b"")?;
            return Ok(false);
        }

        let start = Instant::now();
        let staging = self.dest.join(format!(".lindrop-{}", crate::ident::hex(&crate::ident::random::<4>())));
        fs::create_dir_all(&staging)?;
        let result = (|| -> Result<Vec<PathBuf>> {
            let body = http::body(head, s);
            let x = if ctype == "application/x-dvzip" {
                archive::extract(BufReader::new(dvzip::Reader::new(body)), &staging, &limits)?
            } else {
                archive::extract(maybe_gzip(BufReader::new(body))?, &staging, &limits)?
            };
            tracing::info!("ricevuti {} file, {}", x.files.len(), human(x.bytes));
            place(&staging, &x.files, &ask, &self.dest)
        })();
        let _ = fs::remove_dir_all(&staging);

        match result {
            Ok(paths) => {
                http::respond(s.get_mut(), 200, &[("Connection", "close")], b"")?;
                // Only a transfer the phone declared as a link opens the browser.
                if ask.is_link() {
                    let links = take_weblocs(&paths);
                    if !links.is_empty() {
                        open_links(&links);
                    }
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
                let to = move_unique(&staging.join(d), dest, &d.file_name().unwrap().to_string_lossy())?;
                moved_dirs.push(d.to_path_buf());
                out.push(to);
            }
            continue;
        }
        let name = rel.file_name().context("file senza nome")?.to_string_lossy();
        out.push(move_unique(&staging.join(rel), dest, &name)?);
    }
    if out.is_empty() {
        bail!("l'archivio non conteneva file");
    }
    Ok(out)
}

/// Candidate names: `name`, then `stem (1).ext`, `stem (2).ext`, …
fn candidates(dest: &Path, name: &str) -> impl Iterator<Item = PathBuf> {
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s.to_string(), format!(".{e}")),
        _ => (name.to_string(), String::new()),
    };
    let first = dest.join(name);
    let dest = dest.to_path_buf();
    std::iter::once(first).chain((1..).map(move |n| dest.join(format!("{stem} ({n}){ext}"))))
}

/// Moves `src` into `dest` under the first free name. RENAME_NOREPLACE makes
/// the check and the move one step: two transfers never overwrite each other.
fn move_unique(src: &Path, dest: &Path, name: &str) -> Result<PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    let from = std::ffi::CString::new(src.as_os_str().as_bytes())?;
    for to in candidates(dest, name).take(10_000) {
        let c_to = std::ffi::CString::new(to.as_os_str().as_bytes())?;
        let r = unsafe {
            libc::renameat2(libc::AT_FDCWD, from.as_ptr(), libc::AT_FDCWD, c_to.as_ptr(), libc::RENAME_NOREPLACE)
        };
        if r == 0 {
            return Ok(to);
        }
        let e = std::io::Error::last_os_error();
        if e.raw_os_error() != Some(libc::EEXIST) {
            return Err(e).with_context(|| format!("sposto in {}", to.display()));
        }
    }
    bail!("nessun nome libero per {name}")
}

/// Free space for an unprivileged user on the filesystem of `p`.
fn free_bytes(p: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(p.as_os_str().as_bytes()).ok()?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    (unsafe { libc::statvfs(c.as_ptr(), &mut st) } == 0).then(|| st.f_bavail as u64 * st.f_frsize as u64)
}

/// Parses a plist after checking, without building it, that it is not nested
/// deeper than [`MAX_DEPTH`].
pub fn parse_plist(body: &[u8]) -> Result<Value> {
    use plist::stream::{Event, Reader};
    let mut depth = 0usize;
    for ev in Reader::new(std::io::Cursor::new(body)) {
        match ev? {
            Event::StartArray(_) | Event::StartDictionary(_) => {
                depth += 1;
                if depth > MAX_DEPTH {
                    bail!("plist annidato oltre {MAX_DEPTH} livelli");
                }
            }
            Event::EndCollection => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    Ok(Value::from_reader(std::io::Cursor::new(body))?)
}

/// Links arrive as `.webloc` files: read the URL, delete the file.
fn take_weblocs(paths: &[PathBuf]) -> Vec<String> {
    let mut urls = Vec::new();
    for p in paths.iter().filter(|p| p.extension().is_some_and(|e| e == "webloc")) {
        let url = fs::read(p)
            .ok()
            .and_then(|b| parse_plist(&b).ok())
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
        fs::write(d.path().join("a.jpg"), "vecchio").unwrap();
        fs::write(d.path().join("a (1).jpg"), "vecchio").unwrap();
        let src = d.path().join("nuovo.tmp");
        fs::write(&src, "nuovo").unwrap();
        assert_eq!(move_unique(&src, d.path(), "a.jpg").unwrap(), d.path().join("a (2).jpg"));
        assert_eq!(fs::read_to_string(d.path().join("a.jpg")).unwrap(), "vecchio");
        fs::write(&src, "x").unwrap();
        assert_eq!(move_unique(&src, d.path(), "nuovo").unwrap(), d.path().join("nuovo"));
    }

    #[test]
    fn deep_plist_is_refused() {
        let deep = format!(
            "<?xml version=\"1.0\"?><plist version=\"1.0\">{}{}</plist>",
            "<array>".repeat(100),
            "</array>".repeat(100)
        );
        assert!(parse_plist(deep.as_bytes()).is_err());
        let ok = "<?xml version=\"1.0\"?><plist version=\"1.0\"><dict><key>a</key><array><string>x</string></array></dict></plist>";
        assert!(parse_plist(ok.as_bytes()).is_ok());
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
