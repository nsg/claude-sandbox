use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions, Permissions};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;
use std::{fs, thread};

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

struct CommandDef {
    group: &'static str,
    subcommand: &'static str,
    is_write: bool,
    allowed_flags: &'static [&'static str],
}

const COMMANDS: &[CommandDef] = &[
    // ── Read commands ──────────────────────────────────────────────
    CommandDef {
        group: "pr",
        subcommand: "list",
        is_write: false,
        allowed_flags: &[
            "--state", "-s", "--limit", "-L", "--json", "--jq", "-q",
            "--label", "-l", "--author", "-A", "--assignee", "-a",
            "--base", "-B", "--head", "-H", "--search", "-S",
            "--draft", "-d", "--template", "-t", "--web", "-w",
            "--repo", "-R", "--app",
        ],
    },
    CommandDef {
        group: "pr",
        subcommand: "view",
        is_write: false,
        allowed_flags: &[
            "--json", "--jq", "-q", "--comments", "-c",
            "--template", "-t", "--web", "-w", "--repo", "-R",
        ],
    },
    CommandDef {
        group: "pr",
        subcommand: "diff",
        is_write: false,
        allowed_flags: &[
            "--color", "--patch", "--name-only", "--repo", "-R",
        ],
    },
    CommandDef {
        group: "pr",
        subcommand: "checks",
        is_write: false,
        allowed_flags: &[
            "--json", "--jq", "-q", "--watch", "--interval", "-i",
            "--fail-fast", "--required", "--web", "-w", "--repo", "-R",
        ],
    },
    CommandDef {
        group: "issue",
        subcommand: "list",
        is_write: false,
        allowed_flags: &[
            "--state", "-s", "--limit", "-L", "--json", "--jq", "-q",
            "--label", "-l", "--author", "-A", "--assignee", "-a",
            "--milestone", "-m", "--search", "-S",
            "--template", "-t", "--web", "-w", "--repo", "-R",
        ],
    },
    CommandDef {
        group: "issue",
        subcommand: "view",
        is_write: false,
        allowed_flags: &[
            "--json", "--jq", "-q", "--comments", "-c",
            "--template", "-t", "--web", "-w", "--repo", "-R",
        ],
    },
    CommandDef {
        group: "repo",
        subcommand: "view",
        is_write: false,
        allowed_flags: &[
            "--json", "--jq", "-q", "--template", "-t",
            "--web", "-w", "--repo", "-R",
        ],
    },
    CommandDef {
        group: "release",
        subcommand: "list",
        is_write: false,
        allowed_flags: &[
            "--limit", "-L", "--json", "--jq", "-q",
            "--exclude-drafts", "--exclude-pre-releases",
            "--order", "-O", "--repo", "-R",
        ],
    },
    CommandDef {
        group: "release",
        subcommand: "view",
        is_write: false,
        allowed_flags: &[
            "--json", "--jq", "-q", "--template", "-t",
            "--web", "-w", "--repo", "-R",
        ],
    },
    CommandDef {
        group: "run",
        subcommand: "list",
        is_write: false,
        allowed_flags: &[
            "--limit", "-L", "--json", "--jq", "-q",
            "--branch", "-b", "--workflow", "-w",
            "--status", "-s", "--event", "-e",
            "--user", "-u", "--commit", "-c", "--repo", "-R",
        ],
    },
    CommandDef {
        group: "run",
        subcommand: "view",
        is_write: false,
        allowed_flags: &[
            "--json", "--jq", "-q", "--log", "--log-failed",
            "--exit-status", "--verbose", "-v",
            "--web", "-w", "--job", "-j", "--attempt", "--repo", "-R",
        ],
    },
    // ── Write commands (no --repo/-R, no --body-file/-F) ───────────
    CommandDef {
        group: "pr",
        subcommand: "create",
        is_write: true,
        allowed_flags: &[
            "--title", "-t", "--body", "-b", "--base", "-B", "--head", "-H",
            "--draft", "-d", "--label", "-l", "--assignee", "-a",
            "--reviewer", "-r", "--milestone", "-m",
            "--fill", "-f", "--fill-first", "--fill-verbose",
            "--web", "-w", "--template", "-T", "--no-maintainer-edit",
        ],
    },
    CommandDef {
        group: "pr",
        subcommand: "comment",
        is_write: true,
        allowed_flags: &[
            "--body", "-b", "--edit-last", "--web", "-w",
        ],
    },
    CommandDef {
        group: "issue",
        subcommand: "create",
        is_write: true,
        allowed_flags: &[
            "--title", "-t", "--body", "-b", "--label", "-l",
            "--assignee", "-a", "--milestone", "-m",
            "--project", "-p", "--web", "-w", "--template", "-T",
        ],
    },
    CommandDef {
        group: "issue",
        subcommand: "comment",
        is_write: true,
        allowed_flags: &[
            "--body", "-b", "--edit-last", "--web", "-w",
        ],
    },
];

fn find_command<'a>(group: &str, subcommand: &str) -> Option<&'a CommandDef> {
    COMMANDS
        .iter()
        .find(|c| c.group == group && c.subcommand == subcommand)
}

/// Extract the flag name from an arg, handling `--flag=value` forms.
fn extract_flag(arg: &str) -> &str {
    if arg.starts_with("--") {
        if let Some(eq) = arg.find('=') {
            return &arg[..eq];
        }
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

fn reject_reason(args: &[String]) -> Option<String> {
    if args.len() < 2 {
        return Some(format!("command not allowed: gh {}", args.join(" ")));
    }

    let group = args[0].as_str();
    let subcommand = args[1].as_str();

    let cmd = match find_command(group, subcommand) {
        Some(c) => c,
        None => return Some(format!("command not allowed: gh {} {}", group, subcommand)),
    };

    if let Err(flag) = check_flags(args, cmd.allowed_flags) {
        return Some(format!("flag not allowed for gh {} {}: {}", group, subcommand, flag));
    }

    if cmd.is_write {
        // Write commands: --repo/-R is not in allowed_flags, so check_flags
        // already rejects it. But give a clearer message if someone tries.
        // (This is a belt-and-suspenders check — check_flags catches it first.)
    }

    None
}

fn timestamp() -> String {
    let dur = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = dur.as_secs();
    // UTC breakdown without external crates
    let days = secs / 86400;
    let time_secs = secs % 86400;
    let h = time_secs / 3600;
    let m = (time_secs % 3600) / 60;
    let s = time_secs % 60;
    // Days since 1970-01-01
    let mut y: u64 = 1970;
    let mut remaining = days;
    loop {
        let leap = y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
        let ydays: u64 = if leap { 366 } else { 365 };
        if remaining < ydays {
            break;
        }
        remaining -= ydays;
        y += 1;
    }
    let leap = y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
    let mdays: &[u64] = if leap {
        &[31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    } else {
        &[31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    };
    let mut mo: u64 = 0;
    for md in mdays {
        if remaining < *md {
            break;
        }
        remaining -= md;
        mo += 1;
    }
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        y,
        mo + 1,
        remaining + 1,
        h,
        m,
        s
    )
}

fn log_line(log: &Arc<Mutex<File>>, message: &str) {
    if let Ok(mut f) = log.lock() {
        let _ = writeln!(f, "{} {}", timestamp(), message);
    }
}

fn handle_request(req: Request, log: &Arc<Mutex<File>>) -> Response {
    let cmd_str = req.args.join(" ");

    if let Some(reason) = reject_reason(&req.args) {
        log_line(log, &format!("DENIED  gh {} ({})", cmd_str, reason));
        return Response {
            exit_code: 1,
            stdout: String::new(),
            stderr: format!("gh-proxy: {}", reason),
        };
    }

    log_line(log, &format!("ALLOWED gh {}", cmd_str));

    match Command::new("gh").args(&req.args).output() {
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

pub fn run(socket_path: &str) {
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
        eprintln!("gh-proxy: failed to bind {}: {}", socket_path, e);
        std::process::exit(1);
    });

    let log_path = path.with_file_name("gh-proxy.log");
    let log_file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .unwrap_or_else(|e| {
            eprintln!("gh-proxy: failed to open log {}: {}", log_path.display(), e);
            std::process::exit(1);
        });
    let log = Arc::new(Mutex::new(log_file));

    log_line(&log, &format!("listening on {}", socket_path));
    eprintln!("gh-proxy: listening on {}", socket_path);

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let log = Arc::clone(&log);
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
                            Ok(req) => handle_request(req, &log),
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
                eprintln!("gh-proxy: connection error: {}", e);
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
        assert!(reject_reason(&strs(&["pr", "list"])).is_none());
        assert!(reject_reason(&strs(&["pr", "list", "--state", "open"])).is_none());
        assert!(reject_reason(&strs(&["pr", "view", "123", "--json", "title"])).is_none());
        assert!(reject_reason(&strs(&["pr", "diff", "123"])).is_none());
        assert!(reject_reason(&strs(&["pr", "checks", "123"])).is_none());
        assert!(reject_reason(&strs(&["issue", "list", "--limit", "10"])).is_none());
        assert!(reject_reason(&strs(&["issue", "view", "42", "--comments"])).is_none());
        assert!(reject_reason(&strs(&["repo", "view", "--json", "description"])).is_none());
        assert!(reject_reason(&strs(&["release", "list"])).is_none());
        assert!(reject_reason(&strs(&["release", "view", "v1.0"])).is_none());
        assert!(reject_reason(&strs(&["run", "list"])).is_none());
        assert!(reject_reason(&strs(&["run", "view", "12345", "--log"])).is_none());
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
        assert!(reject_reason(&strs(&["pr", "create", "--title", "foo", "--body", "bar"])).is_none());
        assert!(reject_reason(&strs(&["pr", "comment", "123", "--body", "hi"])).is_none());
        assert!(reject_reason(&strs(&["issue", "create", "--title", "bug"])).is_none());
        assert!(reject_reason(&strs(&["issue", "comment", "42", "--body", "x"])).is_none());
    }

    #[test]
    fn test_write_commands_block_repo_flag() {
        let r = reject_reason(&strs(&["pr", "create", "-R", "other/repo", "--title", "foo"]));
        assert!(r.is_some());
        assert!(r.unwrap().contains("flag not allowed"));

        assert!(reject_reason(&strs(&["pr", "create", "--repo", "other/repo"])).is_some());
        assert!(reject_reason(&strs(&["pr", "create", "--repo=other/repo"])).is_some());
        assert!(reject_reason(&strs(&["issue", "create", "--repo", "other/repo"])).is_some());
        assert!(reject_reason(&strs(&["issue", "comment", "1", "-R", "other/repo"])).is_some());
    }

    #[test]
    fn test_write_commands_block_body_file() {
        let r = reject_reason(&strs(&["pr", "create", "--title", "t", "--body-file", "/etc/passwd"]));
        assert!(r.is_some());
        assert!(r.unwrap().contains("--body-file"));

        assert!(reject_reason(&strs(&["pr", "comment", "1", "-F", "file.txt"])).is_some());
        assert!(reject_reason(&strs(&["issue", "create", "--body-file", "f"])).is_some());
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
        assert!(reject_reason(&strs(&["issue", "close", "42"])).is_some());
        assert!(reject_reason(&strs(&["issue", "edit", "42"])).is_some());
        assert!(reject_reason(&strs(&["repo", "create"])).is_some());
        assert!(reject_reason(&strs(&["repo", "delete"])).is_some());
        assert!(reject_reason(&strs(&["release", "create"])).is_some());
        assert!(reject_reason(&strs(&["release", "delete"])).is_some());
        assert!(reject_reason(&strs(&["run", "rerun"])).is_some());
        assert!(reject_reason(&strs(&["run", "cancel"])).is_some());
    }

    #[test]
    fn test_empty_args() {
        assert!(reject_reason(&[]).is_some());
    }

    #[test]
    fn test_single_arg() {
        assert!(reject_reason(&strs(&["pr"])).is_some());
    }

    fn strs(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| x.to_string()).collect()
    }
}
