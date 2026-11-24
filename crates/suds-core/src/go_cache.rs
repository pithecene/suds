//! Eviction from Go's build cache.
//!
//! Go offers no trim to a size, and its own trim removes what stood unused
//! for five days. Suds evicts the entries that Go used least recently until
//! the cache meets its budget (docs/decisions/0007-go-cache-eviction.md).

use std::ffi::{CStr, CString};
use std::os::fd::OwnedFd;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use rustix::fs::{AtFlags, Dir, FileType, Mode, OFlags, Stat};
use serde::{Deserialize, Serialize};

use crate::organize::native::NativePath;
use crate::timestamp;

/// Go records a use in an entry's modification time at most once an hour,
/// so an entry may have served a build an hour after that time.
pub const USE_PRECISION_SECONDS: u64 = 3600;

/// The eviction that brings a Go build cache to its budget.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Eviction {
    pub directory: NativePath,
    /// The device and the inode of the planned directory.
    pub device: u64,
    pub inode: u64,
    pub budget_bytes: u64,
    /// The time from which a use keeps an entry, in seconds since the Unix
    /// epoch.
    pub used_since: i64,
}

/// One entry of the cache that Suds may evict.
struct Candidate {
    used: (i64, i64),
    subdirectory: u8,
    name: CString,
    device: i128,
    inode: u64,
    bytes: u64,
}

impl Eviction {
    /// The eviction of a directory as it stands, or the reason that Suds
    /// cannot read it.
    pub fn of(directory: &Path, budget_bytes: u64, used_since: i64) -> Result<Self, String> {
        let meta = std::fs::symlink_metadata(directory)
            .map_err(|err| format!("could not read {}: {err}", directory.display()))?;
        if !meta.is_dir() {
            return Err(format!("{} is no directory", directory.display()));
        }
        Ok(Eviction {
            directory: NativePath::new(directory.to_path_buf()),
            device: meta.dev(),
            inode: meta.ino(),
            budget_bytes,
            used_since,
        })
    }

    /// The eviction in words, for display and for the journal.
    pub fn display(&self) -> String {
        format!(
            "evict from {} the entries that Go used least recently before {}, until {} bytes remain",
            self.directory.display(),
            timestamp::format_utc(self.used_since),
            self.budget_bytes
        )
    }

    /// Evicts entries until the cache meets its budget or holds no entry
    /// that stood unused. A failure to remove one entry leaves the others to
    /// proceed, and the result names the first.
    pub fn run(&self) -> Result<(), String> {
        let path = self.directory.as_path();
        let root = open_directory(rustix::fs::CWD, path)
            .map_err(|err| format!("could not open {}: {err}", path.display()))?;
        // A link in place of a parent would lead to a directory that the plan
        // did not name.
        let meta = std::fs::File::from(root.try_clone().map_err(|err| err.to_string())?)
            .metadata()
            .map_err(|err| err.to_string())?;
        if (meta.dev(), meta.ino()) != (self.device, self.inode) {
            return Err(format!(
                "{} is not the directory that the plan named",
                path.display()
            ));
        }
        // The plan proved the layout, and the directory may have changed since.
        if !has_layout(&root) {
            return Err(format!(
                "{} no longer has the layout of Go's build cache",
                path.display()
            ));
        }
        // A repository may have appeared around the cache since the plan.
        if inside_checkout(path)? {
            return Err(format!("{} lies inside a checkout", path.display()));
        }
        // The check read the path, so the path must lead to the opened cache.
        let now = Eviction::of(path, 0, 0)?;
        if (now.device, now.inode) != (self.device, self.inode) {
            return Err(format!("{} changed during the run", path.display()));
        }
        let mut candidates = Vec::new();
        let mut total = 0u64;
        for subdirectory in 0..=u8::MAX {
            if let Some(directory) = open_subdirectory(&root, subdirectory)? {
                total += self.collect(&directory, subdirectory, &mut candidates)?;
            }
        }
        candidates.sort_by(|a, b| (a.used, &a.name).cmp(&(b.used, &b.name)));

        let mut excess = total.saturating_sub(self.budget_bytes);
        let mut failures = 0u64;
        let mut first_failure = None;
        for candidate in &candidates {
            if excess == 0 {
                break;
            }
            match evict(&root, candidate) {
                Ok(Outcome::Gone) => excess = excess.saturating_sub(candidate.bytes),
                Ok(Outcome::Kept) => {}
                Err(reason) => {
                    failures += 1;
                    first_failure.get_or_insert(reason);
                }
            }
        }
        match first_failure {
            None => Ok(()),
            Some(reason) => Err(format!(
                "{failures} entries stayed, and the first: {reason}"
            )),
        }
    }

    /// Adds each entry of `directory` that stood unused since the cutoff, and
    /// returns the allocated bytes of every entry that Suds may ever evict.
    fn collect(
        &self,
        directory: &OwnedFd,
        subdirectory: u8,
        candidates: &mut Vec<Candidate>,
    ) -> Result<u64, String> {
        let failed = |err: rustix::io::Errno| {
            format!("could not read the directory {subdirectory:02x} of the cache: {err}")
        };
        let mut total = 0u64;
        #[allow(clippy::unnecessary_fallible_conversions, clippy::useless_conversion)]
        for entry in Dir::read_from(directory).map_err(failed)? {
            let entry = entry.map_err(failed)?;
            let name = entry.file_name();
            if !is_entry_name(name, subdirectory) {
                continue;
            }
            // Go may remove an entry during the read.
            let Some(stat) = entry_stat(directory, name) else {
                continue;
            };
            // A directory of another filesystem, mounted here, is no part of
            // the cache.
            if u64::try_from(stat.st_dev).ok() != Some(self.device) {
                continue;
            }
            let bytes = u64::try_from(stat.st_blocks).unwrap_or(0) * 512;
            total += bytes;
            if used(&stat).0 < self.used_since {
                candidates.push(Candidate {
                    used: used(&stat),
                    subdirectory,
                    name: name.to_owned(),
                    device: i128::from(stat.st_dev),
                    inode: stat.st_ino,
                    bytes,
                });
            }
        }
        Ok(total)
    }
}

/// Whether a checkout contains `directory`, wherever its path leads. A client
/// may hold that checkout, and the scan may not have reached it.
pub fn inside_checkout(directory: &Path) -> Result<bool, String> {
    let failed =
        |path: &Path, err: std::io::Error| format!("could not read {}: {err}", path.display());
    let real = std::fs::canonicalize(directory).map_err(|err| failed(directory, err))?;
    for ancestor in real.ancestors() {
        let repository = ancestor.join(".git");
        match std::fs::symlink_metadata(&repository) {
            Ok(_) => return Ok(true),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(failed(&repository, err)),
        }
    }
    Ok(false)
}

/// What became of one entry.
#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    /// Suds removed the entry, or Go's own trim did.
    Gone,
    /// Something used or replaced the entry since Suds read it.
    Kept,
}

/// Removes one entry, unless something used or replaced it since Suds read it.
fn evict(root: &OwnedFd, candidate: &Candidate) -> Result<Outcome, String> {
    let Some(directory) = open_subdirectory(root, candidate.subdirectory)? else {
        return Ok(Outcome::Kept);
    };
    let Some(stat) = entry_stat(&directory, &candidate.name) else {
        let absent = rustix::fs::statat(&directory, &candidate.name, AtFlags::SYMLINK_NOFOLLOW)
            .is_err_and(|err| err == rustix::io::Errno::NOENT);
        return Ok(if absent { Outcome::Gone } else { Outcome::Kept });
    };
    let unchanged = used(&stat) == candidate.used
        && stat.st_ino == candidate.inode
        && i128::from(stat.st_dev) == candidate.device;
    if !unchanged {
        return Ok(Outcome::Kept);
    }
    match rustix::fs::unlinkat(&directory, &candidate.name, AtFlags::empty()) {
        Ok(()) | Err(rustix::io::Errno::NOENT) => Ok(Outcome::Gone),
        Err(err) => Err(format!(
            "could not remove {}: {err}",
            candidate.name.to_string_lossy()
        )),
    }
}

/// The time at which Go last recorded a use.
fn used(stat: &Stat) -> (i64, i64) {
    #[allow(clippy::unnecessary_cast, clippy::useless_conversion)]
    (stat.st_mtime as i64, stat.st_mtime_nsec as i64)
}

/// The status of a regular file that this user owns, or none. An entry of
/// another kind, such as the directory of a cached executable, stays. So
/// does a file with a second link, whose removal frees nothing.
fn entry_stat(directory: &OwnedFd, name: &CStr) -> Option<Stat> {
    let stat = rustix::fs::statat(directory, name, AtFlags::SYMLINK_NOFOLLOW).ok()?;
    let regular = FileType::from_raw_mode(stat.st_mode) == FileType::RegularFile;
    let own = stat.st_uid == rustix::process::geteuid().as_raw();
    (regular && own && stat.st_nlink == 1).then_some(stat)
}

/// Whether `name` is the name that Go gives an entry of this directory: the
/// 64 hexadecimal digits of its hash, then `-a` or `-d`.
fn is_entry_name(name: &CStr, subdirectory: u8) -> bool {
    let name = name.to_bytes();
    name.len() == 66
        && name[..64]
            .iter()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b))
        && name.starts_with(format!("{subdirectory:02x}").as_bytes())
        && (name.ends_with(b"-a") || name.ends_with(b"-d"))
}

fn has_layout(root: &OwnedFd) -> bool {
    let kind = |name: &str| {
        rustix::fs::statat(root, name, AtFlags::SYMLINK_NOFOLLOW)
            .ok()
            .map(|stat| FileType::from_raw_mode(stat.st_mode))
    };
    kind("README") == Some(FileType::RegularFile)
        && kind("trim.txt") == Some(FileType::RegularFile)
        && kind("00") == Some(FileType::Directory)
        && kind("ff") == Some(FileType::Directory)
}

/// One of the cache's 256 directories, or none where Go made none or a link
/// stands in its place.
fn open_subdirectory(root: &OwnedFd, subdirectory: u8) -> Result<Option<OwnedFd>, String> {
    use rustix::io::Errno;
    match open_directory(root, Path::new(&format!("{subdirectory:02x}"))) {
        Ok(directory) => Ok(Some(directory)),
        Err(Errno::NOENT | Errno::NOTDIR | Errno::LOOP) => Ok(None),
        Err(err) => Err(format!(
            "could not open the directory {subdirectory:02x} of the cache: {err}"
        )),
    }
}

fn open_directory(at: impl std::os::fd::AsFd, path: &Path) -> rustix::io::Result<OwnedFd> {
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    rustix::fs::openat(at, path, flags, Mode::empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{Duration, SystemTime};

    /// The one entry of a cache as a run collects it, and the entry's path.
    fn collected(root: &Path) -> (OwnedFd, Candidate, PathBuf) {
        let directory = root.join("0c");
        fs::create_dir(&directory).unwrap();
        let path = directory.join(format!("0c{}-d", "a".repeat(62)));
        fs::write(&path, "entry").unwrap();
        let used = SystemTime::now() - Duration::from_secs(500 * 3600);
        fs::File::open(&path).unwrap().set_modified(used).unwrap();
        let eviction = Eviction::of(root, 0, i64::MAX).unwrap();
        let root = open_directory(rustix::fs::CWD, root).unwrap();
        let mut candidates = Vec::new();
        let subdirectory = open_subdirectory(&root, 0x0c).unwrap().unwrap();
        eviction
            .collect(&subdirectory, 0x0c, &mut candidates)
            .unwrap();
        (root, candidates.remove(0), path)
    }

    #[test]
    fn evicts_an_entry_that_nothing_touched_since_the_read() {
        let dir = tempfile::TempDir::new().unwrap();
        let (root, candidate, path) = collected(dir.path());
        assert_eq!(evict(&root, &candidate), Ok(Outcome::Gone));
        assert!(!path.exists());
        // Go may remove the entry first, which frees its space as well.
        assert_eq!(evict(&root, &candidate), Ok(Outcome::Gone));
    }

    #[test]
    fn collects_no_entry_of_another_device() {
        let dir = tempfile::TempDir::new().unwrap();
        let (root, _, path) = collected(dir.path());
        let eviction = Eviction {
            device: u64::MAX,
            ..Eviction::of(dir.path(), 0, i64::MAX).unwrap()
        };
        let mut candidates = Vec::new();
        let subdirectory = open_subdirectory(&root, 0x0c).unwrap().unwrap();
        let total = eviction.collect(&subdirectory, 0x0c, &mut candidates);
        assert_eq!(total, Ok(0));
        assert!(candidates.is_empty() && path.exists());
    }

    #[test]
    fn keeps_an_entry_that_a_build_used_or_replaced_since_the_read() {
        let dir = tempfile::TempDir::new().unwrap();
        let (root, candidate, path) = collected(dir.path());
        let file = fs::File::open(&path).unwrap();
        let read = file.metadata().unwrap().modified().unwrap();
        file.set_modified(SystemTime::now()).unwrap();
        assert_eq!(evict(&root, &candidate), Ok(Outcome::Kept));

        // A new file of the same name and time is another entry.
        let staged = path.with_file_name("staged");
        fs::write(&staged, "another").unwrap();
        fs::File::open(&staged).unwrap().set_modified(read).unwrap();
        fs::rename(&staged, &path).unwrap();
        assert_eq!(evict(&root, &candidate), Ok(Outcome::Kept));
        assert_eq!(fs::read_to_string(&path).unwrap(), "another");
    }
}
