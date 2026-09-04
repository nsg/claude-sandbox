use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions, Permissions};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::managed_push::ApprovalScope;

const MAX_PENDING_CANDIDATES: usize = 100;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Source {
    pub host: String,
    pub repository: String,
}

impl Source {
    pub fn display(&self) -> String {
        format!("git@{}:{}", self.host, self.repository)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Candidate {
    pub source: Source,
    pub requested_from: String,
    pub requested_at: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Approval {
    pub source: Source,
    pub scope: ApprovalScope,
    pub approved_at: u64,
}

pub fn state_dir(home: &Path, instance: &str) -> PathBuf {
    home.join(".claude-sandbox/projects")
        .join(instance)
        .join("git-fetch")
}

pub fn prepare_state_dir(path: &Path) -> Result<PathBuf, String> {
    ensure_private_dir(path)?;
    fs::canonicalize(path).map_err(|error| format!("could not resolve {}: {error}", path.display()))
}

fn ensure_private_dir(path: &Path) -> Result<(), String> {
    fs::create_dir_all(path)
        .map_err(|error| format!("could not create {}: {error}", path.display()))?;
    fs::set_permissions(path, Permissions::from_mode(0o700))
        .map_err(|error| format!("could not protect {}: {error}", path.display()))
}

fn atomic_write_json<T: Serialize>(path: &Path, value: &T) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", path.display()))?;
    ensure_private_dir(parent)?;
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let temp = parent.join(format!(
        ".{}.{}.{}.tmp",
        path.file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("state"),
        std::process::id(),
        nonce
    ));
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(&temp)
        .map_err(|error| format!("could not create {}: {error}", temp.display()))?;
    let bytes = serde_json::to_vec_pretty(value)
        .map_err(|error| format!("could not encode {}: {error}", path.display()))?;
    if let Err(error) = file.write_all(&bytes).and_then(|_| file.sync_all()) {
        let _ = fs::remove_file(&temp);
        return Err(format!("could not write {}: {error}", temp.display()));
    }
    fs::rename(&temp, path).map_err(|error| {
        let _ = fs::remove_file(&temp);
        format!("could not replace {}: {error}", path.display())
    })
}

fn stable_id(source: &Source) -> String {
    let mut hash = 0xcbf29ce484222325_u64;
    for part in [&source.host, &source.repository] {
        for byte in part.as_bytes().iter().chain(std::iter::once(&0)) {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x100000001b3);
        }
    }
    format!("{hash:016x}")
}

fn state_file(directory: &Path, id: &str) -> Result<PathBuf, String> {
    if id.len() != 16 || !id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("invalid state identifier".to_string());
    }
    Ok(directory.join(format!("{id}.json")))
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T, String> {
    let contents =
        fs::read(path).map_err(|error| format!("could not read {}: {error}", path.display()))?;
    serde_json::from_slice(&contents)
        .map_err(|error| format!("could not parse {}: {error}", path.display()))
}

fn list_json<T: for<'de> Deserialize<'de>>(directory: &Path) -> Result<Vec<(String, T)>, String> {
    if !directory.exists() {
        return Ok(Vec::new());
    }
    let mut values = Vec::new();
    for entry in fs::read_dir(directory)
        .map_err(|error| format!("could not list {}: {error}", directory.display()))?
    {
        let entry = entry.map_err(|error| format!("could not read state entry: {error}"))?;
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        let Some(id) = path.file_stem().and_then(|value| value.to_str()) else {
            continue;
        };
        if let Ok(value) = read_json(&path) {
            values.push((id.to_string(), value));
        }
    }
    Ok(values)
}

fn candidates_dir(state: &Path) -> PathBuf {
    state.join("pending")
}

fn approvals_dir(state: &Path) -> PathBuf {
    state.join("approved")
}

pub fn record_candidate(
    state: &Path,
    source: &Source,
    requested_from: &str,
) -> Result<String, String> {
    let id = stable_id(source);
    let path = state_file(&candidates_dir(state), &id)?;
    if !path.exists() && list_candidates(state)?.len() >= MAX_PENDING_CANDIDATES {
        return Err(
            "too many pending fetch approvals; dismiss an existing request first".to_string(),
        );
    }
    atomic_write_json(
        &path,
        &Candidate {
            source: source.clone(),
            requested_from: requested_from.to_string(),
            requested_at: unix_time(),
        },
    )?;
    Ok(id)
}

pub fn list_candidates(state: &Path) -> Result<Vec<(String, Candidate)>, String> {
    let mut candidates: Vec<(String, Candidate)> = list_json(&candidates_dir(state))?;
    candidates.sort_by_key(|candidate| std::cmp::Reverse(candidate.1.requested_at));
    Ok(candidates)
}

pub fn read_candidate(state: &Path, id: &str) -> Result<Candidate, String> {
    let candidate: Candidate = read_json(&state_file(&candidates_dir(state), id)?)?;
    if stable_id(&candidate.source) != id {
        return Err("candidate identifier does not match its source".to_string());
    }
    Ok(candidate)
}

pub fn remove_candidate(state: &Path, id: &str) -> Result<(), String> {
    remove_file(&state_file(&candidates_dir(state), id)?)
}

pub fn approve(state: &Path, source: &Source, scope: ApprovalScope) -> Result<(), String> {
    let path = state_file(&approvals_dir(state), &stable_id(source))?;
    atomic_write_json(
        &path,
        &Approval {
            source: source.clone(),
            scope,
            approved_at: unix_time(),
        },
    )
}

pub fn read_approval(state: &Path, source: &Source) -> Result<Option<Approval>, String> {
    let id = stable_id(source);
    let path = state_file(&approvals_dir(state), &id)?;
    match read_json::<Approval>(&path) {
        Ok(approval) if approval.source == *source => Ok(Some(approval)),
        Ok(_) => Err("approval identifier does not match its source".to_string()),
        Err(_) if !path.exists() => Ok(None),
        Err(error) => Err(error),
    }
}

pub fn list_approvals(state: &Path) -> Result<Vec<(String, Approval)>, String> {
    let mut approvals: Vec<(String, Approval)> = list_json(&approvals_dir(state))?;
    approvals.sort_by(|left, right| {
        left.1
            .source
            .host
            .cmp(&right.1.source.host)
            .then_with(|| left.1.source.repository.cmp(&right.1.source.repository))
    });
    Ok(approvals)
}

pub fn revoke(state: &Path, source: &Source) -> Result<(), String> {
    remove_file(&state_file(&approvals_dir(state), &stable_id(source))?)
}

pub fn consume_once(state: &Path, approval: &Approval) -> Result<bool, String> {
    if approval.scope != ApprovalScope::Once {
        return Ok(true);
    }
    let path = state_file(&approvals_dir(state), &stable_id(&approval.source))?;
    match fs::remove_file(&path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(format!("could not consume {}: {error}", path.display())),
    }
}

fn remove_file(path: &Path) -> Result<(), String> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("could not remove {}: {error}", path.display())),
    }
}

fn unix_time() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_root(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "claude-sandbox-managed-fetch-{label}-{}-{}",
            std::process::id(),
            unix_time()
        ))
    }

    fn source(name: &str) -> Source {
        Source {
            host: "github.com".to_string(),
            repository: format!("org/{name}.git"),
        }
    }

    #[test]
    fn approval_is_exact_and_one_time_is_atomic() {
        let root = test_root("approval");
        let state = prepare_state_dir(&root).unwrap();
        let approved = source("project");
        approve(&state, &approved, ApprovalScope::Once).unwrap();

        let approval = read_approval(&state, &approved).unwrap().unwrap();
        assert_eq!(
            fs::metadata(&state).unwrap().permissions().mode() & 0o777,
            0o700
        );
        let approval_file = fs::read_dir(state.join("approved"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        assert_eq!(
            fs::metadata(approval_file).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(consume_once(&state, &approval).unwrap());
        assert!(!consume_once(&state, &approval).unwrap());
        assert!(read_approval(&state, &source("other")).unwrap().is_none());

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn concurrent_one_time_consumers_cannot_both_succeed() {
        let root = test_root("concurrent");
        let state = prepare_state_dir(&root).unwrap();
        let approved = source("project");
        approve(&state, &approved, ApprovalScope::Once).unwrap();
        let approval = read_approval(&state, &approved).unwrap().unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));

        let handles: Vec<_> = (0..2)
            .map(|_| {
                let state = state.clone();
                let approval = approval.clone();
                let barrier = std::sync::Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    consume_once(&state, &approval).unwrap()
                })
            })
            .collect();
        barrier.wait();
        let successes = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .filter(|success| *success)
            .count();

        assert_eq!(successes, 1);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn candidate_records_request_context_without_scoping_approval_to_it() {
        let root = test_root("candidate");
        let state = prepare_state_dir(&root).unwrap();
        let requested = source("dependency");
        let id = record_candidate(&state, &requested, "project/src").unwrap();
        let candidate = read_candidate(&state, &id).unwrap();

        assert_eq!(candidate.source, requested);
        assert_eq!(candidate.requested_from, "project/src");
        approve(&state, &candidate.source, ApprovalScope::Persistent).unwrap();
        remove_candidate(&state, &id).unwrap();
        assert!(read_approval(&state, &candidate.source).unwrap().is_some());

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn pending_candidates_are_bounded_and_deduplicated() {
        let root = test_root("bounded");
        let state = prepare_state_dir(&root).unwrap();
        for index in 0..MAX_PENDING_CANDIDATES {
            record_candidate(&state, &source(&index.to_string()), "project").unwrap();
        }
        record_candidate(&state, &source("0"), "another/path").unwrap();
        assert_eq!(
            list_candidates(&state).unwrap().len(),
            MAX_PENDING_CANDIDATES
        );
        assert!(record_candidate(&state, &source("overflow"), "project").is_err());

        fs::remove_dir_all(root).unwrap();
    }
}
