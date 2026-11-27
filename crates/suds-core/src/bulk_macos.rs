//! Batched directory listing, regular-file metadata, and APFS clone lookup on
//! macOS through `getattrlistbulk(2)`.

use std::cell::RefCell;
use std::ffi::OsString;
use std::io;
use std::mem::size_of;
use std::os::unix::ffi::OsStringExt;
use std::path::Path;

use rustix::fs::{Mode, OFlags, open};

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

thread_local! {
    /// Reusable per-thread buffer so directory scans avoid a 64 KiB allocation
    /// for every directory they visit.
    static BULK_BUF: RefCell<Vec<u8>> = RefCell::new(vec![0_u8; BULK_BUFFER_BYTES]);
}

/// One named directory entry returned by `getattrlistbulk(2)`.
pub(crate) struct BulkEntry {
    pub(crate) name: OsString,
    pub(crate) kind: Option<EntryKind>,
    pub(crate) probed: Option<io::Result<Probed>>,
}

/// Outcome of reading a directory through `getattrlistbulk(2)`.
pub(crate) enum BulkOutcome {
    Listed {
        entries: Vec<BulkEntry>,
        unnamed: Vec<io::Error>,
    },
    Deferred(usize),
    Stopped,
    Unsupported,
}

enum ParsedRecord {
    Entry(BulkEntry),
    UnnamedError(io::Error),
}

/// Safe byte-slice cursor that decodes packed attributes inside one
/// `getattrlistbulk(2)` record without unsafe pointer casts.
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

    fn take<const N: usize>(&mut self) -> Option<[u8; N]> {
        let end = self.offset.checked_add(N)?;
        let slice = self.bytes.get(self.offset..end)?;
        self.offset = end;
        slice.try_into().ok()
    }

    fn read_u32(&mut self) -> Option<u32> {
        Some(u32::from_ne_bytes(self.take::<4>()?))
    }

    fn read_i32(&mut self) -> Option<i32> {
        Some(i32::from_ne_bytes(self.take::<4>()?))
    }

    fn read_u64(&mut self) -> Option<u64> {
        Some(u64::from_ne_bytes(self.take::<8>()?))
    }

    fn read_i64(&mut self) -> Option<i64> {
        Some(i64::from_ne_bytes(self.take::<8>()?))
    }

    fn read_attr_set(&mut self) -> Option<libc::attribute_set_t> {
        Some(libc::attribute_set_t {
            commonattr: self.read_u32()?,
            volattr: self.read_u32()?,
            dirattr: self.read_u32()?,
            fileattr: self.read_u32()?,
            forkattr: self.read_u32()?,
        })
    }

    fn read_timespec(&mut self) -> Option<(i64, i64)> {
        let tv_sec = self.read_i64()?;
        let tv_nsec = self.read_i64()?;
        Some((tv_sec, tv_nsec))
    }

    fn read_name(&mut self) -> Option<OsString> {
        let field_pos = self.offset;
        let data_offset = self.read_i32()?;
        let data_length = self.read_u32()?;
        let rel = usize::try_from(data_offset).ok()?;
        let len = usize::try_from(data_length).ok()?;
        let start = field_pos.checked_add(rel)?;
        let end = start.checked_add(len)?;
        let raw = self.bytes.get(start..end)?;
        let trimmed = match raw.iter().position(|&b| b == 0) {
            Some(nul) => &raw[..nul],
            None => raw,
        };
        Some(OsString::from_vec(trimmed.to_vec()))
    }
}

/// Lists `dir` with `getattrlistbulk(2)` while enforcing `max_entries` and
/// cancellation, returning pre-probed metadata and APFS clone info for regular
/// files while preserving traversal errors.
pub(crate) fn read_dir_bulk(
    dir: &Path,
    device: u64,
    max_entries: Option<usize>,
    tracker: &BudgetTracker,
) -> io::Result<BulkOutcome> {
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
    let fd = open(dir, flags, Mode::empty()).map_err(io::Error::from)?;

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

    BULK_BUF.with(|cell| {
        let mut buf = cell.borrow_mut();
        if buf.len() < BULK_BUFFER_BYTES {
            buf.resize(BULK_BUFFER_BYTES, 0);
        }
        let mut entries = Vec::new();
        let mut unnamed = Vec::new();
        let mut seen = 0_usize;

        loop {
            if tracker.is_stopped() {
                return Ok(BulkOutcome::Stopped);
            }
            let count = match clones::query_dir_bulk(&fd, &mut request, &mut buf) {
                Ok(count) => count,
                Err(err) => {
                    if seen == 0 && matches!(err.raw_os_error(), Some(libc::ENOTSUP | libc::EINVAL))
                    {
                        return Ok(BulkOutcome::Unsupported);
                    }
                    return Err(err);
                }
            };
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
                let Some(len_slice) = buf.get(pos..pos.saturating_add(size_of::<u32>())) else {
                    return Ok(BulkOutcome::Unsupported);
                };
                let rec_len = u32::from_ne_bytes(len_slice.try_into().unwrap()) as usize;
                let rec_end = pos.saturating_add(rec_len);
                if rec_len < size_of::<u32>() + size_of::<libc::attribute_set_t>()
                    || rec_end > buf.len()
                {
                    return Ok(BulkOutcome::Unsupported);
                }
                match parse_record(&buf[pos..rec_end], device) {
                    Some(ParsedRecord::Entry(entry)) => entries.push(entry),
                    Some(ParsedRecord::UnnamedError(err)) => unnamed.push(err),
                    None => return Ok(BulkOutcome::Unsupported),
                }
                pos = rec_end;
            }
        }

        entries.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(BulkOutcome::Listed { entries, unnamed })
    })
}

fn parse_record(record: &[u8], device: u64) -> Option<ParsedRecord> {
    let mut cur = RecordCursor::new(record, size_of::<u32>());
    let returned = cur.read_attr_set()?;
    let cmn = returned.commonattr;

    // XNU packs `ATTR_CMN_ERROR` immediately after `ATTR_CMN_RETURNED_ATTRS`,
    // ahead of `ATTR_CMN_NAME` and all other common attributes.
    let entry_err = if cmn & ATTR_CMN_ERROR != 0 {
        cur.read_u32()?
    } else {
        0
    };
    let name = if cmn & libc::ATTR_CMN_NAME != 0 {
        Some(cur.read_name()?)
    } else {
        None
    };
    let dev = if cmn & libc::ATTR_CMN_DEVID != 0 {
        Some(u64::from(cur.read_i32()?.cast_unsigned()))
    } else {
        None
    };
    let kind = if cmn & libc::ATTR_CMN_OBJTYPE != 0 {
        let obj_type = cur.read_u32()?;
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
        Some(cur.read_timespec()?)
    } else {
        None
    };
    let uid = if cmn & libc::ATTR_CMN_OWNERID != 0 {
        Some(cur.read_u32()?)
    } else {
        None
    };
    let ino = if cmn & libc::ATTR_CMN_FILEID != 0 {
        Some(cur.read_u64()?)
    } else {
        None
    };

    if entry_err != 0 {
        let err = io::Error::from_raw_os_error(entry_err.cast_signed());
        return match name {
            Some(name) => Some(ParsedRecord::Entry(BulkEntry {
                name,
                kind,
                probed: Some(Err(err)),
            })),
            None => Some(ParsedRecord::UnnamedError(err)),
        };
    }

    let name = name?;
    let file = returned.fileattr;
    let nlink = if file & libc::ATTR_FILE_LINKCOUNT != 0 {
        Some(u64::from(cur.read_u32()?))
    } else {
        None
    };
    let alloc_size = if file & libc::ATTR_FILE_ALLOCSIZE != 0 {
        Some(u64::try_from(cur.read_i64()?).unwrap_or(0))
    } else {
        None
    };
    let data_len = if file & libc::ATTR_FILE_DATALENGTH != 0 {
        Some(u64::try_from(cur.read_i64()?).unwrap_or(0))
    } else {
        None
    };
    let data_alloc_size = if file & libc::ATTR_FILE_DATAALLOCSIZE != 0 {
        Some(u64::try_from(cur.read_i64()?).unwrap_or(0))
    } else {
        None
    };

    let fork = returned.forkattr;
    let clone_id = if fork & libc::ATTR_CMNEXT_CLONEID != 0 {
        Some(cur.read_u64()?)
    } else {
        None
    };
    let ext_flags = if fork & libc::ATTR_CMNEXT_EXT_FLAGS != 0 {
        Some(cur.read_u64()?)
    } else {
        None
    };

    let raw = RawAttrs {
        device,
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
    let probed = build_probed(raw).map(Ok);
    Some(ParsedRecord::Entry(BulkEntry { name, kind, probed }))
}

struct RawAttrs {
    device: u64,
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
    let (dev, ino, kind, (mtime, mtime_nsec), uid) =
        (raw.dev?, raw.ino?, raw.kind?, raw.mtime?, raw.uid?);
    let mount = dev != raw.device;
    // Regular files carry exact logical size, allocated blocks, and APFS clone
    // flags in the bulk response. Directories, symlinks, and other entries fall
    // back to `probe_at` so directory `logical_bytes` in `EntryRevision` stays
    // exact.
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
        EntryKind::Directory | EntryKind::Symlink | EntryKind::Other => None,
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;
    use std::fs;
    use std::os::unix::fs::{MetadataExt, symlink};

    use rustix::fs::{CWD, CloneFlags, fclonefileat};
    use tempfile::TempDir;

    use crate::budget::{Budget, BudgetTracker};
    use crate::model::EntryKind;
    use crate::scan::probe_at;

    use super::{ATTR_CMN_ERROR, BulkOutcome, ParsedRecord, parse_record, read_dir_bulk};

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
        let BulkOutcome::Listed { entries, unnamed } =
            read_dir_bulk(root, device, None, &tracker).unwrap()
        else {
            panic!("expected Listed from getattrlistbulk on macOS");
        };
        assert!(unnamed.is_empty());
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
                        .expect("getattrlistbulk pre-probes regular files")
                        .unwrap();
                    assert_eq!(bulk.meta, reference.meta, "meta for {:?}", entry.name);
                    assert_eq!(
                        bulk.shared.unwrap(),
                        reference.shared.unwrap(),
                        "shared extents for {:?}",
                        entry.name
                    );
                    assert_eq!(bulk.mount, reference.mount);
                }
                EntryKind::Directory | EntryKind::Symlink | EntryKind::Other => {
                    assert!(
                        entry.probed.is_none(),
                        "directories and symlinks fall back to probe_at"
                    );
                }
            }
        }
    }

    #[test]
    fn parse_record_decodes_cmn_error_before_name_and_preserves_errno() {
        // Build a synthetic XNU record: length(4) + attribute_set_t(20) +
        // ATTR_CMN_ERROR(4) + attrreference_t(8) + NUL-terminated name bytes.
        let name = b"secret\0";
        let mut record = Vec::new();
        record.extend_from_slice(&0_u32.to_ne_bytes()); // length placeholder
        record.extend_from_slice(
            &(libc::ATTR_CMN_RETURNED_ATTRS | ATTR_CMN_ERROR | libc::ATTR_CMN_NAME).to_ne_bytes(),
        );
        record.extend_from_slice(&0_u32.to_ne_bytes()); // volattr
        record.extend_from_slice(&0_u32.to_ne_bytes()); // dirattr
        record.extend_from_slice(&0_u32.to_ne_bytes()); // fileattr
        record.extend_from_slice(&0_u32.to_ne_bytes()); // forkattr
        record.extend_from_slice(&(libc::EACCES as u32).to_ne_bytes());
        record.extend_from_slice(&8_i32.to_ne_bytes()); // offset from attrreference_t to name
        record.extend_from_slice(&(name.len() as u32).to_ne_bytes());
        record.extend_from_slice(name);
        let total = record.len() as u32;
        record[0..4].copy_from_slice(&total.to_ne_bytes());

        let Some(ParsedRecord::Entry(entry)) = parse_record(&record, 1) else {
            panic!("expected named entry carrying the kernel error");
        };
        assert_eq!(entry.name, OsStr::new("secret"));
        let err = entry.probed.unwrap().err().expect("expected kernel error");
        assert_eq!(err.raw_os_error(), Some(libc::EACCES));

        // Nameless error record must preserve its errno in unnamed errors.
        let mut nameless = Vec::new();
        nameless.extend_from_slice(&28_u32.to_ne_bytes());
        nameless.extend_from_slice(&(libc::ATTR_CMN_RETURNED_ATTRS | ATTR_CMN_ERROR).to_ne_bytes());
        nameless.extend_from_slice(&[0_u8; 16]);
        nameless.extend_from_slice(&(libc::EIO as u32).to_ne_bytes());
        let Some(ParsedRecord::UnnamedError(err)) = parse_record(&nameless, 1) else {
            panic!("expected unnamed error");
        };
        assert_eq!(err.raw_os_error(), Some(libc::EIO));
    }
}
