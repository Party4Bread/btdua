mod actions;
mod app;
mod btrfs;
mod export;
mod fsat;
mod fsopen;
mod rng;
mod sampler;
mod stats;
mod tree;
mod ui;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use clap::Parser;
use crossbeam_channel::{Sender, unbounded};
use parking_lot::RwLock;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind};

use app::{Action, App, Dialog, ExactState};
use export::Meta;
use fsopen::FsHandle;
use sampler::{ChunkMap, Resolver, Sampler};
use tree::{NodeId, ROOT, Tree};

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

/// Results from background threads.
enum Bg {
    Deleted(NodeId),
    DeleteFailed(String),
    DeleteDone,
    Exact(NodeId, Result<actions::Exact, String>),
}

struct Live {
    fs: Arc<FsHandle>,
    resolver: Arc<Resolver>,
    paused: Arc<AtomicBool>,
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
    if let Some(file) = &cli.import {
        let (meta, tree) = export::read(file)?;
        return run_tui(meta, Arc::new(RwLock::new(tree)), None);
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
    let resolver = Arc::new(Resolver::new(fs.clone()));
    let sampler = Sampler::start(resolver.clone(), map, threads, seed, tree.clone());
    let res = match &cli.export {
        Some(out) => headless(&cli, out, meta, &tree),
        None => {
            let live = Live { fs: fs.clone(), resolver, paused: sampler.paused.clone() };
            run_tui(meta, tree, Some(live))
        }
    };
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

fn run_tui(meta: Meta, tree: Arc<RwLock<Tree>>, live: Option<Live>) -> Result<()> {
    let mut terminal = ratatui::init();
    let res = tui_loop(&mut terminal, &meta, &tree, live.as_ref());
    ratatui::restore();
    res
}

fn tui_loop(terminal: &mut ratatui::DefaultTerminal, meta: &Meta, tree: &RwLock<Tree>, live: Option<&Live>) -> Result<()> {
    let (bg_tx, bg_rx) = unbounded();
    let mut app = App::new(live.is_some());
    let mut subvols_checked: Option<Instant> = None;
    if let Some(l) = live {
        app.protected = l.fs.mounted.clone();
    }
    loop {
        for ev in bg_rx.try_iter() {
            apply_bg(&mut app, tree, ev);
        }
        if let Some(l) = live
            && subvols_checked.is_none_or(|t| t.elapsed() > Duration::from_secs(5))
        {
            refresh_subvols(l, tree);
            subvols_checked = Some(Instant::now());
        }
        {
            let t = tree.read();
            terminal.draw(|f| ui::draw(f, &mut app, &t, meta))?;
        }
        if !event::poll(Duration::from_millis(250))? {
            continue;
        }
        let Event::Key(key) = event::read()? else { continue };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        if let (Some(l), KeyCode::Char('d')) = (live, key.code) {
            // Sampling may not have hit every subvolume; the delete
            // confirmation must know about all of them.
            refresh_subvols(l, tree);
        }
        let action = app.on_key(key, &tree.read());
        match action {
            Action::None => {}
            Action::Quit => return Ok(()),
            Action::TogglePause => {
                if let Some(l) = live {
                    l.paused.store(app.paused, Ordering::Relaxed);
                }
            }
            Action::Export(name) => {
                let t = tree.read();
                let mut m = meta.clone();
                m.samples = t.total_samples;
                m.timestamp = unix_now();
                app.dialog = match export::write(Path::new(&name), &m, &t) {
                    Ok(()) => Dialog::message("Exported", format!("Saved to {name}")),
                    Err(e) => Dialog::message("Export failed", format!("{e:#}")),
                };
            }
            Action::Delete(ids) => {
                if let Some(l) = live {
                    start_delete(&mut app, &tree.read(), l, ids, bg_tx.clone());
                }
            }
            Action::Exact(id) => {
                if let Some(l) = live {
                    app.exact.insert(id, ExactState::Running);
                    let path = tree.read().path_of(id);
                    let (fs, tx) = (l.fs.clone(), bg_tx.clone());
                    thread::spawn(move || {
                        let r = actions::exact_size(&fs.top, &path).map_err(|e| e.to_string());
                        let _ = tx.send(Bg::Exact(id, r));
                    });
                }
            }
        }
    }
}

fn refresh_subvols(live: &Live, tree: &RwLock<Tree>) {
    let paths = live.resolver.subvolume_paths();
    let mut t = tree.write();
    for p in &paths {
        t.mark_subvol(p);
    }
}

fn start_delete(app: &mut App, t: &Tree, live: &Live, ids: Vec<NodeId>, tx: Sender<Bg>) {
    let jobs: Vec<(NodeId, String)> = ids.into_iter().map(|id| (id, t.path_of(id))).collect();
    app.deleting += jobs.len();
    let fs = live.fs.clone();
    let resolver = live.resolver.clone();
    thread::spawn(move || {
        for (id, path) in jobs {
            let res = fsat::delete_rel(&fs.top, &path);
            // Even a failed delete may have destroyed nested subvolumes, and
            // cached fds would keep resolving into them.
            resolver.invalidate();
            let ev = match res {
                Ok(()) => Bg::Deleted(id),
                Err(e) => Bg::DeleteFailed(format!("/{path}: {e}")),
            };
            let _ = tx.send(ev);
        }
        let _ = tx.send(Bg::DeleteDone);
    });
}

fn apply_bg(app: &mut App, tree: &RwLock<Tree>, ev: Bg) {
    match ev {
        Bg::Deleted(id) => {
            let mut t = tree.write();
            t.remove(id);
            app.marks.remove(&id);
            app.deleting = app.deleting.saturating_sub(1);
            while !t.is_live(app.dir) {
                app.dir = t.node(app.dir).parent.unwrap_or(ROOT);
            }
        }
        Bg::DeleteFailed(msg) => {
            app.delete_errors.push(msg);
            app.deleting = app.deleting.saturating_sub(1);
        }
        Bg::DeleteDone => {
            if app.deleting == 0 && !app.delete_errors.is_empty() {
                let errs = std::mem::take(&mut app.delete_errors);
                app.dialog = Dialog::message("Some deletions failed", errs.join("\n"));
            }
        }
        Bg::Exact(id, r) => {
            let state = match r {
                Ok(e) => ExactState::Done(e),
                Err(e) => ExactState::Failed(e),
            };
            app.exact.insert(id, state);
        }
    }
}
