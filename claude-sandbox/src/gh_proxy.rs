use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::os::unix::io::AsRawFd;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use std::{fs, process, thread};

use crate::{proxy_log, proxy_socket};

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
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RepositoryId {
    host: String,
    owner: String,
    name: String,
}

impl RepositoryId {
    fn selector(&self) -> String {
        if self.host.eq_ignore_ascii_case("github.com") {
            format!("{}/{}", self.owner, self.name)
        } else {
            format!("{}/{}/{}", self.host, self.owner, self.name)
        }
    }

    fn matches(&self, other: &Self) -> bool {
        self.host.eq_ignore_ascii_case(&other.host)
            && self.owner.eq_ignore_ascii_case(&other.owner)
            && self.name.eq_ignore_ascii_case(&other.name)
    }

    fn api_path(&self, suffix: &str) -> String {
        format!("/repos/{}/{}{}", self.owner, self.name, suffix)
    }
}

struct RepositoryGrant {
    root: PathBuf,
    handle: File,
    device: u64,
    inode: u64,
    repository: RepositoryId,
}

struct ProxyConfig {
    workspace_root: PathBuf,
    neutral_dir: PathBuf,
    grants: Vec<RepositoryGrant>,
}

struct CommandDef {
    group: &'static str,
    subcommand: &'static str,
    is_write: bool,
    allowed_flags: &'static [&'static str],
}

struct ExtCommandDef {
    group: &'static str,
    subcommand: &'static str,
    description: &'static str,
    help_text: &'static str,
    handler: fn(&[String], &RepositoryGrant, &ProxyConfig) -> Response,
}

const COMMANDS: &[CommandDef] = &[
    // ── Read commands ──────────────────────────────────────────────
    CommandDef {
        group: "auth",
        subcommand: "status",
        is_write: false,
        allowed_flags: &[
            "--active",
            "-a",
            "--hostname",
            "--json",
            "--jq",
            "--template",
        ],
    },
    CommandDef {
        group: "pr",
        subcommand: "list",
        is_write: false,
        allowed_flags: &[
            "--state",
            "-s",
            "--limit",
            "-L",
            "--json",
            "--jq",
            "-q",
            "--label",
            "-l",
            "--author",
            "-A",
            "--assignee",
            "-a",
            "--base",
            "-B",
            "--head",
            "-H",
            "--search",
            "-S",
            "--draft",
            "-d",
            "--template",
            "-t",
            "--web",
            "-w",
            "--repo",
            "-R",
            "--app",
        ],
    },
    CommandDef {
        group: "pr",
        subcommand: "view",
        is_write: false,
        allowed_flags: &[
            "--json",
            "--jq",
            "-q",
            "--comments",
            "-c",
            "--template",
            "-t",
            "--web",
            "-w",
            "--repo",
            "-R",
        ],
    },
    CommandDef {
        group: "pr",
        subcommand: "diff",
        is_write: false,
        allowed_flags: &["--color", "--patch", "--name-only", "--repo", "-R"],
    },
    CommandDef {
        group: "pr",
        subcommand: "checks",
        is_write: false,
        allowed_flags: &[
            "--json",
            "--jq",
            "-q",
            "--watch",
            "--interval",
            "-i",
            "--fail-fast",
            "--required",
            "--web",
            "-w",
            "--repo",
            "-R",
        ],
    },
    CommandDef {
        group: "issue",
        subcommand: "list",
        is_write: false,
        allowed_flags: &[
            "--state",
            "-s",
            "--limit",
            "-L",
            "--json",
            "--jq",
            "-q",
            "--label",
            "-l",
            "--author",
            "-A",
            "--assignee",
            "-a",
            "--milestone",
            "-m",
            "--search",
            "-S",
            "--template",
            "-t",
            "--web",
            "-w",
            "--repo",
            "-R",
        ],
    },
    CommandDef {
        group: "issue",
        subcommand: "view",
        is_write: false,
        allowed_flags: &[
            "--json",
            "--jq",
            "-q",
            "--comments",
            "-c",
            "--template",
            "-t",
            "--web",
            "-w",
            "--repo",
            "-R",
        ],
    },
    CommandDef {
        group: "repo",
        subcommand: "view",
        is_write: false,
        allowed_flags: &[
            "--json",
            "--jq",
            "-q",
            "--template",
            "-t",
            "--web",
            "-w",
            "--repo",
            "-R",
        ],
    },
    CommandDef {
        group: "repo",
        subcommand: "list",
        is_write: false,
        allowed_flags: &[
            "--archived",
            "--fork",
            "--jq",
            "-q",
            "--json",
            "--language",
            "-l",
            "--limit",
            "-L",
            "--no-archived",
            "--source",
            "--template",
            "-t",
            "--topic",
            "--visibility",
        ],
    },
    CommandDef {
        group: "release",
        subcommand: "list",
        is_write: false,
        allowed_flags: &[
            "--limit",
            "-L",
            "--json",
            "--jq",
            "-q",
            "--exclude-drafts",
            "--exclude-pre-releases",
            "--order",
            "-O",
            "--repo",
            "-R",
        ],
    },
    CommandDef {
        group: "release",
        subcommand: "view",
        is_write: false,
        allowed_flags: &[
            "--json",
            "--jq",
            "-q",
            "--template",
            "-t",
            "--web",
            "-w",
            "--repo",
            "-R",
        ],
    },
    CommandDef {
        group: "run",
        subcommand: "list",
        is_write: false,
        allowed_flags: &[
            "--limit",
            "-L",
            "--json",
            "--jq",
            "-q",
            "--branch",
            "-b",
            "--workflow",
            "-w",
            "--status",
            "-s",
            "--event",
            "-e",
            "--user",
            "-u",
            "--commit",
            "-c",
            "--repo",
            "-R",
        ],
    },
    CommandDef {
        group: "run",
        subcommand: "view",
        is_write: false,
        allowed_flags: &[
            "--json",
            "--jq",
            "-q",
            "--log",
            "--log-failed",
            "--exit-status",
            "--verbose",
            "-v",
            "--web",
            "-w",
            "--job",
            "-j",
            "--attempt",
            "--repo",
            "-R",
        ],
    },
    CommandDef {
        group: "run",
        subcommand: "watch",
        is_write: false,
        allowed_flags: &[
            "--exit-status",
            "--interval",
            "-i",
            "--compact",
            "--repo",
            "-R",
        ],
    },
    CommandDef {
        group: "workflow",
        subcommand: "list",
        is_write: false,
        allowed_flags: &[
            "--all",
            "-a",
            "--jq",
            "-q",
            "--json",
            "--limit",
            "-L",
            "--template",
            "-t",
            "--repo",
            "-R",
        ],
    },
    CommandDef {
        group: "search",
        subcommand: "code",
        is_write: false,
        allowed_flags: &[
            "--extension",
            "--filename",
            "--jq",
            "-q",
            "--json",
            "--language",
            "--limit",
            "-L",
            "--match",
            "--owner",
            "--repo",
            "-R",
            "--size",
            "--template",
            "-t",
            "--web",
            "-w",
        ],
    },
    CommandDef {
        group: "search",
        subcommand: "commits",
        is_write: false,
        allowed_flags: &[
            "--author",
            "--author-date",
            "--author-email",
            "--author-name",
            "--committer",
            "--committer-date",
            "--committer-email",
            "--committer-name",
            "--hash",
            "--jq",
            "-q",
            "--json",
            "--limit",
            "-L",
            "--merge",
            "--order",
            "--owner",
            "--parent",
            "--repo",
            "-R",
            "--sort",
            "--template",
            "-t",
            "--tree",
            "--visibility",
            "--web",
            "-w",
        ],
    },
    CommandDef {
        group: "search",
        subcommand: "issues",
        is_write: false,
        allowed_flags: &[
            "--app",
            "--archived",
            "--assignee",
            "--author",
            "--closed",
            "--commenter",
            "--comments",
            "--created",
            "--include-prs",
            "--interactions",
            "--involves",
            "--jq",
            "-q",
            "--json",
            "--label",
            "--language",
            "--limit",
            "-L",
            "--locked",
            "--match",
            "--mentions",
            "--milestone",
            "--no-assignee",
            "--no-label",
            "--no-milestone",
            "--no-project",
            "--order",
            "--owner",
            "--project",
            "--reactions",
            "--repo",
            "-R",
            "--sort",
            "--state",
            "--team-mentions",
            "--template",
            "-t",
            "--updated",
            "--visibility",
            "--web",
            "-w",
        ],
    },
    CommandDef {
        group: "search",
        subcommand: "prs",
        is_write: false,
        allowed_flags: &[
            "--app",
            "--archived",
            "--assignee",
            "--author",
            "--base",
            "-B",
            "--checks",
            "--closed",
            "--commenter",
            "--comments",
            "--created",
            "--draft",
            "--head",
            "-H",
            "--interactions",
            "--involves",
            "--jq",
            "-q",
            "--json",
            "--label",
            "--language",
            "--limit",
            "-L",
            "--locked",
            "--match",
            "--mentions",
            "--merged",
            "--merged-at",
            "--milestone",
            "--no-assignee",
            "--no-label",
            "--no-milestone",
            "--no-project",
            "--order",
            "--owner",
            "--project",
            "--reactions",
            "--repo",
            "-R",
            "--review",
            "--review-requested",
            "--reviewed-by",
            "--sort",
            "--state",
            "--team-mentions",
            "--template",
            "-t",
            "--updated",
            "--visibility",
            "--web",
            "-w",
        ],
    },
    CommandDef {
        group: "search",
        subcommand: "repos",
        is_write: false,
        allowed_flags: &[
            "--archived",
            "--created",
            "--followers",
            "--forks",
            "--good-first-issues",
            "--help-wanted-issues",
            "--include-forks",
            "--jq",
            "-q",
            "--json",
            "--language",
            "--license",
            "--limit",
            "-L",
            "--match",
            "--number-topics",
            "--order",
            "--owner",
            "--size",
            "--sort",
            "--stars",
            "--template",
            "-t",
            "--topic",
            "--updated",
            "--visibility",
            "--web",
            "-w",
        ],
    },
    // ── Write commands (no --repo/-R, no --body-file/-F) ───────────
    CommandDef {
        group: "pr",
        subcommand: "comment",
        is_write: true,
        allowed_flags: &["--body", "-b", "--edit-last", "--web", "-w"],
    },
    CommandDef {
        group: "issue",
        subcommand: "create",
        is_write: true,
        allowed_flags: &[
            "--title",
            "-t",
            "--body",
            "-b",
            "--label",
            "-l",
            "--assignee",
            "-a",
            "--milestone",
            "-m",
            "--project",
            "-p",
            "--web",
            "-w",
            "--template",
            "-T",
        ],
    },
    CommandDef {
        group: "issue",
        subcommand: "comment",
        is_write: true,
        allowed_flags: &["--body", "-b", "--edit-last", "--web", "-w"],
    },
    CommandDef {
        group: "issue",
        subcommand: "close",
        is_write: true,
        allowed_flags: &["--comment", "-c", "--reason", "-r", "--duplicate-of"],
    },
    CommandDef {
        group: "run",
        subcommand: "rerun",
        is_write: true,
        allowed_flags: &["--failed", "--job", "-j", "--debug"],
    },
    CommandDef {
        group: "issue",
        subcommand: "edit",
        is_write: true,
        allowed_flags: &[
            "--title",
            "-t",
            "--body",
            "-b",
            "--milestone",
            "-m",
            "--add-assignee",
            "--remove-assignee",
            "--add-label",
            "--remove-label",
            "--add-project",
            "--remove-project",
            "--remove-milestone",
        ],
    },
];

// ── Extension commands (gh ext …) ─────────────────────────────────────

const EXT_COMMANDS: &[ExtCommandDef] = &[
    ExtCommandDef {
        group: "ext",
        subcommand: "run-logs",
        description: "Download workflow run logs",
        help_text: "gh ext run-logs <run-id> (current launch-snapshotted repo only)\n\n\
                        Download workflow run logs for the current repository.\n\
                        Saves zip to .claude-sandbox/run-<run-id>.zip and prints the path.\n",
        handler: handle_run_logs,
    },
    ExtCommandDef {
        group: "ext",
        subcommand: "milestone-create",
        description: "Create a milestone",
        help_text: "gh ext milestone-create <title> [--description <text>] [--due-on <date>] \
                        (current launch-snapshotted repo only)\n\n\
                        Create a milestone in the current repository.\n\
                        --description, -d  Milestone description\n\
                        --due-on           Due date (ISO 8601: YYYY-MM-DDTHH:MM:SSZ)\n",
        handler: handle_milestone_create,
    },
    ExtCommandDef {
        group: "ext",
        subcommand: "milestone-list",
        description: "List milestones",
        help_text: "gh ext milestone-list [--state <open|closed|all>] \
                        (current launch-snapshotted repo only)\n\n\
                        List milestones in the current repository.\n\
                        --state, -s  Filter by state: open (default), closed, all\n",
        handler: handle_milestone_list,
    },
];

fn find_ext_command(group: &str, subcommand: &str) -> Option<&'static ExtCommandDef> {
    EXT_COMMANDS
        .iter()
        .find(|c| c.group == group && c.subcommand == subcommand)
}

fn find_command<'a>(group: &str, subcommand: &str) -> Option<&'a CommandDef> {
    COMMANDS
        .iter()
        .find(|c| c.group == group && c.subcommand == subcommand)
}

/// Extract the flag name from an arg, handling `--flag=value` forms.
fn extract_flag(arg: &str) -> &str {
    if arg.starts_with("--")
        && let Some(eq) = arg.find('=')
    {
        return &arg[..eq];
    }
    arg
}

/// Check all flags in args[2..] against the allowed set.
/// Positional args (not starting with `-`) are always allowed.
/// After `--` separator, all remaining args are treated as positional.
fn check_flags(args: &[String], allowed_flags: &[&str]) -> Result<(), String> {
    let mut past_separator = false;

    for arg in &args[2..] {
        if past_separator {
            continue;
        }
        if arg == "--" {
            past_separator = true;
            continue;
        }
        if arg.starts_with('-') {
            let flag = extract_flag(arg);
            if !allowed_flags.contains(&flag) {
                return Err(flag.to_string());
            }
        }
    }

    Ok(())
}

fn valid_repository_part(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn repository_from_path(host: &str, path: &str) -> Option<RepositoryId> {
    let mut parts = path.trim_matches('/').split('/');
    let owner = parts.next()?;
    let name = parts.next()?.trim_end_matches(".git");
    if parts.next().is_some()
        || !valid_repository_part(host)
        || !valid_repository_part(owner)
        || !valid_repository_part(name)
    {
        return None;
    }
    Some(RepositoryId {
        host: host.to_string(),
        owner: owner.to_string(),
        name: name.to_string(),
    })
}

fn repository_from_remote(remote: &str) -> Option<RepositoryId> {
    if let Some((scheme, rest)) = remote.split_once("://") {
        if !matches!(scheme, "http" | "https" | "ssh") {
            return None;
        }
        let (authority, path) = rest.split_once('/')?;
        let host = authority.rsplit('@').next()?.split(':').next()?;
        return repository_from_path(host, path);
    }

    let (authority, path) = remote.split_once(':')?;
    let host = authority.rsplit('@').next()?;
    repository_from_path(host, path)
}

fn repository_from_item_url(value: &str, group: &str) -> Result<Option<RepositoryId>, String> {
    if !value.contains("://") {
        return Ok(None);
    }
    let Some((scheme, rest)) = value.split_once("://") else {
        return Err("invalid repository URL".to_string());
    };
    if !matches!(scheme, "http" | "https") {
        return Err("only HTTP(S) GitHub item URLs are allowed".to_string());
    }
    let (authority, path) = rest
        .split_once('/')
        .ok_or_else(|| "invalid GitHub item URL".to_string())?;
    let host = authority.rsplit('@').next().unwrap_or(authority);
    let path = path.split(['?', '#']).next().unwrap_or(path);
    let parts: Vec<&str> = path.trim_matches('/').split('/').collect();
    let item_kind = if group == "pr" { "pull" } else { "issues" };
    if parts.len() != 4
        || parts[2] != item_kind
        || !parts[3].bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(format!("invalid {} URL", group));
    }
    repository_from_path(host, &format!("{}/{}", parts[0], parts[1]))
        .map(Some)
        .ok_or_else(|| "invalid GitHub repository in item URL".to_string())
}

fn proc_fd_path(handle: &File) -> PathBuf {
    PathBuf::from(format!(
        "/proc/{}/fd/{}",
        std::process::id(),
        handle.as_raw_fd()
    ))
}

fn open_untrusted_path(path: &Path) -> Option<File> {
    // Linux O_NONBLOCK prevents an attacker-controlled FIFO from hanging launch;
    // O_NOFOLLOW rejects a symlink in the final path component.
    const O_NONBLOCK: i32 = 0o4000;
    const O_NOFOLLOW: i32 = 0o400000;
    OpenOptions::new()
        .read(true)
        .custom_flags(O_NONBLOCK | O_NOFOLLOW)
        .open(path)
        .ok()
}

fn read_small_regular_file(file: File, limit: u64) -> Option<String> {
    let metadata = file.metadata().ok()?;
    if !metadata.is_file() || metadata.len() > limit {
        return None;
    }
    let mut value = String::new();
    file.take(limit + 1).read_to_string(&mut value).ok()?;
    (value.len() as u64 <= limit).then_some(value)
}

fn open_directory_inside(path: &Path, workspace_root: &Path) -> Option<File> {
    let handle = open_untrusted_path(path)?;
    if !handle.metadata().ok()?.is_dir() {
        return None;
    }
    let resolved = fs::canonicalize(proc_fd_path(&handle)).ok()?;
    resolved.starts_with(workspace_root).then_some(handle)
}

fn git_directory(root: &File, workspace_root: &Path) -> Option<File> {
    let marker = open_untrusted_path(&proc_fd_path(root).join(".git"))?;
    let metadata = marker.metadata().ok()?;
    if metadata.is_dir() {
        let resolved = fs::canonicalize(proc_fd_path(&marker)).ok()?;
        return resolved.starts_with(workspace_root).then_some(marker);
    }
    if !metadata.is_file() {
        return None;
    }

    let marker = read_small_regular_file(marker, 8 * 1024)?;
    let target = marker.trim().strip_prefix("gitdir:")?.trim();
    if target.is_empty() {
        return None;
    }
    let root_path = fs::canonicalize(proc_fd_path(root)).ok()?;
    let target = Path::new(target);
    let target = if target.is_absolute() {
        target.to_path_buf()
    } else {
        root_path.join(target)
    };
    open_directory_inside(&target, workspace_root)
}

fn common_git_directory(git_directory: File, workspace_root: &Path) -> Option<File> {
    let commondir_path = proc_fd_path(&git_directory).join("commondir");
    let Some(commondir) = open_untrusted_path(&commondir_path) else {
        return Some(git_directory);
    };
    let commondir = read_small_regular_file(commondir, 8 * 1024)?;
    let target = commondir.trim();
    if target.is_empty() {
        return None;
    }
    let git_directory_path = fs::canonicalize(proc_fd_path(&git_directory)).ok()?;
    let target = Path::new(target);
    let target = if target.is_absolute() {
        target.to_path_buf()
    } else {
        git_directory_path.join(target)
    };
    open_directory_inside(&target, workspace_root)
}

fn origin_from_pinned_root(root: &File, workspace_root: &Path) -> Option<RepositoryId> {
    let git_directory = git_directory(root, workspace_root)?;
    let common_directory = common_git_directory(git_directory, workspace_root)?;
    let config = open_untrusted_path(&proc_fd_path(&common_directory).join("config"))?;
    let config = read_small_regular_file(config, 1024 * 1024)?;
    let mut child = Command::new("git")
        .args([
            "config",
            "--no-includes",
            "--file",
            "-",
            "--get",
            "remote.origin.url",
        ])
        .current_dir("/")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .ok()?;
    child.stdin.take()?.write_all(config.as_bytes()).ok()?;
    let output = child.wait_with_output().ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8(output.stdout).ok()?.trim().to_string();
    repository_from_remote(&value)
}

fn skipped_scan_directory(name: &str) -> bool {
    matches!(
        name,
        ".claude-sandbox" | ".git" | "node_modules" | "target" | "vendor"
    )
}

fn discover_repository_grants(workspace_root: &Path) -> Result<Vec<RepositoryGrant>, String> {
    let workspace_root = fs::canonicalize(workspace_root)
        .map_err(|error| format!("could not resolve workspace root: {error}"))?;
    let mut pending = vec![workspace_root.clone()];
    let mut grants = Vec::new();

    while let Some(directory) = pending.pop() {
        let Some(handle) = open_directory_inside(&directory, &workspace_root) else {
            continue;
        };
        let metadata = handle.metadata().map_err(|error| {
            format!(
                "could not inspect repository candidate {}: {error}",
                directory.display()
            )
        })?;
        if let Some(repository) = origin_from_pinned_root(&handle, &workspace_root) {
            grants.push(RepositoryGrant {
                root: directory,
                handle,
                device: metadata.dev(),
                inode: metadata.ino(),
                repository,
            });
            continue;
        }

        let entries = fs::read_dir(&directory)
            .map_err(|error| format!("could not scan {}: {error}", directory.display()))?;
        for entry in entries {
            let entry = entry.map_err(|error| {
                format!(
                    "could not inspect entry in {}: {error}",
                    directory.display()
                )
            })?;
            let file_type = entry.file_type().map_err(|error| {
                format!("could not inspect {}: {error}", entry.path().display())
            })?;
            let name = entry.file_name();
            if file_type.is_dir()
                && !file_type.is_symlink()
                && !skipped_scan_directory(&name.to_string_lossy())
            {
                pending.push(entry.path());
            }
        }
    }

    grants.sort_by(|left, right| {
        right
            .root
            .components()
            .count()
            .cmp(&left.root.components().count())
    });
    Ok(grants)
}

fn resolve_container_path(config: &ProxyConfig, cwd: &str) -> Result<PathBuf, String> {
    let container_path = Path::new(cwd);
    let relative = container_path
        .strip_prefix("/workspace")
        .map_err(|_| "working directory must be inside /workspace".to_string())?;
    if relative.components().any(|component| {
        matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    }) {
        return Err("working directory contains an invalid path component".to_string());
    }
    let path = fs::canonicalize(config.workspace_root.join(relative))
        .map_err(|error| format!("could not resolve working directory: {error}"))?;
    if !path.starts_with(&config.workspace_root) {
        return Err("working directory escapes the mounted workspace".to_string());
    }
    Ok(path)
}

fn resolve_grant<'a>(
    config: &'a ProxyConfig,
    cwd: Option<&str>,
) -> Result<&'a RepositoryGrant, String> {
    let cwd = cwd.ok_or_else(|| "request did not include a working directory".to_string())?;
    let path = resolve_container_path(config, cwd)?;
    let grant = config
        .grants
        .iter()
        .find(|grant| path.starts_with(&grant.root))
        .ok_or_else(|| {
            "working directory is not in a launch-approved GitHub repository".to_string()
        })?;
    let metadata = fs::metadata(&grant.root)
        .map_err(|error| format!("approved repository is unavailable: {error}"))?;
    if metadata.dev() != grant.device || metadata.ino() != grant.inode {
        return Err("approved repository directory changed after launch".to_string());
    }
    Ok(grant)
}

fn safe_gh_command(config: &ProxyConfig) -> Command {
    let mut command = Command::new("gh");
    command
        .current_dir(&config.neutral_dir)
        .env_remove("GH_REPO")
        .env("GIT_CEILING_DIRECTORIES", &config.neutral_dir)
        .env("GIT_DISCOVERY_ACROSS_FILESYSTEM", "0");
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
    command
}

fn safe_gh_api_command(config: &ProxyConfig, repository: &RepositoryId) -> Command {
    let mut command = safe_gh_command(config);
    command.arg("api");
    if !repository.host.eq_ignore_ascii_case("github.com") {
        command.args(["--hostname", &repository.host]);
    }
    command
}

fn append_repo_selector(args: &mut Vec<String>, repository: &RepositoryId) {
    let separator = args
        .iter()
        .position(|arg| arg == "--")
        .unwrap_or(args.len());
    args.insert(separator, "--repo".to_string());
    args.insert(separator + 1, repository.selector());
}

// ── Extension command handlers ────────────────────────────────────────

fn maybe_ext_command(
    args: &[String],
    grant: &RepositoryGrant,
    config: &ProxyConfig,
) -> Option<Response> {
    if args.len() < 2 {
        return None;
    }
    let ext = find_ext_command(&args[0], &args[1])?;
    Some((ext.handler)(&args[2..], grant, config))
}

fn artifact_container_path(config: &ProxyConfig, grant: &RepositoryGrant, name: &str) -> String {
    let relative = grant
        .root
        .strip_prefix(&config.workspace_root)
        .unwrap_or(Path::new(""));
    Path::new("/workspace")
        .join(relative)
        .join(".claude-sandbox")
        .join(name)
        .display()
        .to_string()
}

fn write_repository_artifact(
    grant: &RepositoryGrant,
    name: &str,
    contents: &[u8],
) -> Result<(), String> {
    let pinned_root = PathBuf::from(format!(
        "/proc/{}/fd/{}",
        std::process::id(),
        grant.handle.as_raw_fd()
    ));
    let resolved_root = fs::canonicalize(&pinned_root)
        .map_err(|error| format!("could not resolve approved repository: {error}"))?;
    let directory = pinned_root.join(".claude-sandbox");
    match fs::DirBuilder::new().mode(0o700).create(&directory) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => {
            return Err(format!("could not create {}: {error}", directory.display()));
        }
    }

    let handle = open_untrusted_path(&directory)
        .ok_or_else(|| format!("could not safely open {}", directory.display()))?;
    if !handle
        .metadata()
        .map_err(|error| format!("could not inspect {}: {error}", directory.display()))?
        .is_dir()
    {
        return Err("artifact directory is not a directory".to_string());
    }
    let pinned_directory = PathBuf::from(format!(
        "/proc/{}/fd/{}",
        std::process::id(),
        handle.as_raw_fd()
    ));
    let resolved = fs::canonicalize(&pinned_directory)
        .map_err(|error| format!("could not resolve artifact directory: {error}"))?;
    if !resolved.starts_with(&resolved_root) {
        return Err("artifact directory escapes the approved repository".to_string());
    }

    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let temporary_name = format!(".{name}.{}.{}.tmp", std::process::id(), nonce);
    let temporary = pinned_directory.join(&temporary_name);
    let destination = pinned_directory.join(name);
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(&temporary)
        .map_err(|error| format!("could not create artifact: {error}"))?;
    if let Err(error) = file.write_all(contents).and_then(|_| file.sync_all()) {
        let _ = fs::remove_file(&temporary);
        return Err(format!("could not write artifact: {error}"));
    }
    fs::rename(&temporary, &destination).map_err(|error| {
        let _ = fs::remove_file(&temporary);
        format!("could not publish artifact: {error}")
    })
}

fn handle_run_logs(args: &[String], grant: &RepositoryGrant, config: &ProxyConfig) -> Response {
    if args.len() != 1 {
        return Response {
            exit_code: 1,
            stdout: String::new(),
            stderr: "gh-proxy: usage: gh ext run-logs <run-id>".to_string(),
        };
    }

    let run_id = &args[0];

    // Validate run_id is numeric to prevent path traversal
    if !run_id.chars().all(|c| c.is_ascii_digit()) {
        return Response {
            exit_code: 1,
            stdout: String::new(),
            stderr: format!("gh-proxy: invalid run id: {}", run_id),
        };
    }

    let api_path = grant
        .repository
        .api_path(&format!("/actions/runs/{run_id}/logs"));
    let name = format!("run-{}.zip", run_id);
    let out_path = artifact_container_path(config, grant, &name);

    match safe_gh_api_command(config, &grant.repository)
        .arg(&api_path)
        .output()
    {
        Ok(output) => {
            let exit_code = output.status.code().unwrap_or(1);
            if exit_code == 0
                && let Err(e) = write_repository_artifact(grant, &name, &output.stdout)
            {
                return Response {
                    exit_code: 1,
                    stdout: String::new(),
                    stderr: format!("gh-proxy: failed to write {}: {}", out_path, e),
                };
            }
            Response {
                exit_code,
                stdout: if exit_code == 0 {
                    out_path
                } else {
                    String::new()
                },
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            }
        }
        Err(e) => Response {
            exit_code: 1,
            stdout: String::new(),
            stderr: format!("gh-proxy: failed to execute gh api: {}", e),
        },
    }
}

fn handle_milestone_create(
    args: &[String],
    grant: &RepositoryGrant,
    config: &ProxyConfig,
) -> Response {
    let usage = "gh-proxy: usage: gh ext milestone-create <title> \
                 [--description <text>] [--due-on <date>]";

    if args.is_empty() {
        return Response {
            exit_code: 1,
            stdout: String::new(),
            stderr: usage.to_string(),
        };
    }

    let mut title: Option<&str> = None;
    let mut description: Option<&str> = None;
    let mut due_on: Option<&str> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--description" | "-d" => {
                i += 1;
                if i >= args.len() {
                    return Response {
                        exit_code: 1,
                        stdout: String::new(),
                        stderr: "gh-proxy: missing value for --description".to_string(),
                    };
                }
                description = Some(&args[i]);
            }
            "--due-on" => {
                i += 1;
                if i >= args.len() {
                    return Response {
                        exit_code: 1,
                        stdout: String::new(),
                        stderr: "gh-proxy: missing value for --due-on".to_string(),
                    };
                }
                due_on = Some(&args[i]);
            }
            arg if arg.starts_with('-') => {
                return Response {
                    exit_code: 1,
                    stdout: String::new(),
                    stderr: format!("gh-proxy: unknown flag: {}", arg),
                };
            }
            _ if title.is_none() => title = Some(&args[i]),
            _ => {
                return Response {
                    exit_code: 1,
                    stdout: String::new(),
                    stderr: "gh-proxy: unexpected positional argument".to_string(),
                };
            }
        }
        i += 1;
    }

    let title = match title {
        Some(t) => t,
        None => {
            return Response {
                exit_code: 1,
                stdout: String::new(),
                stderr: usage.to_string(),
            };
        }
    };

    let api_path = grant.repository.api_path("/milestones");

    let mut body = format!("{{\"title\":{}", serde_json::to_string(title).unwrap());
    if let Some(desc) = description {
        body.push_str(&format!(
            ",\"description\":{}",
            serde_json::to_string(desc).unwrap()
        ));
    }
    if let Some(due) = due_on {
        body.push_str(&format!(
            ",\"due_on\":{}",
            serde_json::to_string(due).unwrap()
        ));
    }
    body.push('}');

    match safe_gh_api_command(config, &grant.repository)
        .args([&api_path, "-X", "POST", "--input", "-"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(mut child) => {
            if let Some(ref mut stdin) = child.stdin {
                let _ = stdin.write_all(body.as_bytes());
            }
            match child.wait_with_output() {
                Ok(output) => Response {
                    exit_code: output.status.code().unwrap_or(1),
                    stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                    stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
                },
                Err(e) => Response {
                    exit_code: 1,
                    stdout: String::new(),
                    stderr: format!("gh-proxy: failed to execute gh api: {}", e),
                },
            }
        }
        Err(e) => Response {
            exit_code: 1,
            stdout: String::new(),
            stderr: format!("gh-proxy: failed to execute gh api: {}", e),
        },
    }
}

fn handle_milestone_list(
    args: &[String],
    grant: &RepositoryGrant,
    config: &ProxyConfig,
) -> Response {
    let mut state = "open";
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--state" | "-s" => {
                i += 1;
                if i >= args.len() {
                    return Response {
                        exit_code: 1,
                        stdout: String::new(),
                        stderr: "gh-proxy: missing value for --state".to_string(),
                    };
                }
                match args[i].as_str() {
                    "open" | "closed" | "all" => state = &args[i],
                    _ => {
                        return Response {
                            exit_code: 1,
                            stdout: String::new(),
                            stderr: format!(
                                "gh-proxy: invalid state '{}', must be open, closed, or all",
                                args[i]
                            ),
                        };
                    }
                }
            }
            arg if arg.starts_with('-') => {
                return Response {
                    exit_code: 1,
                    stdout: String::new(),
                    stderr: format!("gh-proxy: unknown flag: {}", arg),
                };
            }
            _ => {
                return Response {
                    exit_code: 1,
                    stdout: String::new(),
                    stderr: "gh-proxy: unexpected positional argument".to_string(),
                };
            }
        }
        i += 1;
    }

    let api_path = grant
        .repository
        .api_path(&format!("/milestones?state={state}"));

    match safe_gh_api_command(config, &grant.repository)
        .arg(&api_path)
        .output()
    {
        Ok(output) => Response {
            exit_code: output.status.code().unwrap_or(1),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        },
        Err(e) => Response {
            exit_code: 1,
            stdout: String::new(),
            stderr: format!("gh-proxy: failed to execute gh api: {}", e),
        },
    }
}

// ── Help text generation (derived from COMMANDS) ──────────────────────

fn is_help_flag(arg: &str) -> bool {
    arg == "-h" || arg == "--help"
}

/// Format flags for display: pair short+long together, e.g. "-s, --state"
fn format_flags(flags: &[&str]) -> Vec<String> {
    let mut result = Vec::new();
    let mut used: BTreeSet<usize> = BTreeSet::new();

    for (i, flag) in flags.iter().enumerate() {
        if used.contains(&i) {
            continue;
        }
        if flag.starts_with("--") {
            // Look for a preceding short flag (single dash, single char)
            let short = if i > 0
                && !used.contains(&(i - 1))
                && flags[i - 1].starts_with('-')
                && !flags[i - 1].starts_with("--")
            {
                used.insert(i - 1);
                Some(flags[i - 1])
            } else {
                None
            };
            used.insert(i);
            match short {
                Some(s) => result.push(format!("  {}, {}", s, flag)),
                None => result.push(format!("      {}", flag)),
            }
        } else if flag.starts_with('-') && !flag.starts_with("--") {
            // Short flag without a following long flag — check next
            if i + 1 < flags.len() && flags[i + 1].starts_with("--") {
                // Will be handled when we reach the long flag
                continue;
            }
            used.insert(i);
            result.push(format!("  {}", flag));
        }
    }
    result
}

fn help_toplevel() -> String {
    let mut groups: Vec<&str> = Vec::new();
    for cmd in COMMANDS {
        if !groups.contains(&cmd.group) {
            groups.push(cmd.group);
        }
    }
    for ext in EXT_COMMANDS {
        if !groups.contains(&ext.group) {
            groups.push(ext.group);
        }
    }

    let mut out =
        String::from("gh - GitHub CLI (proxy, restricted subset)\n\nAvailable command groups:\n");
    for group in &groups {
        let mut subs: Vec<&str> = COMMANDS
            .iter()
            .filter(|c| c.group == *group)
            .map(|c| c.subcommand)
            .collect();
        for ext in EXT_COMMANDS.iter().filter(|c| c.group == *group) {
            subs.push(ext.subcommand);
        }
        out.push_str(&format!("  {:12} {}\n", group, subs.join(", ")));
    }
    out.push_str("\nRun 'gh <command> -h' for more information about a command.\n");
    out.push_str(
        "Note: This is a sandboxed proxy. Only the commands listed above are available.\n",
    );
    out
}

fn help_group(group: &str) -> Option<String> {
    let cmds: Vec<&CommandDef> = COMMANDS.iter().filter(|c| c.group == group).collect();
    let exts: Vec<&ExtCommandDef> = EXT_COMMANDS.iter().filter(|c| c.group == group).collect();
    if cmds.is_empty() && exts.is_empty() {
        return None;
    }

    let mut out = format!("gh {} - available subcommands:\n\n", group);
    for cmd in &cmds {
        let rw = if cmd.is_write { " (write)" } else { "" };
        out.push_str(&format!("  {:12}{}\n", cmd.subcommand, rw));
    }
    for ext in &exts {
        out.push_str(&format!("  {:12} {}\n", ext.subcommand, ext.description));
    }
    out.push_str(&format!(
        "\nRun 'gh {} <subcommand> -h' for more information.\n",
        group
    ));
    Some(out)
}

fn help_command(group: &str, subcommand: &str) -> Option<String> {
    if let Some(ext) = find_ext_command(group, subcommand) {
        return Some(ext.help_text.to_string());
    }

    let cmd = find_command(group, subcommand)?;

    let rw = if cmd.is_write {
        " (write — current launch-snapshotted repo only, no -R/--repo)"
    } else {
        " (read)"
    };
    let mut out = format!("gh {} {}{}\n\nAllowed flags:\n", group, subcommand, rw);
    for line in format_flags(cmd.allowed_flags) {
        out.push_str(&line);
        out.push('\n');
    }
    Some(out)
}

/// Check if args represent a help request and return help text if so.
fn maybe_help(args: &[String]) -> Option<String> {
    // `gh` (no args)
    if args.is_empty() {
        return Some(help_toplevel());
    }

    // `gh -h` / `gh --help` / `gh help`
    if args.len() == 1 && (is_help_flag(&args[0]) || args[0] == "help") {
        return Some(help_toplevel());
    }

    // `gh help <group>` or `gh help <group> <sub>`
    if args[0] == "help" {
        if args.len() == 2 {
            return help_group(&args[1]).or_else(|| Some(help_toplevel()));
        }
        if args.len() >= 3 {
            return help_command(&args[1], &args[2]).or_else(|| help_group(&args[1]));
        }
    }

    // `gh <group> -h`
    if args.len() == 2 && is_help_flag(&args[1]) {
        return help_group(&args[0]).or_else(|| Some(help_toplevel()));
    }

    // `gh <group> <sub> -h` or any args containing -h/--help
    if args.len() >= 2 && args[2..].iter().any(|a| is_help_flag(a)) {
        return help_command(&args[0], &args[1]);
    }

    None
}

fn write_flag_takes_value(group: &str, subcommand: &str, flag: &str) -> bool {
    match (group, subcommand) {
        ("pr", "comment") | ("issue", "comment") => matches!(flag, "--body" | "-b"),
        ("issue", "create") => matches!(
            flag,
            "--title"
                | "-t"
                | "--body"
                | "-b"
                | "--label"
                | "-l"
                | "--assignee"
                | "-a"
                | "--milestone"
                | "-m"
                | "--project"
                | "-p"
                | "--template"
                | "-T"
        ),
        ("issue", "close") => matches!(
            flag,
            "--comment" | "-c" | "--reason" | "-r" | "--duplicate-of"
        ),
        ("issue", "edit") => !matches!(flag, "--remove-milestone"),
        ("run", "rerun") => matches!(flag, "--job" | "-j"),
        _ => false,
    }
}

fn write_positionals(args: &[String]) -> Result<Vec<&str>, String> {
    let group = args[0].as_str();
    let subcommand = args[1].as_str();
    let mut positionals = Vec::new();
    let mut index = 2;
    while index < args.len() {
        let arg = &args[index];
        if arg == "--" {
            return Err("the -- separator is not allowed for write commands".to_string());
        }
        if arg.starts_with('-') {
            let flag = extract_flag(arg);
            if !arg.contains('=') && write_flag_takes_value(group, subcommand, flag) {
                index += 1;
                if index >= args.len() {
                    return Err(format!("missing value for {flag}"));
                }
            }
        } else {
            positionals.push(arg.as_str());
        }
        index += 1;
    }
    Ok(positionals)
}

fn validate_item_target(
    value: &str,
    group: &str,
    grant: &RepositoryGrant,
    allow_branch: bool,
) -> Result<(), String> {
    match repository_from_item_url(value, group)? {
        Some(repository) if grant.repository.matches(&repository) => Ok(()),
        Some(repository) => Err(format!(
            "write target {}/{} is outside the current repository {}",
            repository.owner,
            repository.name,
            grant.repository.selector()
        )),
        None if value.bytes().all(|byte| byte.is_ascii_digit()) => Ok(()),
        None if allow_branch && !value.is_empty() => Ok(()),
        None => Err(format!("invalid {group} target: {value}")),
    }
}

fn validate_duplicate_targets(args: &[String], grant: &RepositoryGrant) -> Result<(), String> {
    let mut index = 2;
    while index < args.len() {
        if let Some(value) = args[index].strip_prefix("--duplicate-of=") {
            validate_item_target(value, "issue", grant, false)?;
        } else if args[index] == "--duplicate-of" {
            let value = args
                .get(index + 1)
                .ok_or_else(|| "missing value for --duplicate-of".to_string())?;
            validate_item_target(value, "issue", grant, false)?;
            index += 1;
        }
        index += 1;
    }
    Ok(())
}

fn validate_write_targets(args: &[String], grant: &RepositoryGrant) -> Result<(), String> {
    let positionals = write_positionals(args)?;
    match (args[0].as_str(), args[1].as_str()) {
        ("pr", "comment") if positionals.len() == 1 => {
            validate_item_target(positionals[0], "pr", grant, true)
        }
        ("issue", "create") if positionals.is_empty() => Ok(()),
        ("issue", "comment") if positionals.len() == 1 => {
            validate_item_target(positionals[0], "issue", grant, false)
        }
        ("issue", "close") if positionals.len() == 1 => {
            validate_item_target(positionals[0], "issue", grant, false)?;
            validate_duplicate_targets(args, grant)
        }
        ("issue", "edit") if !positionals.is_empty() => positionals
            .iter()
            .try_for_each(|target| validate_item_target(target, "issue", grant, false)),
        ("run", "rerun")
            if positionals.len() == 1
                && positionals[0].bytes().all(|byte| byte.is_ascii_digit()) =>
        {
            Ok(())
        }
        (group, subcommand) => Err(format!(
            "invalid positional arguments for gh {group} {subcommand}"
        )),
    }
}

fn has_repo_flag(args: &[String]) -> bool {
    args.iter()
        .any(|arg| matches!(arg.as_str(), "--repo" | "-R") || arg.starts_with("--repo="))
}

fn read_command_uses_current_repo(args: &[String]) -> bool {
    match (args[0].as_str(), args[1].as_str()) {
        ("auth", _) | ("repo", "list") | ("search", _) => false,
        ("repo", "view") => {
            let mut skip_value = false;
            for arg in &args[2..] {
                if skip_value {
                    skip_value = false;
                    continue;
                }
                if matches!(arg.as_str(), "--web" | "-w") {
                    continue;
                }
                if arg.starts_with('-') {
                    if !arg.contains('=') {
                        skip_value = true;
                    }
                    continue;
                }
                return false;
            }
            true
        }
        ("pr", "view" | "diff" | "checks") | ("issue", "view") => !args.iter().any(|arg| {
            repository_from_item_url(arg, &args[0])
                .ok()
                .flatten()
                .is_some()
        }),
        _ => true,
    }
}

fn reject_reason(args: &[String]) -> Option<String> {
    if args.len() == 1 && args[0] == "--version" {
        return None;
    }

    if args.len() < 2 {
        return Some(format!("command not allowed: gh {}", args.join(" ")));
    }

    let group = args[0].as_str();
    let subcommand = args[1].as_str();

    // Guide callers trying to use `gh api` for milestones to the extension commands
    if group == "api" && args[1..].iter().any(|a| a.contains("milestone")) {
        return Some(
            "command not allowed: gh api is not available. \
             Use 'gh ext milestone-list' and 'gh ext milestone-create <title>' instead."
                .to_string(),
        );
    }

    let cmd = match find_command(group, subcommand) {
        Some(c) => c,
        None => return Some(format!("command not allowed: gh {} {}", group, subcommand)),
    };

    if let Err(flag) = check_flags(args, cmd.allowed_flags) {
        return Some(format!(
            "flag not allowed for gh {} {}: {}",
            group, subcommand, flag
        ));
    }

    None
}

use crate::logging::log_line;

fn denied_response(reason: String) -> Response {
    Response {
        exit_code: 1,
        stdout: String::new(),
        stderr: format!("gh-proxy: {reason}"),
    }
}

fn handle_request(req: Request, log: &Arc<Mutex<File>>, config: &ProxyConfig) -> Response {
    let cmd_str = req.args.join(" ");

    if let Some(help_text) = maybe_help(&req.args) {
        log_line(log, &format!("HELP    gh {}", cmd_str));
        return Response {
            exit_code: 0,
            stdout: help_text,
            stderr: String::new(),
        };
    }

    if req.args.len() >= 2 && find_ext_command(&req.args[0], &req.args[1]).is_some() {
        let grant = match resolve_grant(config, req.cwd.as_deref()) {
            Ok(grant) => grant,
            Err(reason) => {
                log_line(log, &format!("DENIED  gh {} ({})", cmd_str, reason));
                return denied_response(reason);
            }
        };
        let response = maybe_ext_command(&req.args, grant, config)
            .expect("extension was resolved immediately before dispatch");
        let tag = if response.exit_code == 0 {
            "EXT"
        } else {
            "EXT_ERR"
        };
        log_line(
            log,
            &format!("{} gh {} -> {}", tag, cmd_str, response.exit_code),
        );
        return response;
    }

    if let Some(reason) = reject_reason(&req.args) {
        log_line(log, &format!("DENIED  gh {} ({})", cmd_str, reason));
        return denied_response(reason);
    }

    let mut execution_args = req.args.clone();
    if req.args.len() >= 2
        && let Some(command) = find_command(&req.args[0], &req.args[1])
    {
        if command.is_write {
            let grant = match resolve_grant(config, req.cwd.as_deref()) {
                Ok(grant) => grant,
                Err(reason) => {
                    log_line(log, &format!("DENIED  gh {} ({})", cmd_str, reason));
                    return denied_response(reason);
                }
            };
            if let Err(reason) = validate_write_targets(&req.args, grant) {
                log_line(log, &format!("DENIED  gh {} ({})", cmd_str, reason));
                return denied_response(reason);
            }
            append_repo_selector(&mut execution_args, &grant.repository);
            log_line(
                log,
                &format!("ROUTED  gh {} -> {}", cmd_str, grant.repository.selector()),
            );
        } else if !has_repo_flag(&req.args) && read_command_uses_current_repo(&req.args) {
            match resolve_grant(config, req.cwd.as_deref()) {
                Ok(grant) => {
                    append_repo_selector(&mut execution_args, &grant.repository);
                    log_line(
                        log,
                        &format!("ROUTED  gh {} -> {}", cmd_str, grant.repository.selector()),
                    );
                }
                Err(reason) => {
                    log_line(log, &format!("DENIED  gh {} ({})", cmd_str, reason));
                    return denied_response(reason);
                }
            }
        }
    }

    log_line(log, &format!("ALLOWED gh {}", cmd_str));

    match safe_gh_command(config).args(&execution_args).output() {
        Ok(output) => {
            let exit_code = output.status.code().unwrap_or(1);
            log_line(log, &format!("EXIT    gh {} -> {}", cmd_str, exit_code));
            Response {
                exit_code,
                stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            }
        }
        Err(e) => {
            log_line(log, &format!("ERROR   gh {} ({})", cmd_str, e));
            Response {
                exit_code: 1,
                stdout: String::new(),
                stderr: format!("gh-proxy: failed to execute gh: {}", e),
            }
        }
    }
}

pub fn run(socket_path: &str, log_path: &Path, workspace_root: &Path) {
    let path = Path::new(socket_path);
    let log_file = proxy_log::open(log_path).unwrap_or_else(|e| {
        eprintln!("gh-proxy: failed to open log {}: {}", log_path.display(), e);
        std::process::exit(1);
    });
    let log = Arc::new(Mutex::new(log_file));

    let workspace_root = fs::canonicalize(workspace_root).unwrap_or_else(|error| {
        eprintln!(
            "gh-proxy: failed to resolve workspace root {}: {}",
            workspace_root.display(),
            error
        );
        std::process::exit(1);
    });
    let grants = discover_repository_grants(&workspace_root).unwrap_or_else(|error| {
        eprintln!("gh-proxy: failed to snapshot workspace repositories: {error}");
        std::process::exit(1);
    });
    let neutral_dir =
        fs::canonicalize(path.parent().unwrap_or(Path::new("/"))).unwrap_or_else(|error| {
            eprintln!("gh-proxy: failed to resolve neutral directory: {error}");
            std::process::exit(1);
        });
    let config = Arc::new(ProxyConfig {
        workspace_root,
        neutral_dir,
        grants,
    });

    let bound = proxy_socket::bind(path).unwrap_or_else(|e| {
        eprintln!("gh-proxy: failed to bind {}: {}", socket_path, e);
        std::process::exit(1);
    });
    let listener = bound.listener;
    let socket_identity = bound.identity;

    log_line(
        &log,
        &format!(
            "listening on {} with {} launch-approved repositories",
            socket_path,
            config.grants.len()
        ),
    );

    // Watchdog: exit when parent process (podman after exec) dies.
    // After exec(), our ppid is podman's PID. When podman exits, ppid
    // becomes 1 (init). Poll every 2s and clean up when that happens.
    let parent_pid = std::os::unix::process::parent_id();
    let watchdog_socket = socket_identity.clone();
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
                let _ = watchdog_socket.remove_if_owned();
                process::exit(0);
            }
        }
    });

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let log = Arc::clone(&log);
                let config = Arc::clone(&config);
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
                            Ok(req) => handle_request(req, &log, &config),
                            Err(e) => {
                                log_line(&log, &format!("INVALID ({})", e));
                                Response {
                                    exit_code: 1,
                                    stdout: String::new(),
                                    stderr: format!("gh-proxy: invalid request: {}", e),
                                }
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

    // ── Read commands ──────────────────────────────────────────────

    #[test]
    fn test_read_commands_allowed() {
        assert!(reject_reason(&strs(&["--version"])).is_none());
        assert!(reject_reason(&strs(&["auth", "status"])).is_none());
        assert!(reject_reason(&strs(&["auth", "status", "--hostname", "github.com"])).is_none());
        assert!(reject_reason(&strs(&["pr", "list"])).is_none());
        assert!(reject_reason(&strs(&["pr", "list", "--state", "open"])).is_none());
        assert!(reject_reason(&strs(&["pr", "view", "123", "--json", "title"])).is_none());
        assert!(reject_reason(&strs(&["pr", "diff", "123"])).is_none());
        assert!(reject_reason(&strs(&["pr", "checks", "123"])).is_none());
        assert!(reject_reason(&strs(&["issue", "list", "--limit", "10"])).is_none());
        assert!(reject_reason(&strs(&["issue", "view", "42", "--comments"])).is_none());
        assert!(reject_reason(&strs(&["repo", "view", "--json", "description"])).is_none());
        assert!(reject_reason(&strs(&["repo", "list", "--limit", "30"])).is_none());
        assert!(reject_reason(&strs(&["release", "list"])).is_none());
        assert!(reject_reason(&strs(&["release", "view", "v1.0"])).is_none());
        assert!(reject_reason(&strs(&["run", "list"])).is_none());
        assert!(reject_reason(&strs(&["run", "view", "12345", "--log"])).is_none());
        assert!(reject_reason(&strs(&["run", "watch", "12345", "--exit-status"])).is_none());
        assert!(reject_reason(&strs(&["run", "watch", "12345", "-i", "15"])).is_none());
        assert!(reject_reason(&strs(&["workflow", "list", "--all"])).is_none());
        assert!(reject_reason(&strs(&["search", "code", "panic", "--repo", "cli/cli"])).is_none());
        assert!(
            reject_reason(&strs(&["search", "commits", "fix", "--author", "octocat"])).is_none()
        );
        assert!(reject_reason(&strs(&["search", "issues", "bug", "--state", "open"])).is_none());
        assert!(reject_reason(&strs(&["search", "prs", "fix", "--draft"])).is_none());
        assert!(reject_reason(&strs(&["search", "repos", "cli", "--stars", ">100"])).is_none());
    }

    #[test]
    fn test_auth_status_blocks_token_output() {
        assert!(reject_reason(&strs(&["auth", "status", "--show-token"])).is_some());
        assert!(reject_reason(&strs(&["auth", "status", "-t"])).is_some());
        assert!(reject_reason(&strs(&["auth", "token"])).is_some());
    }

    #[test]
    fn test_read_commands_allow_repo_flag() {
        assert!(reject_reason(&strs(&["pr", "list", "-R", "owner/repo"])).is_none());
        assert!(reject_reason(&strs(&["pr", "list", "--repo", "owner/repo"])).is_none());
        assert!(reject_reason(&strs(&["issue", "view", "1", "--repo=owner/repo"])).is_none());
    }

    // ── Write commands ─────────────────────────────────────────────

    #[test]
    fn test_write_commands_allowed() {
        assert!(reject_reason(&strs(&["pr", "comment", "123", "--body", "hi"])).is_none());
        assert!(reject_reason(&strs(&["issue", "create", "--title", "bug"])).is_none());
        assert!(reject_reason(&strs(&["issue", "comment", "42", "--body", "x"])).is_none());
        assert!(reject_reason(&strs(&["issue", "close", "42"])).is_none());
        assert!(reject_reason(&strs(&["issue", "close", "42", "--reason", "completed"])).is_none());
        assert!(reject_reason(&strs(&["issue", "close", "42", "--comment", "done"])).is_none());
        assert!(reject_reason(&strs(&["issue", "edit", "42", "--title", "new"])).is_none());
        assert!(reject_reason(&strs(&["run", "rerun", "12345"])).is_none());
        assert!(reject_reason(&strs(&["run", "rerun", "12345", "--failed"])).is_none());
        assert!(
            reject_reason(&strs(&[
                "issue",
                "edit",
                "42",
                "--add-label",
                "bug",
                "--milestone",
                "v1"
            ]))
            .is_none()
        );
    }

    #[test]
    fn test_write_commands_block_repo_flag() {
        let r = reject_reason(&strs(&[
            "issue",
            "create",
            "-R",
            "other/repo",
            "--title",
            "foo",
        ]));
        assert!(r.is_some());
        assert!(r.unwrap().contains("flag not allowed"));

        assert!(reject_reason(&strs(&["issue", "create", "--repo", "other/repo"])).is_some());
        assert!(reject_reason(&strs(&["issue", "comment", "1", "-R", "other/repo"])).is_some());
        assert!(reject_reason(&strs(&["issue", "close", "42", "-R", "other/repo"])).is_some());
        assert!(reject_reason(&strs(&["issue", "edit", "42", "--repo", "other/repo"])).is_some());
        assert!(reject_reason(&strs(&["run", "rerun", "1", "-R", "other/repo"])).is_some());
    }

    #[test]
    fn test_write_commands_block_body_file() {
        assert!(reject_reason(&strs(&["pr", "comment", "1", "-F", "file.txt"])).is_some());
        assert!(reject_reason(&strs(&["issue", "create", "--body-file", "f"])).is_some());
        assert!(reject_reason(&strs(&["issue", "edit", "42", "--body-file", "f"])).is_some());
        assert!(reject_reason(&strs(&["issue", "edit", "42", "-F", "f"])).is_some());
    }

    // ── Flag whitelist enforcement ─────────────────────────────────

    #[test]
    fn test_unknown_flags_rejected() {
        let r = reject_reason(&strs(&["pr", "list", "--some-future-flag"]));
        assert!(r.is_some());
        assert!(r.unwrap().contains("flag not allowed"));
    }

    #[test]
    fn test_long_flag_with_equals() {
        assert!(reject_reason(&strs(&["pr", "list", "--state=open"])).is_none());
        assert!(reject_reason(&strs(&["pr", "list", "--bogus=value"])).is_some());
    }

    #[test]
    fn test_double_dash_separator() {
        // After --, anything goes (treated as positional)
        assert!(reject_reason(&strs(&["pr", "list", "--", "--not-a-flag"])).is_none());
    }

    #[test]
    fn test_positional_args_allowed() {
        assert!(reject_reason(&strs(&["pr", "view", "123"])).is_none());
        assert!(reject_reason(&strs(&["issue", "view", "42"])).is_none());
        assert!(reject_reason(&strs(&["release", "view", "v1.0.0"])).is_none());
    }

    // ── Disallowed commands ────────────────────────────────────────

    #[test]
    fn test_disallowed_commands() {
        assert!(reject_reason(&strs(&["api", "repos"])).is_some());
        assert!(reject_reason(&strs(&["auth", "login"])).is_some());
        assert!(reject_reason(&strs(&["secret", "set"])).is_some());
        assert!(reject_reason(&strs(&["ssh-key", "list"])).is_some());
        assert!(reject_reason(&strs(&["gpg-key", "list"])).is_some());
        assert!(reject_reason(&strs(&["pr", "merge", "123"])).is_some());
        assert!(reject_reason(&strs(&["pr", "close", "123"])).is_some());
        assert!(reject_reason(&strs(&["pr", "edit", "123"])).is_some());
        assert!(reject_reason(&strs(&["issue", "delete", "42"])).is_some());
        assert!(reject_reason(&strs(&["repo", "create"])).is_some());
        assert!(reject_reason(&strs(&["repo", "delete"])).is_some());
        assert!(reject_reason(&strs(&["release", "create"])).is_some());
        assert!(reject_reason(&strs(&["release", "delete"])).is_some());
        assert!(reject_reason(&strs(&["run", "cancel"])).is_some());
        assert!(reject_reason(&strs(&["pr", "create", "--title", "new PR"])).is_some());
    }

    #[test]
    fn test_empty_args() {
        assert!(reject_reason(&[]).is_some());
    }

    #[test]
    fn test_single_arg() {
        assert!(reject_reason(&strs(&["pr"])).is_some());
    }

    // ── Help ────────────────────────────────────────────────────────

    #[test]
    fn test_help_toplevel() {
        let h = maybe_help(&[]).unwrap();
        assert!(h.contains("pr"));
        assert!(h.contains("issue"));
        assert!(h.contains("repo"));
        assert!(h.contains("release"));
        assert!(h.contains("run"));
        assert!(h.contains("auth"));
        assert!(h.contains("workflow"));
        assert!(h.contains("search"));

        // Also triggered by -h, --help, help
        assert!(maybe_help(&strs(&["--help"])).is_some());
        assert!(maybe_help(&strs(&["-h"])).is_some());
        assert!(maybe_help(&strs(&["help"])).is_some());
    }

    #[test]
    fn test_help_group() {
        let h = maybe_help(&strs(&["pr", "-h"])).unwrap();
        assert!(h.contains("list"));
        assert!(h.contains("view"));
        assert!(!h.contains("create"));
        assert!(h.contains("comment"));

        // Via `gh help pr`
        let h2 = maybe_help(&strs(&["help", "pr"])).unwrap();
        assert!(h2.contains("list"));
    }

    #[test]
    fn test_help_command() {
        let h = maybe_help(&strs(&["pr", "list", "--help"])).unwrap();
        assert!(h.contains("--state"));
        assert!(h.contains("--limit"));
        assert!(h.contains("--json"));
        assert!(h.contains("(read)"));

        // Write command shows workspace restriction
        let h2 = maybe_help(&strs(&["pr", "comment", "-h"])).unwrap();
        assert!(h2.contains("--body"));
        assert!(h2.contains("launch-snapshotted repo only"));

        // Via `gh help pr list`
        let h3 = maybe_help(&strs(&["help", "pr", "list"])).unwrap();
        assert!(h3.contains("--state"));
    }

    #[test]
    fn test_help_unknown_group_falls_back() {
        // Unknown group via `gh help bogus` falls back to toplevel
        let h = maybe_help(&strs(&["help", "bogus"])).unwrap();
        assert!(h.contains("Available command groups"));
    }

    #[test]
    fn test_no_help_for_normal_commands() {
        assert!(maybe_help(&strs(&["pr", "list", "--state", "open"])).is_none());
        assert!(maybe_help(&strs(&["pr", "view", "123"])).is_none());
    }

    // ── Extension commands (gh ext) ──────────────────────────────────

    fn test_config() -> ProxyConfig {
        let root = fs::canonicalize(env!("CARGO_MANIFEST_DIR")).unwrap();
        let handle = File::open(&root).unwrap();
        let metadata = handle.metadata().unwrap();
        ProxyConfig {
            workspace_root: root.clone(),
            neutral_dir: root.clone(),
            grants: vec![RepositoryGrant {
                root,
                handle,
                device: metadata.dev(),
                inode: metadata.ino(),
                repository: RepositoryId {
                    host: "github.com".to_string(),
                    owner: "example".to_string(),
                    name: "project".to_string(),
                },
            }],
        }
    }

    fn temporary_workspace(label: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "claude-sandbox-gh-proxy-{label}-{}-{nonce}",
            std::process::id()
        ))
    }

    fn create_test_repository(path: &Path, origin: &str) {
        let git_dir = path.join(".git");
        fs::create_dir_all(&git_dir).unwrap();
        fs::create_dir_all(git_dir.join("objects")).unwrap();
        fs::create_dir_all(git_dir.join("refs/heads")).unwrap();
        fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        fs::write(
            git_dir.join("config"),
            format!(
                "[core]\n\trepositoryformatversion = 0\n\tbare = false\n[remote \"origin\"]\n\turl = {origin}\n"
            ),
        )
        .unwrap();
    }

    fn ext_response(args: &[&str]) -> Option<Response> {
        let config = test_config();
        maybe_ext_command(&strs(args), &config.grants[0], &config)
    }

    #[test]
    fn test_repository_remote_parsing() {
        let expected = RepositoryId {
            host: "github.com".to_string(),
            owner: "octocat".to_string(),
            name: "hello-world".to_string(),
        };
        assert_eq!(
            repository_from_remote("git@github.com:octocat/hello-world.git"),
            Some(expected.clone())
        );
        assert_eq!(
            repository_from_remote("https://github.com/octocat/hello-world.git"),
            Some(expected.clone())
        );
        assert_eq!(
            repository_from_remote("ssh://git@github.com/octocat/hello-world.git"),
            Some(expected)
        );
        assert!(repository_from_remote("file:///private/repo").is_none());
        assert!(repository_from_remote("https://github.com/too/many/parts").is_none());

        let enterprise = RepositoryId {
            host: "github.example.com".to_string(),
            owner: "team".to_string(),
            name: "project".to_string(),
        };
        assert_eq!(enterprise.selector(), "github.example.com/team/project");
        assert_eq!(
            enterprise.api_path("/milestones"),
            "/repos/team/project/milestones"
        );
    }

    #[test]
    fn test_repository_grants_are_a_launch_snapshot() {
        let root = temporary_workspace("snapshot");
        let outside = temporary_workspace("outside-gitdir");
        create_test_repository(&root.join("one"), "git@github.com:example/one.git");
        create_test_repository(
            &root.join("group/two"),
            "https://github.com/example/two.git",
        );
        create_test_repository(&outside, "git@github.com:private/outside.git");
        let misleading = root.join("misleading");
        fs::create_dir_all(&misleading).unwrap();
        fs::write(
            misleading.join(".git"),
            format!("gitdir: {}\n", outside.join(".git").display()),
        )
        .unwrap();
        create_test_repository(
            &misleading.join("child"),
            "git@github.com:example/child.git",
        );
        let include_only = root.join("include-only");
        create_test_repository(&include_only, "git@github.com:unused/overwritten.git");
        fs::write(
            include_only.join(".git/config"),
            format!(
                "[core]\n\trepositoryformatversion = 0\n[include]\n\tpath = {}\n",
                outside.join(".git/config").display()
            ),
        )
        .unwrap();
        create_test_repository(
            &include_only.join("child"),
            "git@github.com:example/include-child.git",
        );

        let grants = discover_repository_grants(&root).unwrap();
        assert_eq!(grants.len(), 4);
        assert!(
            grants
                .iter()
                .all(|grant| grant.repository.selector() != "private/outside")
        );
        let workspace_root = fs::canonicalize(&root).unwrap();
        let config = ProxyConfig {
            neutral_dir: workspace_root.clone(),
            workspace_root,
            grants,
        };
        assert_eq!(
            resolve_grant(&config, Some("/workspace/one"))
                .unwrap()
                .repository
                .selector(),
            "example/one"
        );

        create_test_repository(&root.join("three"), "git@github.com:example/three.git");
        assert!(resolve_grant(&config, Some("/workspace/three")).is_err());

        drop(config);
        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(outside).unwrap();
    }

    #[test]
    fn test_write_targets_cannot_escape_current_repository() {
        let config = test_config();
        let grant = &config.grants[0];
        assert!(
            validate_write_targets(
                &strs(&[
                    "issue",
                    "comment",
                    "https://github.com/example/project/issues/12",
                    "--body",
                    "ok"
                ]),
                grant
            )
            .is_ok()
        );
        assert!(
            validate_write_targets(
                &strs(&[
                    "issue",
                    "comment",
                    "https://github.com/other/project/issues/12",
                    "--body",
                    "no"
                ]),
                grant
            )
            .is_err()
        );
        assert!(
            validate_write_targets(
                &strs(&[
                    "issue",
                    "close",
                    "12",
                    "--duplicate-of",
                    "https://github.com/other/project/issues/9"
                ]),
                grant
            )
            .is_err()
        );
        assert!(validate_write_targets(&strs(&["run", "rerun", "not-a-run-id"]), grant).is_err());
    }

    #[test]
    fn test_urls_in_write_bodies_are_not_treated_as_targets() {
        let config = test_config();
        assert!(
            validate_write_targets(
                &strs(&[
                    "issue",
                    "create",
                    "--title",
                    "link",
                    "--body",
                    "https://github.com/other/project/issues/12"
                ]),
                &config.grants[0]
            )
            .is_ok()
        );
    }

    #[test]
    fn test_repo_selector_is_added_before_separator() {
        let mut args = strs(&["pr", "list", "--", "value"]);
        append_repo_selector(&mut args, &test_config().grants[0].repository);
        assert_eq!(
            args,
            strs(&["pr", "list", "--repo", "example/project", "--", "value"])
        );
    }

    #[test]
    fn test_ext_run_logs_valid_id() {
        assert!(find_ext_command("ext", "run-logs").is_some());
    }

    #[test]
    fn test_ext_run_logs_rejects_non_numeric_id() {
        let r = ext_response(&["ext", "run-logs", "../etc/passwd"]).unwrap();
        assert_eq!(r.exit_code, 1);
        assert!(r.stderr.contains("invalid run id"));
    }

    #[test]
    fn test_ext_run_logs_missing_id() {
        let r = ext_response(&["ext", "run-logs"]).unwrap();
        assert_eq!(r.exit_code, 1);
        assert!(r.stderr.contains("usage"));
    }

    #[test]
    fn test_ext_run_logs_rejects_extra_arguments() {
        let r = ext_response(&["ext", "run-logs", "123", "extra"]).unwrap();
        assert_eq!(r.exit_code, 1);
        assert!(r.stderr.contains("usage"));
    }

    #[test]
    fn test_ext_not_matched_for_other_commands() {
        assert!(ext_response(&["pr", "list"]).is_none());
        assert!(ext_response(&["run", "list"]).is_none());
        assert!(ext_response(&["run", "logs"]).is_none());
    }

    #[test]
    fn test_ext_run_logs_help() {
        let h = maybe_help(&strs(&["ext", "run-logs", "-h"])).unwrap();
        assert!(h.contains("run-id"));
        assert!(h.contains("launch-snapshotted repo only"));
    }

    #[test]
    fn test_ext_group_help() {
        let h = maybe_help(&strs(&["ext", "-h"])).unwrap();
        assert!(h.contains("run-logs"));
    }

    #[test]
    fn test_toplevel_help_includes_ext() {
        let h = maybe_help(&strs(&["-h"])).unwrap();
        assert!(h.contains("ext"));
    }

    // ── Milestone extension commands ──────────────────────────────

    #[test]
    fn test_ext_milestone_create_missing_title() {
        let r = ext_response(&["ext", "milestone-create"]).unwrap();
        assert_eq!(r.exit_code, 1);
        assert!(r.stderr.contains("usage"));
    }

    #[test]
    fn test_ext_milestone_create_unknown_flag() {
        let r = ext_response(&["ext", "milestone-create", "v1", "--bogus"]).unwrap();
        assert_eq!(r.exit_code, 1);
        assert!(r.stderr.contains("unknown flag"));
    }

    #[test]
    fn test_ext_milestone_create_extra_positional() {
        let r = ext_response(&["ext", "milestone-create", "v1", "extra"]).unwrap();
        assert_eq!(r.exit_code, 1);
        assert!(r.stderr.contains("unexpected positional"));
    }

    #[test]
    fn test_ext_milestone_create_rejects_missing_flag_value() {
        let r = ext_response(&["ext", "milestone-create", "v1", "--description"]).unwrap();
        assert_eq!(r.exit_code, 1);
        assert!(r.stderr.contains("missing value"));
    }

    #[test]
    fn test_ext_milestone_list_unknown_flag() {
        let r = ext_response(&["ext", "milestone-list", "--bogus"]).unwrap();
        assert_eq!(r.exit_code, 1);
        assert!(r.stderr.contains("unknown flag"));
    }

    #[test]
    fn test_ext_milestone_list_invalid_state() {
        let r = ext_response(&["ext", "milestone-list", "--state", "invalid"]).unwrap();
        assert_eq!(r.exit_code, 1);
        assert!(r.stderr.contains("invalid state"));
    }

    #[test]
    fn test_ext_milestone_list_rejects_missing_state() {
        let r = ext_response(&["ext", "milestone-list", "--state"]).unwrap();
        assert_eq!(r.exit_code, 1);
        assert!(r.stderr.contains("missing value"));
    }

    #[test]
    fn test_ext_milestone_list_unexpected_positional() {
        let r = ext_response(&["ext", "milestone-list", "something"]).unwrap();
        assert_eq!(r.exit_code, 1);
        assert!(r.stderr.contains("unexpected positional"));
    }

    #[test]
    fn test_ext_milestone_help() {
        let h = maybe_help(&strs(&["ext", "milestone-create", "-h"])).unwrap();
        assert!(h.contains("--description"));
        assert!(h.contains("--due-on"));

        let h = maybe_help(&strs(&["ext", "milestone-list", "-h"])).unwrap();
        assert!(h.contains("--state"));
    }

    #[test]
    fn test_ext_group_help_includes_milestones() {
        let h = maybe_help(&strs(&["ext", "-h"])).unwrap();
        assert!(h.contains("milestone-create"));
        assert!(h.contains("milestone-list"));
    }

    // ── API milestone hint ────────────────────────────────────────

    #[test]
    fn test_api_milestone_hint() {
        let r = reject_reason(&strs(&["api", "/repos/owner/repo/milestones"]));
        assert!(r.is_some());
        let msg = r.unwrap();
        assert!(msg.contains("gh ext milestone-list"));
        assert!(msg.contains("gh ext milestone-create"));
    }

    #[test]
    fn test_api_non_milestone_no_hint() {
        let r = reject_reason(&strs(&["api", "/repos/owner/repo/releases"]));
        assert!(r.is_some());
        let msg = r.unwrap();
        assert!(!msg.contains("milestone"));
    }

    fn strs(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| x.to_string()).collect()
    }
}
