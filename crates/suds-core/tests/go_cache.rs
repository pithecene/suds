//! Tests of the eviction from Go's build cache, in temporary directories.

use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

use suds_core::go_cache::{Eviction, inside_checkout};
use suds_core::organize::native::NativePath;
use tempfile::TempDir;

const KIB: u64 = 1024;
const HOUR: u64 = 3600;

/// A directory with the layout of Go's build cache.
struct Cache {
    _dir: TempDir,
    path: PathBuf,
    now: SystemTime,
}

impl Cache {
    fn new() -> Self {
        let dir = TempDir::new().unwrap();
        let path = fs::canonicalize(dir.path()).unwrap().join("go-build");
        for n in 0..256 {
            fs::create_dir_all(path.join(format!("{n:02x}"))).unwrap();
        }
        fs::write(path.join("README"), "cache").unwrap();
        fs::write(path.join("trim.txt"), "0").unwrap();
        Cache {
            path,
            now: SystemTime::now(),
            _dir: dir,
        }
    }

    fn age(&self, path: &Path, hours: u64) {
        let used = self.now - Duration::from_secs(hours * HOUR);
        fs::File::open(path).unwrap().set_modified(used).unwrap();
    }

    /// An entry of 64 KiB in the directory `first`, unused for `hours`.
    fn entry(&self, first: &str, kind: &str, hours: u64) -> PathBuf {
        let name = format!("{first}{}-{kind}", "a".repeat(62));
        let path = self.path.join(first).join(name);
        fs::write(&path, vec![7u8; 64 * KIB as usize]).unwrap();
        self.age(&path, hours);
        path
    }

    /// The eviction that keeps what Go used within two hours.
    fn evict_to(&self, budget_bytes: u64) -> Result<(), String> {
        let since = self.now - Duration::from_secs(2 * HOUR);
        let since = since.duration_since(SystemTime::UNIX_EPOCH).unwrap();
        Eviction::of(
            &self.path,
            budget_bytes,
            i64::try_from(since.as_secs()).unwrap(),
        )
        .unwrap()
        .run()
    }
}

#[test]
fn evicts_the_least_recently_used_entries_and_stops_at_the_budget() {
    let cache = Cache::new();
    let newer = cache.entry("01", "d", 10);
    let oldest = cache.entry("ff", "d", 90);
    let older = cache.entry("80", "a", 50);
    // Three entries hold 192 KiB, and the removal of one meets the budget.
    cache.evict_to(160 * KIB).unwrap();
    assert!(!oldest.exists());
    assert!(older.exists() && newer.exists());
    cache.evict_to(0).unwrap();
    assert!(!older.exists() && !newer.exists());
}

#[test]
fn keeps_each_entry_that_go_used_recently_whatever_the_budget() {
    let cache = Cache::new();
    let recent = cache.entry("10", "d", 1);
    let stale = cache.entry("11", "d", 3);
    cache.evict_to(0).unwrap();
    assert!(recent.exists());
    assert!(!stale.exists());
}

#[test]
fn leaves_everything_that_is_no_entry_of_go() {
    let cache = Cache::new();
    let outside = cache.path.parent().unwrap().join("outside");
    fs::write(&outside, "kept").unwrap();
    cache.age(&outside, 500);
    let hash = "a".repeat(62);
    let fuzz = cache.path.join("fuzz/pkg/corpus");
    fs::create_dir_all(fuzz.parent().unwrap()).unwrap();
    let executable = cache.path.join(format!("20/20{hash}-d"));
    fs::create_dir(&executable).unwrap();
    let link = cache.path.join(format!("21/21{hash}-d"));
    symlink(&outside, &link).unwrap();
    let files = [
        fuzz,
        executable.join("tool"),
        cache.path.join("README"),
        cache.path.join("trim.txt"),
        cache.path.join("22/notes.txt"),
        // A name of another directory, of another kind, and of no hash.
        cache.path.join(format!("23/24{hash}-d")),
        cache.path.join(format!("25/25{hash}-x")),
        cache.path.join(format!("26/26{}-d", "Z".repeat(62))),
        cache.path.join(format!("stray{hash}-d")),
    ];
    for file in &files {
        fs::write(file, "kept").unwrap();
        cache.age(file, 500);
    }
    cache.evict_to(0).unwrap();
    for file in &files {
        assert!(file.exists(), "{}", file.display());
    }
    assert!(fs::symlink_metadata(&link).is_ok());
    assert_eq!(fs::read_to_string(&outside).unwrap(), "kept");
}

#[test]
fn follows_no_link_in_place_of_a_directory() {
    let cache = Cache::new();
    let elsewhere = cache.path.parent().unwrap().join("elsewhere");
    fs::create_dir(&elsewhere).unwrap();
    let name = format!("30{}-d", "a".repeat(62));
    let foreign = elsewhere.join(&name);
    fs::write(&foreign, "kept").unwrap();
    cache.age(&foreign, 500);
    fs::remove_dir(cache.path.join("30")).unwrap();
    symlink(&elsewhere, cache.path.join("30")).unwrap();
    cache.evict_to(0).unwrap();
    assert!(foreign.exists());

    // The cache's own path is no link either.
    let alias = cache.path.parent().unwrap().join("alias");
    symlink(&cache.path, &alias).unwrap();
    let stale = cache.entry("31", "d", 500);
    assert!(Eviction::of(&alias, 0, i64::MAX).is_err());
    let linked = Eviction {
        directory: NativePath::new(alias),
        ..Eviction::of(&cache.path, 0, i64::MAX).unwrap()
    };
    assert!(linked.run().is_err());
    assert!(stale.exists());
}

#[test]
fn refuses_a_directory_that_replaced_the_planned_one() {
    let cache = Cache::new();
    let planned = Eviction::of(&cache.path, 0, i64::MAX).unwrap();
    let other = Cache::new();
    let stale = other.entry("50", "d", 500);
    // A parent becomes a link, so the planned path leads to another cache.
    let parent = cache.path.parent().unwrap();
    fs::rename(parent, parent.with_extension("moved")).unwrap();
    symlink(other.path.parent().unwrap(), parent).unwrap();
    let reason = planned.run().unwrap_err();
    fs::remove_file(parent).unwrap();
    fs::rename(parent.with_extension("moved"), parent).unwrap();
    assert!(
        reason.contains("not the directory that the plan named"),
        "{reason}"
    );
    assert!(stale.exists());
}

#[test]
fn finds_the_checkout_that_contains_a_cache_wherever_its_path_leads() {
    let dir = TempDir::new().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    let inside = root.join("repo/deep/cache");
    fs::create_dir_all(&inside).unwrap();
    fs::create_dir(root.join("outside")).unwrap();
    // A linked checkout holds a file in place of the directory.
    fs::write(root.join("repo/.git"), "gitdir: elsewhere").unwrap();
    symlink(root.join("repo/deep"), root.join("alias")).unwrap();
    let dotted = root.join("outside/../repo/deep/cache");
    for path in [inside, dotted, root.join("alias/cache")] {
        assert_eq!(inside_checkout(&path), Ok(true), "{}", path.display());
    }
    assert_eq!(inside_checkout(&root.join("outside")), Ok(false));
    assert!(inside_checkout(&root.join("absent")).is_err());
}

#[test]
fn refuses_a_cache_that_a_repository_came_to_contain() {
    let cache = Cache::new();
    let stale = cache.entry("52", "d", 500);
    let planned = Eviction::of(&cache.path, 0, i64::MAX).unwrap();
    fs::create_dir(cache.path.parent().unwrap().join(".git")).unwrap();
    let reason = planned.run().unwrap_err();
    assert!(reason.contains("lies inside a checkout"), "{reason}");
    assert!(stale.exists());
}

#[test]
fn leaves_a_file_with_a_second_link() {
    let cache = Cache::new();
    let linked = cache.entry("51", "d", 500);
    fs::hard_link(&linked, cache.path.join("elsewhere")).unwrap();
    cache.evict_to(0).unwrap();
    assert!(linked.exists());
}

#[test]
fn refuses_a_directory_without_the_layout_of_the_cache() {
    for missing in ["README", "trim.txt"] {
        let cache = Cache::new();
        let stale = cache.entry("40", "d", 500);
        fs::remove_file(cache.path.join(missing)).unwrap();
        let reason = cache.evict_to(0).unwrap_err();
        assert!(reason.contains("no longer has the layout"), "{reason}");
        assert!(stale.exists());
    }
}

/// Go itself judges the result: it builds from a cache that Suds evicted
/// from, and it builds while Suds evicts. A machine without Go skips the test.
#[test]
fn go_builds_from_an_evicted_cache_and_during_an_eviction() {
    let dir = TempDir::new().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    let cache = root.join("go-build");
    let module = root.join("module");
    fs::create_dir_all(&module).unwrap();
    fs::write(module.join("go.mod"), "module example.com/m\n\ngo 1.21\n").unwrap();
    fs::write(
        module.join("main.go"),
        "package main\n\nimport (\n\t\"fmt\"\n\t\"strings\"\n)\n\nfunc main() { fmt.Println(strings.ToUpper(\"built\")) }\n",
    )
    .unwrap();
    let go = |arguments: &[&str]| {
        Command::new("go")
            .args(arguments)
            .current_dir(&module)
            .env("GOCACHE", &cache)
            .env("GOWORK", "off")
            .env("GOFLAGS", "-p=2")
            .env("GOTOOLCHAIN", "local")
            .output()
    };
    if !go(&["version"]).is_ok_and(|out| out.status.success()) {
        eprintln!("skipped: the machine runs no Go");
        return;
    }
    let build = || {
        let out = go(&["build", "-o", "app", "."]).unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    build();
    let eviction = Eviction::of(&cache, 0, i64::MAX).unwrap();
    // Go writes its trim record at its first trim, which a fresh cache makes
    // in the first build.
    eviction.run().unwrap();
    build();
    let out = Command::new(module.join("app")).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "BUILT");

    // Entries of a running build are recent, so the cutoff keeps them.
    let since = SystemTime::now() - Duration::from_secs(2 * HOUR);
    let since = since.duration_since(SystemTime::UNIX_EPOCH).unwrap();
    let concurrent = Eviction {
        used_since: i64::try_from(since.as_secs()).unwrap(),
        ..eviction
    };
    std::thread::scope(|scope| {
        let evicting = scope.spawn(|| {
            for _ in 0..20 {
                concurrent.run().unwrap();
            }
        });
        for n in 0..3 {
            fs::write(
                module.join("extra.go"),
                format!("package main\n\nconst extra = {n}\n"),
            )
            .unwrap();
            build();
        }
        evicting.join().unwrap();
    });
}
