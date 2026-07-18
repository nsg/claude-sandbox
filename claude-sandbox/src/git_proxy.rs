use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions, Permissions};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use std::{fs, process, thread};

use crate::logging::log_line;

#[derive(Deserialize)]
struct Request {
    args: Vec<String>,
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

fn denied_local_config() -> Result<Vec<String>, String> {
    let output = Command::new("git")
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
    let output = Command::new("git")
        .args(["remote", "get-url", "origin"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let url = String::from_utf8(output.stdout).ok()?.trim().to_string();
    if url.is_empty() { None } else { Some(url) }
}

fn deny(stderr: String) -> Response {
    Response {
        exit_code: 1,
        stdout: String::new(),
        stderr,
    }
}

fn handle_request(req: Request, expected_origin: &str, log: &Arc<Mutex<File>>) -> Response {
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

    match denied_local_config() {
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

    match origin_url() {
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

    log_line(log, &format!("ALLOWED git {}", cmd_str));

    // -c has command-line precedence, so these pins survive even a raced
    // rewrite of the workspace .git/config after the audit above.
    let mut cmd = Command::new("git");
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
    // Explicit remote: ignores remote.pushDefault / branch.*.pushRemote,
    // so the audited+pinned origin is the only possible destination.
    cmd.arg("origin");

    match cmd.output() {
        Ok(output) => {
            let exit_code = output.status.code().unwrap_or(1);
            log_line(log, &format!("EXIT    git {} -> {}", cmd_str, exit_code));
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

pub fn run(socket_path: &str, origin: &str) {
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

    log_line(
        &log,
        &format!("listening on {} (origin: {})", socket_path, origin),
    );

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

    let origin = Arc::new(origin.to_string());
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let log = Arc::clone(&log);
                let origin = Arc::clone(&origin);
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
                            Ok(req) => handle_request(req, &origin, &log),
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
