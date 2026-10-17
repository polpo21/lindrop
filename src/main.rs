//! lindrop: AirDrop between iPhone and Linux.

/// println!/eprintln! that do not panic when the reader went away (Ctrl+C
/// in a pipe kills `tee` first, and a panic here would abort the teardown).
macro_rules! say {
    ($($t:tt)*) => {{ use std::io::Write as _; let _ = writeln!(std::io::stdout(), $($t)*); }};
}
macro_rules! say_err {
    ($($t:tt)*) => {{ use std::io::Write as _; let _ = writeln!(std::io::stderr(), $($t)*); }};
}

mod airdrop;
mod archive;
mod ble;
mod config;
mod dvzip;
mod http;
mod ident;
mod mdns;
mod radio;
mod send;
mod ui;

use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};

use config::Config;

#[derive(Parser)]
#[command(name = "lindrop", version, about = "AirDrop tra iPhone e Linux")]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Diventa visibile all'iPhone e ricevi file
    Ricevi {
        /// Minuti di visibilità (predefinito dal config, 10; 0 = senza limite)
        #[arg(short, long)]
        minuti: Option<u64>,
    },
    /// Invia file a un iPhone vicino
    Invia {
        #[arg(required = true)]
        file: Vec<PathBuf>,
        /// Solo al dispositivo con questo nome (basta una parte, es. "Paolo")
        #[arg(short, long)]
        a: Option<String>,
    },
    /// Servizio radio (root, avviato da systemd)
    #[command(hide = true)]
    Radio {
        /// Rimetti a posto il Wi-Fi da una sessione rimasta aperta ed esci
        #[arg(long)]
        ripristina: bool,
    },
    /// Apre la cartella dei file ricevuti (la usa la barra)
    #[command(hide = true)]
    Apri,
    /// Prova la sveglia Bluetooth per qualche secondo
    #[command(hide = true)]
    Sveglia {
        #[arg(default_value_t = 10)]
        secondi: u64,
    },
}

static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_: libc::c_int) {
    STOP.store(true, Ordering::SeqCst);
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("LINDROP_LOG").unwrap_or_else(|_| "lindrop=info".into()),
        )
        .with_target(false)
        .without_time()
        .init();
    let r = match Cli::parse().cmd {
        None => stato(),
        Some(Cmd::Ricevi { minuti }) => ricevi(minuti),
        Some(Cmd::Invia { file, a }) => invia(file, a),
        Some(Cmd::Apri) => Config::load().and_then(|c| {
            std::process::Command::new("xdg-open").arg(&c.cartella).spawn()?;
            Ok(())
        }),
        Some(Cmd::Radio { ripristina: false }) => radio::serve(),
        Some(Cmd::Radio { ripristina: true }) => radio::restore(),
        Some(Cmd::Sveglia { secondi }) => ble::Wake::start().map(|_w| std::thread::sleep(Duration::from_secs(secondi))),
    };
    if let Err(e) = r {
        say_err!("errore: {e:#}");
        std::process::exit(1);
    }
}

fn stato() -> Result<()> {
    let cfg = Config::load()?;
    say!("lindrop {}", env!("CARGO_PKG_VERSION"));
    say!("  nome       {}", cfg.nome);
    say!("  cartella   {}", cfg.cartella.display());
    let canale = if cfg.canale == 0 { "automatico".to_string() } else { cfg.canale.to_string() };
    say!("  modalità   {:?}, canale {canale}", cfg.modalita);
    let radio = std::path::Path::new(radio::SOCKET).exists();
    say!("  radio      {}", if radio { "servizio pronto" } else { "servizio non installato (./install.sh)" });
    let driver = if radio::patched_driver() {
        "mt7921e di lindrop (veloce)"
    } else {
        "di serie: l'iPhone perde i pacchetti a 5 GHz (scripts/driver.sh)"
    };
    say!("  driver     {driver}");
    let ricezione = std::process::Command::new("systemctl")
        .args(["--user", "is-active", "lindrop-ricevi.service"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    say!("  barra      ricezione {}", if ricezione == "active" { "accesa" } else { "spenta" });
    say!("\n  lindrop ricevi          diventa visibile all'iPhone");
    say!("  lindrop invia <file>    invia a un iPhone vicino");
    Ok(())
}

/// Radio up, link-local address ready, mDNS running. Undone on drop.
struct Awdl {
    link: Option<radio::Link>,
    mdns: Arc<mdns::Mdns>,
    stop: Arc<AtomicBool>,
    addr: std::net::Ipv6Addr,
    dropped: std::sync::mpsc::Receiver<String>,
}

impl Awdl {
    fn up(cfg: &Config) -> Result<Self> {
        unsafe {
            libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
            libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
        }
        say!("Accendo AWDL…");
        let mut link = radio::Link::up(cfg.canale, &cfg.paese, cfg.modalita, &cfg.velocita)?;
        let wifi = if link.condivisa { "il Wi-Fi resta acceso" } else { "il Wi-Fi è spento finché sei visibile" };
        let speed = if link.velocita.is_empty() { String::new() } else { format!(", {}", link.velocita) };
        say!("AWDL sul canale {} ({wifi}{speed})", link.canale);
        let dropped = link.dropped();
        let t = Instant::now();
        let (addr, _) = loop {
            match mdns::link_local(radio::AWDL) {
                Ok(a) => break a,
                Err(e) if t.elapsed() > Duration::from_secs(8) => return Err(e),
                Err(_) => std::thread::sleep(Duration::from_millis(100)),
            }
        };
        let mdns = mdns::Mdns::open(radio::AWDL)?;
        let stop = Arc::new(AtomicBool::new(false));
        {
            let (m, s) = (Arc::clone(&mdns), Arc::clone(&stop));
            std::thread::spawn(move || m.run(&s));
        }
        tracing::info!("AWDL pronto su {} ({addr})", link.scheda);
        Ok(Self { link: Some(link), mdns, stop, addr, dropped })
    }

    /// `Some(reason)` once it is time to stop.
    fn should_stop(&self) -> Option<String> {
        if STOP.load(Ordering::SeqCst) {
            return Some("interrotto".into());
        }
        self.dropped.try_recv().ok()
    }
}

impl Drop for Awdl {
    fn drop(&mut self) {
        self.mdns.withdraw();
        self.stop.store(true, Ordering::SeqCst);
        if let Some(l) = self.link.take() {
            let shared = l.condivisa;
            l.close();
            say!("{}", if shared { "AWDL spento." } else { "AWDL spento, il Wi-Fi torna da solo." });
        }
    }
}

fn host_name() -> String {
    let h = std::fs::read_to_string("/etc/hostname").unwrap_or_default();
    let h: String = h.trim().chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').collect();
    if h.is_empty() { "linux".into() } else { h }
}

fn ricevi(minuti: Option<u64>) -> Result<()> {
    let cfg = Config::load()?;
    let minuti = minuti.unwrap_or(cfg.minuti);
    std::fs::create_dir_all(&cfg.cartella).with_context(|| format!("creo {}", cfg.cartella.display()))?;
    let ident = ident::Ident::load_or_create(&config::data_dir(), &cfg.nome)?;
    let awdl = Awdl::up(&cfg)?;

    let ui = ui::Desktop { ask: cfg.chiedi, dest: cfg.cartella.clone() };
    let rx = airdrop::Receiver::new(cfg.nome.clone(), cfg.cartella.clone(), ident.server_config()?, Box::new(ui));
    let listener = airdrop::listen(radio::AWDL, airdrop::PORT)?;
    std::thread::spawn(move || rx.serve_on(listener));
    awdl.mdns.announce(&ident.service_id, &host_name(), awdl.addr, airdrop::PORT);

    if minuti == 0 {
        say!("Visibile come «{}» finché non lo spegni.", cfg.nome);
    } else {
        say!("Visibile come «{}» per {minuti} minuti. Ctrl+C per smettere.", cfg.nome);
    }
    say!("Sull'iPhone: Centro di Controllo → AirDrop → «Tutti per 10 minuti», poi Condividi.");
    // 0 = no limit (the bar's switch, lindrop-ricevi.service).
    let end = (minuti > 0).then(|| Instant::now() + Duration::from_secs(minuti * 60));
    while end.is_none_or(|e| Instant::now() < e) {
        if let Some(why) = awdl.should_stop() {
            say!("\n{why}");
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    Ok(())
}


/// More than one receiver: ask which, on a terminal; elsewhere refuse.
fn choose(list: &[(std::net::SocketAddr, String)]) -> Result<(std::net::SocketAddr, String)> {
    use std::io::IsTerminal;
    if !std::io::stdin().is_terminal() {
        let names: Vec<_> = list.iter().map(|(_, n)| format!("«{n}»")).collect();
        bail!("più dispositivi vicini ({}): scegli con --a NOME", names.join(", "));
    }
    say!("Dispositivi vicini:");
    for (i, (_, n)) in list.iter().enumerate() {
        say!("  {}. {n}", i + 1);
    }
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    let i: usize = line.trim().parse().ok().filter(|i| (1..=list.len()).contains(i)).context("scelta non valida")?;
    Ok(list[i - 1].clone())
}

fn invia(files: Vec<PathBuf>, to: Option<String>) -> Result<()> {
    for f in &files {
        if !f.is_file() {
            bail!("{} non è un file", f.display());
        }
    }
    let cfg = Config::load()?;
    let ident = ident::Ident::load_or_create(&config::data_dir(), &cfg.nome)?;
    let sender = send::Sender::new(&ident, &cfg.nome)?;
    let awdl = Awdl::up(&cfg)?;
    // After the radio: on combo chips bringing Wi-Fi up resets Bluetooth too.
    let _wake = match ble::Wake::start() {
        Ok(w) => Some(w),
        Err(e) => {
            say_err!("sveglia Bluetooth non partita ({e:#}): l'iPhone va tenuto sulla schermata AirDrop");
            None
        }
    };
    awdl.mdns.browse(true);
    say!("Cerco iPhone… sull'iPhone: AirDrop su «Tutti per 10 minuti» e Condividi → AirDrop aperto");

    // Name every receiver through /Discover, once each. Never send to the first
    // that answers blindly: anything on the link can claim to be a receiver.
    let limit = Duration::from_secs(std::env::var("LINDROP_CERCA_S").ok().and_then(|v| v.parse().ok()).unwrap_or(120));
    let wanted = to.as_deref().map(str::to_lowercase);
    let t = Instant::now();
    let mut asked = std::collections::HashSet::new();
    let mut named: Vec<(std::net::SocketAddr, String)> = Vec::new();
    let mut first_match: Option<Instant> = None;
    let (addr, name) = loop {
        if let Some(why) = awdl.should_stop() {
            bail!("{why}");
        }
        for p in awdl.mdns.peers() {
            if awdl.should_stop().is_some() {
                break;
            }
            if asked.insert(p.addr)
                && let Ok(Some(n)) = sender.discover(p.addr)
            {
                named.push((p.addr, n));
            }
        }
        let matches: Vec<_> =
            named.iter().filter(|(_, n)| wanted.as_ref().is_none_or(|w| n.to_lowercase().contains(w))).cloned().collect();
        if !matches.is_empty() {
            // A couple of seconds more, so a second device nearby shows up too.
            let since = *first_match.get_or_insert_with(Instant::now);
            if matches.len() == 1 && since.elapsed() >= Duration::from_secs(2) {
                break matches[0].clone();
            }
            if matches.len() > 1 && since.elapsed() >= Duration::from_secs(2) {
                break choose(&matches)?;
            }
        }
        if t.elapsed() > limit {
            let seen: Vec<_> = named.iter().map(|(_, n)| format!("«{n}»")).collect();
            match &to {
                Some(w) if !seen.is_empty() => bail!("nessun «{w}» tra {}", seen.join(", ")),
                _ => bail!("nessun iPhone trovato in {} secondi", limit.as_secs()),
            }
        }
        std::thread::sleep(Duration::from_millis(500));
    };
    let p = addr;
    say!("Trovato «{name}». Conferma sull'iPhone…");
    let total: u64 = files.iter().filter_map(|f| f.metadata().ok()).map(|m| m.len()).sum();
    let start = Instant::now();
    let out = sender.send(p, &files, &STOP, &|sent, tot| {
        let mut out = std::io::stdout();
        let _ = write!(out, "\r  {:>3}%  {} di {}", sent * 100 / tot.max(1), airdrop::human(sent), airdrop::human(tot));
        let _ = out.flush();
    })?;
    say!();
    match out {
        send::Outcome::Sent => {
            let secs = start.elapsed().as_secs_f64();
            say!("Inviato a «{name}» ({}, {:.0} s)", airdrop::human(total), secs);
            Ok(())
        }
        send::Outcome::Declined => bail!("«{name}» ha rifiutato"),
    }
}
