use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions, Permissions};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::io::AsRawFd;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::Command;
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

fn parse_push_args(args: &[String]) -> Option<Push> {
    match args {
        [p] if p == "push" => Some(Push::Branch),
        [p, t] if p == "push" && t == "--tags" => Some(Push::Tags),
        _ => None,
    }
}

// Repo-local config keys that could make the host-side `git push` execute
// agent-controlled code or redirect the push somewhere unexpected. The
// workspace is agent-writable, so its .git/config is untrusted.
const DENIED_KEYS: &[&str] = &[
    "core.sshcommand",
    "core.hookspath",
    "core.fsmonitor",
    "core.askpass",
    "core.gitproxy",
    "core.pager",
    "remote.pushdefault",
];

const DENIED_PREFIXES: &[&str] = &[
    "credential.",
    "http.",
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

fn config_keys(raw: &[u8]) -> Vec<String> {
    config_entries(raw).into_iter().map(|(k, _)| k).collect()
}

fn credential_entries(entries: &[(String, String)]) -> Vec<(String, String)> {
    entries
        .iter()
        .filter(|(k, _)| k.starts_with("credential."))
        .cloned()
        .collect()
}

/// Credential config from the host's system and global scopes — the scopes
/// the agent cannot write to. Re-applied on the push command line after the
/// helper list is cleared, so a raced write to the workspace .git/config
/// between audit and push cannot inject a credential helper.
fn trusted_credential_config() -> Vec<(String, String)> {
    let mut trusted = Vec::new();
    for scope in ["--system", "--global"] {
        if let Ok(output) = Command::new("git")
            .args(["config", scope, "--list", "-z", "--includes"])
            .output()
            && output.status.success()
        {
            trusted.extend(credential_entries(&config_entries(&output.stdout)));
        }
    }
    trusted
}

fn denied_local_config(repository: &Path) -> Result<Vec<String>, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repository)
        .args(["config", "--local", "--list", "-z", "--includes"])
        .output()
        .map_err(|e| format!("failed to run git config: {}", e))?;
    if !output.status.success() {
        return Err(format!(
            "git config failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let mut denied: Vec<String> = config_keys(&output.stdout)
        .into_iter()
        .filter(|k| is_denied_key(k))
        .collect();
    denied.dedup();
    Ok(denied)
}

pub fn origin_url() -> Option<String> {
    let cwd = std::env::current_dir().ok()?;
    managed_push::origin_url_at(&cwd)
}

fn deny(stderr: String) -> Response {
    Response {
        exit_code: 1,
        stdout: String::new(),
        stderr,
    }
}

fn resolve_request_repository(
    req: &Request,
    mode: &Mode,
    log: &Arc<Mutex<File>>,
) -> Result<(PathBuf, String), Response> {
    match mode {
        Mode::Single { repository, origin } => Ok((repository.clone(), origin.clone())),
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
                        Ok(true) => Ok((repository_path, repository.origin)),
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

    let (repository, expected_origin) = match resolve_request_repository(&req, mode, log) {
        Ok(repository) => repository,
        Err(response) => return response,
    };
    let repository_label = repository.display();
    // Keep the validated directory open and address it through procfs for all
    // subsequent Git commands. Replacing the workspace path with a symlink
    // cannot redirect this request after authorization.
    let repository_handle = match File::open(&repository) {
        Ok(handle) => handle,
        Err(error) => {
            return deny(format!(
                "git-proxy: could not open approved repository: {error}"
            ));
        }
    };
    let repository_command_path = PathBuf::from(format!(
        "/proc/{}/fd/{}",
        std::process::id(),
        repository_handle.as_raw_fd()
    ));

    match denied_local_config(&repository_command_path) {
        Ok(keys) if keys.is_empty() => {}
        Ok(keys) => {
            let list = keys.join(", ");
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
        Err(e) => {
            log_line(log, &format!("ERROR   git {} ({})", cmd_str, e));
            return deny(format!("git-proxy: {}", e));
        }
    }

    match managed_push::origin_url_at(&repository_command_path) {
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

    log_line(
        log,
        &format!("ALLOWED git {} ({})", cmd_str, repository_label),
    );

    // -c has command-line precedence, so these pins survive even a raced
    // rewrite of the workspace .git/config after the audit above.
    let mut cmd = Command::new("git");
    cmd.current_dir(&repository_command_path);
    cmd.env("GIT_TERMINAL_PROMPT", "0");
    cmd.args([
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
    ]);
    for (key, value) in trusted_credential_config() {
        cmd.arg("-c").arg(format!("{}={}", key, value));
    }
    cmd.args(["push", "--no-verify"]);
    if push == Push::Tags {
        cmd.arg("--tags");
    }
    // Use the snapshotted URL itself. Passing the remote name here would let
    // an agent race a rewrite of remote.origin.url after the audit above.
    cmd.arg(&expected_origin);

    match cmd.output() {
        Ok(output) => {
            let exit_code = output.status.code().unwrap_or(1);
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
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
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
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "claude-sandbox-managed-push-{}-{nonce}",
            std::process::id()
        ));
        let workspace = root.join("workspace");
        let repository = workspace.join("project");
        let remote = root.join("remote.git");
        let state = root.join("state");
        fs::create_dir_all(&repository).unwrap();
        fs::create_dir_all(&state).unwrap();
        run_git(&root, &["init", "--bare", remote.to_str().unwrap()]);
        run_git(&root, &["init", repository.to_str().unwrap()]);
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

        let log_file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(root.join("proxy.log"))
            .unwrap();
        let log = Arc::new(Mutex::new(log_file));
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
        let output = Command::new("git")
            .arg("--git-dir")
            .arg(&remote)
            .args(["for-each-ref", "--format=%(refname)", "refs/heads/"])
            .output()
            .unwrap();
        assert!(String::from_utf8_lossy(&output.stdout).contains("refs/heads/"));

        run_git(&repository, &["config", "--unset", "push.default"]);
        run_git(&repository, &["config", "branch.master.remote", "origin"]);
        run_git(
            &repository,
            &["config", "branch.master.merge", "refs/heads/master"],
        );
        fs::write(repository.join("file.txt"), "second\n").unwrap();
        run_git(&repository, &["commit", "-am", "second"]);
        let simple_push = handle_request(request(), &mode, &log);
        assert_eq!(simple_push.exit_code, 0, "{}", simple_push.stderr);

        managed_push::revoke(&state, &candidate.repository.relative_path).unwrap();
        managed_push::approve(
            &state,
            &candidate.repository,
            managed_push::ApprovalScope::Once,
        )
        .unwrap();
        let one_time = handle_request(request(), &mode, &log);
        assert_eq!(one_time.exit_code, 0, "{}", one_time.stderr);
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
    }

    #[test]
    fn test_config_keys_parsing() {
        let raw = b"core.bare\nfalse\0remote.origin.url\nhttps://x\0key.with\nmulti\nline value\0";
        assert_eq!(
            config_keys(raw),
            vec!["core.bare", "remote.origin.url", "key.with"]
        );
    }

    #[test]
    fn test_config_keys_empty() {
        assert!(config_keys(b"").is_empty());
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
