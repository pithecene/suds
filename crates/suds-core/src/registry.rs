//! The registry of checkouts that clients hold.
//!
//! A client that starts work in a checkout registers it, and a client that
//! ends that work releases it. A registration adds knowledge and no
//! permission: Suds still verifies every fact before it removes anything.
//! The registry is an append-only file with one JSON record on each line.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use rustix::fs::OFlags;
use serde::{Deserialize, Serialize};

use crate::inspection::EntryRevision;
use crate::organize::native::NativePath;
use crate::timestamp;

/// The schema tag of every registry record.
pub const REGISTRY_SCHEMA: &str = "suds.registry/v1";

/// One immutable record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    pub schema: String,
    /// The time of the record, in RFC 3339 form and UTC.
    pub at: String,
    pub checkout: NativePath,
    /// The program that holds or held the checkout, such as `bosun`.
    pub client: String,
    /// The client's own name for the work, such as a workflow's identifier.
    pub attempt: Option<String>,
    pub event: Event,
    /// On a registration, the client's text for who claims the checkout, such
    /// as a session. Suds shows the text and reads nothing from it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claim: Option<String>,
    /// On a release, the head that the client reports as landed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub landed: Option<String>,
    /// On a release that states an ended claim, the revision of the checkout's
    /// `.git` entry. A new checkout at the same path has another one. A release
    /// of an earlier version states nothing, so the claim holds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claim_ended: Option<EntryRevision>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Event {
    Registered,
    Released,
}

/// The claim that one client and attempt made on a checkout, as a plan states
/// it to the person who approves the plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Claim {
    pub checkout: NativePath,
    pub client: String,
    pub attempt: Option<String>,
    /// The client's latest record for the attempt, and its time.
    pub event: Event,
    pub at: String,
    /// The checkout that the latest record names as one whose claim ended.
    pub ended: Option<EntryRevision>,
    /// The position of the latest record in the registry, which differs after
    /// any later record of the client and attempt, even in the same second.
    pub position: usize,
    /// The text of the latest registration, and that registration's time.
    pub text: Option<String>,
    pub registered_at: Option<String>,
}

impl Claim {
    /// The claim in one line, beside the revision of the `.git` entry that
    /// stands at the claim's path, where the reader knows it.
    pub fn line(&self, standing: Option<&EntryRevision>) -> String {
        let standing = match (self.event, &self.ended) {
            (Event::Registered, _) => "held",
            (Event::Released, None) => "released without an end",
            (Event::Released, Some(ended)) if standing.is_some_and(|now| now != ended) => {
                "ended for another .git revision"
            }
            (Event::Released, Some(_)) => "ended",
        };
        let mut line = format!("{standing} since {}, client {:?}", self.at, self.client);
        if let Some(attempt) = &self.attempt {
            line.push_str(&format!(", attempt {attempt:?}"));
        }
        match (&self.registered_at, &self.text) {
            (Some(at), Some(text)) => line.push_str(&format!(", registered {at} as {text:?}")),
            (Some(at), None) => line.push_str(&format!(", registered {at} without a text")),
            (None, _) => line.push_str(", never registered"),
        }
        format!("{line}, on {:?}", self.checkout.display())
    }
}

/// The newest outstanding registration of a checkout, or its latest release.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Standing {
    pub event: Event,
    pub client: String,
    pub attempt: Option<String>,
    pub at: String,
    /// The head that a release reported as landed.
    pub landed: Option<String>,
    /// The checkout that the latest record of each client and attempt names as
    /// one whose claim ended.
    pub claim_ended: Option<EntryRevision>,
    /// The claim of each client and attempt.
    pub claims: Vec<Claim>,
}

/// The latest standing of each checkout that the registry names.
pub type Standings = HashMap<PathBuf, Standing>;

/// The standings, or the reason that Suds could not read the registry. A
/// registry that Suds cannot read protects nothing, so a removal treats the
/// failure as a fact that it could not verify.
pub type Reading = Result<Standings, String>;

/// Reads the registry at `path` and returns each checkout's standing.
pub fn reading(path: &Path) -> Reading {
    read(path).map(|records| standings(&records))
}

/// The registration that holds `checkout`: its own, or that of a checkout
/// above it, because a registration covers everything beneath its checkout.
pub fn holder<'a>(standings: &'a Standings, checkout: &Path) -> Option<(&'a Path, &'a Standing)> {
    standings
        .iter()
        .filter(|(_, standing)| standing.event == Event::Registered)
        .map(|(path, standing)| (path.as_path(), standing))
        .find(|(path, _)| checkout.starts_with(path))
}

/// The registration of `path` or of a path beneath it. A client that holds
/// a directory inside a candidate holds the candidate too.
pub fn held_beneath<'a>(standings: &'a Standings, path: &Path) -> Option<(&'a Path, &'a Standing)> {
    standings
        .iter()
        .filter(|(_, standing)| standing.event == Event::Registered)
        .map(|(held, standing)| (held.as_path(), standing))
        .find(|(held, _)| held.starts_with(path))
}

/// The claims that name `checkout`, a checkout above it, or a path that `path`
/// contains, in a stable order.
pub fn claims(standings: &Standings, checkout: &Path, path: &Path) -> Vec<Claim> {
    let mut claims: Vec<Claim> = standings
        .iter()
        .filter(|(held, _)| checkout.starts_with(held) || held.starts_with(path))
        .flat_map(|(_, standing)| standing.claims.iter().cloned())
        .collect();
    claims.sort_by(|a, b| {
        let key = |claim: &Claim| {
            (
                claim.checkout.as_path().to_path_buf(),
                claim.client.clone(),
                claim.attempt.clone(),
            )
        };
        key(a).cmp(&key(b))
    });
    claims
}

/// Appends a registration of `checkout` whose client states `claim` as the
/// text for who claims it.
pub fn append_claim(
    path: &Path,
    checkout: &Path,
    client: &str,
    attempt: Option<&str>,
    claim: &str,
    now: i64,
) -> Result<Record, String> {
    let claim = claim.trim();
    if claim.is_empty() {
        return Err("the text of a claim is empty".to_owned());
    }
    if claim.chars().any(char::is_control) {
        return Err("the text of a claim holds a control character".to_owned());
    }
    let record = Record {
        schema: REGISTRY_SCHEMA.to_owned(),
        at: timestamp::format_utc(now),
        checkout: NativePath::new(checkout.to_path_buf()),
        client: client.to_owned(),
        attempt: attempt.map(str::to_owned),
        event: Event::Registered,
        claim: Some(claim.to_owned()),
        landed: None,
        claim_ended: None,
    };
    write(path, record)
}

/// Appends one record to the registry at `path`, and creates the file and
/// its directory when they do not exist. The file opens without following a
/// symbolic link, and one write holds the whole line.
pub fn append(
    path: &Path,
    checkout: &Path,
    client: &str,
    attempt: Option<&str>,
    event: Event,
    now: i64,
) -> Result<Record, String> {
    let record = Record {
        schema: REGISTRY_SCHEMA.to_owned(),
        at: timestamp::format_utc(now),
        checkout: NativePath::new(checkout.to_path_buf()),
        client: client.to_owned(),
        attempt: attempt.map(str::to_owned),
        event,
        claim: None,
        landed: None,
        claim_ended: None,
    };
    write(path, record)
}

/// Appends a release of `checkout` that reports the commit `landed` at its
/// head as landed. The commit is a full object name in lowercase
/// hexadecimal digits.
pub fn append_landed(
    path: &Path,
    checkout: &Path,
    client: &str,
    attempt: Option<&str>,
    landed: &str,
    now: i64,
) -> Result<Record, String> {
    release(path, checkout, client, attempt, Some(landed), None, now)
}

/// Appends a release of `checkout` that states that the claim ended, with the
/// head that landed, if any.
pub fn append_ended(
    path: &Path,
    checkout: &Path,
    client: &str,
    attempt: Option<&str>,
    landed: Option<&str>,
    now: i64,
) -> Result<Record, String> {
    let git = checkout.join(".git");
    let meta = fs::symlink_metadata(&git)
        .map_err(|err| format!("could not read {}: {err}", git.display()))?;
    let ended = Some(EntryRevision::from_metadata(&meta));
    release(path, checkout, client, attempt, landed, ended, now)
}

fn release(
    path: &Path,
    checkout: &Path,
    client: &str,
    attempt: Option<&str>,
    landed: Option<&str>,
    claim_ended: Option<EntryRevision>,
    now: i64,
) -> Result<Record, String> {
    if let Some(landed) = landed {
        let full = matches!(landed.len(), 40 | 64)
            && landed
                .bytes()
                .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'));
        if !full {
            return Err(format!("{landed} is no full commit name"));
        }
    }
    let record = Record {
        schema: REGISTRY_SCHEMA.to_owned(),
        at: timestamp::format_utc(now),
        checkout: NativePath::new(checkout.to_path_buf()),
        client: client.to_owned(),
        attempt: attempt.map(str::to_owned),
        event: Event::Released,
        claim: None,
        landed: landed.map(str::to_owned),
        claim_ended,
    };
    write(path, record)
}

/// Writes `record` as the registry's next line, and creates the file and
/// its directory when they do not exist.
fn write(path: &Path, record: Record) -> Result<Record, String> {
    let failed = |err: io::Error| format!("could not write {}: {err}", path.display());
    if let Some(directory) = path.parent() {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(directory)
            .map_err(failed)?;
    }
    let mut line = serde_json::to_vec(&record).map_err(|err| failed(err.into()))?;
    line.push(b'\n');
    let mut file = OpenOptions::new()
        .read(true)
        .append(true)
        .create(true)
        .mode(0o600)
        .custom_flags(OFlags::NOFOLLOW.bits().cast_signed())
        .open(path)
        .map_err(failed)?;
    // Two clients can register at once, so each append holds the file's
    // lock from its check of the tail to its write.
    file.lock().map_err(failed)?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).map_err(failed)?;
    let committed = committed_length(&bytes);
    if committed < bytes.len() {
        // An interrupted append left a part of a line. The next record
        // must start a line of its own, or the two would form one line
        // that no reader accepts.
        let length = u64::try_from(committed).unwrap_or(u64::MAX);
        file.set_len(length).map_err(failed)?;
    }
    file.write_all(&line)
        .and_then(|()| file.sync_data())
        .map_err(failed)?;
    Ok(record)
}

/// Reads the registry at `path`. A missing file holds no record. Bytes after
/// the last line feed remain from an append that did not finish, and the
/// reader sets them aside.
pub fn read(path: &Path) -> Result<Vec<Record>, String> {
    let mut bytes = Vec::new();
    let opened = OpenOptions::new()
        .read(true)
        .custom_flags(OFlags::NOFOLLOW.bits().cast_signed())
        .open(path);
    match opened {
        Ok(mut file) => file
            .read_to_end(&mut bytes)
            .map_err(|err| format!("could not read {}: {err}", path.display()))?,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(format!("could not read {}: {err}", path.display())),
    };
    let committed = committed_length(&bytes);
    bytes[..committed]
        .split_inclusive(|byte| *byte == b'\n')
        .enumerate()
        .map(|(index, line)| {
            serde_json::from_slice::<Record>(line)
                .ok()
                .filter(|record| record.schema == REGISTRY_SCHEMA)
                .ok_or_else(|| {
                    format!(
                        "line {} of {} is not a record that this release reads",
                        index + 1,
                        path.display()
                    )
                })
        })
        .collect()
}

/// The length of the complete lines at the start of `bytes`.
fn committed_length(bytes: &[u8]) -> usize {
    bytes
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |last| last + 1)
}

/// One client can release only its own registration for an attempt.
pub fn standings(records: &[Record]) -> Standings {
    // A release states no text, so it keeps that of its registration.
    let mut claims = BTreeMap::new();
    for (position, record) in records.iter().enumerate() {
        let attempt = record.attempt.as_deref();
        let key = (record.checkout.as_path(), record.client.as_str(), attempt);
        let claim = claims.entry(key).or_insert_with(|| Claim {
            checkout: record.checkout.clone(),
            client: record.client.clone(),
            attempt: record.attempt.clone(),
            event: record.event,
            at: record.at.clone(),
            ended: None,
            position,
            text: None,
            registered_at: None,
        });
        (claim.event, claim.at) = (record.event, record.at.clone());
        claim.ended = match record.event {
            Event::Registered => None,
            Event::Released => record.claim_ended,
        };
        claim.position = position;
        if record.event == Event::Registered {
            claim.text.clone_from(&record.claim);
            claim.registered_at = Some(record.at.clone());
        }
    }
    let mut seen = HashSet::new();
    let mut standings = Standings::new();
    for record in records.iter().rev() {
        let path = record.checkout.as_path();
        if !seen.insert((path, record.client.as_str(), record.attempt.as_deref())) {
            continue;
        }
        let ended = record.claim_ended.as_ref();
        let ended = ended.filter(|_| record.event == Event::Released);
        if let Some(current) = standings.get_mut(path)
            && current.claim_ended.as_ref() != ended
        {
            current.claim_ended = None;
        }
        let claim_ended = match standings.get(path) {
            Some(current) => current.claim_ended,
            None => ended.cloned(),
        };
        if let Some(current) = standings.get(path)
            && (current.event == Event::Registered || record.event == Event::Released)
        {
            continue;
        }
        standings.insert(
            path.to_path_buf(),
            Standing {
                event: record.event,
                client: record.client.clone(),
                attempt: record.attempt.clone(),
                at: record.at.clone(),
                landed: record.landed.clone(),
                claim_ended,
                claims: Vec::new(),
            },
        );
    }
    for ((path, _, _), claim) in claims {
        if let Some(standing) = standings.get_mut(path) {
            standing.claims.push(claim);
        }
    }
    standings
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use tempfile::TempDir;

    use super::{Event, append, holder, read, reading, standings};

    #[test]
    fn the_latest_record_of_a_checkout_decides_its_standing() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("state/registry.jsonl");
        assert!(read(&path).unwrap().is_empty());
        let (first, second) = (Path::new("/work/a"), Path::new("/work/b"));
        append(&path, first, "bosun", Some("wf-1"), Event::Registered, 10).unwrap();
        append(&path, second, "hook", None, Event::Registered, 20).unwrap();
        append(&path, first, "bosun", Some("wf-1"), Event::Released, 30).unwrap();

        let records = read(&path).unwrap();
        assert_eq!(records.len(), 3);
        let standings = standings(&records);
        assert_eq!(standings[first].event, Event::Released);
        assert_eq!(standings[first].attempt.as_deref(), Some("wf-1"));
        assert_eq!(standings[second].event, Event::Registered);
        assert_eq!(standings[second].client, "hook");
    }

    #[test]
    fn sets_aside_a_cut_line_and_refuses_a_link_and_a_damaged_line() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("registry.jsonl");
        append(
            &path,
            Path::new("/work/a"),
            "bosun",
            None,
            Event::Registered,
            10,
        )
        .unwrap();
        let mut bytes = fs::read(&path).unwrap();
        bytes.extend_from_slice(b"{\"schema\":");
        fs::write(&path, &bytes).unwrap();
        assert_eq!(read(&path).unwrap().len(), 1);

        // The next append removes the cut line, so every record stays
        // readable.
        append(
            &path,
            Path::new("/work/b"),
            "bosun",
            None,
            Event::Registered,
            20,
        )
        .unwrap();
        let records = read(&path).unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[1].checkout.as_path(), Path::new("/work/b"));

        fs::write(&path, b"{\"schema\":\"other\"}\n").unwrap();
        assert!(read(&path).is_err());

        let link = dir.path().join("link.jsonl");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(read(&link).is_err());
        assert!(append(&link, Path::new("/work/a"), "x", None, Event::Released, 1).is_err());
    }

    #[test]
    fn a_registration_holds_every_checkout_beneath_its_own() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("registry.jsonl");
        append(
            &path,
            Path::new("/work/repo"),
            "bosun",
            None,
            Event::Registered,
            10,
        )
        .unwrap();
        append(
            &path,
            Path::new("/work/old"),
            "bosun",
            None,
            Event::Released,
            20,
        )
        .unwrap();
        let standings = reading(&path).unwrap();
        let nested = Path::new("/work/repo/.worktrees/feature");
        assert_eq!(
            holder(&standings, nested).unwrap().0,
            Path::new("/work/repo")
        );
        assert!(holder(&standings, Path::new("/work/repository")).is_none());
        assert!(holder(&standings, Path::new("/work/old/x")).is_none());

        fs::write(&path, b"damaged\n").unwrap();
        assert!(reading(&path).is_err());
    }
}
