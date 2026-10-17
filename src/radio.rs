//! `lindrop radio`: the only privileged part, run as root by systemd.
//!
//! It owns the Wi-Fi card for the length of a session: takes it from
//! NetworkManager, builds the monitor interfaces, starts the AWDL daemon,
//! opens the firewall on `awdl0`, and undoes every step when the client
//! hangs up, the AWDL daemon dies, or systemd stops the service. Each change
//! is recorded with its inverse in /run/lindrop/sessione.json before it is
//! made, so even a crash is undone on the next start.
//!
//! The socket speaks one JSON object per line and accepts only fixed
//! requests; nothing the client sends reaches a command line except a
//! channel from a fixed list and a two-letter country.

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

pub const SOCKET: &str = "/run/lindrop/radio.sock";
const STATE: &str = "/run/lindrop/sessione.json";
const OWL_LOG: &str = "/run/lindrop/owl.log";
const USERS: &str = "/etc/lindrop/utenti";
/// Root-owned AWDL daemon (phase 1: OWL). Never a user-writable path.
const OWL: &[&str] = &["/usr/lib/lindrop/owl", "/usr/local/lib/lindrop/owl"];
const MON: &str = "ldmon0";
const MONA: &str = "ldmon1";
pub const AWDL: &str = "awdl0";
const IDLE_EXIT: Duration = Duration::from_secs(120);

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Request {
    /// Bring AWDL up for this connection; it goes down when the connection closes.
    Su { canale: u8, paese: String },
    Stato,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Reply {
    Pronto { iface: String, scheda: String, canale: u8 },
    Stato { attivo: bool, scheda: Option<String> },
    /// The session ended on its own (AWDL daemon died).
    Caduto { motivo: String },
    Errore { messaggio: String },
}

/// One recorded change and how to take it back.
#[derive(Debug, Clone, Serialize, Deserialize)]
enum Undo {
    StartUnit(String),
    NmManaged(String),
    DelIface(String),
    LinkUp(String),
    Debugfs(PathBuf, String),
    UfwDelete,
    Kill(u32),
}

struct Card {
    iface: String,
    phy: String,
    driver: String,
}

impl Card {
    /// The first Wi-Fi interface in station mode, and what drives it.
    fn find() -> Result<Self> {
        let mut found = Vec::new();
        for e in fs::read_dir("/sys/class/net")? {
            let p = e?.path();
            if !p.join("wireless").exists() && !p.join("phy80211").exists() {
                continue;
            }
            let iface = p.file_name().unwrap().to_string_lossy().into_owned();
            if [MON, MONA, AWDL].contains(&iface.as_str()) {
                continue;
            }
            let link = |rel: &str| fs::read_link(p.join(rel)).ok().and_then(|l| l.file_name().map(|n| n.to_string_lossy().into_owned()));
            if let (Some(phy), Some(driver)) = (link("phy80211"), link("device/driver")) {
                found.push(Card { iface, phy, driver });
            }
        }
        found.sort_by(|a, b| a.iface.cmp(&b.iface));
        found.into_iter().next().context("nessuna scheda Wi-Fi trovata")
    }

    fn debugfs(&self) -> PathBuf {
        Path::new("/sys/kernel/debug/ieee80211").join(&self.phy).join("mt76")
    }
}

/// Supported cards. Phase 1: the recipe proven on MT7921/MT7922.
fn supported(driver: &str) -> bool {
    driver == "mt7921e"
}

fn mhz(ch: u8) -> Option<u32> {
    match ch {
        6 => Some(2437),
        36 => Some(5180),
        44 => Some(5220),
        149 => Some(5745),
        _ => None,
    }
}

struct Session {
    undo: Vec<Undo>,
    owl: Option<Child>,
    card: String,
    canale: u8,
}

impl Session {
    fn record(&mut self, u: Undo) -> Result<()> {
        self.undo.push(u);
        save(&self.undo)
    }

    fn up(canale: u8, paese: &str) -> Result<Self> {
        if canale != 0 && mhz(canale).is_none() {
            bail!("canale {canale} non valido (0, 6, 36, 44 o 149)");
        }
        if !(paese.is_empty() || (paese.len() == 2 && paese.bytes().all(|b| b.is_ascii_uppercase()))) {
            bail!("paese {paese:?} non valido (due lettere maiuscole, es. IT)");
        }
        let card = Card::find()?;
        if !supported(&card.driver) {
            bail!("la scheda {} (driver {}) non è ancora supportata", card.iface, card.driver);
        }
        let owl = OWL.iter().map(Path::new).find(|p| root_owned(p)).context("OWL non installato (vedi scripts/owl.sh)")?;
        let mut s = Session { undo: Vec::new(), owl: None, card: format!("{} ({})", card.iface, card.driver), canale: 0 };
        if let Err(e) = s.build(&card, canale, paese, owl) {
            s.down();
            return Err(e);
        }
        Ok(s)
    }

    fn build(&mut self, card: &Card, canale: u8, paese: &str, owl: &Path) -> Result<()> {
        // Automatic: the channel the station is on now. A phone on the same
        // Wi-Fi spends most of its AWDL slots, multicast slot 0 included, there.
        let canale = match canale {
            0 => station_channel(&card.iface).filter(|c| mhz(*c).is_some()).unwrap_or(149),
            c => c,
        };
        let freq = mhz(canale).expect("validated");
        self.canale = canale;
        tracing::info!("canale AWDL {canale} ({freq} MHz)");
        // Read the country before letting go of the AP: it is the AP's IE that sets it.
        let paese = if paese.is_empty() { current_country().unwrap_or_default() } else { paese.to_string() };
        if paese.is_empty() && canale != 6 {
            bail!("paese sconosciuto: imposta `paese` nel config (senza, il canale {canale} non può trasmettere)");
        }

        // Take the card from the station side. iwd keeps the interface even
        // when NetworkManager lets go of it, so it is stopped outright.
        if active("iwd") {
            self.record(Undo::StartUnit("iwd".into()))?;
            run("systemctl", &["stop", "iwd"])?;
        } else if active("NetworkManager") {
            self.record(Undo::NmManaged(card.iface.clone()))?;
            run("nmcli", &["device", "set", &card.iface, "managed", "no"])?;
        }
        if !paese.is_empty() {
            run("iw", &["reg", "set", &paese])?;
        }
        self.record(Undo::LinkUp(card.iface.clone()))?;
        run("ip", &["link", "set", &card.iface, "down"])?;

        if !Path::new("/sys/kernel/debug/ieee80211").exists() {
            let _ = run("mount", &["-t", "debugfs", "none", "/sys/kernel/debug"]);
        }
        // The pair (airdrop-mt7921 F§14): a plain monitor owns the channel,
        // the `active` one beside it rides that channel and ACKs unicast.
        for (name, active_flag) in [(MON, false), (MONA, true)] {
            let _ = run("iw", &["dev", name, "del"]);
            self.record(Undo::DelIface(name.into()))?;
            let mut args = vec!["phy", &card.phy, "interface", "add", name, "type", "monitor"];
            if active_flag {
                args.extend(["flags", "active"]);
            }
            run("iw", &args)?;
            run("ip", &["link", "set", name, "up"])?;
            // Runtime PM silently kills monitor RX on mt76 (F§9).
            self.pm_off(card)?;
            if !active_flag {
                run("iw", &["dev", name, "set", "freq", &freq.to_string()])?;
            }
        }

        self.record(Undo::DelIface(AWDL.into()))?;
        let log = fs::File::create(OWL_LOG)?;
        let child = Command::new(owl)
            .args(["-i", MONA, "-c", &canale.to_string(), "-N", "-S", "verbatim", "-v"])
            .stdout(log.try_clone()?)
            .stderr(log)
            .stdin(Stdio::null())
            .spawn()
            .context("avvio di OWL")?;
        self.record(Undo::Kill(child.id()))?;
        self.owl = Some(child);

        let t = Instant::now();
        while !Path::new("/sys/class/net").join(AWDL).exists() {
            if t.elapsed() > Duration::from_secs(6) || self.owl_dead().is_some() {
                bail!("OWL non ha creato {AWDL} (log in {OWL_LOG})");
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let _ = run("ip", &["link", "set", AWDL, "up"]);
        let _ = run("nmcli", &["device", "set", AWDL, "managed", "no"]);

        if ufw_active() {
            self.record(Undo::UfwDelete)?;
            run("ufw", &["insert", "1", "allow", "in", "on", AWDL, "comment", "lindrop"])?;
        }
        Ok(())
    }

    fn pm_off(&mut self, card: &Card) -> Result<()> {
        for f in ["runtime-pm", "deep-sleep"] {
            let p = card.debugfs().join(f);
            if let Ok(old) = fs::read_to_string(&p) {
                if !self.undo.iter().any(|u| matches!(u, Undo::Debugfs(q, _) if *q == p)) {
                    self.record(Undo::Debugfs(p.clone(), old.trim().to_string()))?;
                }
                fs::write(&p, "0").with_context(|| format!("scrivo {}", p.display()))?;
            }
        }
        Ok(())
    }

    fn owl_dead(&mut self) -> Option<String> {
        let c = self.owl.as_mut()?;
        match c.try_wait() {
            Ok(Some(st)) => Some(format!("OWL è uscito ({st})")),
            _ => None,
        }
    }

    fn down(&mut self) {
        if let Some(mut c) = self.owl.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
        undo_all(std::mem::take(&mut self.undo));
    }
}

fn undo_all(steps: Vec<Undo>) {
    for u in steps.into_iter().rev() {
        let r = match &u {
            Undo::StartUnit(unit) => run("systemctl", &["start", unit]),
            Undo::NmManaged(i) => run("nmcli", &["device", "set", i, "managed", "yes"]),
            Undo::DelIface(i) => {
                if Path::new("/sys/class/net").join(i).exists() {
                    run("iw", &["dev", i, "del"])
                } else {
                    Ok(())
                }
            }
            Undo::LinkUp(i) => run("ip", &["link", "set", i, "up"]),
            Undo::Debugfs(p, v) => fs::write(p, v).map_err(Into::into),
            Undo::UfwDelete => run("ufw", &["delete", "allow", "in", "on", AWDL]),
            Undo::Kill(pid) => {
                // Only if it is still our OWL, not a recycled pid.
                let exe = fs::read_link(format!("/proc/{pid}/exe")).ok();
                if exe.is_some_and(|e| OWL.iter().any(|o| e == Path::new(o))) {
                    unsafe { libc::kill(*pid as i32, libc::SIGTERM) };
                }
                Ok(())
            }
        };
        if let Err(e) = r {
            tracing::warn!("ripristino {u:?}: {e:#}");
        }
    }
    let _ = fs::remove_file(STATE);
}

fn save(undo: &[Undo]) -> Result<()> {
    fs::create_dir_all("/run/lindrop")?;
    let tmp = format!("{STATE}.tmp");
    fs::write(&tmp, serde_json::to_vec(undo)?)?;
    fs::rename(tmp, STATE)?;
    Ok(())
}

fn run(cmd: &str, args: &[&str]) -> Result<()> {
    let out = Command::new(cmd).args(args).stdin(Stdio::null()).output().with_context(|| format!("{cmd} non trovato"))?;
    tracing::debug!("{cmd} {}", args.join(" "));
    if !out.status.success() {
        bail!("{cmd} {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

fn active(unit: &str) -> bool {
    Command::new("systemctl").args(["is-active", "--quiet", unit]).status().is_ok_and(|s| s.success())
}

fn ufw_active() -> bool {
    Command::new("ufw").arg("status").output().is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains("Status: active"))
}

/// The channel the interface is associated on, if any.
fn station_channel(iface: &str) -> Option<u8> {
    let out = Command::new("iw").args(["dev", iface, "link"]).output().ok()?;
    let s = String::from_utf8_lossy(&out.stdout);
    let freq: u32 = s.lines().find_map(|l| l.trim().strip_prefix("freq: "))?.split('.').next()?.parse().ok()?;
    match freq {
        2412..=2472 => Some(((freq - 2407) / 5) as u8),
        5000..=5900 => Some(((freq - 5000) / 5) as u8),
        _ => None,
    }
}

fn current_country() -> Option<String> {
    let out = Command::new("iw").args(["reg", "get"]).output().ok()?;
    String::from_utf8_lossy(&out.stdout).lines().find_map(|l| {
        let c = l.strip_prefix("country ")?.get(..2)?.to_string();
        (c.bytes().all(|b| b.is_ascii_uppercase())).then_some(c)
    })
}

fn root_owned(p: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    fs::metadata(p).is_ok_and(|m| m.is_file() && m.uid() == 0 && m.mode() & 0o022 == 0)
}

/// uid of the process on the other end of the socket.
fn peer_uid(s: &UnixStream) -> Option<u32> {
    let mut cred = libc::ucred { pid: 0, uid: 0, gid: 0 };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let r = unsafe { libc::getsockopt(s.as_raw_fd(), libc::SOL_SOCKET, libc::SO_PEERCRED, (&raw mut cred).cast(), &mut len) };
    (r == 0).then_some(cred.uid)
}

/// Root, or a user listed (by name or uid) in /etc/lindrop/utenti.
fn allowed(uid: u32) -> bool {
    if uid == 0 {
        return true;
    }
    let name = user_name(uid);
    fs::read_to_string(USERS).is_ok_and(|s| {
        s.lines().map(str::trim).any(|l| l == uid.to_string() || name.as_deref() == Some(l))
    })
}

fn user_name(uid: u32) -> Option<String> {
    fs::read_to_string("/etc/passwd").ok()?.lines().find_map(|l| {
        let f: Vec<&str> = l.split(':').collect();
        (f.len() > 2 && f[2] == uid.to_string()).then(|| f[0].to_string())
    })
}

static TERM: AtomicBool = AtomicBool::new(false);

extern "C" fn on_term(_: libc::c_int) {
    TERM.store(true, Ordering::SeqCst);
}

/// The service: socket-activated by systemd, or binds the socket itself.
pub fn serve() -> Result<()> {
    if unsafe { libc::geteuid() } != 0 {
        bail!("`lindrop radio` va eseguito come root (è il servizio lindrop-radio)");
    }
    unsafe {
        libc::signal(libc::SIGTERM, on_term as *const () as libc::sighandler_t);
        libc::signal(libc::SIGINT, on_term as *const () as libc::sighandler_t);
    }
    // A previous run died mid-session: put the system back first.
    if let Ok(s) = fs::read(STATE) {
        if let Ok(undo) = serde_json::from_slice::<Vec<Undo>>(&s) {
            tracing::warn!("sessione precedente non chiusa: ripristino");
            undo_all(undo);
        }
    }
    let listener = if std::env::var("LISTEN_FDS").as_deref() == Ok("1") {
        unsafe { UnixListener::from_raw_fd(3) }
    } else {
        fs::create_dir_all("/run/lindrop")?;
        let _ = fs::remove_file(SOCKET);
        let l = UnixListener::bind(SOCKET)?;
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(SOCKET, fs::Permissions::from_mode(0o666))?;
        l
    };
    listener.set_nonblocking(true)?;
    let mut idle = Instant::now();
    while !TERM.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((s, _)) => {
                if let Err(e) = client(s) {
                    tracing::warn!("{e:#}");
                }
                idle = Instant::now();
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if idle.elapsed() > IDLE_EXIT {
                    break;
                }
                std::thread::sleep(Duration::from_millis(200));
            }
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

fn client(s: UnixStream) -> Result<()> {
    s.set_nonblocking(false)?;
    let uid = peer_uid(&s).unwrap_or(u32::MAX);
    let mut w = s.try_clone()?;
    if !allowed(uid) {
        send(&mut w, &Reply::Errore { messaggio: format!("utente {uid} non autorizzato (aggiungilo a {USERS})") })?;
        return Ok(());
    }
    s.set_read_timeout(Some(Duration::from_millis(300)))?;
    let mut r = BufReader::new(s);
    let mut session: Option<Session> = None;
    let mut line = String::new();
    let result = loop {
        if TERM.load(Ordering::SeqCst) {
            break Ok(());
        }
        if let Some(why) = session.as_mut().and_then(Session::owl_dead) {
            let _ = send(&mut w, &Reply::Caduto { motivo: why });
            break Ok(());
        }
        line.clear();
        match r.read_line(&mut line) {
            Ok(0) => break Ok(()), // client gone: tear down
            Ok(_) => {}
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => continue,
            Err(e) => break Err(e.into()),
        }
        let reply = match serde_json::from_str::<Request>(line.trim()) {
            Ok(Request::Stato) => Reply::Stato { attivo: session.is_some(), scheda: session.as_ref().map(|s| s.card.clone()) },
            Ok(Request::Su { .. }) if session.is_some() => Reply::Errore { messaggio: "già attivo".into() },
            Ok(Request::Su { canale, paese }) => {
                tracing::info!("su: canale {canale}, paese {paese:?} (utente {uid})");
                match Session::up(canale, &paese) {
                    Ok(s) => {
                        let r = Reply::Pronto { iface: AWDL.into(), scheda: s.card.clone(), canale: s.canale };
                        session = Some(s);
                        r
                    }
                    Err(e) => Reply::Errore { messaggio: format!("{e:#}") },
                }
            }
            Err(e) => Reply::Errore { messaggio: format!("richiesta non valida: {e}") },
        };
        if send(&mut w, &reply).is_err() {
            break Ok(());
        }
    };
    if let Some(mut s) = session {
        tracing::info!("giù");
        s.down();
    }
    result
}

fn send(w: &mut UnixStream, r: &impl Serialize) -> Result<()> {
    let mut b = serde_json::to_vec(r)?;
    b.push(b'\n');
    w.write_all(&b)?;
    Ok(())
}

/// The user side of the socket. AWDL stays up while this lives.
pub struct Link {
    w: UnixStream,
    r: Option<BufReader<UnixStream>>,
    pub scheda: String,
    pub canale: u8,
}

impl Link {
    pub fn up(canale: u8, paese: &str) -> Result<Self> {
        let s = UnixStream::connect(SOCKET).with_context(|| {
            format!("servizio radio non raggiungibile ({SOCKET}): è installato? `systemctl status lindrop-radio.socket`")
        })?;
        let mut w = s.try_clone()?;
        let mut r = BufReader::new(s);
        send(&mut w, &Request::Su { canale, paese: paese.into() })?;
        match read_reply(&mut r)? {
            Reply::Pronto { scheda, canale, .. } => Ok(Self { w, r: Some(r), scheda, canale }),
            Reply::Errore { messaggio } => bail!("{messaggio}"),
            other => bail!("risposta inattesa: {other:?}"),
        }
    }

    /// Fires once with the reason if the radio drops the session on its own.
    pub fn dropped(&mut self) -> std::sync::mpsc::Receiver<String> {
        let (tx, rx) = std::sync::mpsc::channel();
        if let Some(mut r) = self.r.take() {
            std::thread::spawn(move || {
                let why = match read_reply(&mut r) {
                    Ok(Reply::Caduto { motivo }) => motivo,
                    Ok(other) => format!("{other:?}"),
                    Err(e) => format!("{e:#}"),
                };
                let _ = tx.send(why);
            });
        }
        rx
    }

    pub fn close(self) {
        let _ = self.w.shutdown(std::net::Shutdown::Both);
    }
}

fn read_reply(r: &mut BufReader<UnixStream>) -> Result<Reply> {
    let mut line = String::new();
    if r.read_line(&mut line)? == 0 {
        bail!("il servizio radio ha chiuso la connessione");
    }
    Ok(serde_json::from_str(line.trim())?)
}
