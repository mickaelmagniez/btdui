# btdui

A sampling disk usage profiler for btrfs with a GPU-accelerated GUI
([gpui](https://gpui.rs)), in the spirit of [btdu](https://github.com/CyberShadow/btdu).

It shows where the space of a btrfs filesystem actually goes, including what
`du` cannot see: snapshots sharing data, DUP/RAID copies, metadata, slack in
block groups, unreachable extent parts, and unallocated device space.

## Views

- **Rings**: concentric sunburst (like GNOME Disk Usage Analyzer and Filelight).
  Click a segment to zoom in. Click the center or right-click to go back up.
- **Treemap**: nested squarified rectangles (like QDirStat). Click a directory
  to zoom in. Right-click to go back up.
- **List**: sortable table with represented, distributed, exclusive and shared
  sizes, plus a stacked bar for each entry.

The side panel shows the hovered or selected entry: each size with its
statistical margin, an explanation of special `<...>` nodes, and the other
paths sharing the same data (snapshots, reflinks).

## How it works

Like btdu, it samples instead of scanning. Random offsets are drawn uniformly
over the **physical** space of all devices, so each sample stands for
`total size / samples` bytes, and DUP/RAID copies count for what they cost.
Each offset is resolved like this:

1. Device extents (dev tree) map it to a block group, or to `<UNALLOCATED>`.
2. In a data block group, `LOGICAL_INO_V2` returns the `(root, inode)` pairs
   that reference that exact byte. If none, it is `<UNUSED>/<DATA>`. If an
   extent exists but no file references that part, it is `<UNREACHABLE>`.
3. Paths come from `INO_LOOKUP` and from the `ROOT_BACKREF` items of the root
   tree. This works for every subvolume, whichever one is mounted.
4. In a metadata block group, the extent tree gives the tree block and its
   owner roots. Shared back references are followed up to the owners, so each
   subvolume's metadata is attributed to it under `<METADATA>`.

Each sample updates four counters per node:

| Size        | Meaning                                                              |
|-------------|----------------------------------------------------------------------|
| Represented | Each sample is assigned to one path (the shortest), so sizes add up. |
| Distributed | Each sample is split evenly between all its references (1/n each).   |
| Exclusive   | All references are inside this subtree: what deleting it would free. |
| Shared      | At least one reference is inside this subtree: what `du` reports.    |

The GUI runs as your user. Only the sampler (`btdui --sampler PATH`) runs as
root, through `pkexec`. It streams results over a pipe and exits when the GUI
closes.

## Install

Download `btdui-*-x86_64-linux.tar.gz` from the
[releases](https://github.com/mickaelmagniez/btdui/releases). It is a single
binary that needs glibc 2.35 or newer, plus `libxcb` and `libxkbcommon`,
which every Linux desktop has. Or build it from source (Rust 1.88+, and
`clang` for the btrfs bindings):

```sh
cargo build --release
```

## Usage

```sh
./target/release/btdui                # pick a mounted btrfs filesystem
./target/release/btdui /              # sample the filesystem mounted at /
./target/release/btdui --view treemap /
./target/release/btdui --report / 10  # text summary after 10 s, no GUI
```

Environment variables:

- `BTDUI_ELEVATE`: how to get root for the sampler (default `pkexec`).
  For example `sudo -A`, or `none` when already root.
- `BTDUI_THREADS`: number of sampler threads (default: CPU count).

Keys: `↑↓` select, `⏎`/`→` open, `⌫`/`←` go up, `v` switch view,
`1`–`5` sort, `space` pause, `ctrl-c` copy path, `ctrl-q` quit.

## Limitations

- Striped profiles (RAID0/10/5/6) map physical to logical offsets
  proportionally inside a chunk. This keeps sampling uniform but is not
  stripe-exact. Parity in RAID5/6 is attributed to the data it protects.
- Files reachable only through extended inode refs (lots of hard links) show
  up as `<UNLINKED>`.
- `libc` is pinned below 0.2.190, because `xattr` 0.2 (pulled in by gpui) does
  not build with newer versions.
