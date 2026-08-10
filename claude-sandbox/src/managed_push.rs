use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions, Permissions};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

const CONTAINER_WORKSPACE: &str = "/workspace";

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Repository {
    pub relative_path: String,
    pub origin: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Candidate {
    pub repository: Repository,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previously_approved_origin: Option<String>,
    pub requested_at: u64,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalScope {
    Once,
    Persistent,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Approval {
    pub repository: Repository,
    pub scope: ApprovalScope,
    pub approved_at: u64,
}

pub fn state_dir(home: &Path, instance: &str) -> PathBuf {
    home.join(".claude-sandbox/projects")
        .join(instance)
        .join("git-push")
}

pub fn prepare_state_dir(path: &Path) -> Result<PathBuf, String> {
    ensure_private_dir(path)?;
    fs::canonicalize(path).map_err(|e| format!("could not resolve {}: {}", path.display(), e))
}

fn ensure_private_dir(path: &Path) -> Result<(), String> {
    fs::create_dir_all(path).map_err(|e| format!("could not create {}: {}", path.display(), e))?;
    fs::set_permissions(path, Permissions::from_mode(0o700))
        .map_err(|e| format!("could not protect {}: {}", path.display(), e))
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
        path.file_name().and_then(|v| v.to_str()).unwrap_or("state"),
        std::process::id(),
        nonce
    ));
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(&temp)
        .map_err(|e| format!("could not create {}: {}", temp.display(), e))?;
    let bytes = serde_json::to_vec_pretty(value)
        .map_err(|e| format!("could not encode {}: {}", path.display(), e))?;
    if let Err(error) = file.write_all(&bytes).and_then(|_| file.sync_all()) {
        let _ = fs::remove_file(&temp);
        return Err(format!("could not write {}: {}", temp.display(), error));
    }
    fs::rename(&temp, path).map_err(|e| {
        let _ = fs::remove_file(&temp);
        format!("could not replace {}: {}", path.display(), e)
    })
}

fn stable_id(parts: &[&str]) -> String {
    let mut hash = 0xcbf29ce484222325_u64;
    for part in parts {
        for byte in part.as_bytes().iter().chain(std::iter::once(&0)) {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x100000001b3);
        }
    }
    format!("{hash:016x}")
}

pub fn candidate_id(repository: &Repository) -> String {
    stable_id(&[&repository.relative_path, &repository.origin])
}

pub fn approval_id(relative_path: &str) -> String {
    stable_id(&[relative_path])
}

fn state_file(directory: &Path, id: &str) -> Result<PathBuf, String> {
    if id.len() != 16 || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("invalid state identifier".to_string());
    }
    Ok(directory.join(format!("{id}.json")))
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T, String> {
    let contents =
        fs::read(path).map_err(|e| format!("could not read {}: {}", path.display(), e))?;
    serde_json::from_slice(&contents)
        .map_err(|e| format!("could not parse {}: {}", path.display(), e))
}

fn list_json<T: for<'de> Deserialize<'de>>(directory: &Path) -> Result<Vec<(String, T)>, String> {
    if !directory.exists() {
        return Ok(Vec::new());
    }
    let mut values = Vec::new();
    for entry in fs::read_dir(directory)
        .map_err(|e| format!("could not list {}: {}", directory.display(), e))?
    {
        let entry = entry.map_err(|e| format!("could not read state entry: {}", e))?;
        let path = entry.path();
        if path.extension().and_then(|v| v.to_str()) != Some("json") {
            continue;
        }
        let Some(id) = path.file_stem().and_then(|v| v.to_str()) else {
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
    repository: &Repository,
    previously_approved_origin: Option<String>,
) -> Result<String, String> {
    let id = candidate_id(repository);
    let path = state_file(&candidates_dir(state), &id)?;
    let candidate = Candidate {
        repository: repository.clone(),
        previously_approved_origin,
        requested_at: unix_time(),
    };
    atomic_write_json(&path, &candidate)?;
    Ok(id)
}

pub fn list_candidates(state: &Path) -> Result<Vec<(String, Candidate)>, String> {
    let mut candidates: Vec<(String, Candidate)> = list_json(&candidates_dir(state))?;
    candidates.sort_by(|left, right| {
        right
            .1
            .requested_at
            .cmp(&left.1.requested_at)
            .then_with(|| {
                left.1
                    .repository
                    .relative_path
                    .cmp(&right.1.repository.relative_path)
            })
    });
    Ok(candidates)
}

pub fn read_candidate(state: &Path, id: &str) -> Result<Candidate, String> {
    let candidate: Candidate = read_json(&state_file(&candidates_dir(state), id)?)?;
    if candidate_id(&candidate.repository) != id {
        return Err("candidate identifier does not match its repository".to_string());
    }
    Ok(candidate)
}

pub fn remove_candidate(state: &Path, id: &str) -> Result<(), String> {
    let path = state_file(&candidates_dir(state), id)?;
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("could not remove {}: {}", path.display(), error)),
    }
}

pub fn approve(state: &Path, repository: &Repository, scope: ApprovalScope) -> Result<(), String> {
    let id = approval_id(&repository.relative_path);
    let path = state_file(&approvals_dir(state), &id)?;
    atomic_write_json(
        &path,
        &Approval {
            repository: repository.clone(),
            scope,
            approved_at: unix_time(),
        },
    )
}

pub fn read_approval(state: &Path, relative_path: &str) -> Result<Option<Approval>, String> {
    let path = state_file(&approvals_dir(state), &approval_id(relative_path))?;
    match read_json::<Approval>(&path) {
        Ok(approval) if approval.repository.relative_path == relative_path => Ok(Some(approval)),
        Ok(_) => Err(format!(
            "approval identifier collision for repository {relative_path}"
        )),
        Err(_) if !path.exists() => Ok(None),
        Err(error) => Err(error),
    }
}

pub fn list_approvals(state: &Path) -> Result<Vec<(String, Approval)>, String> {
    let mut approvals: Vec<(String, Approval)> = list_json(&approvals_dir(state))?;
    approvals.sort_by(|left, right| {
        left.1
            .repository
            .relative_path
            .cmp(&right.1.repository.relative_path)
    });
    Ok(approvals)
}

pub fn revoke(state: &Path, relative_path: &str) -> Result<(), String> {
    let path = state_file(&approvals_dir(state), &approval_id(relative_path))?;
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("could not remove {}: {}", path.display(), error)),
    }
}

pub fn consume_once(state: &Path, approval: &Approval) -> Result<bool, String> {
    if approval.scope != ApprovalScope::Once {
        return Ok(true);
    }
    let path = state_file(
        &approvals_dir(state),
        &approval_id(&approval.repository.relative_path),
    )?;
    match fs::remove_file(&path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(format!("could not consume {}: {}", path.display(), error)),
    }
}

pub fn resolve_repository(
    workspace_root: &Path,
    container_cwd: &str,
) -> Result<(PathBuf, Repository), String> {
    let workspace_root = fs::canonicalize(workspace_root)
        .map_err(|e| format!("could not resolve workspace root: {}", e))?;
    let container_path = Path::new(container_cwd);
    let relative = container_path
        .strip_prefix(CONTAINER_WORKSPACE)
        .map_err(|_| format!("working directory must be inside {CONTAINER_WORKSPACE}"))?;
    if relative.components().any(|component| {
        matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    }) {
        return Err("working directory contains an invalid path component".to_string());
    }

    let candidate = fs::canonicalize(workspace_root.join(relative))
        .map_err(|e| format!("could not resolve working directory: {}", e))?;
    if !candidate.starts_with(&workspace_root) {
        return Err("working directory escapes the mounted workspace".to_string());
    }

    let output = Command::new("git")
        .args([
            "-C",
            candidate.to_str().ok_or("repository path is not UTF-8")?,
        ])
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .map_err(|e| format!("failed to locate repository: {}", e))?;
    if !output.status.success() {
        return Err("working directory is not in a git repository".to_string());
    }
    let top = String::from_utf8(output.stdout)
        .map_err(|_| "git returned a non-UTF-8 repository path".to_string())?;
    let repository_path = fs::canonicalize(top.trim())
        .map_err(|e| format!("could not resolve repository root: {}", e))?;
    if !repository_path.starts_with(&workspace_root) {
        return Err("repository root escapes the mounted workspace".to_string());
    }
    let relative_path = repository_path
        .strip_prefix(&workspace_root)
        .map_err(|_| "repository root escapes the mounted workspace".to_string())?
        .to_str()
        .ok_or("repository path is not UTF-8")?;
    let relative_path = if relative_path.is_empty() {
        ".".to_string()
    } else {
        relative_path.to_string()
    };
    let origin = origin_url_at(&repository_path)
        .ok_or_else(|| "repository does not have an 'origin' remote".to_string())?;
    let branch = git_stdout(
        &repository_path,
        &["symbolic-ref", "--quiet", "--short", "HEAD"],
    );

    Ok((
        repository_path,
        Repository {
            relative_path,
            origin,
            branch,
        },
    ))
}

pub fn resolve_relative_repository(
    workspace_root: &Path,
    relative_path: &str,
) -> Result<(PathBuf, Repository), String> {
    let container_path = if relative_path == "." {
        CONTAINER_WORKSPACE.to_string()
    } else {
        format!("{CONTAINER_WORKSPACE}/{relative_path}")
    };
    resolve_repository(workspace_root, &container_path)
}

pub fn origin_url_at(repository: &Path) -> Option<String> {
    git_stdout(repository, &["remote", "get-url", "origin"])
}

fn git_stdout(repository: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repository)
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8(output.stdout).ok()?.trim().to_string();
    if value.is_empty() { None } else { Some(value) }
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

    #[test]
    fn identifiers_are_stable_and_scoped() {
        let repo = Repository {
            relative_path: "project".to_string(),
            origin: "git@example.test:org/project.git".to_string(),
            branch: Some("main".to_string()),
        };
        assert_eq!(candidate_id(&repo), candidate_id(&repo));
        assert_ne!(candidate_id(&repo), approval_id(&repo.relative_path));
    }

    #[test]
    fn rejects_container_paths_outside_workspace() {
        let error = resolve_repository(Path::new("/"), "/etc").unwrap_err();
        assert!(error.contains("/workspace"));
    }

    #[test]
    fn validates_state_identifiers() {
        assert!(state_file(Path::new("/tmp"), "0123456789abcdef").is_ok());
        assert!(state_file(Path::new("/tmp"), "../bad").is_err());
    }
}
