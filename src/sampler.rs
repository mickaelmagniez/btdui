//! The privileged half: picks random physical offsets on the filesystem's
//! devices, figures out what lives there, and streams the answers to stdout.
//!
//! Sampling is done in *physical* space (the sum of all device sizes), so
//! that RAID/DUP copies, unallocated space and slack in block groups are all
//! accounted for proportionally to what they really cost on disk.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{self, BufWriter, Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::Duration;

use rand::Rng;

use crate::btrfs::{self, Fs, Key, le_u16, le_u64};
use crate::protocol::{self, Header, Message};

const LAYOUT_REFRESH: Duration = Duration::from_secs(30);
const INODE_CACHE_LIMIT: usize = 1 << 20;
const MAX_BACKREF_VISITS: usize = 64;

struct DevExtent {
    start: u64,
    len: u64,
    chunk: u64,
}

struct Device {
    devid: u64,
    size: u64,
    /// Sorted by `start`.
    extents: Vec<DevExtent>,
}

struct Chunk {
    len: u64,
    flags: u64,
}

/// Physical layout of the filesystem: which device ranges belong to which chunk.
struct Layout {
    devices: Vec<Device>,
    chunks: BTreeMap<u64, Chunk>,
    total: u64,
}

impl Layout {
    fn load(fs: &Fs) -> io::Result<Self> {
        let mut devices: Vec<Device> = fs
            .tree_search(
                btrfs::CHUNK_TREE_OBJECTID,
                Key::new(btrfs::DEV_ITEMS_OBJECTID, btrfs::DEV_ITEM_KEY, 0),
                Key::new(btrfs::DEV_ITEMS_OBJECTID, btrfs::DEV_ITEM_KEY, u64::MAX),
                usize::MAX,
            )?
            .into_iter()
            .map(|it| Device {
                devid: le_u64(&it.data, 0),
                size: le_u64(&it.data, 8),
                extents: Vec::new(),
            })
            .collect();
        devices.sort_by_key(|d| d.devid);

        let chunks = fs
            .tree_search(
                btrfs::CHUNK_TREE_OBJECTID,
                Key::new(btrfs::FIRST_FREE_OBJECTID, btrfs::CHUNK_ITEM_KEY, 0),
                Key::new(btrfs::FIRST_FREE_OBJECTID, btrfs::CHUNK_ITEM_KEY, u64::MAX),
                usize::MAX,
            )?
            .into_iter()
            .map(|it| {
                (
                    it.key.offset,
                    Chunk {
                        len: le_u64(&it.data, 0),
                        flags: le_u64(&it.data, 24),
                    },
                )
            })
            .collect();

        let dev_extents = fs.tree_search(
            btrfs::DEV_TREE_OBJECTID,
            Key::new(0, btrfs::DEV_EXTENT_KEY, 0),
            Key::new(u64::MAX, btrfs::DEV_EXTENT_KEY, u64::MAX),
            usize::MAX,
        )?;
        for it in dev_extents
            .into_iter()
            .filter(|it| it.key.ty == btrfs::DEV_EXTENT_KEY)
        {
            if let Some(dev) = devices.iter_mut().find(|d| d.devid == it.key.objectid) {
                dev.extents.push(DevExtent {
                    start: it.key.offset,
                    chunk: le_u64(&it.data, 16),
                    len: le_u64(&it.data, 24),
                });
            }
        }
        for dev in &mut devices {
            dev.extents.sort_by_key(|e| e.start);
        }
        let total = devices.iter().map(|d| d.size).sum();
        if total == 0 {
            return Err(io::Error::other("filesystem reports no device space"));
        }
        Ok(Self {
            devices,
            chunks,
            total,
        })
    }

    fn locate(&self, mut phys: u64) -> Location {
        let multi = self.devices.len() > 1;
        for dev in &self.devices {
            if phys >= dev.size {
                phys -= dev.size;
                continue;
            }
            let idx = dev.extents.partition_point(|e| e.start <= phys);
            if idx > 0 {
                let ext = &dev.extents[idx - 1];
                let off = phys - ext.start;
                if off < ext.len
                    && let Some(chunk) = self.chunks.get(&ext.chunk)
                {
                    // Striped profiles interleave the chunk over several
                    // devices; mapping proportionally keeps sampling uniform.
                    let in_chunk = if chunk.flags & btrfs::BG_STRIPED != 0 {
                        ((off as u128 * chunk.len as u128) / ext.len as u128) as u64
                    } else {
                        off
                    };
                    return Location::Chunk {
                        logical: ext.chunk + in_chunk.min(chunk.len.saturating_sub(1)),
                        flags: chunk.flags,
                    };
                }
            }
            return Location::Unallocated {
                devid: multi.then_some(dev.devid),
            };
        }
        Location::Unallocated { devid: None }
    }
}

enum Location {
    Unallocated { devid: Option<u64> },
    Chunk { logical: u64, flags: u64 },
}

fn errno_label(err: &io::Error) -> String {
    let name = match err.raw_os_error() {
        Some(libc::ENOENT) => "ENOENT",
        Some(libc::EPERM) => "EPERM",
        Some(libc::EACCES) => "EACCES",
        Some(libc::EINVAL) => "EINVAL",
        Some(libc::ENOMEM) => "ENOMEM",
        Some(libc::EIO) => "EIO",
        Some(libc::EOVERFLOW) => "EOVERFLOW",
        Some(libc::ENOTTY) => "ENOTTY",
        Some(libc::ENAMETOOLONG) => "ENAMETOOLONG",
        _ => return format!("<ERROR>/<{err}>"),
    };
    format!("<ERROR>/<{name}>")
}

fn join(a: &str, b: &str) -> String {
    match (a.is_empty(), b.is_empty()) {
        (true, _) => b.to_owned(),
        (_, true) => a.to_owned(),
        _ => format!("{a}/{b}"),
    }
}

fn tree_name(root: u64) -> Option<&'static str> {
    Some(match root {
        1 => "<ROOT TREE>",
        2 => "<EXTENT TREE>",
        3 => "<CHUNK TREE>",
        4 => "<DEV TREE>",
        7 => "<CSUM TREE>",
        8 => "<QUOTA TREE>",
        9 => "<UUID TREE>",
        10 => "<FREE SPACE TREE>",
        11 => "<BLOCK GROUP TREE>",
        12 => "<RAID STRIPE TREE>",
        r if r == (-7i64) as u64 => "<TREE LOG>",
        r if r == (-8i64) as u64 => "<TREE RELOC>",
        r if r == (-9i64) as u64 => "<DATA RELOC TREE>",
        _ => return None,
    })
}

fn is_fs_tree(root: u64) -> bool {
    root == btrfs::FS_TREE_OBJECTID
        || (btrfs::FIRST_FREE_OBJECTID..=(-256i64) as u64).contains(&root)
}

/// Per-thread resolver with its own caches (no cross-thread contention).
struct Resolver {
    fs: Arc<Fs>,
    nodesize: u64,
    subvols: HashMap<u64, Option<String>>,
    inodes: HashMap<(u64, u64), Option<String>>,
}

impl Resolver {
    fn new(fs: Arc<Fs>, nodesize: u64) -> Self {
        Self {
            fs,
            nodesize,
            subvols: HashMap::new(),
            inodes: HashMap::new(),
        }
    }

    /// Location of subvolume `root` relative to the top-level subvolume.
    fn subvol_path(&mut self, root: u64) -> Option<String> {
        if root == btrfs::FS_TREE_OBJECTID {
            return Some(String::new());
        }
        if let Some(hit) = self.subvols.get(&root) {
            return hit.clone();
        }
        // Mark as in progress to guard against cycles in a corrupted tree.
        self.subvols.insert(root, None);
        let resolved = self.lookup_subvol(root);
        self.subvols.insert(root, resolved.clone());
        resolved
    }

    fn lookup_subvol(&mut self, root: u64) -> Option<String> {
        let refs = self
            .fs
            .tree_search(
                btrfs::ROOT_TREE_OBJECTID,
                Key::new(root, btrfs::ROOT_BACKREF_KEY, 0),
                Key::new(root, btrfs::ROOT_BACKREF_KEY, u64::MAX),
                1,
            )
            .ok()?;
        let item = refs.into_iter().next()?;
        let parent = item.key.offset;
        // struct btrfs_root_ref { u64 dirid; u64 sequence; u16 name_len; } + name
        let dirid = le_u64(&item.data, 0);
        let name_len = le_u16(&item.data, 16) as usize;
        let name = String::from_utf8_lossy(item.data.get(18..18 + name_len)?).into_owned();
        let parent_path = self.subvol_path(parent)?;
        let dir = self.fs.ino_lookup(parent, dirid).ok()?;
        let dir = String::from_utf8_lossy(&dir);
        Some(join(&parent_path, &format!("{dir}{name}")))
    }

    fn inode_path(&mut self, root: u64, inode: u64) -> String {
        let Some(subvol) = self.subvol_path(root) else {
            return "<ORPHAN>".to_owned();
        };
        if let Some(hit) = self.inodes.get(&(root, inode)) {
            return hit.clone().unwrap_or_else(|| join(&subvol, "<UNLINKED>"));
        }
        let rel = self
            .fs
            .ino_lookup(root, inode)
            .ok()
            .map(|p| String::from_utf8_lossy(&p).trim_end_matches('/').to_owned());
        if self.inodes.len() >= INODE_CACHE_LIMIT {
            self.inodes.clear();
        }
        self.inodes
            .insert((root, inode), rel.clone().map(|r| join(&subvol, &r)));
        match rel {
            Some(r) => join(&subvol, &r),
            None => join(&subvol, "<UNLINKED>"),
        }
    }

    /// `Ok(None)` means nothing is allocated at `logical`.
    fn resolve_data(&mut self, logical: u64) -> io::Result<Option<Vec<String>>> {
        let refs = match self.fs.logical_ino(logical) {
            Err(e) if e.raw_os_error() == Some(libc::ENOENT) => return Ok(None),
            r => r?,
        };
        if refs.is_empty() {
            // The extent exists, but no file references this part of it
            // anymore (e.g. a partially overwritten "bookend" extent).
            return Ok(Some(vec!["<UNREACHABLE>".to_owned()]));
        }
        Ok(Some(
            refs.into_iter()
                .map(|(inode, _, root)| self.inode_path(root, inode))
                .collect(),
        ))
    }

    /// Finds the tree block covering `logical` and the trees owning it.
    /// `Ok(None)` means no tree block is allocated there.
    fn resolve_metadata(&mut self, logical: u64) -> io::Result<Option<Vec<String>>> {
        let mut roots = HashSet::new();
        let mut visited = 0;
        if !self.metadata_roots(logical, &mut roots, &mut visited)? {
            return Ok(None);
        }
        if roots.is_empty() {
            return Ok(Some(vec!["<METADATA>/<UNKNOWN OWNER>".to_owned()]));
        }
        let paths = roots
            .into_iter()
            .map(|root| match tree_name(root) {
                Some(name) => format!("<METADATA>/{name}"),
                None if is_fs_tree(root) => match self.subvol_path(root) {
                    Some(p) if p.is_empty() => "<METADATA>/<SUBVOLUMES>/<TOP LEVEL>".to_owned(),
                    Some(p) => format!("<METADATA>/<SUBVOLUMES>/{p}/<TREE>"),
                    None => "<METADATA>/<SUBVOLUMES>/<ORPHAN>".to_owned(),
                },
                None => format!("<METADATA>/<TREE {root}>"),
            })
            .collect();
        Ok(Some(paths))
    }

    /// Walks tree-block back references up to the owning roots. Returns
    /// whether an extent item covers `logical`.
    fn metadata_roots(
        &mut self,
        logical: u64,
        roots: &mut HashSet<u64>,
        visited: &mut usize,
    ) -> io::Result<bool> {
        *visited += 1;
        let lo = logical.saturating_sub(self.nodesize - 1);
        let items = self.fs.tree_search(
            btrfs::EXTENT_TREE_OBJECTID,
            Key::new(lo, btrfs::EXTENT_ITEM_KEY, 0),
            Key::new(logical, btrfs::SHARED_BLOCK_REF_KEY, u64::MAX),
            4096,
        )?;
        let extent = items.iter().rev().find(|it| {
            let len = match it.key.ty {
                btrfs::METADATA_ITEM_KEY => self.nodesize,
                btrfs::EXTENT_ITEM_KEY => it.key.offset,
                _ => return false,
            };
            it.key.objectid <= logical && logical < it.key.objectid + len
        });
        let Some(extent) = extent else {
            return Ok(false);
        };
        let bytenr = extent.key.objectid;
        let mut parents = Vec::new();

        // Inline references, after struct btrfs_extent_item { refs, generation, flags }.
        let data = &extent.data;
        let mut pos = 24;
        if extent.key.ty == btrfs::EXTENT_ITEM_KEY
            && le_u64(data, 16) & btrfs::EXTENT_FLAG_TREE_BLOCK != 0
        {
            pos += 18; // struct btrfs_tree_block_info { key, level }
        }
        while pos + 9 <= data.len() {
            let ty = data[pos];
            let offset = le_u64(data, pos + 1);
            match ty {
                btrfs::TREE_BLOCK_REF_KEY => {
                    roots.insert(offset);
                }
                btrfs::SHARED_BLOCK_REF_KEY => parents.push(offset),
                btrfs::EXTENT_OWNER_REF_KEY => {}
                btrfs::EXTENT_DATA_REF_KEY => pos += 20,
                btrfs::SHARED_DATA_REF_KEY => pos += 4,
                _ => break,
            }
            pos += 9;
        }
        // Keyed (non-inline) references.
        for it in items.iter().filter(|it| it.key.objectid == bytenr) {
            match it.key.ty {
                btrfs::TREE_BLOCK_REF_KEY => {
                    roots.insert(it.key.offset);
                }
                btrfs::SHARED_BLOCK_REF_KEY => parents.push(it.key.offset),
                _ => {}
            }
        }
        // Shared (full) back references point at a parent block: whoever
        // owns the parent also owns this block.
        for parent in parents {
            if *visited >= MAX_BACKREF_VISITS {
                break;
            }
            self.metadata_roots(parent, roots, visited)?;
        }
        Ok(true)
    }

    fn sample(&mut self, layout: &Layout, phys: u64) -> Vec<String> {
        let result = match layout.locate(phys) {
            Location::Unallocated { devid: Some(id) } => {
                Ok(vec![format!("<UNALLOCATED>/<DEVICE {id}>")])
            }
            Location::Unallocated { devid: None } => Ok(vec!["<UNALLOCATED>".to_owned()]),
            Location::Chunk { flags, .. } if flags & btrfs::BG_SYSTEM != 0 => {
                Ok(vec!["<SYSTEM>".to_owned()])
            }
            Location::Chunk { logical, flags } => self.sample_chunk(logical, flags),
        };
        let mut paths = result.unwrap_or_else(|e| vec![errno_label(&e)]);
        // The representative path goes first: the shortest, then the
        // alphabetically smallest, so a given extent is always attributed
        // to the same place.
        paths.sort_by(|a, b| {
            a.matches('/')
                .count()
                .cmp(&b.matches('/').count())
                .then_with(|| a.cmp(b))
        });
        paths.dedup();
        paths
    }

    fn sample_chunk(&mut self, logical: u64, flags: u64) -> io::Result<Vec<String>> {
        if flags & btrfs::BG_DATA != 0
            && let Some(paths) = self.resolve_data(logical)?
        {
            return Ok(paths);
        }
        if flags & btrfs::BG_METADATA != 0
            && let Some(paths) = self.resolve_metadata(logical)?
        {
            return Ok(paths);
        }
        let kind = if flags & btrfs::BG_DATA != 0 {
            "<DATA>"
        } else {
            "<METADATA>"
        };
        Ok(vec![format!("<UNUSED>/{kind}")])
    }
}

/// Entry point of `btdui --sampler <path>`. Never returns on success:
/// the process exits when the GUI closes our stdin.
pub fn run(path: &Path, threads: Option<usize>) -> io::Result<()> {
    let stdout = io::stdout();
    let fail = |msg: String| -> io::Result<()> {
        let mut out = stdout.lock();
        protocol::write(&mut out, &Message::Fatal(msg))?;
        out.flush()
    };

    let fs = match Fs::open(path) {
        Ok(fs) => Arc::new(fs),
        Err(e) => return fail(format!("cannot open {}: {e}", path.display())),
    };
    let nodesize = match fs.nodesize() {
        Ok(n) => n,
        Err(e) => {
            return fail(format!(
                "{} is not a btrfs filesystem ({e})",
                path.display()
            ));
        }
    };
    let layout = match Layout::load(&fs) {
        Ok(l) => l,
        Err(e) if e.raw_os_error() == Some(libc::EPERM) => {
            return fail("permission denied: the sampler must run as root (CAP_SYS_ADMIN)".into());
        }
        Err(e) => return fail(format!("cannot read the chunk layout: {e}")),
    };
    let threads = threads
        .unwrap_or_else(|| thread::available_parallelism().map_or(4, |n| n.get()))
        .max(1);
    let header = Header {
        total_bytes: layout.total,
        nodesize,
        devices: layout.devices.len() as u32,
        threads: threads as u32,
    };
    let layout = Arc::new(RwLock::new(layout));
    let paused = Arc::new(AtomicBool::new(false));

    // Control channel: the GUI owns our stdin.
    {
        let paused = paused.clone();
        thread::spawn(move || {
            let mut stdin = io::stdin();
            let mut byte = [0u8];
            loop {
                match stdin.read(&mut byte) {
                    Ok(0) | Err(_) => std::process::exit(0),
                    Ok(_) => match byte[0] {
                        b'p' => paused.store(true, Ordering::Relaxed),
                        b'r' => paused.store(false, Ordering::Relaxed),
                        _ => {}
                    },
                }
            }
        });
    }

    // Layout refresher: chunks move around during balance.
    {
        let (fs, layout) = (fs.clone(), layout.clone());
        thread::spawn(move || {
            loop {
                thread::sleep(LAYOUT_REFRESH);
                if let Ok(fresh) = Layout::load(&fs) {
                    *layout.write().unwrap() = fresh;
                }
            }
        });
    }

    let (tx, rx) = mpsc::sync_channel::<Vec<String>>(4096);
    for _ in 0..threads {
        let (fs, layout, paused, tx) = (fs.clone(), layout.clone(), paused.clone(), tx.clone());
        thread::spawn(move || {
            let mut resolver = Resolver::new(fs, nodesize as u64);
            let mut rng = rand::rng();
            loop {
                if paused.load(Ordering::Relaxed) {
                    thread::sleep(Duration::from_millis(100));
                    continue;
                }
                let paths = {
                    let layout = layout.read().unwrap();
                    let phys = rng.random_range(0..layout.total);
                    resolver.sample(&layout, phys)
                };
                if tx.send(paths).is_err() {
                    return;
                }
            }
        });
    }
    drop(tx);

    let mut out = BufWriter::with_capacity(1 << 16, stdout.lock());
    protocol::write(&mut out, &Message::Header(header))?;
    out.flush()?;
    let mut interned: HashMap<String, u32> = HashMap::new();
    while let Ok(first) = rx.recv() {
        let mut next = Some(first);
        while let Some(paths) = next {
            let mut ids = Vec::with_capacity(paths.len());
            for path in paths {
                let id = match interned.get(&path) {
                    Some(&id) => id,
                    None => {
                        let id = interned.len() as u32;
                        protocol::write(
                            &mut out,
                            &Message::Path {
                                id,
                                path: path.clone(),
                            },
                        )?;
                        interned.insert(path, id);
                        id
                    }
                };
                ids.push(id);
            }
            protocol::write(&mut out, &Message::Sample { paths: ids })?;
            next = rx.try_recv().ok();
        }
        // Flush whenever we caught up with the samplers.
        out.flush()?;
    }
    Ok(())
}
