use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;

mod mfs;

use mfs::Mfs;

#[derive(Parser)]
#[command(
    name = "mkfs-mfs",
    version,
    about = "Create Macintosh File System (MFS) disk images"
)]
struct Cli {
    /// Populate from this directory tree (subdirectories are flattened
    /// using ':' as the Mac path separator)
    #[arg(short = 'd', long)]
    directory: Option<PathBuf>,

    /// Image size in kilobytes (default: 400, a Sony 3.5" SS floppy)
    #[arg(short = 's', long, default_value_t = 400)]
    size_kb: u32,

    /// Volume name (max 27 bytes; default "Untitled")
    #[arg(short = 'v', long)]
    volume: Option<String>,

    /// Output filename
    filename: PathBuf,
}

fn run() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let mut mfs = Mfs::new(cli.size_kb)?;
    if let Some(name) = cli.volume.as_deref() {
        mfs.set_volume_name(name)?;
    }
    if let Some(src) = &cli.directory {
        mfs.populate_root(src)?;
    }
    let stats = mfs.write_to(&cli.filename)?;

    println!(
        "Created MFS image: {} ({}KB, {} alloc blocks, {} files, sig=0x{:X})",
        cli.filename.display(),
        cli.size_kb,
        stats.num_alloc_blks,
        stats.file_count,
        mfs::SIGNATURE,
    );
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("mkfs-mfs: {e:#}");
            ExitCode::FAILURE
        }
    }
}
