use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{File, OpenOptions, Permissions};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt, symlink};
use std::os::unix::io::AsRawFd;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use std::{fs, process, thread};

use crate::logging::log_line;
use crate::managed_push;

#[derive(Deserialize)]
struct Request {
    args: Vec<String>,
    #[serde(default)]
    cwd: Option<String>,
}

#[derive(Serialize)]
struct Response {
    exit_code: i32,
    stdout: String,
    stderr: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tracking_updates: Vec<TrackingUpdate>,
}

#[derive(Serialize)]
struct TrackingUpdate {
    reference: String,
    old_oid: Option<String>,
    new_oid: String,
}

#[derive(Debug, PartialEq)]
enum Push {
    Branch,
    Tags,
}

#[derive(Clone, Debug)]
pub enum Mode {
    Single {
        repository: PathBuf,
        origin: String,
    },
    Managed {
        workspace_root: PathBuf,
        state_dir: PathBuf,
    },
}

struct PinnedDirectory {
    _handle: File,
    command_path: PathBuf,
    resolved_path: PathBuf,
}

struct PinnedRepository {
    worktree: PinnedDirectory,
    _git_dir: PinnedDirectory,
    _common_dir: PinnedDirectory,
    objects: PinnedDirectory,
    refs: PinnedDirectory,
    allowed_root: PathBuf,
    git_dir_command_path: PathBuf,
    common_dir_command_path: PathBuf,
}

impl PinnedRepository {
    fn command(&self) -> Command {
        let mut command = Command::new("git");
        clear_repository_environment(&mut command);
        command
            .current_dir(&self.worktree.command_path)
            .env("GIT_WORK_TREE", &self.worktree.command_path)
            .env("GIT_DIR", &self.git_dir_command_path)
            .env("GIT_COMMON_DIR", &self.common_dir_command_path);
        command
    }
}

struct LocalConfigAudit {
    denied: Vec<String>,
    origin_fetch_refspecs: Vec<String>,
    snapshot_entries: Vec<(String, String)>,
}

fn config_entries_for_scope(
    repository: &PinnedRepository,
    scope: &str,
) -> Result<Vec<(String, String)>, String> {
    let output = repository
        .command()
        .args(["config", scope, "--list", "-z", "--includes"])
        .output()
        .map_err(|error| format!("failed to run git config {scope}: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "git config {scope} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(config_entries(&output.stdout))
}

fn bool_config(
    repository: &PinnedRepository,
    scope: Option<&str>,
    key: &str,
) -> Result<Option<bool>, String> {
    let mut command = repository.command();
    command.arg("config");
    if let Some(scope) = scope {
        command.arg(scope);
    }
    let output = command
        .args(["--type=bool", "--get", key])
        .output()
        .map_err(|error| format!("failed to inspect {key}: {error}"))?;
    if output.status.code() == Some(1) {
        return Ok(None);
    }
    if !output.status.success() {
        return Err(format!(
            "could not inspect {key}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    match String::from_utf8_lossy(&output.stdout).trim() {
        "true" => Ok(Some(true)),
        "false" => Ok(Some(false)),
        _ => Err(format!("git returned an invalid {key} value")),
    }
}

fn clear_repository_environment(command: &mut Command) {
    for key in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_COMMON_DIR",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    ] {
        command.env_remove(key);
    }
}

fn fd_path(file: &File) -> PathBuf {
    PathBuf::from(format!(
        "/proc/{}/fd/{}",
        std::process::id(),
        file.as_raw_fd()
    ))
}

fn pin_directory(
    path: &Path,
    allowed_root: &Path,
    description: &str,
) -> Result<PinnedDirectory, String> {
    let handle = File::open(path)
        .map_err(|error| format!("could not open {description} {}: {error}", path.display()))?;
    let metadata = handle
        .metadata()
        .map_err(|error| format!("could not inspect {description}: {error}"))?;
    if !metadata.is_dir() {
        return Err(format!("{description} is not a directory"));
    }

    let command_path = fd_path(&handle);
    let resolved_path = fs::canonicalize(&command_path)
        .map_err(|error| format!("could not resolve opened {description}: {error}"))?;
    if !resolved_path.starts_with(allowed_root) {
        return Err(format!(
            "{description} escapes the approved workspace: {}",
            resolved_path.display()
        ));
    }
    let resolved_metadata = fs::metadata(&resolved_path)
        .map_err(|error| format!("could not inspect resolved {description}: {error}"))?;
    if metadata.dev() != resolved_metadata.dev() || metadata.ino() != resolved_metadata.ino() {
        return Err(format!("{description} changed while it was being opened"));
    }

    Ok(PinnedDirectory {
        _handle: handle,
        command_path,
        resolved_path,
    })
}

fn discover_repository_layout(start: &PinnedDirectory) -> Result<[PathBuf; 3], String> {
    let mut command = Command::new("git");
    clear_repository_environment(&mut command);
    let output = command
        .arg("-C")
        .arg(&start.command_path)
        .args([
            "rev-parse",
            "--path-format=absolute",
            "--show-toplevel",
            "--absolute-git-dir",
            "--git-common-dir",
        ])
        .output()
        .map_err(|error| format!("failed to inspect repository layout: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "could not inspect repository layout: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let stdout = String::from_utf8(output.stdout)
        .map_err(|_| "git returned a non-UTF-8 repository layout".to_string())?;
    let paths: Vec<_> = stdout.lines().map(PathBuf::from).collect();
    paths.try_into().map_err(|_| {
        "git returned an unexpected repository layout; paths containing newlines are unsupported"
            .to_string()
    })
}

fn pin_repository(repository: &Path, allowed_root: &Path) -> Result<PinnedRepository, String> {
    let allowed_root = fs::canonicalize(allowed_root)
        .map_err(|error| format!("could not resolve approved workspace: {error}"))?;
    let start = pin_directory(repository, &allowed_root, "repository directory")?;
    let [worktree_path, git_dir_path, common_dir_path] = discover_repository_layout(&start)?;
    let worktree = pin_directory(&worktree_path, &allowed_root, "repository worktree")?;
    if worktree.resolved_path != start.resolved_path {
        return Err(format!(
            "repository path is not its worktree root: {}",
            start.resolved_path.display()
        ));
    }
    let git_dir = pin_directory(&git_dir_path, &allowed_root, "Git directory")?;
    let common_dir = pin_directory(&common_dir_path, &allowed_root, "Git common directory")?;
    let objects = pin_directory(
        &common_dir.command_path.join("objects"),
        &allowed_root,
        "Git objects directory",
    )?;
    let refs = pin_directory(
        &common_dir.command_path.join("refs"),
        &allowed_root,
        "Git refs directory",
    )?;
    let git_dir_command_path = git_dir.command_path.clone();
    let common_dir_command_path = common_dir.command_path.clone();

    Ok(PinnedRepository {
        worktree,
        _git_dir: git_dir,
        _common_dir: common_dir,
        objects,
        refs,
        allowed_root,
        git_dir_command_path,
        common_dir_command_path,
    })
}

fn parse_push_args(args: &[String]) -> Option<Push> {
    match args {
        [p] if p == "push" => Some(Push::Branch),
        [p, t] if p == "push" && t == "--tags" => Some(Push::Tags),
        _ => None,
    }
}

// Repo-local config keys that could make the host-side `git push` execute
// agent-controlled code, redirect the push, or persist temporary routing.
// The workspace is agent-writable, so its .git/config is untrusted.
const DENIED_KEYS: &[&str] = &[
    "core.sshcommand",
    "core.hookspath",
    "core.fsmonitor",
    "core.askpass",
    "core.alternaterefscommand",
    "core.gitproxy",
    "core.pager",
    "core.worktree",
    "push.gpgsign",
    "push.recursesubmodules",
    "remote.pushdefault",
];

const DENIED_PREFIXES: &[&str] = &[
    "credential.",
    "http.",
    "gpg.",
    "url.",
    "protocol.",
    "ssh.",
    "include.",
    "includeif.",
];

const DENIED_REMOTE_SUFFIXES: &[&str] = &[
    ".pushurl",
    ".proxy",
    ".receivepack",
    ".uploadpack",
    ".vcs",
    ".push",
];

fn is_denied_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    DENIED_KEYS.contains(&key.as_str())
        || DENIED_PREFIXES.iter().any(|p| key.starts_with(p))
        || (key.starts_with("remote.") && DENIED_REMOTE_SUFFIXES.iter().any(|s| key.ends_with(s)))
        || (key.starts_with("branch.") && key.ends_with(".pushremote"))
}

/// Parse `git config --list -z` output into (key, value) pairs. Entries are
/// NUL-separated; within an entry the key ends at the first newline (values
/// may contain newlines, which is why the non-`-z` format is not safe to
/// parse).
fn config_entries(raw: &[u8]) -> Vec<(String, String)> {
    raw.split(|b| *b == 0)
        .filter(|entry| !entry.is_empty())
        .map(|entry| {
            let end = entry
                .iter()
                .position(|b| *b == b'\n')
                .unwrap_or(entry.len());
            let key = String::from_utf8_lossy(&entry[..end]).into_owned();
            let value = if end < entry.len() {
                String::from_utf8_lossy(&entry[end + 1..]).into_owned()
            } else {
                String::new()
            };
            (key, value)
        })
        .collect()
}

fn credential_entries(entries: &[(String, String)]) -> Vec<(String, String)> {
    entries
        .iter()
        .filter(|(k, _)| k.starts_with("credential."))
        .cloned()
        .collect()
}

fn local_config_audit(repository: &PinnedRepository) -> Result<LocalConfigAudit, String> {
    let mut entries = config_entries_for_scope(repository, "--local")?;
    if bool_config(repository, Some("--local"), "extensions.worktreeConfig")?.unwrap_or(false) {
        entries.extend(config_entries_for_scope(repository, "--worktree")?);
    }
    let mut denied: Vec<String> = entries
        .iter()
        .map(|(key, _)| key.clone())
        .filter(|key| is_denied_key(key))
        .collect();
    denied.dedup();
    let origin_fetch_refspecs = entries
        .iter()
        .filter_map(|(key, value)| (key == "remote.origin.fetch").then_some(value.clone()))
        .collect();
    Ok(LocalConfigAudit {
        denied,
        origin_fetch_refspecs,
        snapshot_entries: entries,
    })
}

fn valid_refspec_pattern(repository: &PinnedRepository, value: &str) -> bool {
    repository
        .command()
        .args(["check-ref-format", "--refspec-pattern", value])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn validate_origin_fetch_refspec(
    repository: &PinnedRepository,
    refspec: &str,
) -> Result<(), String> {
    let refspec = refspec.strip_prefix('+').unwrap_or(refspec);
    if let Some(source) = refspec.strip_prefix('^') {
        if source.contains(':') || !valid_refspec_pattern(repository, source) {
            return Err("invalid negative origin fetch refspec".to_string());
        }
        return Ok(());
    }

    let mut parts = refspec.split(':');
    let source = parts.next().unwrap_or_default();
    let destination = parts.next().unwrap_or_default();
    if source.is_empty()
        || destination.is_empty()
        || parts.next().is_some()
        || source.matches('*').count() != destination.matches('*').count()
        || !valid_refspec_pattern(repository, source)
        || !valid_refspec_pattern(repository, destination)
    {
        return Err("invalid origin fetch refspec".to_string());
    }
    if destination
        .strip_prefix("refs/remotes/origin/")
        .is_none_or(str::is_empty)
    {
        return Err(format!(
            "origin fetch refspec destination is outside refs/remotes/origin/: {destination}"
        ));
    }
    Ok(())
}

fn temporary_remote_name() -> Result<String, String> {
    Ok(format!("sandbox-proxy-{}", random_hex()?))
}

fn random_hex() -> Result<String, String> {
    let mut bytes = [0_u8; 16];
    File::open("/dev/urandom")
        .and_then(|mut random| random.read_exact(&mut bytes))
        .map_err(|error| format!("could not read secure randomness: {error}"))?;
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut name = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        name.push(HEX[(byte >> 4) as usize] as char);
        name.push(HEX[(byte & 0x0f) as usize] as char);
    }
    Ok(name)
}

struct TemporaryDirectory {
    path: PathBuf,
    _handle: File,
    command_path: PathBuf,
}

impl TemporaryDirectory {
    fn create() -> Result<Self, String> {
        for _ in 0..10 {
            let path =
                PathBuf::from("/tmp").join(format!("claude-sandbox-git-config-{}", random_hex()?));
            match fs::create_dir(&path) {
                Ok(()) => {
                    fs::set_permissions(&path, Permissions::from_mode(0o700)).map_err(|error| {
                        format!("could not secure temporary Git config directory: {error}")
                    })?;
                    let handle = File::open(&path).map_err(|error| {
                        format!("could not open temporary Git config directory: {error}")
                    })?;
                    let command_path = fd_path(&handle);
                    return Ok(Self {
                        path,
                        _handle: handle,
                        command_path,
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(format!(
                        "could not create temporary Git config directory: {error}"
                    ));
                }
            }
        }
        Err("could not allocate a unique temporary Git config directory".to_string())
    }
}

impl Drop for TemporaryDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

struct RepositoryConfigSnapshot {
    directory: TemporaryDirectory,
    credentials: Vec<(String, String)>,
    tracking_before: BTreeMap<String, String>,
}

fn host_config_entries(repository: &PinnedRepository, scope: &str) -> Vec<(String, String)> {
    repository
        .command()
        .args(["config", scope, "--list", "-z", "--includes"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| config_entries(&output.stdout))
        .unwrap_or_default()
}

fn reference_entries(
    command: &mut Command,
) -> Result<Vec<(String, String, Option<String>)>, String> {
    let output = command
        .args([
            "for-each-ref",
            "--format=%(refname)%00%(objectname)%00%(symref)%00",
        ])
        .output()
        .map_err(|error| format!("failed to snapshot Git refs: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "could not snapshot Git refs: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    let mut entries = Vec::new();
    for record in output.stdout.split(|byte| *byte == b'\n') {
        if record.is_empty() {
            continue;
        }
        let fields: Vec<_> = record.split(|byte| *byte == 0).collect();
        if fields.len() != 4 || !fields[3].is_empty() {
            return Err("git returned malformed ref snapshot data".to_string());
        }
        let reference = String::from_utf8(fields[0].to_vec())
            .map_err(|_| "git returned a non-UTF-8 ref name".to_string())?;
        let oid = String::from_utf8(fields[1].to_vec())
            .map_err(|_| "git returned a non-UTF-8 object ID".to_string())?;
        let symref = if fields[2].is_empty() {
            None
        } else {
            Some(
                String::from_utf8(fields[2].to_vec())
                    .map_err(|_| "git returned a non-UTF-8 symbolic ref".to_string())?,
            )
        };
        entries.push((reference, oid, symref));
    }
    Ok(entries)
}

fn write_private_refs(
    root: &Path,
    entries: &[(String, String, Option<String>)],
) -> Result<BTreeMap<String, String>, String> {
    let refs_root = root.join("refs");
    if fs::symlink_metadata(&refs_root).is_ok() {
        fs::remove_file(&refs_root)
            .map_err(|error| format!("could not replace Git refs snapshot: {error}"))?;
    }
    fs::create_dir(&refs_root)
        .map_err(|error| format!("could not create private Git refs: {error}"))?;

    let mut tracking = BTreeMap::new();
    for (reference, oid, symref) in entries {
        if !reference.starts_with("refs/") || reference.contains("..") {
            return Err(format!("git returned unsafe ref name: {reference}"));
        }
        if reference.starts_with("refs/remotes/origin/")
            && let Some(target) = symref
            && !target.starts_with("refs/remotes/origin/")
        {
            return Err(format!(
                "origin tracking ref points outside the origin namespace: {reference} -> {target}"
            ));
        }
        let path = root.join(reference);
        let parent = path
            .parent()
            .ok_or_else(|| format!("git returned invalid ref name: {reference}"))?;
        fs::create_dir_all(parent)
            .map_err(|error| format!("could not create private ref directory: {error}"))?;
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .map_err(|error| format!("could not create private ref {reference}: {error}"))?;
        if let Some(target) = symref {
            writeln!(file, "ref: {target}")
                .map_err(|error| format!("could not snapshot symbolic ref: {error}"))?;
        } else {
            writeln!(file, "{oid}").map_err(|error| format!("could not snapshot ref: {error}"))?;
            if reference.starts_with("refs/remotes/origin/") {
                tracking.insert(reference.clone(), oid.clone());
            }
        }
    }
    Ok(tracking)
}

fn write_config_snapshot(
    path: &Path,
    entries: &[(String, String)],
    exclude_credentials: bool,
) -> Result<(), String> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|error| format!("could not create Git config snapshot: {error}"))?;

    for (key, value) in entries {
        let normalized = key.to_ascii_lowercase();
        if normalized == "push.autosetupremote"
            || normalized == "extensions.worktreeconfig"
            || normalized.starts_with("include.")
            || normalized.starts_with("includeif.")
            || (exclude_credentials && normalized.starts_with("credential."))
        {
            continue;
        }
        let output = Command::new("git")
            .args(["config", "--file"])
            .arg(path)
            .arg("--add")
            .arg(key)
            .arg(value)
            .output()
            .map_err(|error| format!("failed to write Git config snapshot: {error}"))?;
        if !output.status.success() {
            return Err(format!(
                "could not write Git config snapshot: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
    }
    Ok(())
}

fn reject_object_store_redirection(repository: &PinnedRepository) -> Result<(), String> {
    for name in ["info/alternates", "info/http-alternates"] {
        let path = repository.objects.command_path.join(name);
        if fs::symlink_metadata(&path).is_ok() {
            return Err(format!(
                "repository objects directory declares {name}, which could redirect the push to object stores outside the workspace"
            ));
        }
    }
    Ok(())
}

fn reject_escaping_ref_symlinks(directory: &Path, allowed_root: &Path) -> Result<(), String> {
    let entries = fs::read_dir(directory).map_err(|error| {
        format!(
            "could not inspect refs directory {}: {error}",
            directory.display()
        )
    })?;
    for entry in entries {
        let entry = entry.map_err(|error| format!("could not read refs entry: {error}"))?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)
            .map_err(|error| format!("could not inspect ref {}: {error}", path.display()))?;
        let file_type = metadata.file_type();
        if file_type.is_symlink() {
            // A dangling symlink resolves to no object and is harmless; only a
            // symlink whose target escapes the workspace can leak host objects.
            if let Ok(target) = fs::canonicalize(&path)
                && !target.starts_with(allowed_root)
            {
                return Err(format!(
                    "ref {} is a symlink escaping the workspace: {}",
                    path.display(),
                    target.display()
                ));
            }
        } else if file_type.is_dir() {
            reject_escaping_ref_symlinks(&path, allowed_root)?;
        }
    }
    Ok(())
}

impl RepositoryConfigSnapshot {
    fn create(
        repository: &PinnedRepository,
        local_entries: &[(String, String)],
    ) -> Result<Self, String> {
        reject_object_store_redirection(repository)?;
        reject_escaping_ref_symlinks(&repository.refs.command_path, &repository.allowed_root)?;
        let directory = TemporaryDirectory::create()?;
        let system_entries = host_config_entries(repository, "--system");
        let global_entries = host_config_entries(repository, "--global");
        let mut credentials = credential_entries(&system_entries);
        credentials.extend(credential_entries(&global_entries));

        write_config_snapshot(&directory.path.join("system-config"), &system_entries, true)?;
        write_config_snapshot(&directory.path.join("global-config"), &global_entries, true)?;
        write_config_snapshot(&directory.path.join("config"), local_entries, false)?;
        let private_git_dir = directory.path.join("git");
        fs::create_dir(&private_git_dir)
            .map_err(|error| format!("could not create private Git directory: {error}"))?;
        fs::copy(
            repository.git_dir_command_path.join("HEAD"),
            private_git_dir.join("HEAD"),
        )
        .map_err(|error| format!("could not snapshot Git HEAD: {error}"))?;
        fs::write(private_git_dir.join("commondir"), "..\n").map_err(|error| {
            format!("could not configure private Git common directory: {error}")
        })?;

        symlink(
            &repository.objects.command_path,
            directory.path.join("objects"),
        )
        .map_err(|error| format!("could not pin Git objects directory: {error}"))?;
        symlink(&repository.refs.command_path, directory.path.join("refs"))
            .map_err(|error| format!("could not pin Git refs directory: {error}"))?;
        for name in ["packed-refs", "shallow"] {
            let source = repository.common_dir_command_path.join(name);
            match fs::copy(&source, directory.path.join(name)) {
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(format!("could not snapshot Git {name}: {error}"));
                }
            }
        }

        let mut snapshot_command = repository.command();
        snapshot_command
            .env("GIT_DIR", directory.command_path.join("git"))
            .env("GIT_COMMON_DIR", &directory.command_path)
            .env(
                "GIT_CONFIG_SYSTEM",
                directory.command_path.join("system-config"),
            )
            .env(
                "GIT_CONFIG_GLOBAL",
                directory.command_path.join("global-config"),
            );
        let refs = reference_entries(&mut snapshot_command)?;
        let tracking_before = write_private_refs(&directory.path, &refs)?;

        Ok(Self {
            directory,
            credentials,
            tracking_before,
        })
    }

    fn command(&self, repository: &PinnedRepository) -> Command {
        let mut command = repository.command();
        command
            .env("GIT_DIR", self.directory.command_path.join("git"))
            .env("GIT_COMMON_DIR", &self.directory.command_path)
            .env(
                "GIT_CONFIG_SYSTEM",
                self.directory.command_path.join("system-config"),
            )
            .env(
                "GIT_CONFIG_GLOBAL",
                self.directory.command_path.join("global-config"),
            );
        command
    }

    fn tracking_updates(
        &self,
        repository: &PinnedRepository,
    ) -> Result<Vec<TrackingUpdate>, String> {
        let mut command = self.command(repository);
        let after = reference_entries(&mut command)?;
        let mut updates = Vec::new();
        for (reference, oid, symref) in after {
            if !reference.starts_with("refs/remotes/origin/") {
                continue;
            }
            if symref.is_some() {
                continue;
            }
            let old_oid = self.tracking_before.get(&reference).cloned();
            if old_oid.as_deref() != Some(oid.as_str()) {
                updates.push(TrackingUpdate {
                    reference,
                    old_oid,
                    new_oid: oid,
                });
            }
        }
        Ok(updates)
    }
}

struct PreparedPush {
    _config_snapshot: RepositoryConfigSnapshot,
    command: Command,
}

impl PreparedPush {
    fn tracking_updates(
        &self,
        repository: &PinnedRepository,
    ) -> Result<Vec<TrackingUpdate>, String> {
        self._config_snapshot.tracking_updates(repository)
    }
}

fn prepare_push(
    repository: &PinnedRepository,
    audit: &LocalConfigAudit,
    expected_origin: &str,
    push: &Push,
) -> Result<PreparedPush, String> {
    let config_snapshot = RepositoryConfigSnapshot::create(repository, &audit.snapshot_entries)?;
    let remote_name = temporary_remote_name()?;
    let mut command = config_snapshot.command(repository);
    command
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_NO_LAZY_FETCH", "1");
    command.args([
        "-c",
        "core.hooksPath=/dev/null",
        "-c",
        "core.sshCommand=ssh",
        "-c",
        "core.askpass=",
        "-c",
        "core.fsmonitor=false",
        "-c",
        "protocol.ext.allow=never",
        "-c",
        "credential.helper=",
        "-c",
        "push.gpgSign=false",
        "-c",
        "push.recurseSubmodules=no",
    ]);
    for (key, value) in &config_snapshot.credentials {
        command.arg("-c").arg(format!("{key}={value}"));
    }
    command
        .arg("-c")
        .arg(format!("remote.{remote_name}.pushurl={expected_origin}"));
    for refspec in &audit.origin_fetch_refspecs {
        command
            .arg("-c")
            .arg(format!("remote.{remote_name}.fetch={refspec}"));
    }
    command.args(["-c", "push.autoSetupRemote=false"]);
    command.args(["push", "--no-verify"]);
    if push == &Push::Tags {
        command.arg("--tags");
    }
    command.arg(&remote_name);

    Ok(PreparedPush {
        _config_snapshot: config_snapshot,
        command,
    })
}

fn origin_url_at(repository: &PinnedRepository) -> Option<String> {
    let output = repository
        .command()
        .args(["remote", "get-url", "origin"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8(output.stdout).ok()?.trim().to_string();
    if value.is_empty() { None } else { Some(value) }
}

pub fn origin_url() -> Option<String> {
    let cwd = std::env::current_dir().ok()?;
    managed_push::origin_url_at(&cwd)
}

fn repository_root_at(directory: &Path) -> Option<PathBuf> {
    let output = Command::new("git")
        .arg("-C")
        .arg(directory)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let path = PathBuf::from(String::from_utf8(output.stdout).ok()?.trim());
    fs::canonicalize(path).ok()
}

pub fn repository_root() -> Option<PathBuf> {
    repository_root_at(&std::env::current_dir().ok()?)
}

fn deny(stderr: String) -> Response {
    Response {
        exit_code: 1,
        stdout: String::new(),
        stderr,
        tracking_updates: Vec::new(),
    }
}

fn resolve_request_repository(
    req: &Request,
    mode: &Mode,
    log: &Arc<Mutex<File>>,
) -> Result<(PathBuf, String, PathBuf), Response> {
    match mode {
        Mode::Single { repository, origin } => {
            Ok((repository.clone(), origin.clone(), repository.clone()))
        }
        Mode::Managed {
            workspace_root,
            state_dir,
        } => {
            let cwd = req.cwd.as_deref().ok_or_else(|| {
                deny(
                    "git-proxy: managed push request did not include a working directory"
                        .to_string(),
                )
            })?;
            let (repository_path, repository) =
                managed_push::resolve_repository(workspace_root, cwd)
                    .map_err(|error| deny(format!("git-proxy: push refused: {error}")))?;
            let approval = managed_push::read_approval(state_dir, &repository.relative_path)
                .map_err(|error| deny(format!("git-proxy: push refused: {error}")))?;

            match approval {
                Some(approval) if approval.repository.origin == repository.origin => {
                    match managed_push::consume_once(state_dir, &approval) {
                        Ok(true) => Ok((
                            repository_path,
                            repository.origin,
                            workspace_root.clone(),
                        )),
                        Ok(false) => Err(deny(
                            "git-proxy: the one-time approval was already consumed; approve this repository again"
                                .to_string(),
                        )),
                        Err(error) => Err(deny(format!("git-proxy: push refused: {error}"))),
                    }
                }
                approval => {
                    let previous = approval.map(|value| value.repository.origin);
                    let id = managed_push::record_candidate(state_dir, &repository, previous.clone())
                        .map_err(|error| deny(format!("git-proxy: push refused: {error}")))?;
                    let reason = if let Some(previous) = previous {
                        format!("origin changed from {previous} to {}", repository.origin)
                    } else {
                        "repository is not approved".to_string()
                    };
                    log_line(
                        log,
                        &format!(
                            "PENDING {} ({}; candidate {})",
                            repository.relative_path, reason, id
                        ),
                    );
                    Err(deny(format!(
                        "git-proxy: push pending approval for '{}' ({})\nOpen the T3 admin portal, approve the repository, and retry.",
                        repository.relative_path, repository.origin
                    )))
                }
            }
        }
    }
}

fn handle_request(req: Request, mode: &Mode, log: &Arc<Mutex<File>>) -> Response {
    let cmd_str = req.args.join(" ");

    let push = match parse_push_args(&req.args) {
        Some(p) => p,
        None => {
            log_line(log, &format!("DENIED  git {} (not allowed)", cmd_str));
            return deny(format!(
                "git-proxy: command not allowed: git {}\n\
                 Only 'git push' and 'git push --tags' are bridged to the host.",
                cmd_str
            ));
        }
    };

    let (repository_path, expected_origin, allowed_root) =
        match resolve_request_repository(&req, mode, log) {
            Ok(repository) => repository,
            Err(response) => return response,
        };
    let repository_label = repository_path.display();
    let repository = match pin_repository(&repository_path, &allowed_root) {
        Ok(repository) => repository,
        Err(error) => return deny(format!("git-proxy: push refused: {error}")),
    };

    let audit = match local_config_audit(&repository) {
        Ok(audit) => audit,
        Err(error) => {
            log_line(log, &format!("ERROR   git {} ({})", cmd_str, error));
            return deny(format!("git-proxy: {error}"));
        }
    };
    if !audit.denied.is_empty() {
        let list = audit.denied.join(", ");
        log_line(
            log,
            &format!("DENIED  git {} (local config: {})", cmd_str, list),
        );
        return deny(format!(
            "git-proxy: push refused: the repository's local git config sets \
                 key(s) the host will not honor: {}. Remove them from .git/config \
                 and try again.",
            list
        ));
    }
    for refspec in &audit.origin_fetch_refspecs {
        if let Err(error) = validate_origin_fetch_refspec(&repository, refspec) {
            log_line(
                log,
                &format!("DENIED  git {} (origin fetch refspec: {})", cmd_str, error),
            );
            return deny(format!("git-proxy: push refused: {error}"));
        }
    }

    match origin_url_at(&repository) {
        Some(url) if url == expected_origin => {}
        current => {
            let now = current.unwrap_or_else(|| "<unset>".to_string());
            log_line(
                log,
                &format!(
                    "DENIED  git {} (origin changed: {} -> {})",
                    cmd_str, expected_origin, now
                ),
            );
            return deny(format!(
                "git-proxy: push refused: remote 'origin' changed since the \
                 sandbox was launched (was {}, now {})",
                expected_origin, now
            ));
        }
    }

    let mut prepared = match prepare_push(&repository, &audit, &expected_origin, &push) {
        Ok(prepared) => prepared,
        Err(error) => return deny(format!("git-proxy: {error}")),
    };

    log_line(
        log,
        &format!("ALLOWED git {} ({})", cmd_str, repository_label),
    );

    match prepared.command.output() {
        Ok(output) => {
            let mut exit_code = output.status.code().unwrap_or(1);
            let mut stderr = String::from_utf8_lossy(&output.stderr).into_owned();
            let tracking_updates = if output.status.success() {
                match prepared.tracking_updates(&repository) {
                    Ok(updates) => updates,
                    Err(error) => {
                        exit_code = 1;
                        if !stderr.is_empty() && !stderr.ends_with('\n') {
                            stderr.push('\n');
                        }
                        stderr.push_str(&format!(
                            "git-proxy: remote push succeeded, but tracking refs could not be prepared: {error}\n"
                        ));
                        Vec::new()
                    }
                }
            } else {
                Vec::new()
            };
            log_line(
                log,
                &format!(
                    "EXIT    git {} ({}) -> {}",
                    cmd_str, repository_label, exit_code
                ),
            );
            Response {
                exit_code,
                stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                stderr,
                tracking_updates,
            }
        }
        Err(e) => {
            log_line(log, &format!("ERROR   git {} ({})", cmd_str, e));
            deny(format!("git-proxy: failed to execute git: {}", e))
        }
    }
}

pub fn run(socket_path: &str, mode: Mode) {
    let path = Path::new(socket_path);

    // Remove stale socket if it exists
    if path.exists() {
        let _ = fs::remove_file(path);
    }

    // Ensure parent directory exists with owner-only permissions
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
        let _ = fs::set_permissions(parent, Permissions::from_mode(0o700));
    }

    let listener = UnixListener::bind(path).unwrap_or_else(|e| {
        eprintln!("git-proxy: failed to bind {}: {}", socket_path, e);
        std::process::exit(1);
    });

    let log_path = path.with_file_name("git-proxy.log");
    let log_file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .unwrap_or_else(|e| {
            eprintln!(
                "git-proxy: failed to open log {}: {}",
                log_path.display(),
                e
            );
            std::process::exit(1);
        });
    let log = Arc::new(Mutex::new(log_file));

    log_line(&log, &format!("listening on {} ({mode:?})", socket_path));

    // Watchdog: exit when parent process (podman after exec) dies.
    // After exec(), our ppid is podman's PID. When podman exits, ppid
    // becomes 1 (init). Poll every 2s and clean up when that happens.
    let parent_pid = std::os::unix::process::parent_id();
    let watchdog_socket = socket_path.to_string();
    let watchdog_log = Arc::clone(&log);
    thread::spawn(move || {
        loop {
            thread::sleep(Duration::from_secs(2));
            let current_ppid = std::os::unix::process::parent_id();
            if current_ppid != parent_pid {
                log_line(
                    &watchdog_log,
                    &format!(
                        "parent {} exited (ppid now {}), shutting down",
                        parent_pid, current_ppid
                    ),
                );
                let _ = fs::remove_file(&watchdog_socket);
                process::exit(0);
            }
        }
    });

    let mode = Arc::new(mode);
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let log = Arc::clone(&log);
                let mode = Arc::clone(&mode);
                thread::spawn(move || {
                    let reader = BufReader::new(&stream);
                    let mut writer = &stream;

                    // Read exactly one JSON line
                    let mut line = String::new();
                    if let Ok(n) = reader.take(1_048_576).read_line(&mut line) {
                        if n == 0 {
                            return;
                        }
                        let response = match serde_json::from_str::<Request>(&line) {
                            Ok(req) => handle_request(req, &mode, &log),
                            Err(e) => {
                                log_line(&log, &format!("INVALID ({})", e));
                                deny(format!("git-proxy: invalid request: {}", e))
                            }
                        };
                        let _ = serde_json::to_writer(&mut writer, &response);
                        let _ = writer.write_all(b"\n");
                    }
                });
            }
            Err(e) => {
                log_line(&log, &format!("connection error: {}", e));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strs(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| x.to_string()).collect()
    }

    fn run_git(directory: &Path, args: &[&str]) {
        let output = Command::new("git")
            .arg("-C")
            .arg(directory)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn git_stdout(directory: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(directory)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    }

    fn test_root(label: &str) -> PathBuf {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "claude-sandbox-{label}-{}-{nonce}",
            std::process::id()
        ))
    }

    fn test_log(root: &Path) -> Arc<Mutex<File>> {
        Arc::new(Mutex::new(
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(root.join("proxy.log"))
                .unwrap(),
        ))
    }

    fn initialize_repository(root: &Path) -> (PathBuf, PathBuf) {
        let repository = root.join("project");
        let remote = root.join("remote.git");
        fs::create_dir_all(&repository).unwrap();
        run_git(
            root,
            &[
                "init",
                "--bare",
                "--initial-branch=main",
                remote.to_str().unwrap(),
            ],
        );
        run_git(
            root,
            &[
                "init",
                "--initial-branch=main",
                repository.to_str().unwrap(),
            ],
        );
        run_git(&repository, &["config", "user.name", "Test"]);
        run_git(
            &repository,
            &["config", "user.email", "test@example.invalid"],
        );
        fs::write(repository.join("file.txt"), "content\n").unwrap();
        run_git(&repository, &["add", "file.txt"]);
        run_git(&repository, &["commit", "-m", "initial"]);
        run_git(
            &repository,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        (repository, remote)
    }

    fn single_mode(repository: &Path, remote: &Path) -> Mode {
        Mode::Single {
            repository: repository.to_path_buf(),
            origin: remote.to_str().unwrap().to_string(),
        }
    }

    fn push_request(tags: bool) -> Request {
        Request {
            args: if tags {
                strs(&["push", "--tags"])
            } else {
                strs(&["push"])
            },
            cwd: None,
        }
    }

    fn apply_tracking_updates(repository: &Path, updates: &[TrackingUpdate]) {
        for update in updates {
            let old_oid = update
                .old_oid
                .clone()
                .unwrap_or_else(|| "0".repeat(update.new_oid.len()));
            run_git(
                repository,
                &[
                    "update-ref",
                    "--no-deref",
                    update.reference.as_str(),
                    update.new_oid.as_str(),
                    old_oid.as_str(),
                ],
            );
        }
    }

    // ── Push argument allowlist ────────────────────────────────────

    #[test]
    fn test_plain_push_allowed() {
        assert_eq!(parse_push_args(&strs(&["push"])), Some(Push::Branch));
    }

    #[test]
    fn test_push_tags_allowed() {
        assert_eq!(
            parse_push_args(&strs(&["push", "--tags"])),
            Some(Push::Tags)
        );
    }

    #[test]
    fn managed_push_requires_approval_then_routes_to_repository() {
        let root = test_root("managed-push");
        let workspace = root.join("workspace");
        let repository = workspace.join("project");
        let remote = root.join("remote.git");
        let state = root.join("state");
        fs::create_dir_all(&repository).unwrap();
        fs::create_dir_all(&state).unwrap();
        run_git(
            &root,
            &[
                "init",
                "--bare",
                "--initial-branch=main",
                remote.to_str().unwrap(),
            ],
        );
        run_git(
            &root,
            &[
                "init",
                "--initial-branch=main",
                repository.to_str().unwrap(),
            ],
        );
        run_git(&repository, &["config", "user.name", "Test"]);
        run_git(
            &repository,
            &["config", "user.email", "test@example.invalid"],
        );
        run_git(&repository, &["config", "push.default", "current"]);
        fs::write(repository.join("file.txt"), "content\n").unwrap();
        run_git(&repository, &["add", "file.txt"]);
        run_git(&repository, &["commit", "-m", "initial"]);
        run_git(
            &repository,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );

        let log = test_log(&root);
        let mode = Mode::Managed {
            workspace_root: workspace.clone(),
            state_dir: state.clone(),
        };
        let request = || Request {
            args: strs(&["push"]),
            cwd: Some("/workspace/project".to_string()),
        };

        let denied = handle_request(request(), &mode, &log);
        assert_eq!(denied.exit_code, 1);
        assert!(denied.stderr.contains("pending approval"));

        let candidates = managed_push::list_candidates(&state).unwrap();
        assert_eq!(candidates.len(), 1);
        let (candidate_id, candidate) = &candidates[0];
        managed_push::approve(
            &state,
            &candidate.repository,
            managed_push::ApprovalScope::Persistent,
        )
        .unwrap();
        managed_push::remove_candidate(&state, candidate_id).unwrap();

        let allowed = handle_request(request(), &mode, &log);
        assert_eq!(allowed.exit_code, 0, "{}", allowed.stderr);
        apply_tracking_updates(&repository, &allowed.tracking_updates);
        assert_eq!(
            git_stdout(&repository, &["rev-parse", "HEAD"]),
            git_stdout(&repository, &["rev-parse", "refs/remotes/origin/main"])
        );

        run_git(&repository, &["config", "--unset", "push.default"]);
        run_git(&repository, &["config", "branch.main.remote", "origin"]);
        run_git(
            &repository,
            &["config", "branch.main.merge", "refs/heads/main"],
        );
        fs::write(repository.join("file.txt"), "second\n").unwrap();
        run_git(&repository, &["commit", "-am", "second"]);
        let simple_push = handle_request(request(), &mode, &log);
        assert_eq!(simple_push.exit_code, 0, "{}", simple_push.stderr);
        apply_tracking_updates(&repository, &simple_push.tracking_updates);
        assert_eq!(
            git_stdout(&repository, &["rev-parse", "HEAD"]),
            git_stdout(&repository, &["rev-parse", "refs/remotes/origin/main"])
        );

        managed_push::revoke(&state, &candidate.repository.relative_path).unwrap();
        managed_push::approve(
            &state,
            &candidate.repository,
            managed_push::ApprovalScope::Once,
        )
        .unwrap();
        let one_time = handle_request(request(), &mode, &log);
        assert_eq!(one_time.exit_code, 0, "{}", one_time.stderr);
        apply_tracking_updates(&repository, &one_time.tracking_updates);
        assert!(
            managed_push::read_approval(&state, &candidate.repository.relative_path)
                .unwrap()
                .is_none()
        );
        let consumed = handle_request(request(), &mode, &log);
        assert!(consumed.stderr.contains("pending approval"));

        std::os::unix::fs::symlink(&root, workspace.join("escape")).unwrap();
        let escaped = managed_push::resolve_repository(&workspace, "/workspace/escape");
        assert!(
            escaped
                .unwrap_err()
                .contains("escapes the mounted workspace")
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn single_push_updates_and_repairs_origin_tracking_without_persisting_remote() {
        let root = test_root("single-tracking");
        let (repository, remote) = initialize_repository(&root);
        run_git(&repository, &["config", "push.default", "current"]);
        let mode = single_mode(&repository, &remote);
        let log = test_log(&root);

        let pushed = handle_request(push_request(false), &mode, &log);
        assert_eq!(pushed.exit_code, 0, "{}", pushed.stderr);
        apply_tracking_updates(&repository, &pushed.tracking_updates);
        let head = git_stdout(&repository, &["rev-parse", "HEAD"]);
        assert_eq!(head, git_stdout(&remote, &["rev-parse", "refs/heads/main"]));
        assert_eq!(
            head,
            git_stdout(&repository, &["rev-parse", "refs/remotes/origin/main"])
        );

        let remote_config = Command::new("git")
            .arg("-C")
            .arg(&repository)
            .args([
                "config",
                "--local",
                "--get-regexp",
                "^remote\\.sandbox-proxy-",
            ])
            .output()
            .unwrap();
        assert!(!remote_config.status.success());
        let branch_remote = Command::new("git")
            .arg("-C")
            .arg(&repository)
            .args(["config", "--local", "--get", "branch.main.remote"])
            .output()
            .unwrap();
        assert!(!branch_remote.status.success());

        run_git(
            &repository,
            &["update-ref", "-d", "refs/remotes/origin/main"],
        );
        let repaired = handle_request(push_request(false), &mode, &log);
        assert_eq!(repaired.exit_code, 0, "{}", repaired.stderr);
        apply_tracking_updates(&repository, &repaired.tracking_updates);
        assert_eq!(
            head,
            git_stdout(&repository, &["rev-parse", "refs/remotes/origin/main"])
        );

        run_git(&repository, &["tag", "v1"]);
        let tracking_before_tags =
            git_stdout(&repository, &["rev-parse", "refs/remotes/origin/main"]);
        let tags = handle_request(push_request(true), &mode, &log);
        assert_eq!(tags.exit_code, 0, "{}", tags.stderr);
        apply_tracking_updates(&repository, &tags.tracking_updates);
        assert_eq!(
            git_stdout(&remote, &["rev-parse", "refs/tags/v1"]),
            git_stdout(&repository, &["rev-parse", "refs/tags/v1"])
        );
        assert_eq!(
            tracking_before_tags,
            git_stdout(&repository, &["rev-parse", "refs/remotes/origin/main"])
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn custom_origin_fetch_mapping_controls_tracking_destination() {
        let root = test_root("custom-fetch");
        let (repository, remote) = initialize_repository(&root);
        run_git(&repository, &["config", "push.default", "current"]);
        run_git(
            &repository,
            &[
                "config",
                "--replace-all",
                "remote.origin.fetch",
                "+refs/heads/*:refs/remotes/origin/custom/*",
            ],
        );
        let mode = single_mode(&repository, &remote);
        let pushed = handle_request(push_request(false), &mode, &test_log(&root));
        assert_eq!(pushed.exit_code, 0, "{}", pushed.stderr);
        apply_tracking_updates(&repository, &pushed.tracking_updates);
        assert_eq!(
            git_stdout(&repository, &["rev-parse", "HEAD"]),
            git_stdout(
                &repository,
                &["rev-parse", "refs/remotes/origin/custom/main"]
            )
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unsafe_origin_fetch_mapping_is_rejected_before_push() {
        let root = test_root("unsafe-fetch");
        let (repository, remote) = initialize_repository(&root);
        run_git(&repository, &["config", "push.default", "current"]);
        run_git(
            &repository,
            &[
                "config",
                "--replace-all",
                "remote.origin.fetch",
                "+refs/heads/*:refs/heads/*",
            ],
        );
        let rejected = handle_request(
            push_request(false),
            &single_mode(&repository, &remote),
            &test_log(&root),
        );
        assert_eq!(rejected.exit_code, 1);
        assert!(rejected.stderr.contains("outside refs/remotes/origin/"));
        let remote_head = Command::new("git")
            .arg("-C")
            .arg(&remote)
            .args(["rev-parse", "--verify", "refs/heads/main"])
            .output()
            .unwrap();
        assert!(!remote_head.status.success());

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn config_rewrite_after_snapshot_cannot_redirect_push() {
        let root = test_root("config-race");
        let (repository, approved_remote) = initialize_repository(&root);
        let evil_remote = root.join("evil.git");
        run_git(
            &root,
            &[
                "init",
                "--bare",
                "--initial-branch=main",
                evil_remote.to_str().unwrap(),
            ],
        );
        run_git(&repository, &["config", "push.default", "current"]);

        let pinned = pin_repository(&repository, &repository).unwrap();
        let audit = local_config_audit(&pinned).unwrap();
        let mut prepared = prepare_push(
            &pinned,
            &audit,
            approved_remote.to_str().unwrap(),
            &Push::Branch,
        )
        .unwrap();

        let rewrite_key = format!("url.{}.insteadOf", evil_remote.display());
        run_git(
            &repository,
            &[
                "config",
                rewrite_key.as_str(),
                approved_remote.to_str().unwrap(),
            ],
        );
        let output = prepared.command.output().unwrap();
        assert!(
            output.status.success(),
            "push failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let updates = prepared.tracking_updates(&pinned).unwrap();
        apply_tracking_updates(&repository, &updates);
        assert_eq!(
            git_stdout(&repository, &["rev-parse", "HEAD"]),
            git_stdout(&approved_remote, &["rev-parse", "refs/heads/main"])
        );
        assert_eq!(
            git_stdout(&repository, &["rev-parse", "HEAD"]),
            git_stdout(&repository, &["rev-parse", "refs/remotes/origin/main"])
        );
        let evil_head = Command::new("git")
            .arg("-C")
            .arg(&evil_remote)
            .args(["rev-parse", "--verify", "refs/heads/main"])
            .output()
            .unwrap();
        assert!(!evil_head.status.success());

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn nested_tracking_ref_symlink_cannot_escape_host_push() {
        let root = test_root("tracking-symlink");
        let (repository, remote) = initialize_repository(&root);
        let outside = root.join("outside");
        let remotes = repository.join(".git/refs/remotes");
        fs::create_dir_all(&outside).unwrap();
        fs::create_dir_all(&remotes).unwrap();
        std::os::unix::fs::symlink(&outside, remotes.join("origin")).unwrap();
        run_git(&repository, &["config", "push.default", "current"]);

        let pushed = handle_request(
            push_request(false),
            &single_mode(&repository, &remote),
            &test_log(&root),
        );
        assert_ne!(pushed.exit_code, 0);
        let remote_head = Command::new("git")
            .arg("-C")
            .arg(&remote)
            .args(["rev-parse", "--verify", "refs/heads/main"])
            .output()
            .unwrap();
        assert!(!remote_head.status.success());
        assert!(!outside.join("main").exists());

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn objects_info_alternates_redirection_is_rejected() {
        let root = test_root("objects-alternates");
        let (repository, remote) = initialize_repository(&root);
        run_git(&repository, &["config", "push.default", "current"]);
        let info = repository.join(".git/objects/info");
        fs::create_dir_all(&info).unwrap();
        fs::write(
            info.join("alternates"),
            format!("{}\n", root.join("outside-objects").display()),
        )
        .unwrap();

        let pushed = handle_request(
            push_request(false),
            &single_mode(&repository, &remote),
            &test_log(&root),
        );
        assert_ne!(pushed.exit_code, 0);
        let approved_head = Command::new("git")
            .arg("-C")
            .arg(&remote)
            .args(["rev-parse", "--verify", "refs/heads/main"])
            .output()
            .unwrap();
        assert!(!approved_head.status.success());

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn escaping_ref_symlink_is_rejected() {
        let root = test_root("escaping-ref-symlink");
        let (repository, remote) = initialize_repository(&root);
        run_git(&repository, &["config", "push.default", "current"]);
        let outside = root.join("outside-ref");
        fs::write(
            &outside,
            format!("{}\n", git_stdout(&repository, &["rev-parse", "HEAD"])),
        )
        .unwrap();
        std::os::unix::fs::symlink(&outside, repository.join(".git/refs/heads/loot")).unwrap();

        let pushed = handle_request(
            push_request(false),
            &single_mode(&repository, &remote),
            &test_log(&root),
        );
        assert_ne!(pushed.exit_code, 0);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn clone_origin_head_symref_is_preserved_and_skipped_for_updates() {
        let root = test_root("clone-origin-head");
        let (seed, remote) = initialize_repository(&root);
        run_git(&seed, &["push", "origin", "main"]);
        let repository = root.join("clone");
        run_git(
            &root,
            &[
                "clone",
                remote.to_str().unwrap(),
                repository.to_str().unwrap(),
            ],
        );
        assert_eq!(
            git_stdout(&repository, &["symbolic-ref", "refs/remotes/origin/HEAD"]),
            "refs/remotes/origin/main"
        );
        run_git(&repository, &["config", "user.name", "Clone"]);
        run_git(
            &repository,
            &["config", "user.email", "clone@example.invalid"],
        );
        run_git(&repository, &["config", "push.default", "current"]);
        fs::write(repository.join("clone.txt"), "clone\n").unwrap();
        run_git(&repository, &["add", "clone.txt"]);
        run_git(&repository, &["commit", "-m", "clone"]);

        let pushed = handle_request(
            push_request(false),
            &single_mode(&repository, &remote),
            &test_log(&root),
        );
        assert_eq!(pushed.exit_code, 0, "{}", pushed.stderr);
        assert!(
            pushed
                .tracking_updates
                .iter()
                .all(|update| update.reference != "refs/remotes/origin/HEAD")
        );
        apply_tracking_updates(&repository, &pushed.tracking_updates);
        assert_eq!(
            git_stdout(&repository, &["rev-parse", "HEAD"]),
            git_stdout(&repository, &["rev-parse", "refs/remotes/origin/main"])
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn missing_promisor_object_does_not_trigger_lazy_fetch() {
        let root = test_root("no-lazy-fetch");
        let (repository, approved_remote) = initialize_repository(&root);
        let promisor_remote = root.join("promisor.git");
        run_git(
            &root,
            &[
                "init",
                "--bare",
                "--initial-branch=main",
                promisor_remote.to_str().unwrap(),
            ],
        );
        run_git(
            &repository,
            &[
                "remote",
                "add",
                "promisor",
                promisor_remote.to_str().unwrap(),
            ],
        );
        run_git(&repository, &["push", "promisor", "main"]);
        run_git(
            &repository,
            &["config", "extensions.partialClone", "promisor"],
        );
        run_git(&repository, &["config", "remote.promisor.promisor", "true"]);
        run_git(&repository, &["config", "push.default", "current"]);
        let blob = git_stdout(&repository, &["rev-parse", "HEAD:file.txt"]);
        let (directory, filename) = blob.split_at(2);
        fs::remove_file(
            repository
                .join(".git/objects")
                .join(directory)
                .join(filename),
        )
        .unwrap();

        let pushed = handle_request(
            push_request(false),
            &single_mode(&repository, &approved_remote),
            &test_log(&root),
        );
        assert_ne!(pushed.exit_code, 0);
        let approved_head = Command::new("git")
            .arg("-C")
            .arg(&approved_remote)
            .args(["rev-parse", "--verify", "refs/heads/main"])
            .output()
            .unwrap();
        assert!(!approved_head.status.success());

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn auto_setup_remote_is_suppressed_without_persisting_temporary_name() {
        let root = test_root("auto-setup-remote");
        let (repository, remote) = initialize_repository(&root);
        run_git(&repository, &["config", "push.default", "current"]);
        run_git(&repository, &["config", "push.autoSetupRemote", "true"]);
        let pushed = handle_request(
            push_request(false),
            &single_mode(&repository, &remote),
            &test_log(&root),
        );
        assert_eq!(pushed.exit_code, 0, "{}", pushed.stderr);
        apply_tracking_updates(&repository, &pushed.tracking_updates);
        assert_eq!(
            git_stdout(&repository, &["rev-parse", "HEAD"]),
            git_stdout(&repository, &["rev-parse", "refs/remotes/origin/main"])
        );
        let branch_remote = Command::new("git")
            .arg("-C")
            .arg(&repository)
            .args(["config", "--local", "--get", "branch.main.remote"])
            .output()
            .unwrap();
        assert!(!branch_remote.status.success());

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rejected_non_fast_forward_does_not_advance_tracking_ref() {
        let root = test_root("non-fast-forward");
        let (repository, remote) = initialize_repository(&root);
        run_git(&repository, &["config", "push.default", "current"]);
        let mode = single_mode(&repository, &remote);
        let log = test_log(&root);
        let initial = handle_request(push_request(false), &mode, &log);
        assert_eq!(initial.exit_code, 0, "{}", initial.stderr);
        apply_tracking_updates(&repository, &initial.tracking_updates);
        let base = git_stdout(&repository, &["rev-parse", "HEAD"]);

        let peer = root.join("peer");
        run_git(
            &root,
            &["clone", remote.to_str().unwrap(), peer.to_str().unwrap()],
        );
        run_git(&peer, &["config", "user.name", "Peer"]);
        run_git(&peer, &["config", "user.email", "peer@example.invalid"]);
        fs::write(peer.join("peer.txt"), "peer\n").unwrap();
        run_git(&peer, &["add", "peer.txt"]);
        run_git(&peer, &["commit", "-m", "peer"]);
        run_git(&peer, &["push", "origin", "main"]);

        fs::write(repository.join("local.txt"), "local\n").unwrap();
        run_git(&repository, &["add", "local.txt"]);
        run_git(&repository, &["commit", "-m", "local"]);
        let rejected = handle_request(push_request(false), &mode, &log);
        assert_ne!(rejected.exit_code, 0);
        assert_eq!(
            base,
            git_stdout(&repository, &["rev-parse", "refs/remotes/origin/main"])
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn repository_git_directory_must_stay_inside_approved_root() {
        let root = test_root("git-dir-escape");
        let workspace = root.join("workspace");
        let repository = workspace.join("project");
        let external_git_dir = root.join("external.git");
        fs::create_dir_all(&workspace).unwrap();
        run_git(
            &root,
            &[
                "init",
                "--separate-git-dir",
                external_git_dir.to_str().unwrap(),
                repository.to_str().unwrap(),
            ],
        );

        let error = match pin_repository(&repository, &workspace) {
            Ok(_) => panic!("repository with external Git directory was accepted"),
            Err(error) => error,
        };
        assert!(error.contains("Git directory escapes the approved workspace"));

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn repository_root_resolves_from_subdirectory() {
        let root = test_root("repository-root");
        let (repository, _) = initialize_repository(&root);
        let nested = repository.join("nested").join("directory");
        fs::create_dir_all(&nested).unwrap();

        assert_eq!(repository_root_at(&nested), Some(repository.clone()));

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn worktree_config_is_included_in_security_audit() {
        let root = test_root("worktree-config");
        let (repository, _) = initialize_repository(&root);
        let linked = root.join("linked");
        run_git(
            &repository,
            &["config", "extensions.worktreeConfig", "true"],
        );
        run_git(
            &repository,
            &["worktree", "add", "-b", "linked", linked.to_str().unwrap()],
        );
        run_git(
            &linked,
            &["config", "--worktree", "core.sshCommand", "evil"],
        );

        let pinned = pin_repository(&linked, &root).unwrap();
        let audit = local_config_audit(&pinned).unwrap();
        assert!(audit.denied.contains(&"core.sshcommand".to_string()));

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn linked_worktree_push_uses_pinned_git_and_common_directories() {
        let root = test_root("linked-worktree-push");
        let (repository, remote) = initialize_repository(&root);
        let linked = root.join("linked");
        run_git(&repository, &["config", "push.default", "current"]);
        run_git(
            &repository,
            &["worktree", "add", "-b", "linked", linked.to_str().unwrap()],
        );

        let pinned = pin_repository(&linked, &root).unwrap();
        assert_ne!(pinned.git_dir_command_path, pinned.common_dir_command_path);
        let audit = local_config_audit(&pinned).unwrap();
        let mut prepared =
            prepare_push(&pinned, &audit, remote.to_str().unwrap(), &Push::Branch).unwrap();
        let output = prepared.command.output().unwrap();
        assert!(
            output.status.success(),
            "push failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let updates = prepared.tracking_updates(&pinned).unwrap();
        apply_tracking_updates(&linked, &updates);
        assert_eq!(
            git_stdout(&linked, &["rev-parse", "HEAD"]),
            git_stdout(&remote, &["rev-parse", "refs/heads/linked"])
        );
        assert_eq!(
            git_stdout(&linked, &["rev-parse", "HEAD"]),
            git_stdout(&linked, &["rev-parse", "refs/remotes/origin/linked"])
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn test_everything_else_denied() {
        assert_eq!(parse_push_args(&[]), None);
        assert_eq!(parse_push_args(&strs(&["fetch"])), None);
        assert_eq!(parse_push_args(&strs(&["--tags"])), None);
        assert_eq!(parse_push_args(&strs(&["push", "--force"])), None);
        assert_eq!(parse_push_args(&strs(&["push", "-f"])), None);
        assert_eq!(parse_push_args(&strs(&["push", "origin"])), None);
        assert_eq!(parse_push_args(&strs(&["push", "origin", "main"])), None);
        assert_eq!(
            parse_push_args(&strs(&["push", "--delete", "branch"])),
            None
        );
        assert_eq!(parse_push_args(&strs(&["push", "--tags", "--force"])), None);
        assert_eq!(parse_push_args(&strs(&["push", "--tags", "origin"])), None);
        assert_eq!(parse_push_args(&strs(&["push", "--mirror"])), None);
    }

    // ── Local config audit ─────────────────────────────────────────

    #[test]
    fn test_dangerous_keys_denied() {
        assert!(is_denied_key("core.sshcommand"));
        assert!(is_denied_key("core.sshCommand"));
        assert!(is_denied_key("core.hookspath"));
        assert!(is_denied_key("core.fsmonitor"));
        assert!(is_denied_key("core.askpass"));
        assert!(is_denied_key("core.alternateRefsCommand"));
        assert!(is_denied_key("credential.helper"));
        assert!(is_denied_key("credential.https://github.com.helper"));
        assert!(is_denied_key("http.proxy"));
        assert!(is_denied_key("http.https://github.com.proxy"));
        assert!(is_denied_key("url.ext::sh -c evil.insteadof"));
        assert!(is_denied_key("protocol.ext.allow"));
        assert!(is_denied_key("ssh.variant"));
        assert!(is_denied_key("include.path"));
        assert!(is_denied_key("includeif.gitdir:/x.path"));
        assert!(is_denied_key("remote.origin.pushurl"));
        assert!(is_denied_key("remote.origin.proxy"));
        assert!(is_denied_key("remote.origin.receivepack"));
        assert!(is_denied_key("remote.origin.uploadpack"));
        assert!(is_denied_key("remote.origin.push"));
        assert!(is_denied_key("remote.origin.vcs"));
        assert!(is_denied_key("remote.pushdefault"));
        assert!(is_denied_key("remote.pushDefault"));
        assert!(is_denied_key("branch.master.pushremote"));
        assert!(is_denied_key("push.gpgSign"));
        assert!(is_denied_key("push.recurseSubmodules"));
        assert!(is_denied_key("gpg.program"));
    }

    #[test]
    fn test_normal_keys_allowed() {
        assert!(!is_denied_key("core.bare"));
        assert!(!is_denied_key("core.repositoryformatversion"));
        assert!(!is_denied_key("core.filemode"));
        assert!(!is_denied_key("remote.origin.url"));
        assert!(!is_denied_key("remote.origin.fetch"));
        assert!(!is_denied_key("branch.main.remote"));
        assert!(!is_denied_key("branch.main.merge"));
        assert!(!is_denied_key("user.name"));
        assert!(!is_denied_key("pull.rebase"));
        assert!(!is_denied_key("push.default"));
        assert!(!is_denied_key("push.autoSetupRemote"));
    }

    #[test]
    fn test_config_entries_values() {
        let raw = b"credential.helper\nstore\0core.bare\ntrue\0flagonly\0";
        assert_eq!(
            config_entries(raw),
            vec![
                ("credential.helper".to_string(), "store".to_string()),
                ("core.bare".to_string(), "true".to_string()),
                ("flagonly".to_string(), String::new()),
            ]
        );
    }

    #[test]
    fn test_credential_entries_filter() {
        let entries = vec![
            ("credential.helper".to_string(), "store".to_string()),
            (
                "credential.https://github.com.helper".to_string(),
                "gh".to_string(),
            ),
            ("core.bare".to_string(), "false".to_string()),
            ("user.name".to_string(), "x".to_string()),
        ];
        let creds = credential_entries(&entries);
        assert_eq!(creds.len(), 2);
        assert!(creds.iter().all(|(k, _)| k.starts_with("credential.")));
    }
}
