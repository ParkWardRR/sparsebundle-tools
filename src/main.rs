use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "sparsebundle-tools")]
#[command(about = "Read, analyse and stream Apple sparsebundle disk images")]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Full structural analysis of a sparsebundle
    Info {
        /// Path to the .sparsebundle directory
        path: PathBuf,
    },
    /// List bands with sizes
    Bands {
        /// Path to the .sparsebundle directory
        path: PathBuf,
    },
    /// Read bytes at a logical offset
    Read {
        /// Path to the .sparsebundle directory
        path: PathBuf,
        /// Byte offset into the logical disk image
        #[arg(long)]
        offset: u64,
        /// Number of bytes to read
        #[arg(long)]
        len: usize,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Command::Info { path } => {
            let analysis = sparsebundle_tools::analyse(&path)?;
            print!("{}", analysis.render());
        }
        Command::Bands { path } => {
            let analysis = sparsebundle_tools::analyse(&path)?;
            let band_list = sparsebundle_tools::bands(&path)?;
            println!(
                "{} bands, band size {} bytes ({} MiB)",
                band_list.len(),
                analysis.band_size,
                analysis.band_size / (1024 * 1024)
            );
            for b in &band_list {
                let size = match std::fs::metadata(&b.path) {
                    Ok(m) => m.len(),
                    Err(_) => 0,
                };
                println!("  {:>6x}  {} bytes", b.index, size);
            }
        }
        Command::Read { path, offset, len } => {
            let analysis = sparsebundle_tools::analyse(&path)?;
            let data = sparsebundle_tools::read_at(&path, analysis.band_size, offset, len)?;
            // Write raw bytes to stdout for piping
            use std::io::Write;
            std::io::stdout().write_all(&data)?;
        }
    }

    Ok(())
}
