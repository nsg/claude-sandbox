use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fs::{File, OpenOptions, Permissions};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime};
use std::{fs, process, thread};

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

struct ExtCommandDef {
    group: &'static str,
    subcommand: &'static str,
    description: &'static str,
    help_text: &'static str,
    handler: fn(&[String]) -> Response,
}

const COMMANDS: &[CommandDef] = &[
    // ── Read commands ──────────────────────────────────────────────
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
    // ── Write commands (no --repo/-R, no --body-file/-F) ───────────
    CommandDef {
        group: "pr",
        subcommand: "create",
        is_write: true,
        allowed_flags: &[
            "--title",
            "-t",
            "--body",
            "-b",
            "--base",
            "-B",
            "--head",
            "-H",
            "--draft",
            "-d",
            "--label",
            "-l",
            "--assignee",
            "-a",
            "--reviewer",
            "-r",
            "--milestone",
            "-m",
            "--fill",
            "-f",
            "--fill-first",
            "--fill-verbose",
            "--web",
            "-w",
            "--template",
            "-T",
            "--no-maintainer-edit",
        ],
    },
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
];

// ── Extension commands (gh ext …) ─────────────────────────────────────

const EXT_COMMANDS: &[ExtCommandDef] = &[ExtCommandDef {
    group: "ext",
    subcommand: "run-logs",
    description: "Download workflow run logs",
    help_text: "gh ext run-logs <run-id> (workspace repo only)\n\n\
                    Download workflow run logs for the current repository.\n\
                    Translates to: gh api /repos/{owner}/{repo}/actions/runs/{run-id}/logs\n",
    handler: handle_run_logs,
}];

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

// ── Extension command handlers ────────────────────────────────────────

/// Detect the workspace repo slug (owner/repo) from git remote, cached.
fn detect_repo() -> Option<&'static str> {
    static REPO_SLUG: OnceLock<Option<String>> = OnceLock::new();
    REPO_SLUG
        .get_or_init(|| {
            let output = Command::new("git")
                .args(["remote", "get-url", "origin"])
                .output()
                .ok()?;
            let url = String::from_utf8(output.stdout).ok()?.trim().to_string();
            // Handle SSH: git@github.com:owner/repo.git
            if let Some(rest) = url.strip_prefix("git@github.com:") {
                return Some(rest.trim_end_matches(".git").to_string());
            }
            // Handle HTTPS: https://github.com/owner/repo.git
            if let Some(rest) = url
                .strip_prefix("https://github.com/")
                .or_else(|| url.strip_prefix("http://github.com/"))
            {
                return Some(rest.trim_end_matches(".git").to_string());
            }
            None
        })
        .as_deref()
}

fn maybe_ext_command(args: &[String]) -> Option<Response> {
    if args.len() < 2 {
        return None;
    }
    let ext = find_ext_command(&args[0], &args[1])?;
    Some((ext.handler)(&args[2..]))
}

fn handle_run_logs(args: &[String]) -> Response {
    if args.is_empty() {
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

    let repo = match detect_repo() {
        Some(r) => r,
        None => {
            return Response {
                exit_code: 1,
                stdout: String::new(),
                stderr: "gh-proxy: could not detect repository from git remote".to_string(),
            };
        }
    };

    let api_path = format!("/repos/{}/actions/runs/{}/logs", repo, run_id);

    match Command::new("gh").args(["api", &api_path]).output() {
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
        " (write — workspace repo only, no -R/--repo)"
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
    if args.len() == 1 {
        if is_help_flag(&args[0]) || args[0] == "help" {
            return Some(help_toplevel());
        }
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
        return Some(format!(
            "flag not allowed for gh {} {}: {}",
            group, subcommand, flag
        ));
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

    if let Some(help_text) = maybe_help(&req.args) {
        log_line(log, &format!("HELP    gh {}", cmd_str));
        return Response {
            exit_code: 0,
            stdout: help_text,
            stderr: String::new(),
        };
    }

    if let Some(response) = maybe_ext_command(&req.args) {
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
        assert!(
            reject_reason(&strs(&["pr", "create", "--title", "foo", "--body", "bar"])).is_none()
        );
        assert!(reject_reason(&strs(&["pr", "comment", "123", "--body", "hi"])).is_none());
        assert!(reject_reason(&strs(&["issue", "create", "--title", "bug"])).is_none());
        assert!(reject_reason(&strs(&["issue", "comment", "42", "--body", "x"])).is_none());
    }

    #[test]
    fn test_write_commands_block_repo_flag() {
        let r = reject_reason(&strs(&[
            "pr",
            "create",
            "-R",
            "other/repo",
            "--title",
            "foo",
        ]));
        assert!(r.is_some());
        assert!(r.unwrap().contains("flag not allowed"));

        assert!(reject_reason(&strs(&["pr", "create", "--repo", "other/repo"])).is_some());
        assert!(reject_reason(&strs(&["pr", "create", "--repo=other/repo"])).is_some());
        assert!(reject_reason(&strs(&["issue", "create", "--repo", "other/repo"])).is_some());
        assert!(reject_reason(&strs(&["issue", "comment", "1", "-R", "other/repo"])).is_some());
    }

    #[test]
    fn test_write_commands_block_body_file() {
        let r = reject_reason(&strs(&[
            "pr",
            "create",
            "--title",
            "t",
            "--body-file",
            "/etc/passwd",
        ]));
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

    // ── Help ────────────────────────────────────────────────────────

    #[test]
    fn test_help_toplevel() {
        let h = maybe_help(&[]).unwrap();
        assert!(h.contains("pr"));
        assert!(h.contains("issue"));
        assert!(h.contains("repo"));
        assert!(h.contains("release"));
        assert!(h.contains("run"));

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
    }

    #[test]
    fn test_help_command() {
        let h = maybe_help(&strs(&["pr", "list", "--help"])).unwrap();
        assert!(h.contains("--state"));
        assert!(h.contains("--limit"));
        assert!(h.contains("--json"));
        assert!(h.contains("(read)"));

        // Write command shows workspace restriction
        let h2 = maybe_help(&strs(&["pr", "create", "-h"])).unwrap();
        assert!(h2.contains("--title"));
        assert!(h2.contains("workspace repo only"));

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

    #[test]
    fn test_ext_run_logs_valid_id() {
        let r = maybe_ext_command(&strs(&["ext", "run-logs", "12345"]));
        assert!(r.is_some());
    }

    #[test]
    fn test_ext_run_logs_rejects_non_numeric_id() {
        let r = maybe_ext_command(&strs(&["ext", "run-logs", "../etc/passwd"])).unwrap();
        assert_eq!(r.exit_code, 1);
        assert!(r.stderr.contains("invalid run id"));
    }

    #[test]
    fn test_ext_run_logs_missing_id() {
        let r = maybe_ext_command(&strs(&["ext", "run-logs"])).unwrap();
        assert_eq!(r.exit_code, 1);
        assert!(r.stderr.contains("usage"));
    }

    #[test]
    fn test_ext_not_matched_for_other_commands() {
        assert!(maybe_ext_command(&strs(&["pr", "list"])).is_none());
        assert!(maybe_ext_command(&strs(&["run", "list"])).is_none());
        assert!(maybe_ext_command(&strs(&["run", "logs"])).is_none());
    }

    #[test]
    fn test_ext_run_logs_help() {
        let h = maybe_help(&strs(&["ext", "run-logs", "-h"])).unwrap();
        assert!(h.contains("run-id"));
        assert!(h.contains("workspace repo only"));
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

    fn strs(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| x.to_string()).collect()
    }
}
