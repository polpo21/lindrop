//! `~/.config/lindrop/config.toml` and the standard paths.

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::Deserialize;

pub use crate::radio::Modo as Modalita;

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Name shown on the iPhone.
    pub nome: String,
    /// Where received files land.
    pub cartella: PathBuf,
    pub modalita: Modalita,
    /// AWDL channel: 6, 36, 44 or 149, or 0 = the Wi-Fi's own channel (the
    /// phone at home is on it most of the time), else 149.
    pub canale: u8,
    /// Two-letter regulatory country, pinned while Wi-Fi is down.
    pub paese: String,
    /// Ask before accepting a transfer.
    pub chiedi: bool,
    /// Minutes of visibility for `lindrop ricevi`.
    pub minuti: u64,
    /// OWL data rate: "" automatic (mcs7 with our driver), "base", "mcs0".."mcs15", "6".."54".
    pub velocita: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            nome: default_name(),
            cartella: download_dir(),
            modalita: Modalita::default(),
            canale: 0,
            paese: String::new(),
            chiedi: true,
            minuti: 10,
            velocita: String::new(),
        }
    }
}

impl Config {
    pub fn load() -> Result<Self> {
        let path = config_dir().join("config.toml");
        let cfg: Self = match std::fs::read_to_string(&path) {
            Ok(s) => toml::from_str(&s).with_context(|| format!("{} non è valido", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(e) => return Err(e).with_context(|| format!("leggo {}", path.display())),
        };
        if ![0, 6, 36, 44, 149].contains(&cfg.canale) {
            anyhow::bail!("canale {} non valido in {}: usa 0 (automatico), 6, 36, 44 o 149", cfg.canale, path.display());
        }
        Ok(cfg)
    }
}

fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| "/".into())
}

fn xdg(var: &str, fallback: &str) -> PathBuf {
    std::env::var_os(var).map(PathBuf::from).filter(|p| p.is_absolute()).unwrap_or_else(|| home().join(fallback))
}

pub fn config_dir() -> PathBuf {
    xdg("XDG_CONFIG_HOME", ".config").join("lindrop")
}

pub fn data_dir() -> PathBuf {
    xdg("XDG_DATA_HOME", ".local/share").join("lindrop")
}

/// `XDG_DOWNLOAD_DIR` from user-dirs.dirs, else ~/Downloads.
fn download_dir() -> PathBuf {
    let dirs = xdg("XDG_CONFIG_HOME", ".config").join("user-dirs.dirs");
    if let Ok(s) = std::fs::read_to_string(dirs) {
        for line in s.lines() {
            if let Some(v) = line.strip_prefix("XDG_DOWNLOAD_DIR=") {
                let v = v.trim_matches('"').replace("$HOME", &home().to_string_lossy());
                return PathBuf::from(v);
            }
        }
    }
    home().join("Downloads")
}

/// Pretty hostname if set, else the plain hostname.
fn default_name() -> String {
    let pretty = std::fs::read_to_string("/etc/machine-info").ok().and_then(|s| {
        s.lines()
            .find_map(|l| l.strip_prefix("PRETTY_HOSTNAME=").map(|v| v.trim_matches('"').to_string()))
    });
    pretty
        .filter(|s| !s.is_empty())
        .or_else(|| std::fs::read_to_string("/etc/hostname").ok().map(|s| s.trim().to_string()))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "Linux".into())
}
