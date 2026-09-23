# btdua

A sampling disk usage analyzer for btrfs with an ncdu-style TUI.

Like [btdu](https://github.com/CyberShadow/btdu), btdua picks random points in the
filesystem's allocated space and asks btrfs who owns them. Results appear within
seconds and get more precise the longer it runs. Because it works on extents, it
understands snapshots, reflinks, compression and metadata, which `du`/ncdu cannot.

## Build

```sh
cargo build --release
```

## Usage

```sh
sudo btdua /                       # analyze the filesystem containing /
sudo btdua / --export out.json     # sample 10 s without UI, save results
sudo btdua / --export out.json --samples 1000000
btdua --import out.json            # browse saved results (no root needed)
```

The whole filesystem is shown from its top level (subvolid 5). If the path you
pass is inside a subvolume, btdua privately mounts the top level in its own
mount namespace; nothing appears in the system mount table.

Options: `--threads N` (default: CPU count), `--seed N`, `--seconds S`.

## Columns

| Column  | Meaning |
|---------|---------|
| Size    | Estimate in the current size mode (`m` to cycle) |
| Excl    | Space freed if the entry were deleted |
| Shared  | Space the entry shares with paths outside it |
| Ratio   | Compressed / uncompressed (`-` when unknown) |
| Samples | Samples credited to the entry |

Size modes: **represented** (each sample counted once, at its canonical owner),
**distributed** (shared samples split evenly), **exclusive**, **shared**.
The header shows the 95% error margin for the current directory.

Special entries: `<METADATA>`, `<SYSTEM>`, `<UNUSED>` (allocated but free),
`<UNREACHABLE>` (allocated extents no file references, e.g. parts of partially
overwritten extents or deleted subvolumes awaiting cleanup), `<ERROR>`.

## Keys

| Key | Action |
|-----|--------|
| `↑↓` `j` `k` `PgUp` `PgDn` `g` `G` | move |
| `→` `l` `Enter` / `←` `h` `Backspace` | open directory / go up |
| `s` `n` `C` | sort by size / name / samples (again: reverse) |
| `r` | reverse sort |
| `m` | cycle size mode |
| `c` | toggle Excl/Shared/Ratio columns |
| `Space` / `u` | mark entry / clear all marks |
| `d` | delete marked entries (or the selected one) |
| `i` | info pane; `x` inside it computes the exact size |
| `e` | export JSON |
| `p` | pause/resume sampling |
| `?` | help |
| `q` | quit |

Deleting asks for confirmation, and asks twice when a target is or contains a
subvolume. Subvolumes are destroyed with `BTRFS_IOC_SNAP_DESTROY`, nested ones
first.

## Testing

`cargo test` runs the unit tests. `sudo scripts/loop-smoke.sh` builds a 1 GiB
loopback btrfs image with known contents (a snapshot, a reflink, a compressed
file and a nested subvolume) and prints its mount point, for manual testing.
