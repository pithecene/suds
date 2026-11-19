//! Budgets of the shared stores.
//!
//! Each store belongs to a program, and only that program trims it: Suds
//! runs the program's own command through a typed adapter and never removes
//! a store's entry itself. A store that exceeds its budget takes its
//! program's trim, and Suds only reports a store whose program offers none.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};

use crate::development::{self, DevelopmentScan, StoreStatus};
use crate::organize::journal::{Event, Journal, JournalError, RunStatus};
use crate::organize::plan::new_id;
use crate::reclaim::execute::Authority;
use crate::timestamp;

/// The schema tag of every stores plan.
pub const STORES_SCHEMA: &str = "suds.stores/v1";
/// The schema tag of a stores run's result.
pub const STORES_RUN_SCHEMA: &str = "suds.stores-run/v1";

const GIB: u64 = 1024 * 1024 * 1024;

/// The identifier of the container engine's images.
pub const DOCKER_IMAGES: &str = "docker.images";
/// The identifier of the container engine's build cache.
pub const DOCKER_BUILD_CACHE: &str = "docker.build-cache";
/// The identifier of the volumes that the engine created without a name.
pub const DOCKER_ANONYMOUS_VOLUMES: &str = "docker.anonymous-volumes";
/// The identifier of the named volumes that no container uses.
pub const DOCKER_NAMED_VOLUMES: &str = "docker.named-volumes";

/// The size in bytes that each store may reach before its program trims it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Budgets {
    bytes: HashMap<String, u64>,
}

impl Default for Budgets {
    fn default() -> Self {
        let defaults = [
            ("go.build-cache", 10),
            ("go.module-cache", 10),
            ("pnpm.store", 10),
            ("ccache.cache", 5),
            ("mise.installs", 20),
            (DOCKER_IMAGES, 20),
            (DOCKER_BUILD_CACHE, 10),
            (DOCKER_ANONYMOUS_VOLUMES, 1),
        ];
        Budgets {
            bytes: defaults
                .into_iter()
                .map(|(id, gib)| (id.to_owned(), gib * GIB))
                .collect(),
        }
    }
}

impl Budgets {
    pub fn set(&mut self, id: &str, bytes: u64) {
        self.bytes.insert(id.to_owned(), bytes);
    }

    pub fn of(&self, id: &str) -> Option<u64> {
        self.bytes.get(id).copied()
    }
}

/// The program of each adapter. A test names a stub in place of a program.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Programs {
    pub go: PathBuf,
    pub pnpm: PathBuf,
    pub ccache: PathBuf,
    pub mise: PathBuf,
    pub docker: PathBuf,
}

impl Default for Programs {
    /// Each program by the name that the process's search path resolves.
    fn default() -> Self {
        Programs {
            go: PathBuf::from("go"),
            pnpm: PathBuf::from("pnpm"),
            ccache: PathBuf::from("ccache"),
            mise: PathBuf::from("mise"),
            docker: PathBuf::from("docker"),
        }
    }
}

/// One program's trim of its own store, as the list of its arguments.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Trim {
    pub program: PathBuf,
    pub arguments: Vec<String>,
    /// What the program's command removes.
    pub removes: String,
}

impl Trim {
    fn new(program: &Path, arguments: &[&str], removes: &str) -> Self {
        Trim {
            program: program.to_path_buf(),
            arguments: arguments.iter().map(|&word| word.to_owned()).collect(),
            removes: removes.to_owned(),
        }
    }

    /// The command as a person would type it, for display alone.
    pub fn display(&self) -> String {
        let mut words = vec![self.program.display().to_string()];
        words.extend(self.arguments.iter().cloned());
        words.join(" ")
    }

    fn run(&self) -> Result<(), String> {
        let output = Command::new(&self.program)
            .args(&self.arguments)
            .output()
            .map_err(|err| format!("could not run {}: {err}", self.program.display()))?;
        if output.status.success() {
            Ok(())
        } else {
            Err(format!(
                "{} failed: {}",
                self.display(),
                String::from_utf8_lossy(&output.stderr).trim()
            ))
        }
    }
}

/// The trim that a store's own program offers, or none.
fn trim_of(id: &str, path: Option<&Path>, budget: u64, programs: &Programs) -> Option<Trim> {
    let budget = budget.to_string();
    let trim = match id {
        "go.build-cache" => Trim::new(
            &programs.go,
            &["clean", "-cache"],
            "every cached build output, which the next build regenerates",
        ),
        "go.module-cache" => Trim::new(
            &programs.go,
            &["clean", "-modcache"],
            "every downloaded module, which the next build downloads again",
        ),
        "pnpm.store" => Trim::new(
            &programs.pnpm,
            &["store", "prune"],
            "each package that no project references",
        ),
        "ccache.cache" => Trim::new(
            &programs.ccache,
            &[
                "--trim-dir",
                &path?.to_string_lossy(),
                "--trim-max-size",
                &budget,
            ],
            "the least recently used results beyond the budget",
        ),
        "mise.installs" => Trim::new(
            &programs.mise,
            &["prune", "--yes"],
            "each tool version that no tracked configuration names",
        ),
        DOCKER_BUILD_CACHE => Trim::new(
            &programs.docker,
            &["builder", "prune", "--force", "--max-used-space", &budget],
            "the build cache beyond the budget",
        ),
        DOCKER_IMAGES => Trim::new(
            &programs.docker,
            &["image", "prune", "--all", "--force"],
            "each image that no container uses",
        ),
        DOCKER_ANONYMOUS_VOLUMES => Trim::new(
            &programs.docker,
            &["volume", "prune", "--force"],
            "each anonymous volume that no container uses",
        ),
        _ => return None,
    };
    Some(trim)
}

/// What a plan decides for one store.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum Decision {
    /// The store lies within its budget.
    Within,
    /// The store exceeds its budget, and its program trims it.
    Trim { trim: Trim },
    /// The store has no budget, or its program offers no trim, so Suds
    /// reports its size alone.
    Audit { reason: String },
    /// The store exceeds its budget, and a rule keeps it for now.
    Keep { reason: String },
    /// The entries hold data that no program can regenerate, so they wait
    /// for a person.
    AwaitsPerson { names: Vec<String> },
}

/// One store of a plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub id: String,
    /// The store's directory, or none for a store of the container engine.
    pub path: Option<String>,
    pub measured_bytes: u64,
    pub budget_bytes: Option<u64>,
    #[serde(flatten)]
    pub decision: Decision,
}

/// A plan to hold each store to its budget.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoresPlan {
    pub schema: String,
    pub id: String,
    /// The time at which Suds built the plan, in RFC 3339 form and UTC.
    pub created_at: String,
    pub entries: Vec<Entry>,
}

/// What the container engine holds, in bytes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EngineUsage {
    pub images: u64,
    pub build_cache: u64,
    /// The anonymous volumes that no container uses.
    pub unused_anonymous_volumes: u64,
    /// The named volumes that no container uses, each with its size.
    pub unused_named_volumes: Vec<(String, u64)>,
}

/// Asks the container engine for its disk usage. The engine reports each
/// size as a decimal figure with a unit.
pub fn engine_usage(docker: &Path) -> Result<EngineUsage, String> {
    let output = Command::new(docker)
        .args(["system", "df", "--verbose", "--format", "{{json .}}"])
        .output()
        .map_err(|err| format!("could not run {}: {err}", docker.display()))?;
    if !output.status.success() {
        return Err(format!(
            "{} system df failed: {}",
            docker.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    parse_engine_usage(&output.stdout)
}

fn parse_engine_usage(json: &[u8]) -> Result<EngineUsage, String> {
    let report: serde_json::Value = serde_json::from_slice(json)
        .map_err(|err| format!("the engine's usage report is not JSON: {err}"))?;
    let rows = |key: &str| {
        report[key]
            .as_array()
            .map_or(&[][..], |rows| rows.as_slice())
    };
    let size =
        |row: &serde_json::Value, key: &str| row[key].as_str().and_then(parse_size).unwrap_or(0);
    let mut usage = EngineUsage {
        // An image's unique size leaves out the layers that it shares, so
        // the sum counts each shared layer in no image. The figure is a
        // lower bound.
        images: rows("Images")
            .iter()
            .map(|row| size(row, "UniqueSize"))
            .sum(),
        build_cache: rows("BuildCache").iter().map(|row| size(row, "Size")).sum(),
        ..EngineUsage::default()
    };
    for volume in rows("Volumes") {
        if volume["Links"].as_str() != Some("0") {
            continue;
        }
        let bytes = size(volume, "Size");
        let anonymous = volume["Labels"]
            .as_str()
            .is_some_and(|labels| labels.contains("com.docker.volume.anonymous"));
        if anonymous {
            usage.unused_anonymous_volumes += bytes;
        } else if let Some(name) = volume["Name"].as_str() {
            usage.unused_named_volumes.push((name.to_owned(), bytes));
        }
    }
    Ok(usage)
}

/// Parses a size such as `447.4MB` or `0B`, in the engine's decimal units,
/// or a size such as `10G` or `500M`, in binary units, as a person names a
/// budget.
pub fn parse_size(text: &str) -> Option<u64> {
    let text = text.trim();
    let split = text
        .find(|character: char| !(character.is_ascii_digit() || character == '.'))
        .unwrap_or(text.len());
    let (number, unit) = text.split_at(split);
    let number: f64 = number.parse().ok()?;
    let scale: f64 = match unit.trim() {
        "" | "B" => 1.0,
        "kB" | "KB" => 1e3,
        "MB" => 1e6,
        "GB" => 1e9,
        "TB" => 1e12,
        "K" | "KiB" => 1024.0,
        "M" | "MiB" => 1024.0 * 1024.0,
        "G" | "GiB" => 1024.0 * 1024.0 * 1024.0,
        "T" | "TiB" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        _ => return None,
    };
    let bytes = number * scale;
    if !bytes.is_finite() || bytes < 0.0 {
        return None;
    }
    // A size beyond the range of `u64` saturates, and no store reaches it.
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "the value is finite and not negative, and the cast saturates"
    )]
    Some(bytes as u64)
}

/// What a plan needs beside the inspection.
pub struct Inputs<'a> {
    pub budgets: &'a Budgets,
    pub programs: &'a Programs,
    /// The engine's usage, or the reason that Suds could not read it.
    pub engine: &'a Result<EngineUsage, String>,
    /// The time for which a store must have stood unchanged before its
    /// program trims it, in seconds. A build in progress writes its store.
    pub idle_seconds: u64,
    pub now: i64,
}

/// Plans each store's trim from an inspection and the engine's usage.
pub fn plan_stores(scan: &DevelopmentScan, inputs: &Inputs<'_>) -> StoresPlan {
    let idle = i64::try_from(inputs.idle_seconds).unwrap_or(i64::MAX);
    let idle_since = timestamp::format_utc(inputs.now.saturating_sub(idle));
    let mut entries = Vec::new();
    for store in &scan.stores {
        let budget = inputs.budgets.of(&store.id);
        let measured = store.measure.allocated_bytes;
        let decision = match (store.status, budget) {
            (StoreStatus::Absent, _) => continue,
            (StoreStatus::Unreadable, _) => Decision::Audit {
                reason: "Suds could not examine the store".to_owned(),
            },
            (StoreStatus::Unrecognized, _) => Decision::Audit {
                reason: "the directory lacks its program's marker, so nothing proves its owner"
                    .to_owned(),
            },
            (StoreStatus::Present, None) => Decision::Audit {
                reason: "the store has no budget".to_owned(),
            },
            (StoreStatus::Present, Some(budget)) if measured <= budget => Decision::Within,
            (StoreStatus::Present, Some(budget)) => {
                let recent = store
                    .measure
                    .newest_modified
                    .as_deref()
                    .filter(|newest| *newest > idle_since.as_str());
                match (
                    recent,
                    trim_of(&store.id, Some(&store.path), budget, inputs.programs),
                ) {
                    (Some(newest), _) => Decision::Keep {
                        reason: format!(
                            "an entry changed at {newest}, after {idle_since}, so a build may write the store"
                        ),
                    },
                    (None, Some(trim)) => Decision::Trim { trim },
                    (None, None) => Decision::Audit {
                        reason: "the store's program offers no trim".to_owned(),
                    },
                }
            }
        };
        entries.push(Entry {
            id: store.id.clone(),
            path: Some(store.path.to_string_lossy().into_owned()),
            measured_bytes: measured,
            budget_bytes: budget,
            decision,
        });
    }
    match inputs.engine {
        Ok(usage) => entries.extend(engine_entries(usage, inputs)),
        Err(reason) => entries.push(Entry {
            id: "docker".to_owned(),
            path: None,
            measured_bytes: 0,
            budget_bytes: None,
            decision: Decision::Audit {
                reason: reason.clone(),
            },
        }),
    }
    StoresPlan {
        schema: STORES_SCHEMA.to_owned(),
        id: new_id("stores"),
        created_at: timestamp::format_utc(inputs.now),
        entries,
    }
}

fn engine_entries(usage: &EngineUsage, inputs: &Inputs<'_>) -> Vec<Entry> {
    let mut entries = Vec::new();
    for (id, measured) in [
        (DOCKER_IMAGES, usage.images),
        (DOCKER_BUILD_CACHE, usage.build_cache),
        (DOCKER_ANONYMOUS_VOLUMES, usage.unused_anonymous_volumes),
    ] {
        let budget = inputs.budgets.of(id);
        let decision = match budget {
            None => Decision::Audit {
                reason: "the store has no budget".to_owned(),
            },
            Some(budget) if measured <= budget => Decision::Within,
            Some(budget) => match trim_of(id, None, budget, inputs.programs) {
                Some(trim) => Decision::Trim { trim },
                None => Decision::Audit {
                    reason: "the store's program offers no trim".to_owned(),
                },
            },
        };
        entries.push(Entry {
            id: id.to_owned(),
            path: None,
            measured_bytes: measured,
            budget_bytes: budget,
            decision,
        });
    }
    if !usage.unused_named_volumes.is_empty() {
        entries.push(Entry {
            id: DOCKER_NAMED_VOLUMES.to_owned(),
            path: None,
            measured_bytes: usage
                .unused_named_volumes
                .iter()
                .map(|(_, bytes)| bytes)
                .sum(),
            budget_bytes: None,
            decision: Decision::AwaitsPerson {
                names: usage
                    .unused_named_volumes
                    .iter()
                    .map(|(name, _)| name.clone())
                    .collect(),
            },
        });
    }
    entries
}

/// The result of one stores run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StoresReport {
    pub schema: String,
    pub run: String,
    pub plan: String,
    pub authority: Authority,
    pub status: RunStatus,
    pub freed_bytes: u64,
    pub trims: Vec<TrimOutcome>,
}

/// The outcome of one store's trim.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TrimOutcome {
    pub store: String,
    pub command: String,
    pub before_bytes: u64,
    /// The store's size after the trim, or none when the trim failed.
    pub after_bytes: Option<u64>,
    pub failure: Option<String>,
}

/// Runs each trim of `plan` and measures each store again. The `measure`
/// function returns a store's size after its trim. One failure leaves the
/// other trims to proceed, and a journal failure stops the run.
pub fn apply(
    journal: &mut Journal,
    plan: &StoresPlan,
    authority: Authority,
    measure: &mut dyn FnMut(&Entry) -> Result<u64, String>,
) -> Result<StoresReport, JournalError> {
    let run = new_id("run");
    journal.append(Event::StoresStarted {
        run: run.clone(),
        plan: plan.id.clone(),
        authority,
    })?;
    let mut trims = Vec::new();
    for entry in &plan.entries {
        let Decision::Trim { trim } = &entry.decision else {
            continue;
        };
        journal.append(Event::StoreTrimStarted {
            run: run.clone(),
            store: entry.id.clone(),
            command: trim.display(),
        })?;
        let outcome = trim.run().and_then(|()| measure(entry));
        journal.append(match &outcome {
            Ok(after) => Event::StoreTrimmed {
                run: run.clone(),
                store: entry.id.clone(),
                before_bytes: entry.measured_bytes,
                after_bytes: *after,
            },
            Err(reason) => Event::StoreTrimFailed {
                run: run.clone(),
                store: entry.id.clone(),
                reason: reason.clone(),
            },
        })?;
        trims.push(TrimOutcome {
            store: entry.id.clone(),
            command: trim.display(),
            before_bytes: entry.measured_bytes,
            after_bytes: outcome.as_ref().ok().copied(),
            failure: outcome.err(),
        });
    }
    let succeeded = trims.iter().filter(|trim| trim.failure.is_none()).count();
    let freed_bytes = trims
        .iter()
        .filter_map(|trim| Some(trim.before_bytes.saturating_sub(trim.after_bytes?)))
        .sum();
    let status = RunStatus::from_counts(succeeded, trims.len());
    journal.append(Event::StoresCompleted {
        run: run.clone(),
        status,
        freed_bytes,
    })?;
    Ok(StoresReport {
        schema: STORES_RUN_SCHEMA.to_owned(),
        run,
        plan: plan.id.clone(),
        authority,
        status,
        freed_bytes,
        trims,
    })
}

/// A store's size after its trim: a directory's allocated bytes, or the
/// engine's own figure for one of its stores.
pub fn measure_after(entry: &Entry, docker: &Path) -> Result<u64, String> {
    if let Some(path) = &entry.path {
        let (measure, _) = development::survey_tree(Path::new(path));
        return Ok(measure.allocated_bytes);
    }
    let usage = engine_usage(docker)?;
    Ok(match entry.id.as_str() {
        DOCKER_IMAGES => usage.images,
        DOCKER_BUILD_CACHE => usage.build_cache,
        DOCKER_ANONYMOUS_VOLUMES => usage.unused_anonymous_volumes,
        _ => 0,
    })
}

/// A word of a command, for a caller that names a budget on a command line.
pub fn parse_budget(word: &OsString) -> Result<(String, u64), String> {
    let text = word.to_string_lossy();
    let (id, size) = text
        .split_once('=')
        .ok_or_else(|| format!("{text} is not a budget of the form STORE=SIZE"))?;
    let bytes = parse_size(size).ok_or_else(|| format!("{size} is not a size, such as 10G"))?;
    Ok((id.to_owned(), bytes))
}

#[cfg(test)]
mod tests {
    use super::{parse_engine_usage, parse_size};

    #[test]
    fn parses_the_engine_units_and_a_budget_units() {
        assert_eq!(parse_size("0B"), Some(0));
        assert_eq!(parse_size("447.4MB"), Some(447_400_000));
        assert_eq!(parse_size("3.67GB"), Some(3_670_000_000));
        assert_eq!(parse_size("10G"), Some(10 * 1024 * 1024 * 1024));
        assert_eq!(parse_size("500M"), Some(500 * 1024 * 1024));
        assert_eq!(parse_size("1024"), Some(1024));
        assert_eq!(parse_size("ten"), None);
        assert_eq!(parse_size("5 parsecs"), None);
    }

    #[test]
    fn reads_the_engine_usage_and_separates_the_volumes() {
        let report = br#"{
            "Images": [{"UniqueSize": "2GB"}, {"UniqueSize": "500MB"}],
            "BuildCache": [{"Size": "1GB"}, {"Size": "0B"}],
            "Volumes": [
                {"Name": "aaa", "Labels": "com.docker.volume.anonymous=", "Links": "0", "Size": "100MB"},
                {"Name": "bbb", "Labels": "com.docker.volume.anonymous=", "Links": "1", "Size": "900MB"},
                {"Name": "database", "Labels": "", "Links": "0", "Size": "300MB"}
            ],
            "Containers": []
        }"#;
        let usage = parse_engine_usage(report).unwrap();
        assert_eq!(usage.images, 2_500_000_000);
        assert_eq!(usage.build_cache, 1_000_000_000);
        assert_eq!(usage.unused_anonymous_volumes, 100_000_000);
        assert_eq!(
            usage.unused_named_volumes,
            [("database".to_owned(), 300_000_000)]
        );
    }
}
