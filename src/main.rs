//! lindrop: AirDrop between iPhone and Linux.

mod archive;
mod config;
mod dvzip;
mod http;
mod ident;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "lindrop", version, about = "AirDrop tra iPhone e Linux")]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Pacchetta dei file in un archivio cpio (prova del formato)
    #[command(hide = true)]
    Cpio { out: std::path::PathBuf, files: Vec<std::path::PathBuf> },
}

fn main() -> Result<()> {
    tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::from_default_env()).init();
    match Cli::parse().cmd {
        None => println!("lindrop {}", env!("CARGO_PKG_VERSION")),
        Some(Cmd::Cpio { out, files }) => {
            let mut w = archive::Writer::new(std::fs::File::create(out)?);
            for f in files {
                let meta = std::fs::metadata(&f)?;
                let name = format!("./{}", f.file_name().unwrap().to_string_lossy());
                w.file(&name, meta.len(), 0, &mut std::fs::File::open(&f)?)?;
            }
            w.finish()?;
        }
    }
    Ok(())
}
