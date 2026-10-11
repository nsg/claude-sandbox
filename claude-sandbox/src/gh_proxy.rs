use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::os::unix::io::AsRawFd;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use std::{fs, process, thread};

use crate::{managed_gh, proxy_log, proxy_socket};

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
    /// Always host-qualified: gh resolves a bare `owner/name` against its
    /// default host, which the host environment's `GH_HOST` can redirect.
    fn selector(&self) -> String {
        format!("{}/{}/{}", self.host, self.owner, self.name)
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
    allow_merge: bool,
    managed_gh_state: Option<PathBuf>,
    managed_gh_lock: Mutex<()>,
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
        group: "pr",
        subcommand: "create",
        is_write: true,
        allowed_flags: &[
            "--title", "-t", "--body", "-b", "--head", "-H", "--base", "-B", "--draft", "-d",
            "--repo", "-R",
        ],
    },
    CommandDef {
        group: "pr",
        subcommand: "edit",
        is_write: true,
        allowed_flags: &[
            "--title", "-t", "--body", "-b", "--base", "-B", "--repo", "-R",
        ],
    },
    CommandDef {
        group: "pr",
        subcommand: "ready",
        is_write: true,
        allowed_flags: &["--undo", "--repo", "-R"],
    },
    CommandDef {
        group: "pr",
        subcommand: "close",
        is_write: true,
        allowed_flags: &["--comment", "-c", "--repo", "-R"],
    },
    CommandDef {
        group: "pr",
        subcommand: "reopen",
        is_write: true,
        allowed_flags: &["--comment", "-c", "--repo", "-R"],
    },
    // Opt-in (--allow-merge) and planned by plan_pr_merge: no --admin,
    // --delete-branch, --body-file/-F or --author-email.
    CommandDef {
        group: "pr",
        subcommand: "merge",
        is_write: true,
        allowed_flags: &[
            "-m",
            "--merge",
            "-s",
            "--squash",
            "-r",
            "--rebase",
            "--auto",
            "--disable-auto",
            "--match-head-commit",
            "-t",
            "--subject",
            "-b",
            "--body",
            "-R",
            "--repo",
        ],
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
    CommandDef {
        group: "issue",
        subcommand: "reopen",
        is_write: true,
        allowed_flags: &["--comment", "-c", "--repo", "-R"],
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

const ACCOUNT_PROBE_HELP: &str = "gh api user --jq .login (read-only account-name probe)\n";

const MERGE_DISABLED: &str = "gh pr merge is disabled for this session; \
                              relaunch with 'claude-sandbox --allow-merge' to enable it";

const MERGE_HELP: &str = "\nOnly available when the sandbox was launched with --allow-merge.\n\
                          Takes one pull request number or URL in the current repository and exactly\n\
                          one of --merge, --squash or --rebase; --disable-auto stands alone.\n\
                          -R/--repo may only name the current repository.\n\
                          Pull requests from forks are refused, and branch protection is never\n\
                          bypassed (--admin is unavailable).\n";

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

pub(crate) fn proc_fd_path(handle: &File) -> PathBuf {
    PathBuf::from(format!(
        "/proc/{}/fd/{}",
        std::process::id(),
        handle.as_raw_fd()
    ))
}

pub(crate) fn open_untrusted_path(path: &Path) -> Option<File> {
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
    command.args(["api", "--hostname", &repository.host]);
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
    out.push_str("  api          user --jq .login\n");
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
    if group == "api" {
        return Some(ACCOUNT_PROBE_HELP.to_string());
    }

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
    if group == "api" && subcommand == "user" {
        return Some(ACCOUNT_PROBE_HELP.to_string());
    }

    if let Some(ext) = find_ext_command(group, subcommand) {
        return Some(ext.help_text.to_string());
    }

    let cmd = find_command(group, subcommand)?;

    let rw = if !cmd.is_write {
        " (read)"
    } else if cmd.allowed_flags.contains(&"--repo") {
        " (write — current launch-snapshotted repo only)"
    } else {
        " (write — current launch-snapshotted repo only, no -R/--repo)"
    };
    let mut out = format!("gh {} {}{}\n\nAllowed flags:\n", group, subcommand, rw);
    for line in format_flags(cmd.allowed_flags) {
        out.push_str(&line);
        out.push('\n');
    }
    if is_pr_merge(group, subcommand) {
        out.push_str(MERGE_HELP);
    }
    if group == "pr" && subcommand == "create" {
        out.push_str("\n--head is required because the host does not read the worktree; the branch must already be pushed.\n");
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
    if matches!(flag, "--repo" | "-R") {
        return true;
    }
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

fn is_pr_merge(group: &str, subcommand: &str) -> bool {
    group == "pr" && subcommand == "merge"
}

fn repository_from_selector(selector: &str) -> Option<RepositoryId> {
    match selector.split_once('/') {
        Some((host, path)) if path.contains('/') => repository_from_path(host, path),
        _ => repository_from_path("github.com", selector),
    }
}

fn strict_repository_from_selector(selector: &str) -> Option<RepositoryId> {
    let parts: Vec<&str> = selector.split('/').collect();
    let (host, owner, name) = match parts.as_slice() {
        [owner, name] => ("github.com", *owner, *name),
        [host, owner, name] => (*host, *owner, *name),
        _ => return None,
    };
    if !valid_repository_part(host) || !valid_repository_part(owner) || !valid_repository_part(name)
    {
        return None;
    }
    Some(RepositoryId {
        host: host.to_string(),
        owner: owner.to_string(),
        name: name.to_string(),
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ManagedTarget {
    None,
    Issue,
    PullRequest,
}

#[derive(Clone, Copy)]
struct ManagedFlag {
    long: &'static str,
    short: Option<&'static str>,
    required: bool,
    bare: bool,
}

struct ManagedCommand {
    group: &'static str,
    subcommand: &'static str,
    target: ManagedTarget,
    flags: &'static [ManagedFlag],
    new: bool,
}

const TITLE_BODY_REQUIRED: &[ManagedFlag] = &[
    ManagedFlag {
        long: "--title",
        short: Some("-t"),
        required: true,
        bare: false,
    },
    ManagedFlag {
        long: "--body",
        short: Some("-b"),
        required: true,
        bare: false,
    },
];
const BODY_REQUIRED: &[ManagedFlag] = &[ManagedFlag {
    long: "--body",
    short: Some("-b"),
    required: true,
    bare: false,
}];
const ISSUE_CLOSE_FLAGS: &[ManagedFlag] = &[
    ManagedFlag {
        long: "--comment",
        short: Some("-c"),
        required: false,
        bare: false,
    },
    ManagedFlag {
        long: "--reason",
        short: Some("-r"),
        required: false,
        bare: false,
    },
];
const EDIT_FLAGS: &[ManagedFlag] = &[
    ManagedFlag {
        long: "--title",
        short: Some("-t"),
        required: false,
        bare: false,
    },
    ManagedFlag {
        long: "--body",
        short: Some("-b"),
        required: false,
        bare: false,
    },
];
const PR_CREATE_FLAGS: &[ManagedFlag] = &[
    ManagedFlag {
        long: "--title",
        short: Some("-t"),
        required: true,
        bare: false,
    },
    ManagedFlag {
        long: "--body",
        short: Some("-b"),
        required: true,
        bare: false,
    },
    ManagedFlag {
        long: "--head",
        short: Some("-H"),
        required: true,
        bare: false,
    },
    ManagedFlag {
        long: "--base",
        short: Some("-B"),
        required: false,
        bare: false,
    },
    ManagedFlag {
        long: "--draft",
        short: Some("-d"),
        required: false,
        bare: true,
    },
];
const PR_EDIT_FLAGS: &[ManagedFlag] = &[
    ManagedFlag {
        long: "--title",
        short: Some("-t"),
        required: false,
        bare: false,
    },
    ManagedFlag {
        long: "--body",
        short: Some("-b"),
        required: false,
        bare: false,
    },
    ManagedFlag {
        long: "--base",
        short: Some("-B"),
        required: false,
        bare: false,
    },
];
const READY_FLAGS: &[ManagedFlag] = &[ManagedFlag {
    long: "--undo",
    short: None,
    required: false,
    bare: true,
}];
const COMMENT_OPTIONAL: &[ManagedFlag] = &[ManagedFlag {
    long: "--comment",
    short: Some("-c"),
    required: false,
    bare: false,
}];

const MANAGED_COMMANDS: &[ManagedCommand] = &[
    ManagedCommand {
        group: "issue",
        subcommand: "create",
        target: ManagedTarget::None,
        flags: TITLE_BODY_REQUIRED,
        new: false,
    },
    ManagedCommand {
        group: "issue",
        subcommand: "comment",
        target: ManagedTarget::Issue,
        flags: BODY_REQUIRED,
        new: false,
    },
    ManagedCommand {
        group: "issue",
        subcommand: "close",
        target: ManagedTarget::Issue,
        flags: ISSUE_CLOSE_FLAGS,
        new: false,
    },
    ManagedCommand {
        group: "issue",
        subcommand: "edit",
        target: ManagedTarget::Issue,
        flags: EDIT_FLAGS,
        new: false,
    },
    ManagedCommand {
        group: "pr",
        subcommand: "comment",
        target: ManagedTarget::PullRequest,
        flags: BODY_REQUIRED,
        new: false,
    },
    ManagedCommand {
        group: "pr",
        subcommand: "create",
        target: ManagedTarget::None,
        flags: PR_CREATE_FLAGS,
        new: true,
    },
    ManagedCommand {
        group: "pr",
        subcommand: "edit",
        target: ManagedTarget::PullRequest,
        flags: PR_EDIT_FLAGS,
        new: true,
    },
    ManagedCommand {
        group: "pr",
        subcommand: "ready",
        target: ManagedTarget::PullRequest,
        flags: READY_FLAGS,
        new: true,
    },
    ManagedCommand {
        group: "pr",
        subcommand: "close",
        target: ManagedTarget::PullRequest,
        flags: COMMENT_OPTIONAL,
        new: true,
    },
    ManagedCommand {
        group: "pr",
        subcommand: "reopen",
        target: ManagedTarget::PullRequest,
        flags: COMMENT_OPTIONAL,
        new: true,
    },
    ManagedCommand {
        group: "issue",
        subcommand: "reopen",
        target: ManagedTarget::Issue,
        flags: COMMENT_OPTIONAL,
        new: true,
    },
];

fn managed_command(args: &[String]) -> Option<&'static ManagedCommand> {
    let (group, subcommand) = (args.first()?, args.get(1)?);
    MANAGED_COMMANDS
        .iter()
        .find(|command| command.group == group && command.subcommand == subcommand)
}

pub(crate) fn refused_text_code_point(value: &str) -> Option<char> {
    value.chars().find(|character| {
        let code = *character as u32;
        (code <= 0x1f && !matches!(*character, '\n' | '\t'))
            || (0x7f..=0x9f).contains(&code)
            || matches!(code, 0x00ad | 0x061c | 0x180e | 0xfeff)
            || (0x200b..=0x200f).contains(&code)
            || (0x2028..=0x202f).contains(&code)
            || (0x2060..=0x206f).contains(&code)
            || (0xfe00..=0xfe0e).contains(&code)
            || (0xfff9..=0xfffb).contains(&code)
            || (0xe0000..=0xe0fff).contains(&code)
            || (0xe0100..=0xe01ef).contains(&code)
    })
}

fn validate_managed_text(flag: &str, value: &str) -> Result<(), String> {
    if let Some(character) = refused_text_code_point(value) {
        return Err(format!(
            "{flag} contains refused code point U+{:04X}",
            character as u32
        ));
    }
    Ok(())
}

fn strict_item_url(value: &str, group: &str) -> Result<Option<(RepositoryId, u32)>, String> {
    if !value.contains("://") {
        return Ok(None);
    }
    let rest = value
        .strip_prefix("https://")
        .ok_or_else(|| "only HTTPS GitHub item URLs are allowed".to_string())?;
    let (host, path) = rest
        .split_once('/')
        .ok_or_else(|| "invalid GitHub item URL".to_string())?;
    let parts: Vec<&str> = path.split('/').collect();
    let item_kind = if group == "pr" { "pull" } else { "issues" };
    if parts.len() != 4 || parts[2] != item_kind {
        return Err(format!("invalid {group} URL"));
    }
    let repository = strict_repository_from_selector(&format!("{host}/{}/{}", parts[0], parts[1]))
        .ok_or_else(|| "invalid GitHub repository in item URL".to_string())?;
    let number = strict_item_number(parts[3], group)?;
    Ok(Some((repository, number)))
}

fn strict_item_number(value: &str, group: &str) -> Result<u32, String> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(format!("invalid {group} target: {value}"));
    }
    value
        .parse::<i32>()
        .ok()
        .filter(|number| *number > 0)
        .map(|number| number as u32)
        .ok_or_else(|| format!("invalid {group} target: {value}"))
}

#[derive(Debug, PartialEq, Eq)]
struct ManagedPlan {
    repository: Option<RepositoryId>,
    number: Option<u32>,
    values: std::collections::BTreeMap<&'static str, Option<String>>,
    args: Vec<String>,
}

fn valid_ref_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 255
        && value.is_ascii()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_./+@".contains(&byte))
        && !value.starts_with(['-', '/', '.'])
        && !value.ends_with(['/', '.'])
        && !value.ends_with(".lock")
        && !value.contains("..")
        && !value.contains("//")
        && !value.contains("@{")
}

fn valid_head(value: &str) -> bool {
    match value.split_once(':') {
        Some((owner, branch)) => {
            valid_repository_part(owner) && !branch.contains(':') && valid_ref_name(branch)
        }
        None => valid_ref_name(value),
    }
}

fn strict_managed_plan(args: &[String]) -> Result<ManagedPlan, String> {
    let Some(command) = managed_command(args) else {
        return Err("command is not eligible for managed GitHub approval".to_string());
    };
    let mut repository = None;
    let mut positionals = Vec::new();
    let mut values = std::collections::BTreeMap::<&'static str, Option<String>>::new();
    let mut index = 2;
    while index < args.len() {
        let arg = args[index].as_str();
        if arg == "--" {
            return Err("the -- separator is not allowed for managed GitHub actions".to_string());
        }
        let (flag, inline) = if let Some((flag, value)) = arg.split_once('=') {
            if !flag.starts_with("--") {
                return Err(format!(
                    "combined or attached shorthand is not allowed: {arg}"
                ));
            }
            (flag, Some(value))
        } else {
            (arg, None)
        };
        let spec = command
            .flags
            .iter()
            .find(|spec| flag == spec.long || spec.short.is_some_and(|short| flag == short));
        let (canonical, bare) = if matches!(flag, "--repo" | "-R") {
            ("--repo", false)
        } else if let Some(spec) = spec {
            (spec.long, spec.bare)
        } else if arg.starts_with('-') {
            return Err(format!(
                "flag not allowed for gh {} {}: {flag}",
                command.group, command.subcommand
            ));
        } else {
            positionals.push(arg.to_string());
            index += 1;
            continue;
        };
        if values.contains_key(canonical) || (canonical == "--repo" && repository.is_some()) {
            return Err(format!("{canonical} may be specified at most once"));
        }
        if bare {
            if inline.is_some() {
                return Err(format!("{canonical} does not take a value"));
            }
            values.insert(canonical, None);
            index += 1;
            continue;
        }
        let value = match inline {
            Some(value) => value,
            None => {
                index += 1;
                args.get(index)
                    .ok_or_else(|| format!("missing value for {flag}"))?
            }
        };
        validate_managed_text(canonical, value)?;
        if canonical == "--repo" {
            repository = Some(
                strict_repository_from_selector(value)
                    .ok_or_else(|| format!("invalid repository selector: {value}"))?,
            );
        } else {
            values.insert(canonical, Some(value.to_string()));
        }
        index += 1;
    }

    let number = if command.target == ManagedTarget::None {
        if !positionals.is_empty() {
            return Err(format!(
                "gh {} {} does not take a positional target",
                command.group, command.subcommand
            ));
        }
        None
    } else {
        if positionals.len() != 1 {
            return Err(format!(
                "gh {} {} takes exactly one target",
                command.group, command.subcommand
            ));
        }
        let target = &positionals[0];
        if let Some((url_repository, number)) = strict_item_url(target, command.group)? {
            if let Some(selector) = &repository
                && !selector.matches(&url_repository)
            {
                return Err(
                    "repository selector and item URL name different repositories".to_string(),
                );
            }
            repository = Some(url_repository);
            Some(number)
        } else {
            Some(strict_item_number(target, command.group)?)
        }
    };
    if let Some(repository) = &mut repository {
        repository.host.make_ascii_lowercase();
        repository.owner.make_ascii_lowercase();
        repository.name.make_ascii_lowercase();
    }

    for spec in command.flags {
        let value = values.get(spec.long);
        if spec.required && value.is_none() {
            return Err(format!("{} is required and must not be empty", spec.long));
        }
        if value.and_then(Option::as_ref).is_some_and(String::is_empty) {
            return Err(format!("{} must not be empty", spec.long));
        }
    }
    if command.group == "issue"
        && command.subcommand == "close"
        && let Some(Some(reason)) = values.get("--reason")
        && !matches!(reason.as_str(), "completed" | "not planned")
    {
        return Err("--reason must be exactly 'completed' or 'not planned'".to_string());
    }
    if command.subcommand == "edit" && values.is_empty() {
        return Err(format!(
            "gh {} edit requires at least one editable flag",
            command.group
        ));
    }
    if let Some(Some(head)) = values.get("--head")
        && !valid_head(head)
    {
        return Err("--head is not a valid conservative ref name".to_string());
    }
    if let Some(Some(base)) = values.get("--base")
        && !valid_ref_name(base)
    {
        return Err("--base is not a valid conservative ref name".to_string());
    }

    let mut canonical = vec![command.group.to_string(), command.subcommand.to_string()];
    if let Some(number) = number {
        canonical.push(number.to_string());
    }
    for spec in command.flags {
        if let Some(value) = values.get(spec.long) {
            canonical.push(match value {
                Some(value) => format!("{}={value}", spec.long),
                None => spec.long.to_string(),
            });
        }
    }
    Ok(ManagedPlan {
        repository,
        number,
        values,
        args: canonical,
    })
}

fn finalize_managed_plan(
    mut plan: ManagedPlan,
    repository: &RepositoryId,
    default_base: Option<&str>,
) -> Result<ManagedPlan, String> {
    if plan.args.first().is_some_and(|value| value == "pr")
        && plan.args.get(1).is_some_and(|value| value == "create")
        && !plan.values.contains_key("--base")
    {
        let base = default_base
            .ok_or_else(|| "repository inspection did not return a default branch".to_string())?;
        if !valid_ref_name(base) {
            return Err(
                "repository default branch is not a valid conservative ref name".to_string(),
            );
        }
        plan.values.insert("--base", Some(base.to_string()));
        plan.args = canonical_managed_args(&plan)?;
    }
    validate_managed_plan_size(&plan, repository)?;
    Ok(plan)
}

fn validate_managed_plan_size(plan: &ManagedPlan, repository: &RepositoryId) -> Result<(), String> {
    let total = plan.args.iter().map(String::len).sum::<usize>()
        + format!("--repo={}", repository.selector()).len();
    if total > 16 * 1024 {
        return Err("canonical GitHub action exceeds 16 KiB".to_string());
    }
    Ok(())
}

fn canonical_managed_args(plan: &ManagedPlan) -> Result<Vec<String>, String> {
    let command = managed_command(&plan.args)
        .ok_or_else(|| "command is not eligible for managed GitHub approval".to_string())?;
    let mut args = vec![command.group.to_string(), command.subcommand.to_string()];
    if let Some(number) = plan.number {
        args.push(number.to_string());
    }
    for spec in command.flags {
        if let Some(value) = plan.values.get(spec.long) {
            args.push(match value {
                Some(value) => format!("{}={value}", spec.long),
                None => spec.long.to_string(),
            });
        }
    }
    Ok(args)
}

#[cfg(test)]
fn strict_foreign_plan(
    args: &[String],
    allowed_hosts: &BTreeSet<String>,
) -> Result<ManagedPlan, String> {
    let plan = strict_managed_plan(args)?;
    let repository = plan.repository.clone().ok_or_else(|| {
        "managed GitHub actions must explicitly name a repository with --repo, -R, or an item URL".to_string()
    })?;
    if !allowed_hosts.contains(&repository.host) {
        return Err(format!(
            "GitHub host is not launch-approved: {}",
            repository.host
        ));
    }
    finalize_managed_plan(plan, &repository, None)
}

#[derive(Clone, Debug)]
struct ForeignInspection {
    repository_id: u64,
    full_name: String,
    visibility: String,
    default_branch: Option<String>,
    item_node_id: Option<String>,
    item_title: Option<String>,
    item_state: Option<String>,
    item_is_pull_request: Option<bool>,
    item_repository_url: Option<String>,
    head_sha: Option<String>,
    commits: Option<u64>,
    changed_files: Option<u64>,
}

#[derive(Deserialize)]
struct RepositoryInspection {
    id: u64,
    full_name: String,
    #[serde(default)]
    visibility: Option<String>,
    #[serde(default)]
    private: Option<bool>,
    #[serde(default)]
    default_branch: Option<String>,
}

#[derive(Deserialize)]
struct ItemInspection {
    node_id: String,
    title: String,
    state: String,
    repository_url: String,
    #[serde(default, deserialize_with = "present_json_field")]
    pull_request: bool,
}

#[derive(Deserialize)]
struct CompareInspection {
    total_commits: u64,
    changed_files: u64,
    head_sha: Option<String>,
}

enum InspectionQuery {
    Repository,
    Item(u32),
    Compare { base: String, head: String },
}

fn present_json_field<'de, D>(deserializer: D) -> Result<bool, D::Error>
where
    D: serde::Deserializer<'de>,
{
    serde::de::IgnoredAny::deserialize(deserializer)?;
    Ok(true)
}

fn inspect_managed_action(
    repository: &RepositoryId,
    plan: &ManagedPlan,
    inspect_compare: bool,
    mut inspect: impl FnMut(InspectionQuery) -> Result<Vec<u8>, String>,
) -> Result<ForeignInspection, String> {
    let details: RepositoryInspection =
        serde_json::from_slice(&inspect(InspectionQuery::Repository)?)
            .map_err(|error| format!("could not inspect {}: {error}", repository.selector()))?;
    let visibility = match (details.visibility, details.private) {
        (Some(visibility), _) => visibility,
        (None, Some(private)) => if private { "private" } else { "public" }.to_string(),
        (None, None) => {
            return Err(format!(
                "could not inspect {}: response omitted visibility and private",
                repository.selector()
            ));
        }
    };
    let item = plan
        .number
        .map(|number| {
            serde_json::from_slice::<ItemInspection>(&inspect(InspectionQuery::Item(number))?)
                .map_err(|error| {
                    format!(
                        "could not inspect {} item #{number}: {error}",
                        repository.selector()
                    )
                })
        })
        .transpose()?;
    let compare = if inspect_compare
        && plan.args.first().is_some_and(|value| value == "pr")
        && plan.args.get(1).is_some_and(|value| value == "create")
    {
        let base = plan
            .values
            .get("--base")
            .and_then(Option::as_deref)
            .or(details.default_branch.as_deref())
            .ok_or_else(|| "repository inspection did not return a default branch".to_string())?;
        let head = plan
            .values
            .get("--head")
            .and_then(Option::as_deref)
            .ok_or_else(|| "--head is required".to_string())?;
        if !valid_ref_name(base) {
            return Err(
                "repository default branch is not a valid conservative ref name".to_string(),
            );
        }
        if base.contains("...") || head.contains("...") {
            return Err("--base and --head must not contain ...".to_string());
        }
        let comparison: CompareInspection =
            serde_json::from_slice(&inspect(InspectionQuery::Compare {
                base: base.to_string(),
                head: head.to_string(),
            })?)
            .map_err(|error| {
                format!(
                    "could not inspect {} comparison: {error}",
                    repository.selector()
                )
            })?;
        if comparison.total_commits == 0 {
            return Err("pull request head has no commits ahead of base".to_string());
        }
        if comparison.total_commits > 250 {
            return Err("pull request has more than 250 commits; open it on GitHub".to_string());
        }
        if comparison.head_sha.as_deref().is_none_or(str::is_empty) {
            return Err("pull request comparison omitted the head commit SHA".to_string());
        }
        Some(comparison)
    } else {
        None
    };
    Ok(ForeignInspection {
        repository_id: details.id,
        full_name: details.full_name,
        visibility,
        default_branch: details.default_branch,
        item_node_id: item.as_ref().map(|item| item.node_id.clone()),
        item_title: item.as_ref().map(|item| item.title.clone()),
        item_state: item.as_ref().map(|item| item.state.clone()),
        item_is_pull_request: item.as_ref().map(|item| item.pull_request),
        item_repository_url: item.map(|item| item.repository_url),
        head_sha: compare.as_ref().and_then(|value| value.head_sha.clone()),
        commits: compare.as_ref().map(|value| value.total_commits),
        changed_files: compare.map(|value| value.changed_files),
    })
}

fn inspect_foreign_action(
    config: &ProxyConfig,
    repository: &RepositoryId,
    plan: &ManagedPlan,
    inspect_compare: bool,
) -> Result<ForeignInspection, String> {
    inspect_managed_action(repository, plan, inspect_compare, |query| {
        let mut command = safe_gh_api_command(config, repository);
        match query {
            InspectionQuery::Repository => {
                command.arg(repository.api_path(""));
            }
            InspectionQuery::Item(number) => {
                command.arg(repository.api_path(&format!("/issues/{number}")));
            }
            InspectionQuery::Compare { base, head } => {
                command.arg(repository.api_path(&format!("/compare/{base}...{head}")))
                    .args(["--jq", "{total_commits, changed_files: (.files | length), head_sha: .commits[-1].sha}"]);
            }
        }
        let output = command
            .output()
            .map_err(|error| format!("failed to execute gh: {error}"))?;
        if !output.status.success() {
            return Err(format!(
                "could not inspect {}: {}",
                repository.selector(),
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        Ok(output.stdout)
    })
}

fn managed_request_context(config: &ProxyConfig, cwd: Option<&str>) -> Result<String, String> {
    let cwd = cwd.ok_or_else(|| "request did not include a working directory".to_string())?;
    let resolved = resolve_container_path(config, cwd)?;
    let relative = resolved
        .strip_prefix(&config.workspace_root)
        .map_err(|_| "working directory escapes the mounted workspace".to_string())?;
    Ok(if relative.as_os_str().is_empty() {
        ".".to_string()
    } else {
        relative
            .to_str()
            .ok_or_else(|| "working directory is not valid UTF-8".to_string())?
            .to_string()
    })
}

fn resolve_named_grant<'a>(
    config: &'a ProxyConfig,
    repository: &RepositoryId,
) -> Result<Option<&'a RepositoryGrant>, String> {
    let Some(grant) = config
        .grants
        .iter()
        .find(|grant| grant.repository.matches(repository))
    else {
        return Ok(None);
    };
    let metadata = fs::metadata(&grant.root)
        .map_err(|error| format!("approved repository is unavailable: {error}"))?;
    if metadata.dev() != grant.device || metadata.ino() != grant.inode {
        return Err("approved repository directory changed after launch".to_string());
    }
    Ok(Some(grant))
}

struct RepositorySelection {
    repository: Option<RepositoryId>,
    selector_range: Option<(usize, usize)>,
}

fn legacy_repository_selection(args: &[String]) -> Result<RepositorySelection, String> {
    let mut selected = None;
    let mut selector_range = None;
    let mut index = 2;
    while index < args.len() {
        let arg = &args[index];
        if let Some(value) = arg.strip_prefix("--repo=") {
            if selected.is_some() {
                return Err("--repo may be specified at most once".to_string());
            }
            selected = Some(
                strict_repository_from_selector(value)
                    .ok_or_else(|| format!("invalid repository selector: {value}"))?,
            );
            selector_range = Some((index, 1));
        } else if matches!(arg.as_str(), "--repo" | "-R") {
            if selected.is_some() {
                return Err("--repo may be specified at most once".to_string());
            }
            let value = args
                .get(index + 1)
                .ok_or_else(|| format!("missing value for {arg}"))?;
            selected = Some(
                strict_repository_from_selector(value)
                    .ok_or_else(|| format!("invalid repository selector: {value}"))?,
            );
            selector_range = Some((index, 2));
            index += 1;
        } else if arg.starts_with('-')
            && !arg.contains('=')
            && write_flag_takes_value(&args[0], &args[1], extract_flag(arg))
        {
            index += 1;
        }
        index += 1;
    }
    for positional in write_positionals(args).unwrap_or_default() {
        if let Some(repository) = repository_from_item_url(positional, &args[0])? {
            if let Some(selected) = &selected
                && !selected.matches(&repository)
            {
                return Err(
                    "repository selector and item URL name different repositories".to_string(),
                );
            }
            selected = Some(repository);
        }
    }
    Ok(RepositorySelection {
        repository: selected,
        selector_range,
    })
}

fn strip_repo_selector(args: &[String], selection: &RepositorySelection) -> Vec<String> {
    let Some((start, length)) = selection.selector_range else {
        return args.to_vec();
    };
    args.iter()
        .enumerate()
        .filter(|(index, _)| *index < start || *index >= start + length)
        .map(|(_, value)| value.clone())
        .collect()
}

#[derive(Debug)]
struct ManagedExecution {
    args: Vec<String>,
    repository: RepositoryId,
    pr_create: bool,
}

#[derive(Debug)]
enum ManagedRoute {
    NotManaged,
    LaunchGrant(ManagedExecution),
    Granted(ManagedExecution),
    Approved(ManagedExecution),
    Pending(String),
}

fn prefix_inspection_error(
    repository: &RepositoryId,
    result: Result<ForeignInspection, String>,
) -> Result<ForeignInspection, String> {
    result.map_err(|reason| {
        if reason.starts_with("could not inspect ") {
            reason
        } else {
            format!("could not inspect {}: {reason}", repository.selector())
        }
    })
}

#[cfg(test)]
impl ManagedRoute {
    fn unwrap(self) -> ManagedExecution {
        match self {
            Self::LaunchGrant(execution) | Self::Granted(execution) | Self::Approved(execution) => {
                execution
            }
            Self::NotManaged | Self::Pending(_) => panic!("managed route did not execute"),
        }
    }

    fn is_none(&self) -> bool {
        matches!(self, Self::NotManaged)
    }

    fn is_approved(&self) -> bool {
        matches!(self, Self::Approved(_))
    }

    fn is_granted(&self) -> bool {
        matches!(self, Self::Granted(_))
    }
}

fn route_managed_write(
    args: &[String],
    cwd: Option<&str>,
    config: &ProxyConfig,
    mut inspect: impl FnMut(&RepositoryId, &ManagedPlan, bool) -> Result<ForeignInspection, String>,
) -> Result<ManagedRoute, String> {
    let Some(command) = managed_command(args) else {
        return Ok(ManagedRoute::NotManaged);
    };
    if !command.new && config.managed_gh_state.is_none() {
        return Ok(ManagedRoute::NotManaged);
    }

    if !command.new {
        let selection = legacy_repository_selection(args)?;
        let Some(named_repository) = selection.repository.as_ref() else {
            return Ok(ManagedRoute::NotManaged);
        };
        if let Some(grant) = resolve_named_grant(config, named_repository)? {
            let stripped = strip_repo_selector(args, &selection);
            if let Some(reason) = reject_reason(&stripped) {
                return Err(reason);
            }
            validate_write_targets(&stripped, grant)?;
            let mut routed = stripped;
            append_repo_selector(&mut routed, &grant.repository);
            return Ok(ManagedRoute::LaunchGrant(ManagedExecution {
                args: routed,
                repository: grant.repository.clone(),
                pr_create: false,
            }));
        }
    }

    let mut plan = strict_managed_plan(args)?;
    let repository = if let Some(repository) = plan.repository.clone() {
        repository
    } else if command.new {
        resolve_grant(config, cwd)?.repository.clone()
    } else {
        return Ok(ManagedRoute::NotManaged);
    };

    let hosts: BTreeSet<String> = std::iter::once("github.com".to_string())
        .chain(
            config
                .grants
                .iter()
                .map(|grant| grant.repository.host.to_ascii_lowercase()),
        )
        .collect();
    if !hosts.contains(&repository.host.to_ascii_lowercase()) {
        return Err(format!(
            "GitHub host is not launch-approved: {}",
            repository.host
        ));
    }

    if let Some(grant) = resolve_named_grant(config, &repository)? {
        let inspection = if command.group == "pr"
            && command.subcommand == "create"
            && !plan.values.contains_key("--base")
        {
            Some(prefix_inspection_error(
                &grant.repository,
                inspect(&grant.repository, &plan, false),
            )?)
        } else {
            None
        };
        plan = finalize_managed_plan(
            plan,
            &grant.repository,
            inspection
                .as_ref()
                .and_then(|value| value.default_branch.as_deref()),
        )?;
        let mut routed = plan.args;
        let pr_create = command.group == "pr" && command.subcommand == "create";
        if !pr_create {
            append_repo_selector(&mut routed, &grant.repository);
        }
        return Ok(ManagedRoute::LaunchGrant(ManagedExecution {
            args: routed,
            repository: grant.repository.clone(),
            pr_create,
        }));
    }

    let Some(state) = config.managed_gh_state.as_deref() else {
        return Err(
            "repository is not launch-approved and managed GitHub approvals are disabled"
                .to_string(),
        );
    };
    managed_request_context(config, cwd)?;
    validate_managed_plan_size(&plan, &repository)?;
    let inspection = prefix_inspection_error(&repository, inspect(&repository, &plan, true))?;
    plan = finalize_managed_plan(plan, &repository, inspection.default_branch.as_deref())?;
    if let Some(url) = inspection.item_repository_url.as_deref() {
        let expected = format!("/repos/{}/{}", repository.owner, repository.name);
        if !url
            .to_ascii_lowercase()
            .ends_with(&expected.to_ascii_lowercase())
        {
            return Err("item belongs to a different repository".to_string());
        }
    }
    let expected_full_name = format!("{}/{}", repository.owner, repository.name);
    if !inspection
        .full_name
        .eq_ignore_ascii_case(&expected_full_name)
    {
        return Err(format!(
            "repository was renamed; its current full name is {}",
            inspection.full_name
        ));
    }
    let kind = if command.target == ManagedTarget::PullRequest {
        managed_gh::ItemKind::PullRequest
    } else {
        managed_gh::ItemKind::Issue
    };
    if let Some(is_pull_request) = inspection.item_is_pull_request {
        match (kind, is_pull_request) {
            (managed_gh::ItemKind::Issue, true) if command.subcommand == "comment" => {
                return Err("target is a pull request; use gh pr comment instead".to_string());
            }
            (managed_gh::ItemKind::Issue, true) => {
                return Err("target is a pull request, not an issue".to_string());
            }
            (managed_gh::ItemKind::PullRequest, false) => {
                return Err("target is an issue, not a pull request".to_string());
            }
            _ => {}
        }
    }
    let action = managed_gh::Action {
        repository: managed_gh::Repository {
            host: repository.host.clone(),
            owner: repository.owner.clone(),
            name: repository.name.clone(),
        },
        repository_id: inspection.repository_id,
        item: plan.number.map(|number| managed_gh::Item {
            number,
            kind,
            node_id: inspection.item_node_id.clone().unwrap_or_default(),
        }),
        head_sha: inspection.head_sha,
        args: plan.args.clone(),
    };
    let context = managed_gh::Context {
        repository_visibility: inspection.visibility,
        item_title: inspection.item_title,
        item_state: inspection.item_state,
        commits: inspection.commits,
        changed_files: inspection.changed_files,
        requested_from: managed_request_context(config, cwd)?,
        requested_at: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
    };
    let _guard = config.managed_gh_lock.lock().unwrap();
    if managed_gh::is_granted(state, &action, &context.repository_visibility)? {
        let pr_create = command.group == "pr" && command.subcommand == "create";
        let mut execution = action.args.clone();
        if !pr_create {
            execution.push(format!("--repo={}", action.repository.selector()));
        }
        return Ok(ManagedRoute::Granted(ManagedExecution {
            args: execution,
            repository,
            pr_create,
        }));
    }
    if managed_gh::consume(state, &action)? {
        let pr_create = command.group == "pr" && command.subcommand == "create";
        let mut execution = action.args.clone();
        if !pr_create {
            execution.push(format!("--repo={}", action.repository.selector()));
        }
        return Ok(ManagedRoute::Approved(ManagedExecution {
            args: execution,
            repository,
            pr_create,
        }));
    }
    let handle = managed_gh::record_candidate(state, &action, &context)?;
    Ok(ManagedRoute::Pending(handle))
}

#[derive(Debug, PartialEq, Eq)]
struct MergePlan {
    number: String,
    flags: Vec<String>,
}

impl MergePlan {
    fn lands_changes(&self) -> bool {
        self.flags != ["--disable-auto"]
    }

    fn args(&self, repository: &RepositoryId) -> Vec<String> {
        let mut args = vec![
            "pr".to_string(),
            "merge".to_string(),
            self.number.clone(),
            "--repo".to_string(),
            repository.selector(),
        ];
        args.extend(self.flags.iter().cloned());
        args
    }
}

/// Rebuild a `gh pr merge` invocation from scratch so that only understood
/// arguments reach gh: one pull request of the granted repository, addressed
/// by number, with flags re-emitted in canonical long form.
fn plan_pr_merge(args: &[String], grant: &RepositoryGrant) -> Result<MergePlan, String> {
    let mut target = None;
    let mut methods = 0;
    let mut disable_auto = false;
    let mut flags = Vec::new();
    let mut index = 2;
    while index < args.len() {
        let arg = args[index].as_str();
        let (flag, inline) = match arg.split_once('=') {
            Some((flag, value)) if arg.starts_with("--") => (flag, Some(value)),
            _ => (arg, None),
        };
        let canonical = match flag {
            "-m" => "--merge",
            "-s" => "--squash",
            "-r" => "--rebase",
            "-t" => "--subject",
            "-b" => "--body",
            "-R" => "--repo",
            other => other,
        };
        match canonical {
            "--merge" | "--squash" | "--rebase" | "--auto" | "--disable-auto" => {
                if inline.is_some() {
                    return Err(format!("{flag} does not take a value"));
                }
                match canonical {
                    "--disable-auto" => disable_auto = true,
                    "--auto" => flags.push(canonical.to_string()),
                    _ => {
                        methods += 1;
                        flags.push(canonical.to_string());
                    }
                }
            }
            "--match-head-commit" | "--subject" | "--body" | "--repo" => {
                let value = match inline {
                    Some(value) => value,
                    None => {
                        index += 1;
                        args.get(index)
                            .ok_or_else(|| format!("missing value for {flag}"))?
                    }
                };
                if canonical != "--repo" {
                    flags.push(format!("{canonical}={value}"));
                } else if !repository_from_selector(value)
                    .is_some_and(|repository| grant.repository.matches(&repository))
                {
                    return Err(format!(
                        "{flag} {value} is outside the current repository {}",
                        grant.repository.selector()
                    ));
                }
            }
            _ if arg.starts_with('-') => {
                return Err(format!("flag not allowed for gh pr merge: {flag}"));
            }
            _ if target.is_none() => target = Some(arg),
            _ => return Err("gh pr merge takes exactly one pull request".to_string()),
        }
        index += 1;
    }

    let target =
        target.ok_or_else(|| "gh pr merge requires a pull request number or URL".to_string())?;
    validate_item_target(target, "pr", grant, false)?;
    let path = target.split(['?', '#']).next().unwrap_or(target);
    let number = path.trim_end_matches('/').rsplit('/').next().unwrap_or("");
    // gh reads a number it cannot parse as a branch name, so only a value
    // that is a pull request number to gh as well may pass.
    let number = number
        .parse::<i32>()
        .ok()
        .filter(|parsed| *parsed > 0 && number.bytes().all(|byte| byte.is_ascii_digit()))
        .ok_or_else(|| format!("invalid pr target: {target}"))?;

    if disable_auto {
        if !flags.is_empty() {
            return Err("--disable-auto cannot be combined with other merge flags".to_string());
        }
        flags.push("--disable-auto".to_string());
    } else if methods != 1 {
        return Err("exactly one of --merge, --squash or --rebase is required".to_string());
    }

    Ok(MergePlan {
        number: number.to_string(),
        flags,
    })
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct MergeCandidate {
    is_cross_repository: bool,
}

fn refuse_fork_head(number: &str, candidate: &[u8]) -> Result<(), String> {
    let candidate: MergeCandidate = serde_json::from_slice(candidate)
        .map_err(|error| format!("could not inspect pull request #{number}: {error}"))?;
    if candidate.is_cross_repository {
        return Err(format!(
            "pull request #{number} comes from a fork; review and merge it on GitHub instead"
        ));
    }
    Ok(())
}

/// Validate a merge request and return the argv to run. A merge may only land
/// a branch of the granted repository itself, so the code was pushed by
/// someone with write access rather than proposed by an arbitrary fork.
fn route_pr_merge(
    args: &[String],
    grant: &RepositoryGrant,
    inspect: impl FnOnce(&str) -> Result<Vec<u8>, String>,
) -> Result<Vec<String>, String> {
    let plan = plan_pr_merge(args, grant)?;
    if plan.lands_changes() {
        refuse_fork_head(&plan.number, &inspect(&plan.number)?)?;
    }
    Ok(plan.args(&grant.repository))
}

fn inspect_merge_candidate(
    config: &ProxyConfig,
    repository: &RepositoryId,
    number: &str,
) -> Result<Vec<u8>, String> {
    let output = safe_gh_command(config)
        .args(["pr", "view", number, "--repo", &repository.selector()])
        .args(["--json", "isCrossRepository"])
        .output()
        .map_err(|error| format!("failed to execute gh: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "could not inspect pull request #{number}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(output.stdout)
}

fn has_repo_flag(args: &[String]) -> bool {
    args.iter()
        .any(|arg| matches!(arg.as_str(), "--repo" | "-R") || arg.starts_with("--repo="))
}

fn read_command_uses_current_repo(args: &[String]) -> bool {
    match (args[0].as_str(), args[1].as_str()) {
        ("api", _) | ("auth", _) | ("repo", "list") | ("search", _) => false,
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

fn read_command_needs_repo_selector(args: &[String]) -> bool {
    !has_repo_flag(args) && read_command_uses_current_repo(args)
}

fn is_safe_account_probe(args: &[String]) -> bool {
    args.len() == 4
        && args[0] == "api"
        && args[1] == "user"
        && args[2] == "--jq"
        && args[3] == ".login"
}

fn reject_reason(args: &[String]) -> Option<String> {
    if args.len() == 1 && args[0] == "--version" {
        return None;
    }

    if is_safe_account_probe(args) {
        return None;
    }

    if args.len() < 2 {
        return Some(format!("command not allowed: gh {}", args.join(" ")));
    }

    let group = args[0].as_str();
    let subcommand = args[1].as_str();

    if group == "api" && subcommand == "user" {
        return Some(
            "command not allowed: only 'gh api user --jq .login' is available".to_string(),
        );
    }

    // Guide callers trying to use `gh api` for milestones to the extension commands
    if group == "api" && args[1..].iter().any(|a| a.contains("milestone")) {
        return Some(
            "command not allowed: general gh api access is not available. \
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

#[derive(Debug, PartialEq, Eq, Serialize)]
struct PullRequestBody {
    title: String,
    body: String,
    head: String,
    base: String,
    draft: bool,
}

#[derive(Debug, PartialEq, Eq)]
struct PullRequestRestCall {
    host: String,
    path: String,
    body: PullRequestBody,
}

fn pr_create_rest_call(
    repository: &RepositoryId,
    args: &[String],
) -> Result<PullRequestRestCall, String> {
    if args.first().map(String::as_str) != Some("pr")
        || args.get(1).map(String::as_str) != Some("create")
    {
        return Err("invalid canonical pr create action".to_string());
    }
    let mut title = None;
    let mut body = None;
    let mut head = None;
    let mut base = None;
    let mut draft = false;
    for arg in &args[2..] {
        if let Some(value) = arg.strip_prefix("--title=") {
            title = Some(value.to_string());
        } else if let Some(value) = arg.strip_prefix("--body=") {
            body = Some(value.to_string());
        } else if let Some(value) = arg.strip_prefix("--head=") {
            head = Some(value.to_string());
        } else if let Some(value) = arg.strip_prefix("--base=") {
            base = Some(value.to_string());
        } else if arg == "--draft" {
            draft = true;
        } else {
            return Err(format!("invalid canonical pr create argument: {arg}"));
        }
    }
    Ok(PullRequestRestCall {
        host: repository.host.clone(),
        path: repository.api_path("/pulls"),
        body: PullRequestBody {
            title: title.ok_or("canonical pr create action omitted --title")?,
            body: body.ok_or("canonical pr create action omitted --body")?,
            head: head.ok_or("canonical pr create action omitted --head")?,
            base: base.ok_or("canonical pr create action omitted --base")?,
            draft,
        },
    })
}

fn execute_pr_create(config: &ProxyConfig, execution: &ManagedExecution) -> Response {
    let call = match pr_create_rest_call(&execution.repository, &execution.args) {
        Ok(call) => call,
        Err(reason) => return denied_response(reason),
    };
    let input = match serde_json::to_vec(&call.body) {
        Ok(input) => input,
        Err(error) => return denied_response(format!("could not encode pull request: {error}")),
    };
    let mut command = safe_gh_api_command(config, &execution.repository);
    command
        .args([&call.path, "-X", "POST", "--input", "-"])
        .env("GH_PROMPT_DISABLED", "1")
        .env_remove("GH_FORCE_TTY")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => return denied_response(format!("failed to execute gh api: {error}")),
    };
    if let Some(mut stdin) = child.stdin.take()
        && let Err(error) = stdin.write_all(&input)
    {
        drop(stdin);
        let _ = child.kill();
        let _ = child.wait();
        return denied_response(format!("failed to write gh api input: {error}"));
    }
    match child.wait_with_output() {
        Ok(output) if output.status.success() => {
            #[derive(Deserialize)]
            struct CreatedPullRequest {
                html_url: String,
            }
            match serde_json::from_slice::<CreatedPullRequest>(&output.stdout) {
                Ok(created) => Response {
                    exit_code: 0,
                    stdout: format!("{}\n", created.html_url),
                    stderr: String::new(),
                },
                Err(error) => {
                    denied_response(format!("could not parse created pull request: {error}"))
                }
            }
        }
        Ok(output) => {
            let mut stderr = String::from_utf8_lossy(&output.stderr).into_owned();
            stderr.push_str(&String::from_utf8_lossy(
                &output.stdout[..output.stdout.len().min(4 * 1024)],
            ));
            Response {
                exit_code: output.status.code().unwrap_or(1),
                stdout: String::new(),
                stderr,
            }
        }
        Err(error) => denied_response(format!("failed to execute gh api: {error}")),
    }
}

fn execute_gh_command(config: &ProxyConfig, args: &[String], managed: bool) -> Response {
    let mut command = safe_gh_command(config);
    if managed {
        command
            .env("GH_PROMPT_DISABLED", "1")
            .env_remove("GH_FORCE_TTY")
            .stdin(Stdio::null());
    }
    match command.args(args).output() {
        Ok(output) => Response {
            exit_code: output.status.code().unwrap_or(1),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        },
        Err(error) => Response {
            exit_code: 1,
            stdout: String::new(),
            stderr: format!("gh-proxy: failed to execute gh: {error}"),
        },
    }
}

/// Render a request for the log on a single line, so an argument cannot
/// forge further log entries.
fn loggable_command(args: &[String]) -> String {
    loggable_text(&args.join(" "))
}

fn loggable_text(value: &str) -> String {
    let mut command = String::new();
    for character in value.chars() {
        if character.is_control() || character == '\\' {
            command.extend(character.escape_default());
        } else {
            command.push(character);
        }
    }
    command
}

fn handle_request(req: Request, log: &Arc<Mutex<File>>, config: &ProxyConfig) -> Response {
    handle_request_with_executors(
        req,
        log,
        config,
        |execution| execute_pr_create(config, execution),
        |args, approved| execute_gh_command(config, args, approved),
    )
}

fn handle_request_with_executors(
    req: Request,
    log: &Arc<Mutex<File>>,
    config: &ProxyConfig,
    execute_create: impl FnOnce(&ManagedExecution) -> Response,
    execute_gh: impl FnOnce(&[String], bool) -> Response,
) -> Response {
    let cmd_str = loggable_command(&req.args);

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
                log_line(
                    log,
                    &format!("DENIED  gh {} ({})", cmd_str, loggable_text(&reason)),
                );
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

    let managed_route = match route_managed_write(
        &req.args,
        req.cwd.as_deref(),
        config,
        |repository, plan, compare| inspect_foreign_action(config, repository, plan, compare),
    ) {
        Ok(route) => route,
        Err(reason) => {
            log_line(
                log,
                &format!("DENIED  gh {cmd_str} ({})", loggable_text(&reason)),
            );
            return denied_response(reason);
        }
    };

    if matches!(managed_route, ManagedRoute::NotManaged)
        && let Some(reason) = reject_reason(&req.args)
    {
        log_line(
            log,
            &format!("DENIED  gh {} ({})", cmd_str, loggable_text(&reason)),
        );
        return denied_response(reason);
    }

    let (mut execution_args, managed, managed_execution) = match managed_route {
        ManagedRoute::NotManaged => (req.args.clone(), false, None),
        ManagedRoute::LaunchGrant(execution) => {
            log_line(log, &format!("ROUTED  gh {cmd_str}"));
            (execution.args.clone(), true, Some(execution))
        }
        ManagedRoute::Granted(execution) => {
            log_line(log, &format!("GRANTED gh {cmd_str}"));
            (execution.args.clone(), true, Some(execution))
        }
        ManagedRoute::Approved(execution) => {
            log_line(log, &format!("APPROVED gh {cmd_str}"));
            (execution.args.clone(), true, Some(execution))
        }
        ManagedRoute::Pending(handle) => {
            let reason = format!(
                "pending approval {handle}; open the T3 admin portal, approve it, and retry the same command"
            );
            log_line(
                log,
                &format!("PENDING gh {cmd_str} ({})", loggable_text(&reason)),
            );
            return denied_response(reason);
        }
    };
    if managed_execution.is_none()
        && req.args.len() >= 2
        && let Some(command) = find_command(&req.args[0], &req.args[1])
    {
        if command.is_write {
            let merge = is_pr_merge(command.group, command.subcommand);
            if merge && !config.allow_merge {
                log_line(log, &format!("DENIED  gh {} (merge not enabled)", cmd_str));
                return denied_response(MERGE_DISABLED.to_string());
            }
            let grant = match resolve_grant(config, req.cwd.as_deref()) {
                Ok(grant) => grant,
                Err(reason) => {
                    log_line(
                        log,
                        &format!("DENIED  gh {} ({})", cmd_str, loggable_text(&reason)),
                    );
                    return denied_response(reason);
                }
            };
            let routed = if merge {
                route_pr_merge(&req.args, grant, |number| {
                    inspect_merge_candidate(config, &grant.repository, number)
                })
            } else {
                validate_write_targets(&req.args, grant).map(|()| {
                    let mut args = req.args.clone();
                    append_repo_selector(&mut args, &grant.repository);
                    args
                })
            };
            execution_args = match routed {
                Ok(args) => args,
                Err(reason) => {
                    log_line(
                        log,
                        &format!("DENIED  gh {} ({})", cmd_str, loggable_text(&reason)),
                    );
                    return denied_response(reason);
                }
            };
            log_line(
                log,
                &format!("ROUTED  gh {} -> {}", cmd_str, grant.repository.selector()),
            );
        } else if read_command_needs_repo_selector(&req.args) {
            match resolve_grant(config, req.cwd.as_deref()) {
                Ok(grant) => {
                    append_repo_selector(&mut execution_args, &grant.repository);
                    log_line(
                        log,
                        &format!("ROUTED  gh {} -> {}", cmd_str, grant.repository.selector()),
                    );
                }
                Err(reason) => {
                    log_line(
                        log,
                        &format!("DENIED  gh {} ({})", cmd_str, loggable_text(&reason)),
                    );
                    return denied_response(reason);
                }
            }
        }
    }

    log_line(log, &format!("ALLOWED gh {}", cmd_str));

    if let Some(execution) = managed_execution
        .as_ref()
        .filter(|execution| execution.pr_create)
    {
        let response = execute_create(execution);
        log_line(
            log,
            &format!("EXIT    gh {} -> {}", cmd_str, response.exit_code),
        );
        return response;
    }

    let response = execute_gh(&execution_args, managed);
    log_line(
        log,
        &format!("EXIT    gh {} -> {}", cmd_str, response.exit_code),
    );
    response
}

pub fn run(
    socket_path: &str,
    log_path: &Path,
    workspace_root: &Path,
    allow_merge: bool,
    managed_gh_state: Option<&Path>,
) {
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
        allow_merge,
        managed_gh_state: managed_gh_state.map(Path::to_path_buf),
        managed_gh_lock: Mutex::new(()),
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
            "listening on {} with {} launch-approved repositories (merge {})",
            socket_path,
            config.grants.len(),
            if config.allow_merge {
                "enabled"
            } else {
                "disabled"
            }
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
    fn test_t3_code_read_commands_allowed() {
        let commands = [
            strs(&["--version"]),
            strs(&["auth", "status", "--json", "hosts"]),
            strs(&["api", "user", "--jq", ".login"]),
            strs(&[
                "pr",
                "list",
                "--head",
                "owner:feature",
                "--state",
                "open",
                "--limit",
                "1",
                "--json",
                "number,title,url,baseRefName,headRefName,state,mergedAt,isCrossRepository,headRepository,headRepositoryOwner",
            ]),
            strs(&[
                "pr",
                "list",
                "--head",
                "owner:feature",
                "--state",
                "all",
                "--limit",
                "20",
                "--json",
                "number,title,url,baseRefName,headRefName,state,mergedAt,updatedAt,isCrossRepository,headRepository,headRepositoryOwner",
            ]),
            strs(&[
                "pr",
                "view",
                "123",
                "--json",
                "number,title,url,baseRefName,headRefName,state,mergedAt,isCrossRepository,headRepository,headRepositoryOwner",
            ]),
            strs(&[
                "repo",
                "view",
                "owner/project",
                "--json",
                "nameWithOwner,url,sshUrl",
            ]),
            strs(&[
                "repo",
                "view",
                "--json",
                "defaultBranchRef",
                "--jq",
                ".defaultBranchRef.name",
            ]),
            strs(&[
                "pr",
                "list",
                "--repo",
                "github.com/owner/project",
                "--author",
                "viewer",
                "--search",
                "fix",
                "--state",
                "open",
                "--limit",
                "21",
                "--json",
                "number,title,url,baseRefName,headRefName,state",
            ]),
            strs(&[
                "pr",
                "view",
                "123",
                "--repo",
                "github.com/owner/project",
                "--json",
                "number,title,url,comments,reviews",
            ]),
            strs(&[
                "pr",
                "diff",
                "123",
                "--repo",
                "github.com/owner/project",
                "--color",
                "never",
            ]),
            strs(&[
                "repo",
                "view",
                "github.com/owner/project",
                "--json",
                "viewerPermission,mergeCommitAllowed,rebaseMergeAllowed,squashMergeAllowed",
            ]),
        ];

        for args in commands {
            assert_eq!(reject_reason(&args), None, "gh {}", args.join(" "));
        }
    }

    #[test]
    fn test_auth_status_blocks_token_output() {
        assert!(reject_reason(&strs(&["auth", "status", "--show-token"])).is_some());
        assert!(reject_reason(&strs(&["auth", "status", "-t"])).is_some());
        assert!(reject_reason(&strs(&["auth", "token"])).is_some());
    }

    #[test]
    fn test_account_probe_allowance_is_exact() {
        assert!(reject_reason(&strs(&["api", "user", "--jq", ".login"])).is_none());
        for args in [
            strs(&["api", "user"]),
            strs(&["api", "user", "--jq", ".id"]),
            strs(&["api", "user", "--jq", ".login", "--hostname", "github.com"]),
            strs(&["api", "user", "--jq", ".login", "--method", "DELETE"]),
            strs(&["api", "user", "--jq=.login"]),
            strs(&["api", "--jq", ".login", "user"]),
        ] {
            assert!(reject_reason(&args).is_some(), "gh {}", args.join(" "));
        }

        let args = strs(&["api", "user", "--jq", ".login"]);
        assert!(find_command("api", "user").is_none());
        assert!(!read_command_needs_repo_selector(&args));
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
        assert!(reject_reason(&strs(&["issue", "delete", "42"])).is_some());
        assert!(reject_reason(&strs(&["repo", "create"])).is_some());
        assert!(reject_reason(&strs(&["repo", "delete"])).is_some());
        assert!(reject_reason(&strs(&["release", "create"])).is_some());
        assert!(reject_reason(&strs(&["release", "delete"])).is_some());
        assert!(reject_reason(&strs(&["run", "cancel"])).is_some());
    }

    #[test]
    fn test_t3_code_commands_outside_proxy_boundary_remain_denied() {
        let commands = [
            strs(&["repo", "create", "owner/new-project", "--private"]),
            strs(&["repo", "create", "owner/new-project", "--public"]),
            strs(&["repo", "create", "owner/new-project", "--internal"]),
            strs(&[
                "pr",
                "create",
                "--base",
                "main",
                "--head",
                "owner:feature",
                "--title",
                "New pull request",
                "--body-file",
                "/tmp/t3code-pr-body.md",
            ]),
            strs(&["pr", "checkout", "123"]),
            strs(&["pr", "checkout", "123", "--force"]),
            strs(&["api", "graphql", "--hostname", "github.com", "--input", "-"]),
            strs(&[
                "api",
                "--hostname",
                "github.com",
                "repos/owner/project/pulls/123/files?per_page=100&page=1",
            ]),
            strs(&[
                "api",
                "--method",
                "POST",
                "--hostname",
                "github.com",
                "repos/owner/project/pulls/123/reviews",
                "--input",
                "-",
            ]),
            strs(&[
                "pr",
                "comment",
                "123",
                "--repo",
                "github.com/owner/project",
                "--body-file",
                "-",
            ]),
            strs(&[
                "pr",
                "update-branch",
                "123",
                "--repo",
                "github.com/owner/project",
                "--rebase",
            ]),
        ];

        for args in commands {
            assert!(reject_reason(&args).is_some(), "gh {}", args.join(" "));
        }
    }

    #[test]
    fn test_t3_code_read_commands_route_only_when_repository_is_implicit() {
        assert!(read_command_needs_repo_selector(&strs(&[
            "pr",
            "list",
            "--head",
            "owner:feature"
        ])));
        assert!(read_command_needs_repo_selector(&strs(&[
            "repo",
            "view",
            "--json",
            "defaultBranchRef"
        ])));
        assert!(!read_command_needs_repo_selector(&strs(&[
            "repo",
            "view",
            "owner/project",
            "--json",
            "nameWithOwner"
        ])));
        assert!(!read_command_needs_repo_selector(&strs(&[
            "pr",
            "list",
            "--repo",
            "github.com/owner/project",
            "--state",
            "open"
        ])));
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
        assert!(h.contains("user --jq .login"));
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
        assert!(h.contains("create"));
        assert!(h.contains("comment"));

        // Via `gh help pr`
        let h2 = maybe_help(&strs(&["help", "pr"])).unwrap();
        assert!(h2.contains("list"));

        let api = maybe_help(&strs(&["api", "-h"])).unwrap();
        assert!(api.contains("user --jq .login"));
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

        let merge = maybe_help(&strs(&["pr", "merge", "--help"])).unwrap();
        assert!(merge.contains("-s, --squash"));
        assert!(merge.contains("--allow-merge"));
        assert!(!merge.contains("no -R/--repo"));

        let create = maybe_help(&strs(&["pr", "create", "--help"])).unwrap();
        assert!(create.contains("--head"));
        assert!(create.contains("-H"));
        assert!(create.contains("branch must already be pushed"));
        let reopen = maybe_help(&strs(&["issue", "reopen", "--help"])).unwrap();
        assert!(reopen.contains("--comment"));

        // Via `gh help pr list`
        let h3 = maybe_help(&strs(&["help", "pr", "list"])).unwrap();
        assert!(h3.contains("--state"));

        let api = maybe_help(&strs(&["api", "user", "--help"])).unwrap();
        assert!(api.contains("read-only account-name probe"));
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
            allow_merge: false,
            managed_gh_state: None,
            managed_gh_lock: Mutex::new(()),
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

    fn managed_config(label: &str) -> (ProxyConfig, PathBuf) {
        let state_root = temporary_workspace(label);
        let state = managed_gh::prepare_state_dir(&state_root).unwrap();
        let mut config = test_config();
        config.managed_gh_state = Some(state);
        (config, state_root)
    }

    fn foreign_inspection(is_pull_request: bool) -> ForeignInspection {
        ForeignInspection {
            repository_id: 42,
            full_name: "other/project".to_string(),
            visibility: "public".to_string(),
            default_branch: Some("main".to_string()),
            item_node_id: Some("ITEM_7".to_string()),
            item_title: Some("Title".to_string()),
            item_state: Some("open".to_string()),
            item_is_pull_request: Some(is_pull_request),
            item_repository_url: Some("https://api.github.com/repos/other/project".to_string()),
            head_sha: None,
            commits: None,
            changed_files: None,
        }
    }

    fn successful_response(stdout: String) -> Response {
        Response {
            exit_code: 0,
            stdout,
            stderr: String::new(),
        }
    }

    #[test]
    fn managed_launch_route_is_not_routed_twice_when_arguments_match() {
        let (config, root) = managed_config("single-route");
        let response = handle_request_with_executors(
            Request {
                args: strs(&[
                    "issue",
                    "comment",
                    "7",
                    "--body=x",
                    "--repo",
                    "github.com/example/project",
                ]),
                cwd: None,
            },
            &Arc::new(Mutex::new(File::create("/dev/null").unwrap())),
            &config,
            |_| panic!("not pr create"),
            |args, _| successful_response(args.join("\n")),
        );
        assert_eq!(response.exit_code, 0);
        assert_eq!(response.stdout.matches("--repo").count(), 1);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn selector_shaped_flag_value_is_not_routing_metadata() {
        let (config, root) = managed_config("selector-value");
        let args = strs(&[
            "issue",
            "edit",
            "7",
            "--title",
            "--repo=example/project",
            "--body=y",
        ]);
        assert!(matches!(
            route_managed_write(&args, Some("/workspace"), &config, |_, _, _| panic!(
                "must not inspect"
            ))
            .unwrap(),
            ManagedRoute::NotManaged
        ));
        assert_eq!(
            strip_repo_selector(&args, &legacy_repository_selection(&args).unwrap()),
            args
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn legacy_launch_url_with_fragment_uses_legacy_validation() {
        let (config, root) = managed_config("legacy-url");
        let args = strs(&[
            "issue",
            "comment",
            "https://github.com/example/project/issues/7#issuecomment-1",
            "--body=x",
        ]);
        let routed =
            route_managed_write(&args, None, &config, |_, _, _| panic!("must not inspect"))
                .unwrap();
        assert!(!routed.is_approved());
        let routed = routed.unwrap();
        assert!(
            routed
                .args
                .iter()
                .any(|arg| arg.contains("#issuecomment-1"))
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn denial_reason_is_single_line_in_log() {
        let (config, root) = managed_config("log-reason");
        let log_path = root.join("proxy.log");
        let log = Arc::new(Mutex::new(File::create(&log_path).unwrap()));
        let response = handle_request(
            Request {
                args: strs(&[
                    "issue",
                    "comment",
                    "7",
                    "--body=x",
                    "--repo=other/evil\nAPPROVED forged",
                ]),
                cwd: Some("/workspace".to_string()),
            },
            &log,
            &config,
        );
        assert_ne!(response.exit_code, 0);
        drop(log);
        let contents = fs::read_to_string(log_path).unwrap();
        assert_eq!(contents.lines().count(), 1);
        assert!(contents.contains("\\nAPPROVED forged"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn repository_inspection_requires_visibility_or_private() {
        let plan = strict_managed_plan(&strs(&[
            "issue",
            "create",
            "--title=x",
            "--body=y",
            "--repo=other/project",
        ]))
        .unwrap();
        let repository = plan.repository.clone().unwrap();
        let error = inspect_managed_action(&repository, &plan, false, |_| {
            Ok(br#"{"id":42,"full_name":"other/project"}"#.to_vec())
        })
        .unwrap_err();
        assert!(error.contains("omitted visibility and private"));
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_managed_context_is_rejected_before_inspection() {
        use std::os::unix::ffi::OsStringExt;
        use std::os::unix::fs::symlink;
        let root = temporary_workspace("non-utf8-context");
        fs::create_dir_all(&root).unwrap();
        let non_utf8 = root.join(std::ffi::OsString::from_vec(vec![b'b', 0xff]));
        fs::create_dir(&non_utf8).unwrap();
        symlink(&non_utf8, root.join("link")).unwrap();
        let mut config = test_config();
        config.workspace_root = fs::canonicalize(&root).unwrap();
        config.managed_gh_state = Some(managed_gh::prepare_state_dir(&root.join("state")).unwrap());
        let args = strs(&["issue", "comment", "7", "--body=x", "--repo=other/project"]);
        let error = route_managed_write(&args, Some("/workspace/link"), &config, |_, _, _| {
            panic!("inspection must not run for a bad cwd")
        })
        .unwrap_err();
        assert!(error.contains("not valid UTF-8"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn inspection_failure_names_the_repository() {
        let (config, root) = managed_config("inspection-prefix");
        let args = strs(&["issue", "comment", "7", "--body=x", "--repo=other/project"]);
        let error = route_managed_write(&args, Some("/workspace"), &config, |_, _, _| {
            Err("gh stderr".to_string())
        })
        .unwrap_err();
        assert_eq!(
            error,
            "could not inspect github.com/other/project: gh stderr"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn managed_gh_off_preserves_foreign_repo_refusal() {
        let response = handle_request(
            Request {
                args: strs(&[
                    "issue",
                    "comment",
                    "7",
                    "--body",
                    "hello",
                    "--repo",
                    "other/project",
                ]),
                cwd: Some("/workspace".to_string()),
            },
            &Arc::new(Mutex::new(File::create("/dev/null").unwrap())),
            &test_config(),
        );
        assert_eq!(
            response.stderr,
            "gh-proxy: flag not allowed for gh issue comment: --repo"
        );
    }

    #[test]
    fn managed_gh_approval_is_canonical_and_one_time() {
        let (config, root) = managed_config("canonical");
        let args = strs(&[
            "issue",
            "comment",
            "007",
            "-b",
            "hello",
            "-R",
            "Other/Project",
        ]);
        let pending = route_managed_write(&args, Some("/workspace"), &config, |_, _, _| {
            Ok(foreign_inspection(false))
        })
        .unwrap();
        assert!(matches!(pending, ManagedRoute::Pending(_)));
        let state = config.managed_gh_state.as_deref().unwrap();
        let candidates = managed_gh::list_candidates(state).unwrap();
        assert_eq!(candidates.len(), 1);
        assert_eq!(
            candidates[0].1.action.args,
            strs(&["issue", "comment", "7", "--body=hello"])
        );
        managed_gh::approve(state, &candidates[0].0).unwrap();
        let routed = route_managed_write(&args, Some("/workspace"), &config, |_, _, _| {
            Ok(foreign_inspection(false))
        })
        .unwrap();
        assert!(routed.is_approved());
        let routed = routed.unwrap();
        assert_eq!(
            routed.args,
            strs(&[
                "issue",
                "comment",
                "7",
                "--body=hello",
                "--repo=github.com/other/project",
            ])
        );
        assert!(matches!(
            route_managed_write(&args, Some("/workspace"), &config, |_, _, _| {
                Ok(foreign_inspection(false))
            })
            .unwrap(),
            ManagedRoute::Pending(_)
        ));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn managed_gh_grant_executes_without_consuming_approval_and_is_command_scoped() {
        let (config, root) = managed_config("persistent-grant");
        let approved_args = strs(&[
            "issue",
            "comment",
            "7",
            "--body=approved",
            "--repo=other/project",
        ]);
        route_managed_write(&approved_args, Some("/workspace"), &config, |_, _, _| {
            Ok(foreign_inspection(false))
        })
        .unwrap();
        let state = config.managed_gh_state.as_deref().unwrap();
        let approval_handle = managed_gh::list_candidates(state).unwrap().remove(0).0;
        managed_gh::approve(state, &approval_handle).unwrap();

        let grant_args = strs(&[
            "issue",
            "comment",
            "7",
            "--body=grant",
            "--repo=other/project",
        ]);
        route_managed_write(&grant_args, Some("/workspace"), &config, |_, _, _| {
            Ok(foreign_inspection(false))
        })
        .unwrap();
        let grant_candidate = managed_gh::list_candidates(state).unwrap().remove(0).0;
        managed_gh::grant_candidate(
            state,
            &grant_candidate,
            managed_gh::RequestedGrantScope::Command,
        )
        .unwrap();

        let route = route_managed_write(&approved_args, Some("/workspace"), &config, |_, _, _| {
            Ok(foreign_inspection(false))
        })
        .unwrap();
        assert!(route.is_granted());
        assert_eq!(managed_gh::list_approvals(state).unwrap().len(), 1);

        let edit = strs(&[
            "issue",
            "edit",
            "7",
            "--title=changed",
            "--repo=other/project",
        ]);
        assert!(matches!(
            route_managed_write(&edit, Some("/workspace"), &config, |_, _, _| {
                Ok(foreign_inspection(false))
            })
            .unwrap(),
            ManagedRoute::Pending(_)
        ));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn managed_gh_grant_identity_mismatches_fall_back_to_pending() {
        let (config, root) = managed_config("grant-identity-mismatch");
        let args = strs(&["issue", "comment", "7", "--body=x", "--repo=other/project"]);
        route_managed_write(&args, Some("/workspace"), &config, |_, _, _| {
            Ok(foreign_inspection(false))
        })
        .unwrap();
        let state = config.managed_gh_state.as_deref().unwrap();
        let handle = managed_gh::list_candidates(state).unwrap().remove(0).0;
        managed_gh::grant_candidate(state, &handle, managed_gh::RequestedGrantScope::Command)
            .unwrap();

        assert!(matches!(
            route_managed_write(&args, Some("/workspace"), &config, |_, _, _| {
                let mut inspection = foreign_inspection(false);
                inspection.repository_id = 43;
                Ok(inspection)
            })
            .unwrap(),
            ManagedRoute::Pending(_)
        ));
        assert!(matches!(
            route_managed_write(&args, Some("/workspace"), &config, |_, _, _| {
                let mut inspection = foreign_inspection(false);
                inspection.visibility = "private".to_string();
                Ok(inspection)
            })
            .unwrap(),
            ManagedRoute::Pending(_)
        ));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn managed_gh_grant_does_not_bypass_strict_parsing() {
        let (config, root) = managed_config("grant-strict-parser");
        let args = strs(&["issue", "comment", "7", "--body=x", "--repo=other/project"]);
        route_managed_write(&args, Some("/workspace"), &config, |_, _, _| {
            Ok(foreign_inspection(false))
        })
        .unwrap();
        let state = config.managed_gh_state.as_deref().unwrap();
        let handle = managed_gh::list_candidates(state).unwrap().remove(0).0;
        managed_gh::grant_candidate(state, &handle, managed_gh::RequestedGrantScope::Command)
            .unwrap();

        let refused_flag = strs(&[
            "issue",
            "comment",
            "7",
            "--body=x",
            "--web",
            "--repo=other/project",
        ]);
        assert!(
            route_managed_write(&refused_flag, Some("/workspace"), &config, |_, _, _| {
                panic!("refused flags must not reach inspection")
            })
            .is_err()
        );
        let invisible = strs(&[
            "issue",
            "comment",
            "7",
            "--body=x\u{200b}y",
            "--repo=other/project",
        ]);
        assert!(
            route_managed_write(&invisible, Some("/workspace"), &config, |_, _, _| {
                panic!("invisible text must not reach inspection")
            })
            .is_err()
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn pr_merge_is_refused_for_a_persistently_granted_repository() {
        let (mut config, root) = managed_config("grant-no-merge");
        let grant_args = strs(&["issue", "comment", "7", "--body=x", "--repo=other/project"]);
        route_managed_write(&grant_args, Some("/workspace"), &config, |_, _, _| {
            Ok(foreign_inspection(false))
        })
        .unwrap();
        let state = config.managed_gh_state.as_deref().unwrap();
        let handle = managed_gh::list_candidates(state).unwrap().remove(0).0;
        managed_gh::grant_candidate(state, &handle, managed_gh::RequestedGrantScope::Repository)
            .unwrap();
        config.allow_merge = true;
        let response = handle_request(
            Request {
                args: strs(&["pr", "merge", "7", "--squash", "--repo=other/project"]),
                cwd: Some("/workspace".to_string()),
            },
            &Arc::new(Mutex::new(File::create("/dev/null").unwrap())),
            &config,
        );
        assert_ne!(response.exit_code, 0);
        assert!(response.stderr.contains("outside the current repository"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn managed_gh_changed_argument_does_not_match_approval() {
        let (config, root) = managed_config("changed");
        let original = strs(&[
            "pr",
            "comment",
            "7",
            "--body=original",
            "--repo=other/project",
        ]);
        route_managed_write(&original, Some("/workspace"), &config, |_, _, _| {
            Ok(foreign_inspection(true))
        })
        .unwrap();
        let state = config.managed_gh_state.as_deref().unwrap();
        let handle = managed_gh::list_candidates(state).unwrap().remove(0).0;
        managed_gh::approve(state, &handle).unwrap();
        let changed = strs(&[
            "pr",
            "comment",
            "7",
            "--body=changed",
            "--repo=other/project",
        ]);
        assert!(matches!(
            route_managed_write(&changed, Some("/workspace"), &config, |_, _, _| {
                Ok(foreign_inspection(true))
            })
            .unwrap(),
            ManagedRoute::Pending(_)
        ));
        assert!(
            route_managed_write(&original, Some("/workspace"), &config, |_, _, _| {
                Ok(foreign_inspection(true))
            })
            .unwrap()
            .is_approved()
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn managed_gh_strict_parser_refuses_unsafe_constructs() {
        let hosts = BTreeSet::from(["github.com".to_string()]);
        for args in [
            vec![
                "issue",
                "create",
                "--title=x",
                "--body=y",
                "--repo=other/project",
                "--",
            ],
            vec![
                "issue",
                "create",
                "--title=x",
                "--body=y",
                "--repo=other/project",
                "-tx",
            ],
            vec![
                "issue",
                "create",
                "--title=x",
                "--body=y",
                "--repo=other/project",
                "--unknown=x",
            ],
            vec![
                "issue",
                "create",
                "--title",
                "--body=y",
                "--repo=other/project",
            ],
            vec![
                "issue",
                "create",
                "--title=x",
                "--body=y",
                "--repo=other/project",
                "--web",
            ],
            vec![
                "issue",
                "comment",
                "7",
                "--body=x",
                "--repo=other/project",
                "--edit-last",
            ],
            vec![
                "issue",
                "create",
                "--title=x",
                "--body=y",
                "--repo=other/project",
                "--template=x",
            ],
            vec![
                "issue",
                "create",
                "--title=x",
                "--body=y",
                "--repo=other/project",
                "--body-file=x",
            ],
            vec![
                "issue",
                "create",
                "--title=x",
                "--body=y",
                "--repo=other/project",
                "--project=x",
            ],
            vec![
                "issue",
                "create",
                "--title=x",
                "--body=y",
                "--repo=other/project",
                "--label=x",
            ],
            vec![
                "issue",
                "create",
                "--title=x",
                "--body=y",
                "--repo=other/project",
                "--assignee=x",
            ],
            vec![
                "issue",
                "create",
                "--title=x",
                "--body=y",
                "--repo=other/project",
                "--milestone=x",
            ],
            vec![
                "issue",
                "close",
                "7",
                "--repo=other/project",
                "--duplicate-of=8",
            ],
        ] {
            assert!(
                strict_foreign_plan(&strs(&args), &hosts).is_err(),
                "{}",
                args.join(" ")
            );
        }
    }

    #[test]
    fn managed_gh_strict_parser_refuses_every_excluded_flag() {
        let hosts = BTreeSet::from(["github.com".to_string()]);
        for flag in [
            "--web",
            "-w",
            "--edit-last",
            "--template=x",
            "-T",
            "--body-file=x",
            "-F",
            "--project=x",
            "-p",
            "--label=x",
            "-l",
            "--assignee=x",
            "-a",
            "--milestone=x",
            "-m",
            "--duplicate-of=8",
        ] {
            let mut args = strs(&["issue", "comment", "7", "--body=x", "--repo=other/project"]);
            args.push(flag.to_string());
            assert!(strict_foreign_plan(&args, &hosts).is_err(), "{flag}");
        }
    }

    #[test]
    fn managed_gh_strict_parser_enforces_targets_and_values() {
        let hosts = BTreeSet::from(["github.com".to_string()]);
        for args in [
            &["issue", "comment", "#5", "--body=x", "--repo=other/project"][..],
            &[
                "issue",
                "comment",
                "branch",
                "--body=x",
                "--repo=other/project",
            ],
            &["issue", "comment", "0", "--body=x", "--repo=other/project"],
            &[
                "issue",
                "comment",
                "2147483648",
                "--body=x",
                "--repo=other/project",
            ],
            &[
                "issue",
                "comment",
                "7",
                "8",
                "--body=x",
                "--repo=other/project",
            ],
            &["issue", "comment", "7", "--body=", "--repo=other/project"],
            &[
                "issue",
                "close",
                "7",
                "--reason=wontfix",
                "--repo=other/project",
            ],
            &["issue", "edit", "7", "--repo=other/project"],
            &[
                "issue",
                "create",
                "extra",
                "--title=x",
                "--body=y",
                "--repo=other/project",
            ],
            &[
                "issue",
                "create",
                "--title=x",
                "--body=y",
                "--repo=other/project",
                "--repo=other/project",
            ],
            &[
                "issue",
                "comment",
                "https://github.com/third/project/issues/7",
                "--body=x",
                "--repo=other/project",
            ],
            &[
                "issue",
                "comment",
                "http://github.com/other/project/issues/7",
                "--body=x",
            ],
            &[
                "issue",
                "comment",
                "https://evil.example/other/project/issues/7",
                "--body=x",
            ],
        ] {
            assert!(
                strict_foreign_plan(&strs(args), &hosts).is_err(),
                "{}",
                args.join(" ")
            );
        }
    }

    #[test]
    fn new_managed_commands_use_one_strict_command_table() {
        for args in [
            &[
                "pr",
                "create",
                "--title=x",
                "--body=y",
                "--head=topic",
                "--base=main",
                "--draft",
            ][..],
            &["pr", "edit", "7", "--title=x"],
            &["pr", "ready", "7", "--undo"],
            &["pr", "close", "7", "--comment=x"],
            &["pr", "reopen", "7"],
            &["issue", "reopen", "7", "-c", "x"],
        ] {
            assert!(
                strict_managed_plan(&strs(args)).is_ok(),
                "{}",
                args.join(" ")
            );
        }
        for args in [
            &[
                "pr",
                "create",
                "--title=x",
                "--body=y",
                "--head=topic",
                "--draft=yes",
            ][..],
            &["pr", "edit", "7"],
            &["pr", "ready", "branch"],
            &["pr", "close", "7", "--delete-branch"],
            &["pr", "reopen", "7", "--body=x"],
            &["issue", "reopen", "7", "--web"],
        ] {
            assert!(
                strict_managed_plan(&strs(args)).is_err(),
                "{}",
                args.join(" ")
            );
        }
    }

    #[test]
    fn conservative_ref_names_are_table_driven() {
        for accepted in ["main", "feature/x", "a-b_c.d+e@f", "x.locked"] {
            assert!(valid_ref_name(accepted), "{accepted}");
        }
        for refused in [
            "", "-x", "/x", ".x", "x/", "x.", "x.lock", "x..y", "x//y", "x@{y", "x:y", "x y",
        ] {
            assert!(!valid_ref_name(refused), "{refused}");
        }
        assert!(valid_head("owner:feature/x"));
        assert!(!valid_head("bad/owner:feature"));
        assert!(!valid_head("owner:bad:branch"));
        assert!(!valid_ref_name(&"x".repeat(256)));
    }

    fn pull_request_create_inspection(sha: &str) -> ForeignInspection {
        ForeignInspection {
            repository_id: 42,
            full_name: "other/project".to_string(),
            visibility: "private".to_string(),
            default_branch: Some("main".to_string()),
            item_node_id: None,
            item_title: None,
            item_state: None,
            item_is_pull_request: None,
            item_repository_url: None,
            head_sha: Some(sha.to_string()),
            commits: Some(2),
            changed_files: Some(3),
        }
    }

    #[test]
    fn pr_create_action_changes_when_head_sha_changes() {
        let (config, root) = managed_config("head-sha");
        let args = strs(&[
            "pr",
            "create",
            "--title=x",
            "--body=y",
            "--head=topic",
            "--base=main",
            "--repo=other/project",
        ]);
        assert!(matches!(
            route_managed_write(&args, Some("/workspace"), &config, |_, _, compare| {
                assert!(compare);
                Ok(pull_request_create_inspection("aaa"))
            })
            .unwrap(),
            ManagedRoute::Pending(_)
        ));
        let state = config.managed_gh_state.as_deref().unwrap();
        let handle = managed_gh::list_candidates(state).unwrap().remove(0).0;
        managed_gh::approve(state, &handle).unwrap();
        assert!(matches!(
            route_managed_write(&args, Some("/workspace"), &config, |_, _, _| {
                Ok(pull_request_create_inspection("bbb"))
            })
            .unwrap(),
            ManagedRoute::Pending(_)
        ));
        assert!(matches!(
            route_managed_write(&args, Some("/workspace"), &config, |_, _, _| {
                Ok(pull_request_create_inspection("aaa"))
            })
            .unwrap(),
            ManagedRoute::Approved(_)
        ));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn pr_create_rest_translation_is_exact() {
        let repository = RepositoryId {
            host: "github.example".to_string(),
            owner: "o".to_string(),
            name: "n".to_string(),
        };
        let call = pr_create_rest_call(
            &repository,
            &strs(&[
                "pr",
                "create",
                "--title=Title",
                "--body=Body",
                "--head=alice:topic",
                "--base=main",
                "--draft",
            ]),
        )
        .unwrap();
        assert_eq!(call.host, "github.example");
        assert_eq!(call.path, "/repos/o/n/pulls");
        assert_eq!(
            call.body,
            PullRequestBody {
                title: "Title".to_string(),
                body: "Body".to_string(),
                head: "alice:topic".to_string(),
                base: "main".to_string(),
                draft: true,
            }
        );
    }

    #[test]
    fn omitted_pr_base_uses_inspected_default_on_both_paths() {
        let config = test_config();
        let args = strs(&[
            "pr",
            "create",
            "--title=x",
            "--body=y",
            "--head=topic",
            "--repo=example/project",
        ]);
        let routed = route_managed_write(&args, None, &config, |_, _, compare| {
            assert!(!compare);
            let mut inspection = pull_request_create_inspection("");
            inspection.full_name = "example/project".to_string();
            inspection.head_sha = None;
            inspection.commits = None;
            inspection.changed_files = None;
            Ok(inspection)
        })
        .unwrap()
        .unwrap();
        assert!(routed.pr_create);
        assert!(routed.args.contains(&"--base=main".to_string()));

        let (managed, root) = managed_config("foreign-default-base");
        let foreign = strs(&[
            "pr",
            "create",
            "--title=x",
            "--body=y",
            "--head=topic",
            "--repo=other/project",
        ]);
        assert!(matches!(
            route_managed_write(&foreign, Some("/workspace"), &managed, |_, _, _| {
                Ok(pull_request_create_inspection("abc"))
            })
            .unwrap(),
            ManagedRoute::Pending(_)
        ));
        let candidate = managed_gh::list_candidates(managed.managed_gh_state.as_deref().unwrap())
            .unwrap()
            .remove(0)
            .1;
        assert!(candidate.action.args.contains(&"--base=main".to_string()));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn new_command_launch_routing_works_with_managed_gh_off() {
        let config = test_config();
        let args = strs(&["pr", "edit", "7", "--title=x", "--repo=example/project"]);
        let routed =
            route_managed_write(&args, None, &config, |_, _, _| panic!("must not inspect"))
                .unwrap();
        assert!(!routed.is_approved());
        let routed = routed.unwrap();
        assert_eq!(
            routed.args,
            strs(&[
                "pr",
                "edit",
                "7",
                "--title=x",
                "--repo",
                "github.com/example/project"
            ])
        );
    }

    #[test]
    fn new_foreign_command_is_refused_with_managed_gh_off() {
        let error = route_managed_write(
            &strs(&["pr", "reopen", "7", "--repo=other/project"]),
            Some("/workspace"),
            &test_config(),
            |_, _, _| panic!("must not inspect"),
        )
        .unwrap_err();
        assert!(error.contains("managed GitHub approvals are disabled"));
    }

    #[test]
    fn item_kind_and_repository_ownership_are_both_checked() {
        let (config, root) = managed_config("item-owner");
        let args = strs(&["pr", "reopen", "7", "--repo=other/project"]);
        let kind_error = route_managed_write(&args, Some("/workspace"), &config, |_, _, _| {
            Ok(foreign_inspection(false))
        })
        .unwrap_err();
        assert!(kind_error.contains("target is an issue"));
        let issue_args = strs(&["issue", "comment", "7", "--body=x", "--repo=other/project"]);
        let ownership_error =
            route_managed_write(&issue_args, Some("/workspace"), &config, |_, _, _| {
                let mut inspection = foreign_inspection(false);
                inspection.item_repository_url =
                    Some("https://api.github.com/repos/elsewhere/project".to_string());
                Ok(inspection)
            })
            .unwrap_err();
        assert!(ownership_error.contains("different repository"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn compare_inspection_uses_default_base_and_records_counts() {
        let plan = strict_managed_plan(&strs(&[
            "pr",
            "create",
            "--title=x",
            "--body=y",
            "--head=topic",
            "--repo=other/project",
        ]))
        .unwrap();
        let repository = plan.repository.clone().unwrap();
        let inspection = inspect_managed_action(&repository, &plan, true, |query| match query {
            InspectionQuery::Repository => Ok(br#"{"id":42,"full_name":"other/project","visibility":"private","default_branch":"trunk"}"#.to_vec()),
            InspectionQuery::Compare { base, head } => {
                assert_eq!(base, "trunk"); assert_eq!(head, "topic");
                Ok(br#"{"total_commits":2,"changed_files":3,"head_sha":"abc"}"#.to_vec())
            }
            InspectionQuery::Item(_) => panic!("pr create has no item"),
        }).unwrap();
        assert_eq!(inspection.head_sha.as_deref(), Some("abc"));
        assert_eq!(inspection.commits, Some(2));
        assert_eq!(inspection.changed_files, Some(3));
    }

    #[test]
    fn compare_inspection_rejects_invalid_default_base_before_compare() {
        let plan = strict_managed_plan(&strs(&[
            "pr",
            "create",
            "--title=x",
            "--body=y",
            "--head=topic",
            "--repo=other/project",
        ]))
        .unwrap();
        let repository = plan.repository.clone().unwrap();
        let error = inspect_managed_action(&repository, &plan, true, |query| match query {
            InspectionQuery::Repository => Ok(br#"{"id":42,"full_name":"other/project","visibility":"private","default_branch":"../../pulls"}"#.to_vec()),
            InspectionQuery::Compare { .. } => panic!("invalid base must not reach comparison"),
            InspectionQuery::Item(_) => panic!("pr create has no item"),
        })
        .unwrap_err();
        assert!(error.contains("default branch is not a valid conservative ref name"));
    }

    #[test]
    fn compare_inspection_rejects_more_than_250_commits() {
        let plan = strict_managed_plan(&strs(&[
            "pr",
            "create",
            "--title=x",
            "--body=y",
            "--head=topic",
            "--base=main",
            "--repo=other/project",
        ]))
        .unwrap();
        let repository = plan.repository.clone().unwrap();
        let error = inspect_managed_action(&repository, &plan, true, |query| match query {
            InspectionQuery::Repository => {
                Ok(br#"{"id":42,"full_name":"other/project","visibility":"private"}"#.to_vec())
            }
            InspectionQuery::Compare { .. } => {
                Ok(br#"{"total_commits":251,"changed_files":3,"head_sha":"wrong"}"#.to_vec())
            }
            InspectionQuery::Item(_) => panic!("pr create has no item"),
        })
        .unwrap_err();
        assert_eq!(
            error,
            "pull request has more than 250 commits; open it on GitHub"
        );
    }

    #[test]
    fn foreign_canonical_size_is_checked_before_and_after_inspection() {
        let (config, root) = managed_config("size-before-inspection");
        let oversized = vec![
            "pr".to_string(),
            "create".to_string(),
            "--title=x".to_string(),
            format!("--body={}", "x".repeat(16 * 1024)),
            "--head=topic".to_string(),
            "--repo=other/project".to_string(),
        ];
        let mut inspected = false;
        let error = route_managed_write(&oversized, Some("/workspace"), &config, |_, _, _| {
            inspected = true;
            Ok(pull_request_create_inspection("abc"))
        })
        .unwrap_err();
        assert!(error.contains("16 KiB"));
        assert!(!inspected);

        let near_limit = vec![
            "pr".to_string(),
            "create".to_string(),
            "--title=x".to_string(),
            format!("--body={}", "x".repeat(16_200)),
            "--head=topic".to_string(),
            "--repo=other/project".to_string(),
        ];
        let error = route_managed_write(&near_limit, Some("/workspace"), &config, |_, _, _| {
            let mut inspection = pull_request_create_inspection("abc");
            inspection.default_branch = Some("a".repeat(255));
            Ok(inspection)
        })
        .unwrap_err();
        assert!(error.contains("16 KiB"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn managed_gh_text_policy_and_size_are_enforced() {
        for character in [
            '\0',
            '\u{001f}',
            '\u{007f}',
            '\u{009f}',
            '\u{00ad}',
            '\u{061c}',
            '\u{180e}',
            '\u{200b}',
            '\u{200f}',
            '\u{2028}',
            '\u{202f}',
            '\u{2060}',
            '\u{206f}',
            '\u{feff}',
            '\u{fff9}',
            '\u{fffb}',
            '\u{fe00}',
            '\u{fe0e}',
            '\u{e0000}',
            '\u{e0fff}',
            '\u{e0100}',
            '\u{e01ef}',
        ] {
            assert!(validate_managed_text("--body", &format!("a{character}b")).is_err());
        }
        assert!(validate_managed_text("--body", "line one\nline\ttwo\u{fe0f}").is_ok());

        let hosts = BTreeSet::from(["github.com".to_string()]);
        let oversized = "x".repeat(16 * 1024);
        assert!(
            strict_foreign_plan(
                &[
                    "issue".to_string(),
                    "create".to_string(),
                    "--title=x".to_string(),
                    format!("--body={oversized}"),
                    "--repo=other/project".to_string(),
                ],
                &hosts,
            )
            .unwrap_err()
            .contains("16 KiB")
        );
    }

    #[test]
    fn managed_gh_enforces_item_kind_and_current_repository_name() {
        let (config, root) = managed_config("inspection");
        let issue = strs(&["issue", "comment", "7", "--body=x", "--repo=other/project"]);
        for args in [
            issue.clone(),
            strs(&["issue", "close", "7", "--repo=other/project"]),
            strs(&["issue", "edit", "7", "--title=x", "--repo=other/project"]),
        ] {
            assert!(
                route_managed_write(&args, Some("/workspace"), &config, |_, _, _| {
                    Ok(foreign_inspection(true))
                })
                .is_err()
            );
        }
        let pr = strs(&["pr", "comment", "7", "--body=x", "--repo=other/project"]);
        assert!(
            route_managed_write(&pr, Some("/workspace"), &config, |_, _, _| {
                Ok(foreign_inspection(false))
            })
            .unwrap_err()
            .contains("target is an issue")
        );
        assert!(
            route_managed_write(&issue, Some("/workspace"), &config, |_, _, _| {
                let mut inspection = foreign_inspection(false);
                inspection.full_name = "other/renamed".to_string();
                Ok(inspection)
            })
            .unwrap_err()
            .contains("other/renamed")
        );
        assert!(
            managed_gh::list_candidates(config.managed_gh_state.as_deref().unwrap())
                .unwrap()
                .is_empty()
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn managed_gh_inspection_failure_records_nothing() {
        let (config, root) = managed_config("failure");
        let args = strs(&["issue", "comment", "7", "--body=x", "--repo=other/project"]);
        assert!(
            route_managed_write(&args, Some("/workspace"), &config, |_, _, _| {
                Err("inspection failed".to_string())
            })
            .is_err()
        );
        assert!(
            managed_gh::list_candidates(config.managed_gh_state.as_deref().unwrap())
                .unwrap()
                .is_empty()
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn managed_gh_launch_grant_routes_without_approval() {
        let (config, root) = managed_config("grant");
        let args = strs(&[
            "issue",
            "comment",
            "7",
            "--body=x",
            "--repo=example/project",
        ]);
        let route = route_managed_write(&args, Some("/workspace"), &config, |_, _, _| {
            panic!("launch grants must not be inspected")
        })
        .unwrap();
        assert!(!route.is_approved());
        let route = route.unwrap();
        assert_eq!(
            route.args,
            strs(&[
                "issue",
                "comment",
                "7",
                "--body=x",
                "--repo",
                "github.com/example/project",
            ])
        );
        assert!(
            managed_gh::list_candidates(config.managed_gh_state.as_deref().unwrap())
                .unwrap()
                .is_empty()
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn disallowed_writes_never_enter_managed_gh() {
        let (config, root) = managed_config("disallowed");
        for args in [
            &["pr", "merge", "7", "--squash", "--repo=other/project"][..],
            &["run", "rerun", "7", "--repo=other/project"],
            &["ext", "milestone-create", "v1", "--repo=other/project"],
        ] {
            assert!(
                route_managed_write(&strs(args), Some("/workspace"), &config, |_, _, _| {
                    panic!("disallowed command was inspected")
                })
                .unwrap()
                .is_none()
            );
        }
        fs::remove_dir_all(root).unwrap();
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
            repository_from_remote("git@github.com:octocat/hello-world.git")
                .unwrap()
                .selector(),
            "github.com/octocat/hello-world"
        );
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
                .all(|grant| grant.repository.selector() != "github.com/private/outside")
        );
        let workspace_root = fs::canonicalize(&root).unwrap();
        let config = ProxyConfig {
            neutral_dir: workspace_root.clone(),
            workspace_root,
            grants,
            allow_merge: false,
            managed_gh_state: None,
            managed_gh_lock: Mutex::new(()),
        };
        assert_eq!(
            resolve_grant(&config, Some("/workspace/one"))
                .unwrap()
                .repository
                .selector(),
            "github.com/example/one"
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

    // ── Pull request merge ─────────────────────────────────────────

    fn merge_plan(args: &[&str]) -> Result<Vec<String>, String> {
        let config = test_config();
        let mut full = strs(&["pr", "merge"]);
        full.extend(strs(args));
        assert_eq!(reject_reason(&full), None, "gh {}", full.join(" "));
        let grant = &config.grants[0];
        plan_pr_merge(&full, grant).map(|plan| plan.args(&grant.repository))
    }

    #[test]
    fn test_merge_is_denied_unless_enabled_at_launch() {
        let log = Arc::new(Mutex::new(File::create("/dev/null").unwrap()));
        let request = Request {
            args: strs(&["pr", "merge", "123", "--squash"]),
            cwd: None,
        };
        let response = handle_request(request, &log, &test_config());
        assert_eq!(response.exit_code, 1);
        assert!(response.stderr.contains("--allow-merge"));
    }

    #[test]
    fn test_merge_plan_is_rebuilt_for_the_granted_repository() {
        assert_eq!(
            merge_plan(&["123", "--repo", "github.com/example/project", "--squash"]),
            Ok(strs(&[
                "pr",
                "merge",
                "123",
                "--repo",
                "github.com/example/project",
                "--squash"
            ]))
        );
        assert_eq!(
            merge_plan(&[
                "https://github.com/example/project/pull/45",
                "-R",
                "Example/Project",
                "--auto",
                "-m",
                "-b",
                "done",
                "--match-head-commit=abc123",
            ]),
            Ok(strs(&[
                "pr",
                "merge",
                "45",
                "--repo",
                "github.com/example/project",
                "--auto",
                "--merge",
                "--body=done",
                "--match-head-commit=abc123",
            ]))
        );
        assert_eq!(
            merge_plan(&["7", "--disable-auto"]),
            Ok(strs(&[
                "pr",
                "merge",
                "7",
                "--repo",
                "github.com/example/project",
                "--disable-auto"
            ]))
        );
    }

    #[test]
    fn test_merge_flag_values_cannot_become_flags() {
        assert_eq!(
            merge_plan(&["1", "--rebase", "--subject=--admin", "--body=-d"]),
            Ok(strs(&[
                "pr",
                "merge",
                "1",
                "--repo",
                "github.com/example/project",
                "--rebase",
                "--subject=--admin",
                "--body=-d",
            ]))
        );
    }

    #[test]
    fn test_merge_is_routed_only_after_the_fork_check() {
        let config = test_config();
        let grant = &config.grants[0];
        let route = |args: &[&str], probe: Result<&str, &str>| {
            route_pr_merge(&strs(args), grant, |number| {
                assert_eq!(number, "7");
                probe
                    .map(|json| json.as_bytes().to_vec())
                    .map_err(str::to_string)
            })
        };
        let own = r#"{"isCrossRepository":false}"#;
        let fork = r#"{"isCrossRepository":true}"#;

        assert!(route(&["pr", "merge", "007", "--squash"], Ok(own)).is_ok());
        assert!(route(&["pr", "merge", "7", "--squash"], Ok(fork)).is_err());
        assert!(route(&["pr", "merge", "7", "--auto", "--merge"], Ok(fork)).is_err());
        assert!(route(&["pr", "merge", "7", "--squash"], Err("unreachable")).is_err());
        assert!(route(&["pr", "merge", "7", "--squash"], Ok("{}")).is_err());
        assert!(
            route(
                &["pr", "merge", "7", "--disable-auto"],
                Err("not consulted")
            )
            .is_ok()
        );
    }

    #[test]
    fn test_merge_cannot_target_another_repository() {
        for args in [
            &["1", "--squash", "--repo", "other/project"][..],
            &["1", "--squash", "--repo=github.example.com/example/project"],
            &["1", "--squash", "-R", "example/project/extra/part"],
            &["https://github.com/other/project/pull/1", "--squash"],
            &["https://github.com/example/project/issues/1", "--squash"],
        ] {
            assert!(merge_plan(args).is_err(), "gh pr merge {}", args.join(" "));
        }
    }

    #[test]
    fn test_merge_requires_one_numbered_pull_request_and_one_method() {
        for args in [
            &["--squash"][..],
            &["", "--squash"],
            &["feature-branch", "--squash"],
            &["0", "--squash"],
            &["9223372036854775808", "--squash"],
            &["1", "2", "--squash"],
            &["1"],
            &["1", "--auto"],
            &["1", "--squash", "--rebase"],
            &["1", "--squash=false"],
            &["1", "--disable-auto", "--squash"],
            &["1", "--squash", "--body"],
            &["1", "--squash", "--", "2"],
        ] {
            assert!(merge_plan(args).is_err(), "gh pr merge {}", args.join(" "));
        }
    }

    #[test]
    fn test_merge_blocks_flags_that_widen_its_reach() {
        for flag in [
            "--admin",
            "--delete-branch",
            "-d",
            "--body-file",
            "-F",
            "--author-email",
            "-A",
            "-sd",
        ] {
            let args = strs(&["pr", "merge", "1", "--squash", flag]);
            assert!(reject_reason(&args).is_some(), "gh {}", args.join(" "));
            assert!(plan_pr_merge(&args, &test_config().grants[0]).is_err());
        }
    }

    #[test]
    fn test_logged_command_stays_on_one_line() {
        let args = strs(&["pr", "comment", "1", "--body", "a\nDENIED gh\r\\n"]);
        assert_eq!(
            loggable_command(&args),
            r"pr comment 1 --body a\nDENIED gh\r\\n"
        );
    }

    #[test]
    fn test_repo_selector_is_added_before_separator() {
        let mut args = strs(&["pr", "list", "--", "value"]);
        append_repo_selector(&mut args, &test_config().grants[0].repository);
        assert_eq!(
            args,
            strs(&[
                "pr",
                "list",
                "--repo",
                "github.com/example/project",
                "--",
                "value"
            ])
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
