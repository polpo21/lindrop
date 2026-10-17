//! lindrop: AirDrop between iPhone and Linux.

mod airdrop;
mod archive;
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

use config::{Config, Modalita};

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
        /// Minuti di visibilità (predefinito dal config, 10)
        #[arg(short, long)]
        minuti: Option<u64>,
    },
    /// Invia file a un iPhone vicino
    Invia {
        #[arg(required = true)]
        file: Vec<PathBuf>,
    },
    /// Servizio radio (root, avviato da systemd)
    #[command(hide = true)]
    Radio,
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
        Some(Cmd::Invia { file }) => invia(file),
        Some(Cmd::Radio) => radio::serve(),
    };
    if let Err(e) = r {
        eprintln!("errore: {e:#}");
        std::process::exit(1);
    }
}

fn stato() -> Result<()> {
    let cfg = Config::load()?;
    println!("lindrop {}", env!("CARGO_PKG_VERSION"));
    println!("  nome       {}", cfg.nome);
    println!("  cartella   {}", cfg.cartella.display());
    let canale = if cfg.canale == 0 { "automatico".to_string() } else { cfg.canale.to_string() };
    println!("  modalità   {:?}, canale {canale}", cfg.modalita);
    let radio = std::path::Path::new(radio::SOCKET).exists();
    println!("  radio      {}", if radio { "servizio pronto" } else { "servizio non installato" });
    println!("\n  lindrop ricevi          diventa visibile all'iPhone");
    println!("  lindrop invia <file>    invia a un iPhone vicino");
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
        if cfg.modalita == Modalita::Condivisa {
            bail!("la modalità condivisa non è ancora pronta: usa `modalita = \"esclusiva\"`");
        }
        unsafe {
            libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
            libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
        }
        println!("Accendo AWDL (il Wi-Fi si spegne finché sei visibile)…");
        let mut link = radio::Link::up(cfg.canale, &cfg.paese)?;
        println!("AWDL sul canale {}", link.canale);
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
            l.close();
        }
        println!("AWDL spento, il Wi-Fi torna da solo.");
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

    println!("Visibile come «{}» per {minuti} minuti. Ctrl+C per smettere.", cfg.nome);
    println!("Sull'iPhone: Centro di Controllo → AirDrop → «Tutti per 10 minuti», poi Condividi.");
    let end = Instant::now() + Duration::from_secs(minuti * 60);
    while Instant::now() < end {
        if let Some(why) = awdl.should_stop() {
            println!("\n{why}");
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    Ok(())
}

fn invia(files: Vec<PathBuf>) -> Result<()> {
    for f in &files {
        if !f.is_file() {
            bail!("{} non è un file", f.display());
        }
    }
    let cfg = Config::load()?;
    let ident = ident::Ident::load_or_create(&config::data_dir(), &cfg.nome)?;
    let sender = send::Sender::new(&ident, &cfg.nome)?;
    let awdl = Awdl::up(&cfg)?;
    awdl.mdns.browse(true);
    println!("Cerco iPhone… (sull'iPhone: AirDrop su «Tutti per 10 minuti», schermo acceso)");

    let t = Instant::now();
    let peer = loop {
        if let Some(why) = awdl.should_stop() {
            bail!("{why}");
        }
        let found: Vec<_> = awdl.mdns.peers();
        // Name each one through /Discover; the first that answers wins.
        if let Some((p, name)) = found.iter().find_map(|p| sender.discover(p.addr).ok().flatten().map(|n| (p.clone(), n))) {
            break (p, name);
        }
        if t.elapsed() > Duration::from_secs(60) {
            bail!("nessun iPhone trovato in 60 secondi");
        }
        std::thread::sleep(Duration::from_millis(500));
    };
    let (p, name) = peer;
    println!("Trovato «{name}». Conferma sull'iPhone…");
    let total: u64 = files.iter().filter_map(|f| f.metadata().ok()).map(|m| m.len()).sum();
    let start = Instant::now();
    let out = sender.send(p.addr, &files, &|sent, tot| {
        print!("\r  {:>3}%  {} di {}", sent * 100 / tot.max(1), airdrop::human(sent), airdrop::human(tot));
        let _ = std::io::stdout().flush();
    })?;
    println!();
    match out {
        send::Outcome::Sent => {
            let secs = start.elapsed().as_secs_f64();
            println!("Inviato a «{name}» ({}, {:.0} s)", airdrop::human(total), secs);
            Ok(())
        }
        send::Outcome::Declined => bail!("«{name}» ha rifiutato"),
    }
}
