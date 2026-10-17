use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions, Permissions};
use std::io::Write;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_PENDING_CANDIDATES: usize = 100;
const PENDING_TTL: u64 = 24 * 60 * 60;
const APPROVAL_TTL: u64 = 15 * 60;
const CLAIM_TTL: u64 = 60;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Repository {
    pub host: String,
    pub owner: String,
    pub name: String,
}

impl Repository {
    pub fn selector(&self) -> String {
        format!("{}/{}/{}", self.host, self.owner, self.name)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ItemKind {
    Issue,
    PullRequest,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Item {
    pub number: u32,
    pub kind: ItemKind,
    pub node_id: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Action {
    pub repository: Repository,
    pub repository_id: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub item: Option<Item>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_sha: Option<String>,
    pub args: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Context {
    pub repository_visibility: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub item_title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub item_state: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commits: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub changed_files: Option<u64>,
    pub requested_from: String,
    pub requested_at: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Candidate {
    pub action: Action,
    pub context: Context,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Approval {
    pub action: Action,
    pub context: Context,
    pub approved_at: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GrantScope {
    All,
    Commands(Vec<String>),
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Grant {
    pub repository: Repository,
    pub repository_id: u64,
    pub visibility: String,
    pub scope: GrantScope,
    pub granted_at: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RequestedGrantScope {
    Command,
    Repository,
}

pub struct CommandKind {
    pub command: &'static str,
    pub phrase: &'static str,
}

pub const COMMAND_KINDS: &[CommandKind] = &[
    CommandKind {
        command: "issue create",
        phrase: "issue creation",
    },
    CommandKind {
        command: "issue comment",
        phrase: "issue comments",
    },
    CommandKind {
        command: "issue edit",
        phrase: "issue edits",
    },
    CommandKind {
        command: "issue close",
        phrase: "closing issues",
    },
    CommandKind {
        command: "issue reopen",
        phrase: "reopening issues",
    },
    CommandKind {
        command: "pr create",
        phrase: "pull request creation",
    },
    CommandKind {
        command: "pr comment",
        phrase: "pull request comments",
    },
    CommandKind {
        command: "pr edit",
        phrase: "pull request edits",
    },
    CommandKind {
        command: "pr ready",
        phrase: "pull request ready/draft changes",
    },
    CommandKind {
        command: "pr close",
        phrase: "closing pull requests",
    },
    CommandKind {
        command: "pr reopen",
        phrase: "reopening pull requests",
    },
];

pub fn state_dir(home: &Path, instance: &str) -> PathBuf {
    home.join(".claude-sandbox/projects")
        .join(instance)
        .join("gh-actions")
}

pub fn prepare_state_dir(path: &Path) -> Result<PathBuf, String> {
    ensure_private_dir(path)?;
    ensure_private_dir(&pending_dir(path))?;
    ensure_private_dir(&approved_dir(path))?;
    ensure_private_dir(&handles_dir(path))?;
    ensure_private_dir(&grants_dir(path))?;
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
    let nonce = crate::git_proxy::random_hex_bytes(16)?;
    let temp = parent.join(format!(
        ".{}.{}.tmp",
        path.file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("state"),
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

fn pending_dir(state: &Path) -> PathBuf {
    state.join("pending")
}

fn approved_dir(state: &Path) -> PathBuf {
    state.join("approved")
}

fn handles_dir(state: &Path) -> PathBuf {
    state.join("handles")
}

fn grants_dir(state: &Path) -> PathBuf {
    state.join("grants")
}

fn state_file(directory: &Path, handle: &str) -> Result<PathBuf, String> {
    if handle.len() != 16 || !handle.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("invalid state identifier".to_string());
    }
    Ok(directory.join(format!("{handle}.json")))
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
        let Some(handle) = path.file_stem().and_then(|value| value.to_str()) else {
            continue;
        };
        if state_file(directory, handle).is_ok()
            && let Ok(value) = read_json(&path)
        {
            values.push((handle.to_string(), value));
        }
    }
    Ok(values)
}

fn list_claims(state: &Path) -> Result<Vec<(String, Candidate)>, String> {
    list_claims_at(state, unix_time(), CLAIM_TTL)
}

fn list_claims_at(
    state: &Path,
    now: u64,
    claim_ttl: u64,
) -> Result<Vec<(String, Candidate)>, String> {
    let directory = pending_dir(state);
    let mut values = Vec::new();
    for entry in fs::read_dir(&directory)
        .map_err(|error| format!("could not list {}: {error}", directory.display()))?
    {
        let entry = entry.map_err(|error| format!("could not read state entry: {error}"))?;
        let path = entry.path();
        let Some(handle) = path
            .file_name()
            .and_then(|value| value.to_str())
            .and_then(|value| value.strip_prefix('.'))
            .and_then(|value| value.strip_suffix(".claim"))
        else {
            continue;
        };
        let ctime = entry
            .metadata()
            .ok()
            .and_then(|metadata| u64::try_from(metadata.ctime()).ok());
        if ctime.is_some_and(|ctime| now.saturating_sub(ctime) > claim_ttl) {
            continue;
        }
        if ctime.is_some()
            && state_file(&directory, handle).is_ok()
            && let Ok(value) = read_json(&path)
        {
            values.push((handle.to_string(), value));
        }
    }
    Ok(values)
}

fn remove_file(path: &Path) -> Result<bool, String> {
    match fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(format!("could not remove {}: {error}", path.display())),
    }
}

fn prune_pending(state: &Path, now: u64) -> Result<(), String> {
    prune_pending_at(state, now, CLAIM_TTL)
}

fn prune_pending_at(state: &Path, now: u64, claim_ttl: u64) -> Result<(), String> {
    for (handle, candidate) in list_json::<Candidate>(&pending_dir(state))? {
        if now.saturating_sub(candidate.context.requested_at) >= PENDING_TTL {
            remove_file(&state_file(&pending_dir(state), &handle)?)?;
        }
    }
    let directory = pending_dir(state);
    for entry in fs::read_dir(&directory)
        .map_err(|error| format!("could not list {}: {error}", directory.display()))?
    {
        let entry = entry.map_err(|error| format!("could not read state entry: {error}"))?;
        let path = entry.path();
        let valid_claim = path
            .file_name()
            .and_then(|value| value.to_str())
            .and_then(|value| value.strip_prefix('.'))
            .and_then(|value| value.strip_suffix(".claim"))
            .is_some_and(|handle| state_file(&directory, handle).is_ok());
        let abandoned = entry
            .metadata()
            .ok()
            .and_then(|metadata| u64::try_from(metadata.ctime()).ok())
            .is_some_and(|ctime| now.saturating_sub(ctime) > claim_ttl);
        if valid_claim && abandoned {
            remove_file(&path)?;
        }
    }
    Ok(())
}

fn reserve_handle(state: &Path) -> Result<String, String> {
    for _ in 0..32 {
        let handle = crate::git_proxy::random_hex_bytes(8)?;
        let reservation = handles_dir(state).join(&handle);
        match OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&reservation)
        {
            Ok(file) => {
                file.sync_all()
                    .map_err(|error| format!("could not reserve approval identifier: {error}"))?;
                return Ok(handle);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(format!("could not reserve approval identifier: {error}")),
        }
    }
    Err("could not allocate a unique approval identifier".to_string())
}

pub fn record_candidate(
    state: &Path,
    action: &Action,
    context: &Context,
) -> Result<String, String> {
    let now = unix_time();
    prune_pending(state, now)?;
    let candidates = list_candidates(state)?;
    if let Some((handle, _)) = candidates
        .iter()
        .find(|(_, candidate)| candidate.action == *action)
    {
        return Ok(handle.clone());
    }
    if let Some((handle, _)) = list_claims(state)?
        .iter()
        .find(|(_, candidate)| candidate.action == *action)
    {
        return Ok(handle.clone());
    }
    if candidates.len() >= MAX_PENDING_CANDIDATES {
        return Err(
            "too many pending GitHub action approvals; dismiss an existing request first"
                .to_string(),
        );
    }
    let handle = reserve_handle(state)?;
    atomic_write_json(
        &state_file(&pending_dir(state), &handle)?,
        &Candidate {
            action: action.clone(),
            context: context.clone(),
        },
    )?;
    Ok(handle)
}

pub fn list_candidates(state: &Path) -> Result<Vec<(String, Candidate)>, String> {
    list_candidates_at(state, unix_time())
}

fn list_candidates_at(state: &Path, now: u64) -> Result<Vec<(String, Candidate)>, String> {
    let mut values: Vec<(String, Candidate)> = list_json(&pending_dir(state))?;
    values
        .retain(|(_, candidate)| now.saturating_sub(candidate.context.requested_at) < PENDING_TTL);
    values.sort_by_key(|value| std::cmp::Reverse(value.1.context.requested_at));
    Ok(values)
}

pub fn approve(state: &Path, handle: &str) -> Result<(), String> {
    let pending = state_file(&pending_dir(state), handle)?;
    let claim = pending_dir(state).join(format!(".{handle}.claim"));
    match fs::rename(&pending, &claim) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err("GitHub action is no longer pending".to_string());
        }
        Err(error) => return Err(format!("could not claim {}: {error}", pending.display())),
    }
    let candidate: Candidate = match read_json(&claim) {
        Ok(candidate) => candidate,
        Err(error) => return restore_claim(&claim, &pending, error),
    };
    if unix_time().saturating_sub(candidate.context.requested_at) >= PENDING_TTL {
        let _ = fs::remove_file(&claim);
        return Err("GitHub action is no longer pending".to_string());
    }
    let approval = Approval {
        action: candidate.action,
        context: candidate.context,
        approved_at: unix_time(),
    };
    let result = state_file(&approved_dir(state), handle)
        .and_then(|path| atomic_write_json(&path, &approval));
    if result.is_ok() {
        let _ = fs::remove_file(&claim);
    } else if let Err(error) = result {
        return restore_claim(&claim, &pending, error);
    }
    Ok(())
}

fn restore_claim<T>(claim: &Path, pending: &Path, error: String) -> Result<T, String> {
    fs::rename(claim, pending).map_err(|restore_error| {
        format!(
            "{error}; could not restore {} to {}: {restore_error}",
            claim.display(),
            pending.display()
        )
    })?;
    Err(error)
}

pub fn dismiss(state: &Path, handle: &str) -> Result<bool, String> {
    remove_file(&state_file(&pending_dir(state), handle)?)
}

pub fn list_approvals(state: &Path) -> Result<Vec<(String, Approval)>, String> {
    let now = unix_time();
    let mut values: Vec<(String, Approval)> = list_json(&approved_dir(state))?;
    values.retain(|(handle, approval)| {
        if now.saturating_sub(approval.approved_at) < APPROVAL_TTL {
            true
        } else {
            let _ = remove_file(&state_file(&approved_dir(state), handle).unwrap());
            false
        }
    });
    values.sort_by_key(|value| std::cmp::Reverse(value.1.approved_at));
    Ok(values)
}

pub fn consume(state: &Path, action: &Action) -> Result<bool, String> {
    consume_with_clock(state, action, unix_time)
}

fn consume_with_clock(
    state: &Path,
    action: &Action,
    mut clock: impl FnMut() -> u64,
) -> Result<bool, String> {
    let listed_at = clock();
    for (handle, approval) in list_json::<Approval>(&approved_dir(state))? {
        let path = state_file(&approved_dir(state), &handle)?;
        if listed_at.saturating_sub(approval.approved_at) >= APPROVAL_TTL {
            remove_file(&path)?;
            continue;
        }
        if approval.action == *action && remove_file(&path)? {
            return Ok(clock().saturating_sub(approval.approved_at) < APPROVAL_TTL);
        }
    }
    Ok(false)
}

pub fn revoke(state: &Path, handle: &str) -> Result<bool, String> {
    remove_file(&state_file(&approved_dir(state), handle)?)
}

pub fn command_kind(action: &Action) -> Option<&'static CommandKind> {
    let command = format!("{} {}", action.args.first()?, action.args.get(1)?);
    COMMAND_KINDS.iter().find(|kind| kind.command == command)
}

fn grant_covers_action(grant: &Grant, action: &Action, visibility: &str) -> bool {
    grant.repository == action.repository
        && grant.repository_id == action.repository_id
        && grant.visibility == visibility
        && command_kind(action).is_some_and(|kind| match &grant.scope {
            GrantScope::All => true,
            GrantScope::Commands(commands) => {
                commands.iter().any(|command| command == kind.command)
            }
        })
}

pub fn is_granted(state: &Path, action: &Action, visibility: &str) -> Result<bool, String> {
    Ok(list_grants(state)?
        .iter()
        .any(|(_, grant)| grant_covers_action(grant, action, visibility)))
}

pub fn list_grants(state: &Path) -> Result<Vec<(String, Grant)>, String> {
    let mut values: Vec<(String, Grant)> = list_json(&grants_dir(state))?;
    values.sort_by(|left, right| {
        left.1
            .repository
            .selector()
            .cmp(&right.1.repository.selector())
    });
    Ok(values)
}

pub fn remove_covered_candidates(state: &Path) -> Result<(), String> {
    let grants = list_grants(state)?;
    for (handle, candidate) in list_json::<Candidate>(&pending_dir(state))? {
        if grants.iter().any(|(_, grant)| {
            grant_covers_action(
                grant,
                &candidate.action,
                &candidate.context.repository_visibility,
            )
        }) {
            remove_file(&state_file(&pending_dir(state), &handle)?)?;
        }
    }
    Ok(())
}

fn extend_scope(scope: &GrantScope, command: &str) -> GrantScope {
    match scope {
        GrantScope::All => GrantScope::All,
        GrantScope::Commands(commands) => {
            let mut commands = commands.clone();
            if !commands.iter().any(|existing| existing == command) {
                commands.push(command.to_string());
            }
            commands.sort_by_key(|value| {
                COMMAND_KINDS
                    .iter()
                    .position(|kind| kind.command == value)
                    .unwrap_or(usize::MAX)
            });
            GrantScope::Commands(commands)
        }
    }
}

pub fn grant_candidate(
    state: &Path,
    handle: &str,
    requested_scope: RequestedGrantScope,
) -> Result<String, String> {
    let pending = state_file(&pending_dir(state), handle)?;
    let claim = pending_dir(state).join(format!(".{handle}.claim"));
    match fs::rename(&pending, &claim) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err("GitHub action is no longer pending".to_string());
        }
        Err(error) => return Err(format!("could not claim {}: {error}", pending.display())),
    }
    let candidate: Candidate = match read_json(&claim) {
        Ok(candidate) => candidate,
        Err(error) => return restore_claim(&claim, &pending, error),
    };
    if unix_time().saturating_sub(candidate.context.requested_at) >= PENDING_TTL {
        let _ = fs::remove_file(&claim);
        return Err("GitHub action is no longer pending".to_string());
    }
    let Some(kind) = command_kind(&candidate.action) else {
        return restore_claim(
            &claim,
            &pending,
            "GitHub action cannot be granted".to_string(),
        );
    };
    let repository = Repository {
        host: candidate.action.repository.host.to_ascii_lowercase(),
        owner: candidate.action.repository.owner.to_ascii_lowercase(),
        name: candidate.action.repository.name.to_ascii_lowercase(),
    };
    let existing = match list_grants(state) {
        Ok(grants) => grants,
        Err(error) => return restore_claim(&claim, &pending, error),
    }
    .into_iter()
    .find(|(_, grant)| grant.repository == repository);
    let (grant_handle, existing_scope) = match existing {
        Some((grant_handle, grant)) => {
            let unchanged = grant.repository_id == candidate.action.repository_id
                && grant.visibility == candidate.context.repository_visibility;
            (grant_handle, unchanged.then_some(grant.scope))
        }
        None => match reserve_handle(state) {
            Ok(handle) => (handle, None),
            Err(error) => return restore_claim(&claim, &pending, error),
        },
    };
    let scope = match requested_scope {
        RequestedGrantScope::Repository => GrantScope::All,
        RequestedGrantScope::Command => extend_scope(
            existing_scope
                .as_ref()
                .unwrap_or(&GrantScope::Commands(Vec::new())),
            kind.command,
        ),
    };
    let grant = Grant {
        repository,
        repository_id: candidate.action.repository_id,
        visibility: candidate.context.repository_visibility.clone(),
        scope,
        granted_at: unix_time(),
    };
    let result = state_file(&grants_dir(state), &grant_handle)
        .and_then(|path| atomic_write_json(&path, &grant));
    if let Err(error) = result {
        return restore_claim(&claim, &pending, error);
    }
    let _ = fs::remove_file(&claim);
    remove_covered_candidates(state)?;
    Ok(grant_handle)
}

pub fn revoke_grant(state: &Path, handle: &str) -> Result<bool, String> {
    remove_file(&state_file(&grants_dir(state), handle)?)
}

pub fn remaining_seconds(approval: &Approval) -> u64 {
    APPROVAL_TTL.saturating_sub(unix_time().saturating_sub(approval.approved_at))
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
    use std::sync::{Arc, Barrier};
    use std::thread;

    fn root(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "claude-sandbox-gh-approvals-{label}-{}-{}",
            std::process::id(),
            crate::git_proxy::random_hex_bytes(8).unwrap()
        ))
    }

    fn action(repository_id: u64, node_id: &str) -> Action {
        Action {
            repository: Repository {
                host: "github.com".to_string(),
                owner: "example".to_string(),
                name: "project".to_string(),
            },
            repository_id,
            item: Some(Item {
                number: 7,
                kind: ItemKind::Issue,
                node_id: node_id.to_string(),
            }),
            head_sha: None,
            args: vec![
                "issue".to_string(),
                "comment".to_string(),
                "7".to_string(),
                "--body=hello".to_string(),
            ],
        }
    }

    fn context() -> Context {
        Context {
            repository_visibility: "private".to_string(),
            item_title: Some("Title".to_string()),
            item_state: Some("open".to_string()),
            commits: None,
            changed_files: None,
            requested_from: ".".to_string(),
            requested_at: unix_time(),
        }
    }

    #[test]
    fn approval_matches_the_identical_action_only() {
        let root = root("identity");
        let state = prepare_state_dir(&root).unwrap();
        let expected = action(1, "I_1");
        let handle = record_candidate(&state, &expected, &context()).unwrap();
        approve(&state, &handle).unwrap();
        assert!(!consume(&state, &action(2, "I_1")).unwrap());
        assert!(!consume(&state, &action(1, "I_2")).unwrap());
        assert!(consume(&state, &expected).unwrap());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn one_time_consumption_has_one_concurrent_winner() {
        let root = root("consume");
        let state = prepare_state_dir(&root).unwrap();
        let expected = action(1, "I_1");
        let handle = record_candidate(&state, &expected, &context()).unwrap();
        approve(&state, &handle).unwrap();
        let state = Arc::new(state);
        let barrier = Arc::new(Barrier::new(3));
        let threads: Vec<_> = (0..2)
            .map(|_| {
                let state = Arc::clone(&state);
                let barrier = Arc::clone(&barrier);
                let expected = expected.clone();
                thread::spawn(move || {
                    barrier.wait();
                    consume(&state, &expected).unwrap()
                })
            })
            .collect();
        barrier.wait();
        assert_eq!(
            threads
                .into_iter()
                .map(|thread| thread.join().unwrap())
                .filter(|won| *won)
                .count(),
            1
        );
        assert!(list_approvals(&state).unwrap().is_empty());
        fs::remove_dir_all(state.as_path()).unwrap();
    }

    #[test]
    fn concurrent_approvals_have_one_winner() {
        let root = root("approve");
        let state = prepare_state_dir(&root).unwrap();
        let handle = record_candidate(&state, &action(1, "I_1"), &context()).unwrap();
        let state = Arc::new(state);
        let threads: Vec<_> = (0..2)
            .map(|_| {
                let state = Arc::clone(&state);
                let handle = handle.clone();
                thread::spawn(move || approve(&state, &handle).is_ok())
            })
            .collect();
        assert_eq!(
            threads
                .into_iter()
                .map(|thread| thread.join().unwrap())
                .filter(|won| *won)
                .count(),
            1
        );
        assert_eq!(list_approvals(&state).unwrap().len(), 1);
        fs::remove_dir_all(state.as_path()).unwrap();
    }

    #[test]
    fn consume_removes_expired_approvals() {
        let root = root("expired");
        let state = prepare_state_dir(&root).unwrap();
        let expected = action(1, "I_1");
        let handle = record_candidate(&state, &expected, &context()).unwrap();
        approve(&state, &handle).unwrap();
        let path = state_file(&approved_dir(&state), &handle).unwrap();
        let mut approval: Approval = read_json(&path).unwrap();
        approval.approved_at = unix_time().saturating_sub(APPROVAL_TTL);
        atomic_write_json(&path, &approval).unwrap();
        assert!(!consume(&state, &expected).unwrap());
        assert!(!path.exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn approval_expiring_after_listing_is_not_consumed() {
        let root = root("expiry-race");
        let state = prepare_state_dir(&root).unwrap();
        let expected = action(1, "I_1");
        let handle = record_candidate(&state, &expected, &context()).unwrap();
        approve(&state, &handle).unwrap();
        let path = state_file(&approved_dir(&state), &handle).unwrap();
        let mut approval: Approval = read_json(&path).unwrap();
        approval.approved_at = 1_000;
        atomic_write_json(&path, &approval).unwrap();
        let mut times = [1_000 + APPROVAL_TTL - 1, 1_000 + APPROVAL_TTL].into_iter();
        assert!(!consume_with_clock(&state, &expected, || times.next().unwrap()).unwrap());
        assert!(!path.exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn expired_pending_candidates_cannot_be_listed_or_approved() {
        let root = root("expired-pending");
        let state = prepare_state_dir(&root).unwrap();
        let mut old_context = context();
        old_context.requested_at = unix_time().saturating_sub(PENDING_TTL);
        let handle = record_candidate(&state, &action(1, "I_1"), &old_context).unwrap();
        assert!(list_candidates(&state).unwrap().is_empty());
        assert_eq!(
            approve(&state, &handle).unwrap_err(),
            "GitHub action is no longer pending"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn pending_candidates_are_capped() {
        let root = root("cap");
        let state = prepare_state_dir(&root).unwrap();
        for id in 0..MAX_PENDING_CANDIDATES {
            record_candidate(&state, &action(id as u64, &format!("I_{id}")), &context()).unwrap();
        }
        assert!(record_candidate(&state, &action(101, "overflow"), &context()).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn identical_pending_action_reuses_handle() {
        let root = root("same");
        let state = prepare_state_dir(&root).unwrap();
        let expected = action(1, "I_1");
        let first = record_candidate(&state, &expected, &context()).unwrap();
        let second = record_candidate(&state, &expected, &context()).unwrap();
        assert_eq!(first, second);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn identical_claimed_action_reuses_handle() {
        let root = root("same-claim");
        let state = prepare_state_dir(&root).unwrap();
        let expected = action(1, "I_1");
        let handle = record_candidate(&state, &expected, &context()).unwrap();
        let pending = state_file(&pending_dir(&state), &handle).unwrap();
        let claim = pending_dir(&state).join(format!(".{handle}.claim"));
        fs::rename(&pending, &claim).unwrap();

        assert_eq!(
            record_candidate(&state, &expected, &context()).unwrap(),
            handle
        );
        assert!(claim.exists());
        assert!(!pending.exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn abandoned_claims_are_ignored_and_pruned() {
        let root = root("abandoned-claim");
        let state = prepare_state_dir(&root).unwrap();
        let expected = action(1, "I_1");
        let handle = record_candidate(&state, &expected, &context()).unwrap();
        let pending = state_file(&pending_dir(&state), &handle).unwrap();
        let claim = pending_dir(&state).join(format!(".{handle}.claim"));
        fs::rename(&pending, &claim).unwrap();
        let ctime = u64::try_from(fs::metadata(&claim).unwrap().ctime()).unwrap();
        let abandoned_at = ctime + CLAIM_TTL + 1;

        assert!(
            list_claims_at(&state, abandoned_at, CLAIM_TTL)
                .unwrap()
                .is_empty()
        );
        prune_pending_at(&state, abandoned_at, CLAIM_TTL).unwrap();
        assert!(!claim.exists());
        assert_ne!(
            record_candidate(&state, &expected, &context()).unwrap(),
            handle
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn claims_are_restored_when_prepublication_operations_fail() {
        let root = root("claim-restore");
        let state = prepare_state_dir(&root).unwrap();

        let approve_handle = record_candidate(&state, &action(1, "I_1"), &context()).unwrap();
        let approve_pending = state_file(&pending_dir(&state), &approve_handle).unwrap();
        fs::write(&approve_pending, b"{").unwrap();
        assert!(approve(&state, &approve_handle).is_err());
        assert!(approve_pending.exists());
        assert!(
            !pending_dir(&state)
                .join(format!(".{approve_handle}.claim"))
                .exists()
        );

        let read_handle = record_candidate(&state, &action(2, "I_2"), &context()).unwrap();
        let read_pending = state_file(&pending_dir(&state), &read_handle).unwrap();
        fs::write(&read_pending, b"{").unwrap();
        assert!(grant_candidate(&state, &read_handle, RequestedGrantScope::Command).is_err());
        assert!(read_pending.exists());
        assert!(
            !pending_dir(&state)
                .join(format!(".{read_handle}.claim"))
                .exists()
        );

        let reserve_handle = record_candidate(&state, &action(3, "I_3"), &context()).unwrap();
        let reserve_pending = state_file(&pending_dir(&state), &reserve_handle).unwrap();
        fs::rename(handles_dir(&state), state.join("handles-backup")).unwrap();
        fs::write(handles_dir(&state), b"not a directory").unwrap();
        assert!(
            grant_candidate(&state, &reserve_handle, RequestedGrantScope::Command)
                .unwrap_err()
                .contains("could not reserve approval identifier")
        );
        assert!(reserve_pending.exists());
        assert!(
            !pending_dir(&state)
                .join(format!(".{reserve_handle}.claim"))
                .exists()
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn grant_matches_only_repository_identity_visibility_and_scope() {
        let root = root("grant-identity");
        let state = prepare_state_dir(&root).unwrap();
        let expected = action(1, "I_1");
        let handle = record_candidate(&state, &expected, &context()).unwrap();
        grant_candidate(&state, &handle, RequestedGrantScope::Command).unwrap();
        assert!(is_granted(&state, &expected, "private").unwrap());

        let mut different_repository = expected.clone();
        different_repository.repository.name = "other".to_string();
        assert!(!is_granted(&state, &different_repository, "private").unwrap());
        assert!(!is_granted(&state, &action(2, "I_1"), "private").unwrap());
        assert!(!is_granted(&state, &expected, "public").unwrap());
        let mut different_command = expected.clone();
        different_command.args = vec![
            "issue".to_string(),
            "edit".to_string(),
            "7".to_string(),
            "--title=changed".to_string(),
        ];
        assert!(!is_granted(&state, &different_command, "private").unwrap());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn command_grants_extend_and_repository_scope_replaces_them() {
        let root = root("grant-scope");
        let state = prepare_state_dir(&root).unwrap();
        let comment = action(1, "I_1");
        let mut edit = comment.clone();
        edit.args = vec![
            "issue".to_string(),
            "edit".to_string(),
            "7".to_string(),
            "--title=changed".to_string(),
        ];
        let first = record_candidate(&state, &comment, &context()).unwrap();
        let grant_handle = grant_candidate(&state, &first, RequestedGrantScope::Command).unwrap();
        let second = record_candidate(&state, &edit, &context()).unwrap();
        assert_eq!(
            grant_candidate(&state, &second, RequestedGrantScope::Command).unwrap(),
            grant_handle
        );
        assert!(is_granted(&state, &comment, "private").unwrap());
        assert!(is_granted(&state, &edit, "private").unwrap());
        let third = record_candidate(&state, &comment, &context()).unwrap();
        grant_candidate(&state, &third, RequestedGrantScope::Repository).unwrap();
        assert_eq!(list_grants(&state).unwrap()[0].1.scope, GrantScope::All);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn regrant_replaces_identity_and_visibility() {
        let root = root("grant-replace");
        let state = prepare_state_dir(&root).unwrap();
        let old = action(1, "I_1");
        let first = record_candidate(&state, &old, &context()).unwrap();
        grant_candidate(&state, &first, RequestedGrantScope::Command).unwrap();

        let current = action(2, "I_2");
        assert!(!is_granted(&state, &current, "private").unwrap());
        let second = record_candidate(&state, &current, &context()).unwrap();
        grant_candidate(&state, &second, RequestedGrantScope::Command).unwrap();
        assert!(!is_granted(&state, &old, "private").unwrap());
        assert!(is_granted(&state, &current, "private").unwrap());

        assert!(!is_granted(&state, &current, "public").unwrap());
        let mut public_context = context();
        public_context.repository_visibility = "public".to_string();
        let third = record_candidate(&state, &current, &public_context).unwrap();
        grant_candidate(&state, &third, RequestedGrantScope::Command).unwrap();
        assert!(!is_granted(&state, &current, "private").unwrap());
        assert!(is_granted(&state, &current, "public").unwrap());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn command_regrant_resets_all_scope_when_repository_id_changes() {
        let root = root("grant-id-scope-reset");
        let state = prepare_state_dir(&root).unwrap();
        let first = record_candidate(&state, &action(1, "I_1"), &context()).unwrap();
        grant_candidate(&state, &first, RequestedGrantScope::Repository).unwrap();

        let current = action(2, "I_2");
        let second = record_candidate(&state, &current, &context()).unwrap();
        grant_candidate(&state, &second, RequestedGrantScope::Command).unwrap();

        assert_eq!(
            list_grants(&state).unwrap()[0].1.scope,
            GrantScope::Commands(vec!["issue comment".to_string()])
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn command_regrant_resets_all_scope_when_visibility_changes() {
        let root = root("grant-visibility-scope-reset");
        let state = prepare_state_dir(&root).unwrap();
        let expected = action(1, "I_1");
        let first = record_candidate(&state, &expected, &context()).unwrap();
        grant_candidate(&state, &first, RequestedGrantScope::Repository).unwrap();

        let mut public_context = context();
        public_context.repository_visibility = "public".to_string();
        let second = record_candidate(&state, &expected, &public_context).unwrap();
        grant_candidate(&state, &second, RequestedGrantScope::Command).unwrap();

        assert_eq!(
            list_grants(&state).unwrap()[0].1.scope,
            GrantScope::Commands(vec!["issue comment".to_string()])
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn grants_can_be_revoked_and_one_candidate_can_only_be_claimed_once() {
        let root = root("grant-revoke-claim");
        let state = prepare_state_dir(&root).unwrap();
        let expected = action(1, "I_1");
        let candidate_handle = record_candidate(&state, &expected, &context()).unwrap();
        let grant_handle =
            grant_candidate(&state, &candidate_handle, RequestedGrantScope::Command).unwrap();
        assert!(grant_candidate(&state, &candidate_handle, RequestedGrantScope::Command).is_err());
        assert_eq!(list_grants(&state).unwrap().len(), 1);
        assert!(revoke_grant(&state, &grant_handle).unwrap());
        assert!(!revoke_grant(&state, &grant_handle).unwrap());
        assert!(!is_granted(&state, &expected, "private").unwrap());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn granting_removes_covered_pending_candidates() {
        let root = root("grant-removes-pending");
        let state = prepare_state_dir(&root).unwrap();
        let first = action(1, "I_1");
        let mut covered = action(1, "I_2");
        covered.args[3] = "--body=other".to_string();
        let mut uncovered = action(1, "I_3");
        uncovered.args = vec![
            "issue".to_string(),
            "edit".to_string(),
            "7".to_string(),
            "--title=changed".to_string(),
        ];
        let first_handle = record_candidate(&state, &first, &context()).unwrap();
        record_candidate(&state, &covered, &context()).unwrap();
        record_candidate(&state, &uncovered, &context()).unwrap();
        grant_candidate(&state, &first_handle, RequestedGrantScope::Command).unwrap();
        let remaining = list_candidates(&state).unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].1.action, uncovered);
        fs::remove_dir_all(root).unwrap();
    }
}
