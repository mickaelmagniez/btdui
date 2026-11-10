//! Thin, read-only layer over the `btrfs-uapi` crate.
//!
//! Only what sampling needs: tree search, inode path lookup in any tree,
//! logical-to-inode resolution and the node size. Searching arbitrary trees
//! requires `CAP_SYS_ADMIN`.

use std::fs::File;
use std::io;
use std::os::fd::{AsFd, AsRawFd};
use std::path::Path;

use btrfs_uapi::tree_search::{self, SearchFilter};
use nix::errno::Errno;

// Tree ids.
pub const ROOT_TREE_OBJECTID: u64 = 1;
pub const EXTENT_TREE_OBJECTID: u64 = 2;
pub const CHUNK_TREE_OBJECTID: u64 = 3;
pub const DEV_TREE_OBJECTID: u64 = 4;
pub const FS_TREE_OBJECTID: u64 = 5;
pub const FIRST_FREE_OBJECTID: u64 = 256;

// Key types.
pub const EXTENT_ITEM_KEY: u8 = 168;
pub const METADATA_ITEM_KEY: u8 = 169;
pub const EXTENT_OWNER_REF_KEY: u8 = 172;
pub const TREE_BLOCK_REF_KEY: u8 = 176;
pub const EXTENT_DATA_REF_KEY: u8 = 178;
pub const SHARED_BLOCK_REF_KEY: u8 = 182;
pub const SHARED_DATA_REF_KEY: u8 = 184;
pub const ROOT_BACKREF_KEY: u8 = 144;
pub const DEV_EXTENT_KEY: u8 = 204;
pub const DEV_ITEM_KEY: u8 = 216;
pub const CHUNK_ITEM_KEY: u8 = 228;

pub const DEV_ITEMS_OBJECTID: u64 = 1;
pub const EXTENT_FLAG_TREE_BLOCK: u64 = 2;

// Block group flags.
pub const BG_DATA: u64 = 1 << 0;
pub const BG_SYSTEM: u64 = 1 << 1;
pub const BG_METADATA: u64 = 1 << 2;
pub const BG_RAID0: u64 = 1 << 3;
pub const BG_RAID10: u64 = 1 << 6;
pub const BG_RAID5: u64 = 1 << 7;
pub const BG_RAID6: u64 = 1 << 8;
/// Profiles whose device extents are striped, i.e. not a 1:1 copy of the chunk.
pub const BG_STRIPED: u64 = BG_RAID0 | BG_RAID10 | BG_RAID5 | BG_RAID6;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Key {
    pub objectid: u64,
    pub ty: u8,
    pub offset: u64,
}

impl Key {
    pub const fn new(objectid: u64, ty: u8, offset: u64) -> Self {
        Self {
            objectid,
            ty,
            offset,
        }
    }

    fn to_uapi(self) -> tree_search::Key {
        tree_search::Key {
            objectid: self.objectid,
            item_type: self.ty as u32,
            offset: self.offset,
        }
    }
}

pub struct Item {
    pub key: Key,
    pub data: Vec<u8>,
}

pub fn le_u16(b: &[u8], off: usize) -> u16 {
    u16::from_le_bytes(b[off..off + 2].try_into().unwrap())
}
pub fn le_u64(b: &[u8], off: usize) -> u64 {
    u64::from_le_bytes(b[off..off + 8].try_into().unwrap())
}

fn io_err(e: Errno) -> io::Error {
    io::Error::from_raw_os_error(e as i32)
}

/// Size of the LOGICAL_INO result buffer: room for ~10k references.
const LOGICAL_INO_BUF: u64 = 256 * 1024;

/// An open handle on a mounted btrfs filesystem.
pub struct Fs {
    file: File,
}

impl Fs {
    pub fn open(path: &Path) -> io::Result<Self> {
        Ok(Self {
            file: File::open(path)?,
        })
    }

    pub fn nodesize(&self) -> io::Result<u32> {
        btrfs_uapi::filesystem::filesystem_info(self.file.as_fd())
            .map(|i| i.nodesize)
            .map_err(io_err)
    }

    /// Collects the items of `tree` whose key lies in `[min, max]`
    /// (compound key order: other item types in between are returned too),
    /// stopping after `limit` items.
    pub fn tree_search(
        &self,
        tree: u64,
        min: Key,
        max: Key,
        limit: usize,
    ) -> io::Result<Vec<Item>> {
        let filter = SearchFilter {
            tree_id: tree,
            start: min.to_uapi(),
            end: max.to_uapi(),
            min_transid: 0,
            max_transid: u64::MAX,
        };
        let mut out = Vec::new();
        let res = tree_search::tree_search(self.file.as_fd(), filter, |hdr, data| {
            out.push(Item {
                key: Key::new(hdr.objectid, hdr.item_type as u8, hdr.offset),
                data: data.to_vec(),
            });
            // Abort the walk once we have enough.
            if out.len() >= limit {
                Err(Errno::ECANCELED)
            } else {
                Ok(())
            }
        });
        match res {
            Ok(()) | Err(Errno::ECANCELED) => Ok(out),
            Err(e) => Err(io_err(e)),
        }
    }

    /// Path of inode `objectid` inside subvolume `tree`, relative to that
    /// subvolume's root. Directories come back with a trailing `/`.
    pub fn ino_lookup(&self, tree: u64, objectid: u64) -> io::Result<Vec<u8>> {
        // SAFETY: plain-old-data ioctl argument, zero is a valid value.
        let mut args: btrfs_uapi::raw::btrfs_ioctl_ino_lookup_args = unsafe { std::mem::zeroed() };
        args.treeid = tree;
        args.objectid = objectid;
        // SAFETY: `args` is a properly sized, initialized argument for INO_LOOKUP.
        unsafe { btrfs_uapi::raw::btrfs_ioc_ino_lookup(self.file.as_raw_fd(), &raw mut args) }
            .map_err(io_err)?;
        let name: Vec<u8> = args
            .name
            .iter()
            .take_while(|&&c| c != 0)
            .map(|&c| c as u8)
            .collect();
        Ok(name)
    }

    /// Every `(inode, offset, root)` referencing the data at exactly
    /// `logical` (not the whole extent: bookend parts are not counted).
    /// Fails with `ENOENT` when no extent covers `logical`.
    pub fn logical_ino(&self, logical: u64) -> io::Result<Vec<(u64, u64, u64)>> {
        btrfs_uapi::inode::logical_ino(self.file.as_fd(), logical, false, Some(LOGICAL_INO_BUF))
            .map(|v| v.into_iter().map(|r| (r.inode, r.offset, r.root)).collect())
            .map_err(io_err)
    }
}
