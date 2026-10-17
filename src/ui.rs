//! Desktop notifications: the consent prompt and the results.

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use notify_rust::{Notification, Timeout};

use crate::airdrop::{Ask, Ui, human};

/// How long the prompt waits before declining on its own.
const ASK_TIMEOUT: Duration = Duration::from_secs(60);
const APP: &str = "lindrop";
const ICON: &str = "folder-download";

pub struct Desktop {
    /// Ask before accepting (config `chiedi`).
    pub ask: bool,
    pub dest: PathBuf,
}

impl Ui for Desktop {
    fn ask(&self, a: &Ask) -> bool {
        let who = if a.sender.is_empty() { "Un iPhone" } else { a.sender.as_str() };
        println!("{who} vuole inviarti {}", a.summary());
        if !self.ask {
            return true;
        }
        let shown = Notification::new()
            .appname(APP)
            .summary(&format!("AirDrop da {who}"))
            .body(&format!("Vuole inviarti {}", a.summary()))
            .icon(ICON)
            .action("accetta", "Accetta")
            .action("rifiuta", "Rifiuta")
            .timeout(Timeout::Milliseconds(ASK_TIMEOUT.as_millis() as u32))
            .show();
        let handle = match shown {
            Ok(h) => h,
            Err(e) => {
                // No notification server: fail closed.
                eprintln!("notifica non mostrata ({e}): rifiuto");
                return false;
            }
        };
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || handle.wait_for_action(|act| { let _ = tx.send(act == "accetta"); }));
        let yes = rx.recv_timeout(ASK_TIMEOUT + Duration::from_secs(5)).unwrap_or(false);
        println!("{}", if yes { "accettato" } else { "rifiutato" });
        yes
    }

    fn received(&self, a: &Ask, paths: &[PathBuf], secs: f64) {
        let size: u64 = paths.iter().filter_map(|p| p.metadata().ok()).filter(|m| m.is_file()).map(|m| m.len()).sum();
        let what = match paths {
            [one] => one.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
            many => format!("{} file", many.len()),
        };
        let speed = if secs > 0.0 { format!(", {}/s", human((size as f64 / secs) as u64)) } else { String::new() };
        println!("ricevuto {what} da {} ({}{speed})", a.sender, human(size));
        for p in paths {
            println!("  {}", p.display());
        }
        let dest = self.dest.clone();
        let first = paths.first().cloned();
        let body = format!("{what} in {}", tilde(&self.dest));
        std::thread::spawn(move || {
            let shown = Notification::new()
                .appname(APP)
                .summary("AirDrop ricevuto")
                .body(&body)
                .icon(ICON)
                .action("default", "Apri")
                .action("cartella", "Mostra nella cartella")
                .show();
            if let Ok(h) = shown {
                h.wait_for_action(|act| {
                    let target = match act {
                        "default" => first.unwrap_or(dest),
                        "cartella" => dest,
                        _ => return,
                    };
                    let _ = std::process::Command::new("xdg-open").arg(target).spawn();
                });
            }
        });
    }

    fn failed(&self, what: &str) {
        eprintln!("errore: {what}");
        let _ = Notification::new().appname(APP).summary("AirDrop non riuscito").body(what).icon("dialog-error").show();
    }
}

fn tilde(p: &Path) -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    match p.strip_prefix(&home) {
        Ok(rest) if !home.is_empty() => format!("~/{}", rest.display()),
        _ => p.display().to_string(),
    }
}
