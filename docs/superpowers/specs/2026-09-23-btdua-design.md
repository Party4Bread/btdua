# btdua — sampling disk usage analyzer for btrfs

## Goal

A btdu-like disk usage analyzer for btrfs, written in Rust, with a TUI that
matches ncdu's ergonomics and adds btrfs-aware information (exclusive/shared
space, compression ratio, snapshots) plus in-UI actions (delete, export).

Invocation: `sudo btdua <path>` — analyzes the btrfs filesystem containing
`<path>`; the browsed tree is rooted at the filesystem's top level (subvolid 5).

## Measurement: statistical sampling

1. Read the chunk tree (`BTRFS_IOC_TREE_SEARCH_V2` on tree 3) to list every
   allocated chunk: logical start, length, type (DATA / METADATA / SYSTEM,
   profile). Also read `BTRFS_IOC_FS_INFO` / `BTRFS_IOC_SPACE_INFO` for totals.
2. Pick a uniformly random byte across the summed chunk lengths (weighted by
   chunk length), giving a logical offset.
3. Resolve:
   - METADATA / SYSTEM chunk → bucket `<METADATA>` / `<SYSTEM>`.
   - DATA chunk → `BTRFS_IOC_LOGICAL_INO_V2` (with `IGNORE_OFFSET` flag) →
     list of (inode, offset, root). If the offset is not inside any extent
     (free space in the chunk) → `<UNUSED>`. For each (root, inode), resolve
     paths with `BTRFS_IOC_INO_PATHS` on an fd for that subvolume; subvolume
     path prefix comes from `BTRFS_IOC_INO_LOOKUP` / root backrefs.
   - Extent referenced but no path resolves (orphans, deleted snapshots
     pending cleanup) → `<UNREACHABLE>`.
   - ioctl errors → `<ERROR>/<errno name>`.
4. Compression: look up the extent item's file-extent data
   (`TREE_SEARCH_V2` on the owning root for the inode's EXTENT_DATA items near
   the file offset) to record compression type and disk vs. ram bytes.
   Failure here is non-fatal (ratio shown as `-`).

Each sample is inserted into the path tree. With `N` total samples and
`A` total allocated bytes, a node with `k` samples is estimated at `k/N · A`,
with a 95% interval `±1.96·sqrt(p(1-p)/N)·A`, `p = k/N`.

### Size modes (per node)

- **represented** — sample credited to one canonical owner path (shortest
  path, ties broken lexicographically, preferring non-snapshot subvolumes
  as btdu does: lowest subvolume id).
- **distributed** — sample split `1/m` across all `m` owner paths.
- **exclusive** — counted only when the extent has exactly one owner path.
- **shared** — counted at every owner path when `m > 1` (sum can exceed
  total; displayed but not used for the global total).

Counters propagate to ancestors (for `represented`/`exclusive`, each sample
counts once per ancestor chain; `distributed` sums fractions).

## Architecture

Single crate, binary `btdua`.

| Module | Responsibility |
|---|---|
| `btrfs/` | Raw ioctl wrappers and struct decoding: tree search v2, logical_ino v2, ino_paths, ino_lookup, fs_info, snap_destroy v2. No policy. |
| `sampler` | Chunk map, weighted random offset, resolve to `Sample { kind, owners, compressed }`. N worker threads (default = CPU count) send samples over a crossbeam channel. |
| `tree` | Arena-allocated path trie with interned name segments. Node counters: represented, distributed (f64), exclusive, shared, compressed/uncompressed byte sums, sample count. Special top-level buckets `<METADATA>`, `<SYSTEM>`, `<UNUSED>`, `<UNREACHABLE>`, `<ERROR>`. Nodes flagged as subvolume roots. |
| `stats` | Counts → bytes and confidence intervals. |
| `ui/` | ratatui + crossterm: browser, info pane, help, dialogs. |
| `actions` | Delete file/dir, delete subvolume, JSON export/import. |

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
  - `i` info pane; `d` delete; `e` export; `?` help; `q` quit
- Info pane (`i`): full path, all sizes per mode with ±error, all sharing
  owner paths seen for this node's samples (top 10 by frequency),
  compression breakdown by algorithm, and an on-demand **exact** size
  (`x` inside the pane) computed in a background thread by walking the
  directory with `FIEMAP`, counting unique physical extents (compsize-style).
- Live updates: sizes refresh as samples arrive; selection is tracked by
  node id, not row index, so re-sorting never moves the cursor to a
  different entry.
- Delete: confirmation dialog showing path and estimated size; subvolumes
  require a second confirmation and use `SNAP_DESTROY_V2`. On success the
  node is marked `(deleted)` and its samples are subtracted from ancestors.
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

Fuzzy search, multi-select, mouse support, remote/SSH mode, non-btrfs filesystems.
