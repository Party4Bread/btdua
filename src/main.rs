mod actions;
mod app;
mod btrfs;
mod export;
mod fsopen;
mod rng;
mod sampler;
mod stats;
mod tree;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use clap::Parser;
use parking_lot::RwLock;

use export::Meta;
use fsopen::FsHandle;
use sampler::{ChunkMap, Resolver, Sampler};
use tree::Tree;

#[derive(Parser)]
#[command(version, about = "Sampling disk usage analyzer for btrfs")]
struct Cli {
    /// Any path on the btrfs filesystem to analyze
    path: Option<PathBuf>,
    /// Browse a previously exported result (no root needed)
    #[arg(long, value_name = "FILE", conflicts_with = "path")]
    import: Option<PathBuf>,
    /// Sample without the UI and write JSON to FILE
    #[arg(long, value_name = "FILE")]
    export: Option<PathBuf>,
    /// With --export: stop after N samples
    #[arg(long, value_name = "N")]
    samples: Option<u64>,
    /// With --export: stop after S seconds (default 10 when --samples is not given)
    #[arg(long, value_name = "S")]
    seconds: Option<u64>,
    /// Sampler threads (default: CPU count)
    #[arg(long)]
    threads: Option<usize>,
    /// Fixed RNG seed
    #[arg(long)]
    seed: Option<u64>,
}

fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

fn raise_nofile_limit() {
    // SAFETY: plain rlimit struct.
    unsafe {
        let mut r: libc::rlimit = std::mem::zeroed();
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut r) == 0 {
            r.rlim_cur = r.rlim_max;
            libc::setrlimit(libc::RLIMIT_NOFILE, &r);
        }
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    if cli.import.is_some() {
        bail!("--import needs the TUI (added in a later task)");
    }
    let path = cli.path.clone().context("missing PATH (or use --import FILE)")?;
    let fs = Arc::new(FsHandle::open(&path)?);
    raise_nofile_limit();
    let map = Arc::new(ChunkMap::new(btrfs::chunks(&fs.top).context("reading the chunk tree")?));
    if map.total == 0 {
        bail!("no allocated chunks found");
    }
    let meta = Meta {
        version: 1,
        fsid: btrfs::fsid_string(&btrfs::fsid(&fs.top)?),
        device: fs.device.clone(),
        total_bytes: map.total,
        disk_bytes: fsopen::disk_bytes(&fs.mount)?,
        samples: 0,
        timestamp: unix_now(),
    };
    let tree = Arc::new(RwLock::new(Tree::new()));
    let threads = cli.threads.unwrap_or_else(|| thread::available_parallelism().map_or(4, |n| n.get()));
    let seed = cli.seed.unwrap_or_else(rng::time_seed);
    let sampler = Sampler::start(Arc::new(Resolver::new(fs.clone())), map, threads, seed, tree.clone());
    let out = cli.export.clone().context("the TUI is added in a later task; use --export FILE")?;
    let res = headless(&cli, &out, meta, &tree);
    sampler.stop();
    res
}

fn headless(cli: &Cli, out: &Path, mut meta: Meta, tree: &RwLock<Tree>) -> Result<()> {
    let secs = cli.seconds.or(if cli.samples.is_none() { Some(10) } else { None });
    let deadline = secs.map(|s| Instant::now() + Duration::from_secs(s));
    loop {
        thread::sleep(Duration::from_millis(200));
        let n = tree.read().total_samples;
        eprint!("\r{} samples", stats::fmt_count(n));
        if cli.samples.is_some_and(|s| n >= s) || deadline.is_some_and(|d| Instant::now() >= d) {
            break;
        }
    }
    eprintln!();
    let t = tree.read();
    meta.samples = t.total_samples;
    meta.timestamp = unix_now();
    export::write(out, &meta, &t)?;
    eprintln!("wrote {}", out.display());
    Ok(())
}
