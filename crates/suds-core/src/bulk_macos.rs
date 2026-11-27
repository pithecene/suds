//! Batched directory listing, entry metadata, and APFS clone lookup on macOS
//! through `getattrlistbulk(2)`.

use std::ffi::OsString;
use std::io;
use std::mem::size_of;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStringExt;
use std::path::Path;

use rustix::fs::{Mode, OFlags, fstat, open};

use crate::budget::BudgetTracker;
use crate::clones;
use crate::model::EntryKind;
use crate::scan::{EntryMeta, Probed};

/// Buffer size for each `getattrlistbulk(2)` call.
const BULK_BUFFER_BYTES: usize = 64 * 1024;

/// Kernel `vtype` tags from `<sys/vnode.h>`.
const VREG: u32 = 1;
const VDIR: u32 = 2;
const VLNK: u32 = 5;

/// `ATTR_CMN_ERROR` from macOS `<sys/attr.h>`, omitted by `libc`.
const ATTR_CMN_ERROR: libc::attrgroup_t = 0x2000_0000;

/// One directory entry returned by `getattrlistbulk(2)`.
pub(crate) struct BulkEntry {
    pub(crate) name: OsString,
    pub(crate) kind: Option<EntryKind>,
    pub(crate) probed: Option<Probed>,
}

/// Outcome of reading a directory through `getattrlistbulk(2)`.
pub(crate) enum BulkOutcome {
    Listed(Vec<BulkEntry>),
    Deferred(usize),
    Stopped,
    Unsupported,
}

/// Cursor that decodes packed attributes inside one `getattrlistbulk` record.
struct RecordCursor<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> RecordCursor<'a> {
    fn new(bytes: &'a [u8], start: usize) -> Self {
        RecordCursor {
            bytes,
            offset: start,
        }
    }

    fn read<T: Copy>(&mut self) -> Option<T> {
        let size = size_of::<T>();
        let end = self.offset.checked_add(size)?;
        if end > self.bytes.len() {
            return None;
        }
        let ptr = self.bytes[self.offset..end].as_ptr().cast::<T>();
        self.offset = end;
        #[expect(
            unsafe_code,
            reason = "getattrlistbulk packs 4-byte aligned structs into a byte buffer"
        )]
        // SAFETY: `end <= self.bytes.len()` guarantees `size` readable bytes
        // starting at `ptr`, and `read_unaligned` handles 4-byte packing.
        Some(unsafe { ptr.read_unaligned() })
    }

    fn read_name(&mut self) -> Option<OsString> {
        let field_pos = self.offset;
        let attr_ref: libc::attrreference_t = self.read()?;
        let rel = usize::try_from(attr_ref.attr_dataoffset).ok()?;
        let len = usize::try_from(attr_ref.attr_length).ok()?;
        let start = field_pos.checked_add(rel)?;
        let end = start.checked_add(len)?;
        if end > self.bytes.len() {
            return None;
        }
        let raw = &self.bytes[start..end];
        let trimmed = match raw.iter().position(|&b| b == 0) {
            Some(nul) => &raw[..nul],
            None => raw,
        };
        Some(OsString::from_vec(trimmed.to_vec()))
    }
}

/// Lists `dir` with `getattrlistbulk(2)` while enforcing `max_entries` and
/// cancellation, returning pre-probed metadata and APFS clone info per entry.
pub(crate) fn read_dir_bulk(
    dir: &Path,
    device: u64,
    max_entries: Option<usize>,
    tracker: &BudgetTracker,
) -> io::Result<BulkOutcome> {
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
    let fd = open(dir, flags, Mode::empty()).map_err(io::Error::from)?;
    let dir_stat = fstat(&fd).map_err(io::Error::from)?;
    // On APFS directories occupy 0 blocks. On filesystems where directories
    // allocate blocks, let `probe_at` read directory metadata directly.
    let dir_has_blocks = dir_stat.st_blocks != 0;

    let mut request = libc::attrlist {
        bitmapcount: libc::ATTR_BIT_MAP_COUNT,
        reserved: 0,
        commonattr: libc::ATTR_CMN_RETURNED_ATTRS
            | libc::ATTR_CMN_NAME
            | libc::ATTR_CMN_DEVID
            | libc::ATTR_CMN_OBJTYPE
            | libc::ATTR_CMN_MODTIME
            | libc::ATTR_CMN_OWNERID
            | libc::ATTR_CMN_FILEID
            | ATTR_CMN_ERROR,
        volattr: 0,
        dirattr: 0,
        fileattr: libc::ATTR_FILE_LINKCOUNT
            | libc::ATTR_FILE_ALLOCSIZE
            | libc::ATTR_FILE_DATALENGTH
            | libc::ATTR_FILE_DATAALLOCSIZE,
        forkattr: libc::ATTR_CMNEXT_CLONEID | libc::ATTR_CMNEXT_EXT_FLAGS,
    };

    let mut buf = vec![0_u8; BULK_BUFFER_BYTES];
    let mut entries = Vec::new();
    let mut seen = 0_usize;

    loop {
        if tracker.is_stopped() {
            return Ok(BulkOutcome::Stopped);
        }
        #[expect(
            unsafe_code,
            reason = "no safe interface wraps macOS getattrlistbulk(2)"
        )]
        // SAFETY: `fd` is an open directory descriptor, `request` holds a valid
        // `attrlist`, and `buf` has length `BULK_BUFFER_BYTES`.
        let count = unsafe {
            libc::getattrlistbulk(
                fd.as_raw_fd(),
                (&raw mut request).cast(),
                buf.as_mut_ptr().cast(),
                buf.len(),
                u64::from(libc::FSOPT_ATTR_CMN_EXTENDED),
            )
        };
        if count < 0 {
            let err = io::Error::last_os_error();
            if seen == 0 && matches!(err.raw_os_error(), Some(libc::ENOTSUP | libc::EINVAL)) {
                return Ok(BulkOutcome::Unsupported);
            }
            return Err(err);
        }
        if count == 0 {
            break;
        }
        let mut pos = 0_usize;
        for _ in 0..count {
            if seen.is_multiple_of(64) && tracker.is_stopped() {
                return Ok(BulkOutcome::Stopped);
            }
            seen = seen.saturating_add(1);
            if let Some(limit) = max_entries
                && seen > limit
            {
                return Ok(BulkOutcome::Deferred(limit));
            }
            if pos.saturating_add(size_of::<u32>()) > buf.len() {
                return Ok(BulkOutcome::Unsupported);
            }
            let len_bytes: [u8; 4] = buf[pos..pos + 4].try_into().unwrap();
            let rec_len = u32::from_ne_bytes(len_bytes) as usize;
            let rec_end = pos.saturating_add(rec_len);
            if rec_len < size_of::<u32>() + size_of::<libc::attribute_set_t>()
                || rec_end > buf.len()
            {
                return Ok(BulkOutcome::Unsupported);
            }
            let Some(entry) = parse_record(&buf[pos..rec_end], device, dir_has_blocks) else {
                return Ok(BulkOutcome::Unsupported);
            };
            entries.push(entry);
            pos = rec_end;
        }
    }

    entries.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(BulkOutcome::Listed(entries))
}

fn parse_record(record: &[u8], device: u64, dir_has_blocks: bool) -> Option<BulkEntry> {
    let mut cur = RecordCursor::new(record, size_of::<u32>());
    let returned: libc::attribute_set_t = cur.read()?;
    let cmn = returned.commonattr;

    let name = if cmn & libc::ATTR_CMN_NAME != 0 {
        cur.read_name()?
    } else {
        return None;
    };
    let dev = if cmn & libc::ATTR_CMN_DEVID != 0 {
        let raw: i32 = cur.read()?;
        Some(u64::from(raw.cast_unsigned()))
    } else {
        None
    };
    let kind = if cmn & libc::ATTR_CMN_OBJTYPE != 0 {
        let obj_type: u32 = cur.read()?;
        Some(match obj_type {
            VREG => EntryKind::File,
            VDIR => EntryKind::Directory,
            VLNK => EntryKind::Symlink,
            _ => EntryKind::Other,
        })
    } else {
        None
    };
    let mtime = if cmn & libc::ATTR_CMN_MODTIME != 0 {
        let ts: libc::timespec = cur.read()?;
        Some((ts.tv_sec, ts.tv_nsec))
    } else {
        None
    };
    let uid = if cmn & libc::ATTR_CMN_OWNERID != 0 {
        cur.read::<u32>()
    } else {
        None
    };
    let ino = if cmn & libc::ATTR_CMN_FILEID != 0 {
        cur.read::<u64>()
    } else {
        None
    };
    let entry_err = if cmn & ATTR_CMN_ERROR != 0 {
        cur.read::<u32>().unwrap_or(0)
    } else {
        0
    };

    let file = returned.fileattr;
    let nlink = if file & libc::ATTR_FILE_LINKCOUNT != 0 {
        cur.read::<u32>().map(u64::from)
    } else {
        None
    };
    let alloc_size = if file & libc::ATTR_FILE_ALLOCSIZE != 0 {
        cur.read::<i64>().map(|v| u64::try_from(v).unwrap_or(0))
    } else {
        None
    };
    let data_len = if file & libc::ATTR_FILE_DATALENGTH != 0 {
        cur.read::<i64>().map(|v| u64::try_from(v).unwrap_or(0))
    } else {
        None
    };
    let data_alloc_size = if file & libc::ATTR_FILE_DATAALLOCSIZE != 0 {
        cur.read::<i64>().map(|v| u64::try_from(v).unwrap_or(0))
    } else {
        None
    };

    let fork = returned.forkattr;
    let clone_id = if fork & libc::ATTR_CMNEXT_CLONEID != 0 {
        cur.read::<u64>()
    } else {
        None
    };
    let ext_flags = if fork & libc::ATTR_CMNEXT_EXT_FLAGS != 0 {
        cur.read::<u64>()
    } else {
        None
    };

    let raw = RawAttrs {
        entry_err,
        device,
        dir_has_blocks,
        dev,
        ino,
        kind,
        mtime,
        uid,
        nlink,
        alloc_size,
        data_len,
        data_alloc_size,
        clone_id,
        ext_flags,
    };
    let probed = build_probed(raw);
    Some(BulkEntry { name, kind, probed })
}

struct RawAttrs {
    entry_err: u32,
    device: u64,
    dir_has_blocks: bool,
    dev: Option<u64>,
    ino: Option<u64>,
    kind: Option<EntryKind>,
    mtime: Option<(i64, i64)>,
    uid: Option<u32>,
    nlink: Option<u64>,
    alloc_size: Option<u64>,
    data_len: Option<u64>,
    data_alloc_size: Option<u64>,
    clone_id: Option<u64>,
    ext_flags: Option<u64>,
}

fn build_probed(raw: RawAttrs) -> Option<Probed> {
    if raw.entry_err != 0 {
        return None;
    }
    let (dev, ino, kind, (mtime, mtime_nsec), uid) =
        (raw.dev?, raw.ino?, raw.kind?, raw.mtime?, raw.uid?);
    let mount = dev != raw.device;
    match kind {
        EntryKind::File => {
            let (nlink, allocated_bytes, logical_bytes) =
                (raw.nlink?, raw.alloc_size?, raw.data_len?);
            let shared = if mount || allocated_bytes == 0 || logical_bytes < clones::FLOOR {
                Ok(Vec::new())
            } else if let (Some(data_alloc), Some(number), Some(flags)) =
                (raw.data_alloc_size, raw.clone_id, raw.ext_flags)
            {
                if flags & clones::SHARES_ALL != 0 {
                    let bytes = data_alloc.min(allocated_bytes);
                    Ok(clones::apfs_extent(number, bytes).into_iter().collect())
                } else {
                    Ok(Vec::new())
                }
            } else {
                Ok(Vec::new())
            };
            Some(Probed {
                meta: EntryMeta {
                    dev,
                    ino,
                    nlink,
                    uid,
                    logical_bytes,
                    allocated_bytes,
                    mtime,
                    mtime_nsec,
                },
                shared,
                kind,
                mount,
            })
        }
        EntryKind::Directory if !raw.dir_has_blocks && !mount => Some(Probed {
            meta: EntryMeta {
                dev,
                ino,
                nlink: 1,
                uid,
                logical_bytes: 0,
                allocated_bytes: 0,
                mtime,
                mtime_nsec,
            },
            shared: Ok(Vec::new()),
            kind,
            mount,
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::{MetadataExt, symlink};

    use rustix::fs::{CWD, CloneFlags, fclonefileat};
    use tempfile::TempDir;

    use crate::budget::{Budget, BudgetTracker};
    use crate::model::EntryKind;
    use crate::scan::probe_at;

    use super::{BulkOutcome, read_dir_bulk};

    #[test]
    fn bulk_metadata_matches_probe_at_across_entry_types() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        let device = fs::metadata(root).unwrap().dev();

        fs::write(root.join("small.txt"), b"hello").unwrap();
        let large_bytes: Vec<u8> = (0..64 * 1024_u32).map(|n| (n % 251) as u8).collect();
        let source = root.join("large.bin");
        fs::write(&source, &large_bytes).unwrap();
        fs::hard_link(&source, root.join("hard.bin")).unwrap();
        fs::create_dir(root.join("subdir")).unwrap();
        symlink("small.txt", root.join("link")).unwrap();

        let opened = fs::File::open(&source).unwrap();
        opened.sync_all().unwrap();
        let cloned = root.join("clone.bin");
        let cloned_ok = fclonefileat(&opened, CWD, &cloned, CloneFlags::empty()).is_ok();
        if cloned_ok {
            let fork = cloned.join("..namedfork/rsrc");
            fs::write(&fork, vec![9_u8; 32 * 1024]).unwrap();
        }

        let tracker = BudgetTracker::new(Budget::default());
        let BulkOutcome::Listed(entries) = read_dir_bulk(root, device, None, &tracker).unwrap()
        else {
            panic!("expected Listed from getattrlistbulk on macOS");
        };
        assert!(entries.len() >= 5);

        for entry in entries {
            let path = root.join(&entry.name);
            let reference = probe_at(&path, false, device, false).unwrap();
            assert_eq!(
                entry.kind,
                Some(reference.kind),
                "kind for {:?}",
                entry.name
            );
            match reference.kind {
                EntryKind::File => {
                    let bulk = entry
                        .probed
                        .expect("regular file must be pre-probed by getattrlistbulk");
                    assert_eq!(bulk.meta, reference.meta, "meta for {:?}", entry.name);
                    assert_eq!(
                        bulk.shared.unwrap(),
                        reference.shared.unwrap(),
                        "shared extents for {:?}",
                        entry.name
                    );
                    assert_eq!(bulk.mount, reference.mount);
                }
                EntryKind::Directory => {
                    if let Some(bulk) = entry.probed {
                        assert_eq!(bulk.meta.dev, reference.meta.dev);
                        assert_eq!(bulk.meta.ino, reference.meta.ino);
                        assert_eq!(bulk.meta.uid, reference.meta.uid);
                        assert_eq!(bulk.meta.mtime, reference.meta.mtime);
                        assert_eq!(bulk.meta.allocated_bytes, reference.meta.allocated_bytes);
                        assert_eq!(bulk.mount, reference.mount);
                    }
                }
                EntryKind::Symlink | EntryKind::Other => {
                    assert!(entry.probed.is_none(), "symlinks fall back to probe_at");
                }
            }
        }
    }
}
