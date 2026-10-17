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
use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

pub const SOCKET: &str = "/run/lindrop/radio.sock";
const STATE: &str = "/run/lindrop/sessione.json";
const OWL_LOG: &str = "/run/lindrop/owl.log";
/// Last AWDL channel used, for automatic mode when the station is down.
const LAST: &str = "/run/lindrop/canale";
const USERS: &str = "/etc/lindrop/utenti";
/// Root-owned AWDL daemon (phase 1: OWL). Never a user-writable path.
const OWL: &[&str] = &["/usr/lib/lindrop/owl", "/usr/local/lib/lindrop/owl"];
const MON: &str = "ldmon0";
const MONA: &str = "ldmon1";
pub const AWDL: &str = "awdl0";
const UFW6_CHAIN: &str = "ufw6-before-input";
/// A request line longer than this is not one of ours.
const MAX_REQUEST: u64 = 4096;
const IDLE_EXIT: Duration = Duration::from_secs(120);

/// How AWDL shares the card with the normal Wi-Fi.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Modo {
    /// Shared when associated on a channel AWDL can use, else exclusive.
    #[default]
    Automatica,
    /// Wi-Fi down for the session; AWDL owns the card.
    Esclusiva,
    /// Wi-Fi stays associated; AWDL rides the AP's channel.
    Condivisa,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Request {
    /// Bring AWDL up for this connection; it goes down when the connection closes.
    Su {
        canale: u8,
        paese: String,
        #[serde(default)]
        modo: Modo,
        /// OWL data rate (`-R`): "" = automatic, "base" = OWL's default.
        #[serde(default)]
        velocita: String,
    },
    Stato,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Reply {
    Pronto {
        iface: String,
        scheda: String,
        canale: u8,
        #[serde(default)]
        condivisa: bool,
        /// Data rate and aggregation in use ("" = OWL's defaults).
        #[serde(default)]
        velocita: String,
    },
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
    /// Old sessions only: a persistent ufw rule (now a runtime ip6tables one).
    UfwDelete,
    /// Runtime rule in ufw's own input chain: gone at reboot or ufw reload.
    Ip6Accept,
    /// Country before we pinned it ("" = none to put back).
    Reg(String),
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
    condivisa: bool,
    velocita: String,
}

impl Session {
    fn record(&mut self, u: Undo) -> Result<()> {
        self.undo.push(u);
        save(&self.undo)
    }

    fn up(canale: u8, paese: &str, modo: Modo, velocita: &str) -> Result<Self> {
        if canale != 0 && mhz(canale).is_none() {
            bail!("canale {canale} non valido (0, 6, 36, 44 o 149)");
        }
        if !(paese.is_empty() || (paese.len() == 2 && paese.bytes().all(|b| b.is_ascii_uppercase()))) {
            bail!("paese {paese:?} non valido (due lettere maiuscole, es. IT)");
        }
        // Reaches OWL's command line: a short token, OWL validates the value.
        if velocita.len() > 12 || !velocita.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-') {
            bail!("velocità {velocita:?} non valida (es. mcs7, 24, base)");
        }
        let card = Card::find()?;
        if !supported(&card.driver) {
            bail!("la scheda {} (driver {}) non è ancora supportata", card.iface, card.driver);
        }
        let owl = OWL.iter().map(Path::new).find(|p| root_owned(p)).context("OWL non installato (vedi scripts/owl.sh)")?;
        let mut s = Session {
            undo: Vec::new(),
            owl: None,
            card: format!("{} ({})", card.iface, card.driver),
            canale: 0,
            condivisa: false,
            velocita: String::new(),
        };
        if let Err(e) = s.build(&card, canale, paese, modo, velocita, owl) {
            s.down();
            return Err(e);
        }
        Ok(s)
    }

    fn build(&mut self, card: &Card, canale: u8, paese: &str, modo: Modo, velocita: &str, owl: &Path) -> Result<()> {
        let station = station_channel(&card.iface).filter(|c| mhz(*c).is_some());
        // Automatic: the channel the station is on now. A phone on the same
        // Wi-Fi spends most of its AWDL slots, multicast slot 0 included, there.
        // Not associated (Wi-Fi still reconnecting after the last session):
        // the channel that worked last time is a better guess than 149.
        let canale = match canale {
            0 => station
                .or_else(|| fs::read_to_string(LAST).ok().and_then(|s| s.trim().parse().ok()))
                .filter(|c| mhz(*c).is_some())
                .unwrap_or(149),
            c => c,
        };
        let _ = fs::write(LAST, canale.to_string());
        let freq = mhz(canale).expect("validated");
        self.canale = canale;
        // Shared needs the station associated on the AWDL channel itself: the
        // firmware keeps a monitor on the BSS channel while associated.
        let condivisa = match modo {
            Modo::Esclusiva => false,
            Modo::Condivisa if station != Some(canale) => {
                bail!("la modalità condivisa vuole il Wi-Fi collegato sul canale {canale} (ora: {station:?})")
            }
            Modo::Condivisa => true,
            Modo::Automatica => station == Some(canale),
        };
        self.condivisa = condivisa;
        tracing::info!("canale AWDL {canale} ({freq} MHz), {}", if condivisa { "condivisa" } else { "esclusiva" });

        if !Path::new("/sys/kernel/debug/ieee80211").exists() {
            let _ = run("mount", &["-t", "debugfs", "none", "/sys/kernel/debug"]);
        }
        stopping()?;
        let (vif, strategy) = if condivisa { (self.shared(card)?, "intersect") } else { (self.exclusive(card, freq, canale, paese)?, "verbatim") };

        // Faster data frames and aggregation only with our driver: the stock
        // one turns injected HT rates into CCK 1M, which 5 GHz drops.
        let fast = patched_driver();
        let rate = match velocita {
            "" if fast => "mcs7",
            "" | "base" => "",
            v => v,
        };
        let mut args: Vec<String> = ["-i", vif, "-c", &canale.to_string(), "-N", "-S", strategy, "-v"].map(String::from).to_vec();
        if !rate.is_empty() {
            args.extend(["-R".into(), rate.into()]);
        }
        if fast {
            args.push("-A".into());
        }
        self.velocita = [rate, if fast { "aggregazione" } else { "" }].iter().filter(|s| !s.is_empty()).cloned().collect::<Vec<_>>().join(" + ");
        tracing::info!("OWL {}", args.join(" "));

        stopping()?;
        self.record(Undo::DelIface(AWDL.into()))?;
        let log = fs::File::create(OWL_LOG)?;
        let child = Command::new(owl)
            .args(&args)
            .stdout(log.try_clone()?)
            .stderr(log)
            .stdin(Stdio::null())
            .spawn()
            .context("avvio di OWL")?;
        self.record(Undo::Kill(child.id()))?;
        self.owl = Some(child);

        let t = Instant::now();
        while !Path::new("/sys/class/net").join(AWDL).exists() {
            stopping()?;
            if t.elapsed() > Duration::from_secs(6) || self.owl_dead().is_some() {
                bail!("OWL non ha creato {AWDL} (log in {OWL_LOG})");
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        // The address is derived from the MAC and unique on the link: skip
        // duplicate address detection (about a second at every start).
        let _ = fs::write(format!("/proc/sys/net/ipv6/conf/{AWDL}/accept_dad"), "0");
        let _ = run("ip", &["link", "set", AWDL, "up"]);
        let _ = run("nmcli", &["device", "set", AWDL, "managed", "no"]);
        // systemd-resolved would otherwise answer mDNS on awdl0 for the
        // hostname, beside our own records. The link dies with the session.
        if active("systemd-resolved") {
            let _ = run("resolvectl", &["mdns", AWDL, "no"]);
            let _ = run("resolvectl", &["llmnr", AWDL, "no"]);
        }

        // AWDL is IPv6 link-local only. A runtime rule at the top of ufw's own
        // chain: nothing is written to /etc/ufw, so a power cut mid-session
        // cannot leave the port open for good.
        if ufw_active() {
            self.record(Undo::Ip6Accept)?;
            run("ip6tables", &["-I", UFW6_CHAIN, "1", "-i", AWDL, "-j", "ACCEPT"])?;
        }
        Ok(())
    }

    /// Wi-Fi stays up: a plain monitor carrying the station's MAC rides the
    /// station's channel. The phone's unicast to that MAC is ACKed by the
    /// station itself, in hardware.
    fn shared(&mut self, card: &Card) -> Result<&'static str> {
        let mac = fs::read_to_string(Path::new("/sys/class/net").join(&card.iface).join("address"))?.trim().to_string();
        let _ = run("iw", &["dev", MON, "del"]);
        self.record(Undo::DelIface(MON.into()))?;
        run("iw", &["phy", &card.phy, "interface", "add", MON, "type", "monitor"])?;
        let _ = run("nmcli", &["device", "set", MON, "managed", "no"]);
        run("ip", &["link", "set", MON, "address", &mac])?;
        run("ip", &["link", "set", MON, "up"])?;
        // Runtime PM silently kills monitor RX on mt76 (F§9).
        self.pm_off(card)?;
        Ok(MON)
    }

    /// Wi-Fi down: the card is all AWDL's, on any of the AWDL channels.
    fn exclusive(&mut self, card: &Card, freq: u32, canale: u8, paese: &str) -> Result<&'static str> {
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
            self.record(Undo::Reg(current_country().unwrap_or_default()))?;
            run("iw", &["reg", "set", &paese])?;
        }
        self.record(Undo::LinkUp(card.iface.clone()))?;
        run("ip", &["link", "set", &card.iface, "down"])?;

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
            self.pm_off(card)?;
            if !active_flag {
                run("iw", &["dev", name, "set", "freq", &freq.to_string()])?;
            }
        }
        Ok(MONA)
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
            Undo::Ip6Accept => run("ip6tables", &["-D", UFW6_CHAIN, "-i", AWDL, "-j", "ACCEPT"]),
            Undo::Reg(c) if !c.is_empty() => run("iw", &["reg", "set", c]),
            Undo::Reg(_) => Ok(()),
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

/// Our mt7921e (scripts/driver.sh) is the one loaded.
pub fn patched_driver() -> bool {
    let ours = Path::new("/lib/modules").join(uname_r()).join("updates/lindrop/mt7921e.ko");
    let Ok(loaded) = fs::read_to_string("/sys/module/mt7921e/srcversion") else { return false };
    Command::new("modinfo")
        .args(["-F", "srcversion"])
        .arg(&ours)
        .output()
        .is_ok_and(|o| o.status.success() && String::from_utf8_lossy(&o.stdout).trim() == loaded.trim())
}

fn uname_r() -> String {
    fs::read_to_string("/proc/sys/kernel/osrelease").map(|s| s.trim().to_string()).unwrap_or_default()
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

/// systemd is stopping us: give up building a session (it gets torn down).
fn stopping() -> Result<()> {
    if TERM.load(Ordering::SeqCst) {
        bail!("servizio in arresto");
    }
    Ok(())
}

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
    if let Ok(s) = fs::read(STATE)
        && let Ok(undo) = serde_json::from_slice::<Vec<Undo>>(&s)
    {
        tracing::warn!("sessione precedente non chiusa: ripristino");
        undo_all(undo);
    }
    let activated = std::env::var("LISTEN_FDS").as_deref() == Ok("1")
        && std::env::var("LISTEN_PID").ok().and_then(|p| p.parse::<u32>().ok()) == Some(std::process::id());
    let listener = if activated {
        // Children (OWL, iw, nmcli) must not inherit the listening socket.
        unsafe { libc::fcntl(3, libc::F_SETFD, libc::FD_CLOEXEC) };
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
            // One thread per client, so a second client gets an answer (busy)
            // instead of waiting behind a session that may last hours.
            Ok((s, _)) => {
                CLIENTS.fetch_add(1, Ordering::SeqCst);
                std::thread::spawn(move || {
                    if let Err(e) = client(s) {
                        tracing::warn!("{e:#}");
                    }
                    CLIENTS.fetch_sub(1, Ordering::SeqCst);
                });
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if CLIENTS.load(Ordering::SeqCst) > 0 {
                    idle = Instant::now();
                } else if idle.elapsed() > IDLE_EXIT {
                    break;
                }
                std::thread::sleep(Duration::from_millis(200));
            }
            Err(e) => return Err(e.into()),
        }
    }
    // Stopping: every client sees TERM and tears its session down; wait for it.
    let t = Instant::now();
    while CLIENTS.load(Ordering::SeqCst) > 0 && t.elapsed() < Duration::from_secs(45) {
        std::thread::sleep(Duration::from_millis(100));
    }
    Ok(())
}

/// `lindrop radio --ripristina`: replay a leftover undo journal and exit.
/// Run by systemd after the service stops, however it stopped.
pub fn restore() -> Result<()> {
    if unsafe { libc::geteuid() } != 0 {
        bail!("serve root");
    }
    if let Ok(s) = fs::read(STATE)
        && let Ok(undo) = serde_json::from_slice::<Vec<Undo>>(&s)
    {
        tracing::warn!("sessione non chiusa: ripristino");
        undo_all(undo);
    }
    Ok(())
}

static CLIENTS: AtomicUsize = AtomicUsize::new(0);
/// One AWDL session at a time: the card has one radio.
static BUSY: AtomicBool = AtomicBool::new(false);

fn busy_message() -> String {
    "la radio è già in uso da un'altra sessione AirDrop (la ricezione dalla barra?): spegnila e riprova".into()
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
        match r.by_ref().take(MAX_REQUEST).read_line(&mut line) {
            Ok(0) => break Ok(()), // client gone: tear down
            Ok(_) if !line.ends_with('\n') && line.len() as u64 >= MAX_REQUEST => {
                let _ = send(&mut w, &Reply::Errore { messaggio: "richiesta troppo lunga".into() });
                break Ok(());
            }
            Ok(_) => {}
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => continue,
            Err(e) => break Err(e.into()),
        }
        let reply = match serde_json::from_str::<Request>(line.trim()) {
            Ok(Request::Stato) => Reply::Stato { attivo: session.is_some(), scheda: session.as_ref().map(|s| s.card.clone()) },
            Ok(Request::Su { .. }) if session.is_some() => Reply::Errore { messaggio: "già attivo".into() },
            Ok(Request::Su { .. }) if BUSY.swap(true, Ordering::SeqCst) => Reply::Errore { messaggio: busy_message() },
            Ok(Request::Su { canale, paese, modo, velocita }) => {
                tracing::info!("su: canale {canale}, paese {paese:?}, {modo:?}, velocità {velocita:?} (utente {uid})");
                let up = Session::up(canale, &paese, modo, &velocita);
                if up.is_err() {
                    BUSY.store(false, Ordering::SeqCst);
                }
                match up {
                    Ok(s) => {
                        let r = Reply::Pronto {
                            iface: AWDL.into(),
                            scheda: s.card.clone(),
                            canale: s.canale,
                            condivisa: s.condivisa,
                            velocita: s.velocita.clone(),
                        };
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
        BUSY.store(false, Ordering::SeqCst);
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
    pub condivisa: bool,
    pub velocita: String,
}

impl Link {
    pub fn up(canale: u8, paese: &str, modo: Modo, velocita: &str) -> Result<Self> {
        let s = UnixStream::connect(SOCKET).with_context(|| {
            format!("servizio radio non raggiungibile ({SOCKET}): è installato? `systemctl status lindrop-radio.socket`")
        })?;
        let mut w = s.try_clone()?;
        // Bringing AWDL up takes seconds, never a minute: do not hang on a stuck service.
        s.set_read_timeout(Some(Duration::from_secs(45)))?;
        let mut r = BufReader::new(s);
        send(&mut w, &Request::Su { canale, paese: paese.into(), modo, velocita: velocita.into() })?;
        let reply = read_reply(&mut r);
        r.get_ref().set_read_timeout(None)?;
        match reply? {
            Reply::Pronto { scheda, canale, condivisa, velocita, .. } => {
                Ok(Self { w, r: Some(r), scheda, canale, condivisa, velocita })
            }
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
