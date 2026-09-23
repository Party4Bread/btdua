# btdua — sampling disk usage analyzer for btrfs

## Goal

A btdu-like disk usage analyzer for btrfs, written in Rust, with a TUI that
matches ncdu's ergonomics and adds btrfs-aware information (exclusive/shared
space, compression ratio, snapshots) plus in-UI actions (delete, export).

Invocation: `sudo btdua <path>` — analyzes the btrfs filesystem containing
`<path>`; the browsed tree is rooted at the filesystem's top level (subvolid 5).
If `<path>` is not the top-level subvolume root, btdua unshares its mount
namespace and privately mounts the device with `subvolid=5` on a temporary
directory, so every subvolume is reachable and nothing leaks into the
system mount table.

## Measurement: statistical sampling

1. Read the chunk tree (`BTRFS_IOC_TREE_SEARCH_V2` on tree 3) to list every
   allocated chunk: logical start, length, type (DATA / METADATA / SYSTEM,
   profile). Also read `BTRFS_IOC_FS_INFO` / `BTRFS_IOC_SPACE_INFO` for totals.
2. Pick a uniformly random byte across the summed chunk lengths (weighted by
   chunk length), giving a logical offset.
3. Resolve:
   - METADATA / SYSTEM chunk → bucket `<METADATA>` / `<SYSTEM>`.
   - DATA chunk → `BTRFS_IOC_LOGICAL_INO_V2` (no `IGNORE_OFFSET`, so only
     references covering that exact byte count) → list of (inode, file
     offset, root). `ENOENT` (no extent at that byte) → `<UNUSED>`;
     extent exists but no reference covers the byte → `<UNREACHABLE>`. For each (root, inode), resolve
     paths with `BTRFS_IOC_INO_PATHS` on an fd for that subvolume; subvolume
     path prefix comes from `BTRFS_IOC_INO_LOOKUP` / root backrefs.
   - Extent referenced but no path resolves (orphans, deleted snapshots
     pending cleanup) → `<UNREACHABLE>`.
   - ioctl errors → `<ERROR>/<errno name>`.
4. Compression: for the representative owner, search its EXTENT_DATA
   items with key offset in `[file_offset - 128 KiB, file_offset]`; a
   matching compressed item gives disk/ram bytes. No match means the byte
   is in an uncompressed extent (compressed extents never exceed 128 KiB).
   Failure here is non-fatal (ratio shown as `-`).

Each sample is inserted into the path tree. With `N` total samples and
`A` total allocated bytes, a node with `k` samples is estimated at `k/N · A`,
with a 95% interval `±1.96·sqrt(p(1-p)/N)·A`, `p = k/N`.

### Size modes (per node)

- **represented** — sample credited to one canonical owner path (shortest
  path, ties broken lexicographically, preferring non-snapshot subvolumes
  as btdu does: lowest subvolume id).
- **distributed** — sample split `1/m` across all `m` owner paths.
- **exclusive** — counted at a node when *all* owner paths of the sample
  lie inside that node's subtree (i.e. deleting the node would free it).
- **shared** — counted at a node when some, but not all, owner paths lie
  inside its subtree (sum across siblings can exceed the parent).

Counters propagate to ancestors; each sample counts at most once per node
for represented/exclusive/shared, and `distributed` sums `1/m` fractions.

## Architecture

Single crate, binary `btdua`.

| Module | Responsibility |
|---|---|
| `btrfs/` | Raw ioctl wrappers and struct decoding: tree search v2, logical_ino v2, ino_paths, ino_lookup, fs_info, snap_destroy. No policy. |
| `sampler` | Chunk map, weighted random offset, resolve to `Sample { kind, owners, compressed }`. N worker threads (default = CPU count) send samples over a crossbeam channel. |
| `tree` | Arena-allocated path trie with interned name segments. Node counters: represented, distributed (f64), exclusive, shared, compressed/uncompressed byte sums, sample count. Special top-level buckets `<METADATA>`, `<SYSTEM>`, `<UNUSED>`, `<UNREACHABLE>`, `<ERROR>`. Nodes flagged as subvolume roots. |
| `stats` | Counts → bytes and confidence intervals. |
| `ui/` | ratatui + crossterm: browser, info pane, help, dialogs. |
| `actions` | Delete files/dirs/subvolumes (single or batch), exact size, JSON export/import. |

Data flow: sampler threads → channel → aggregator thread (inserts into
`Arc<RwLock<Tree>>`) → UI thread redraws ~4 Hz from the shared tree.

### Headless modes

- `--export out.json [--samples N | --seconds S]` — sample then write JSON, no TUI.
- `--import out.json` — browse saved results; no root, no btrfs needed
  (sampling, delete and exact-info are disabled).
- `--threads N`, `--seed N` (deterministic offsets for debugging).

JSON format: header (fs uuid, path, total allocated bytes, sample count,
timestamp, version) + nested node objects with counters.

## TUI

```
 btdua  /  ▸ home ▸ p4b                           samples 184,302  ±0.4%  ● sampling
 Sort: size ↓   Mode: [represented] distributed exclusive shared
──────────────────────────────────────────────────────────────────────────────────────
   Size    Excl     Shared   Ratio  [Graph                ]   Items   Name
  41.2 GiB  12.0 GiB 29.2 GiB 0.62  [██████████████▌      ]   8,120   .local/
  18.7 GiB  18.7 GiB     0 B  1.00  [██████▋              ]     902   Documents/
   6.1 GiB   0.3 GiB  5.8 GiB 0.71  [██▏                  ]      44   .snapshots/  ⊞subvol
──────────────────────────────────────────────────────────────────────────────────────
 Total 68.0 GiB · Excl 33.0 GiB · Shared 35.0 GiB · Disk 931 GiB   ?:help  d:delete  e:export
```

- Columns: size (current mode), exclusive, shared, compression ratio
  (disk/uncompressed), bar graph relative to the largest sibling, sample
  count ("Items"), name. `/` suffix for directories, `⊞subvol` tag for
  subvolume roots, special buckets shown as `<NAME>`. `c` toggles
  optional columns.
- Keys (ncdu-compatible where possible):
  - `↑↓ / j k`, `PgUp/PgDn`, `Home/End` — move
  - `→ / l / Enter` — open; `← / h / Backspace` — up
  - `s` size, `n` name, `C` count sort; `r` reverse
  - `m` cycle size mode; `p` pause/resume sampling
  - `Space` mark/unmark entry (cursor advances); `u` clear all marks
  - `i` info pane; `d` delete (marked entries, or the cursor entry if none
    are marked); `e` export; `?` help; `q` quit
- Info pane (`i`): full path, all sizes per mode with ±error, compression
  ratio, subvolume flag, up to 5 other paths recently seen sharing extents
  with this entry, and an on-demand **exact** size (`x` inside the pane)
  computed in a background thread by walking the directory and reading
  each file's EXTENT_DATA items via tree search, summing unique extents
  (compsize-style: disk bytes, uncompressed bytes, referenced bytes).
- Live updates: sizes refresh as samples arrive; selection is tracked by
  node id, not row index, so re-sorting never moves the cursor to a
  different entry.
- Marks: marked rows show a `*` prefix and highlight; marks persist while
  navigating between directories; header shows `N marked · <size>`.
- Delete: confirmation dialog lists the targets (up to 10, then "and N
  more") with their total estimated exclusive size; if any target is or
  contains a subvolume a second confirmation is required. Subvolumes are
  removed with `BTRFS_IOC_SNAP_DESTROY` (nested subvolumes first). Targets
  are deleted sequentially in a background thread; failures are collected
  and shown in a dialog while the rest continue. Each deleted node is
  removed from the tree and its counters subtracted from ancestors.
  Special `<BUCKET>` entries cannot be marked or deleted.
- Export: prompt for file name (default `btdua-<timestamp>.json`).

## Error handling

- Not root or not btrfs → exit with a clear message; suggest `--import`.
- Per-sample failures are recorded in buckets (`<UNREACHABLE>`,
  `<ERROR>/<errno>`), never abort sampling.
- Delete/export failures → error dialog; UI keeps running.
- Terminal restored on exit and on panic (panic hook).

## Testing (kept light)

- Unit tests for the non-ioctl logic: tree counting/size modes, stats math,
  JSON round-trip.
- Manual verification on the real filesystem (`sudo btdua /`) and a quick
  comparison of `--export` output against `btrfs fi du` for a known directory.

## Out of scope

Fuzzy search, mouse support, remote/SSH mode, non-btrfs filesystems.
