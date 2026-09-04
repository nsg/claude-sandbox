use crate::logging::log_line;
use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use std::{process, thread};

use crate::managed_fetch;
use crate::{proxy_log, proxy_socket};

#[derive(Deserialize)]
struct Request {
    args: Vec<String>,
    #[serde(default)]
    cwd: Option<String>,
}

#[derive(Deserialize, Serialize, Clone)]
pub struct Config {
    #[serde(default)]
    pub git: Vec<String>,
    #[serde(default)]
    pub command: Vec<String>,
    #[serde(default)]
    pub host: Vec<String>,
}

#[derive(Serialize)]
struct HandshakeResponse {
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
}

struct ParsedRequest {
    dest: String,
    user: String,
    host: String,
    command: String,
}

#[derive(Clone)]
pub struct ManagedFetch {
    pub workspace_root: PathBuf,
    pub state_dir: PathBuf,
}

struct GitWireCommand {
    service: String,
    repository: String,
}

#[derive(Debug)]
enum Authorization {
    Static,
    ManagedFetch(Vec<String>),
}

const GIT_SERVICES: &[&str] = &["git-receive-pack", "git-upload-pack", "git-upload-archive"];

const FRAME_EXIT: u8 = 0;
const FRAME_STDOUT: u8 = 1;
const FRAME_STDERR: u8 = 2;
const MAX_FRAME: usize = 65536;
const MAX_HANDSHAKE: usize = 16_384;
const MAX_HOST_LEN: usize = 253;
const MAX_REPOSITORY_LEN: usize = 1_024;
const MAX_CONNECTIONS: usize = 32;
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_SESSION_DURATION: Duration = Duration::from_secs(30 * 60);

struct ConnectionGuard(Arc<AtomicUsize>);

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

pub fn default_config() -> Config {
    Config {
        git: vec![],
        command: vec![],
        host: vec![],
    }
}

pub fn is_empty(config: &Config) -> bool {
    config.git.is_empty() && config.command.is_empty() && config.host.is_empty()
}

fn glob_match(pattern: &str, text: &str) -> bool {
    let p = pattern.as_bytes();
    let t = text.as_bytes();
    let mut pi = 0;
    let mut ti = 0;
    let mut star_p = usize::MAX;
    let mut star_t = 0;

    while ti < t.len() {
        if pi < p.len() && p[pi] == b'*' {
            star_p = pi;
            star_t = ti;
            pi += 1;
        } else if pi < p.len() && p[pi] == t[ti] {
            pi += 1;
            ti += 1;
        } else if star_p != usize::MAX {
            pi = star_p + 1;
            star_t += 1;
            ti = star_t;
        } else {
            return false;
        }
    }

    while pi < p.len() && p[pi] == b'*' {
        pi += 1;
    }

    pi == p.len()
}

fn parse_request(args: &[String]) -> Result<ParsedRequest, String> {
    let usage = "expected format: ssh user@host command [args...]";

    if args.is_empty() {
        return Err(format!("no destination. {}", usage));
    }

    for (i, arg) in args.iter().enumerate() {
        if arg.starts_with('-') {
            if i > 0 {
                return Err(format!(
                    "argument {} looks like an SSH flag but flags are not supported. \
                     If this is part of the remote command, pass it as a single quoted string: \
                     ssh {} \"{}\"",
                    arg,
                    args[0],
                    args[1..].join(" ")
                ));
            }
            return Err(format!(
                "ssh flags are not supported (got {}). {}",
                arg, usage
            ));
        }
    }

    let dest = &args[0];
    let at_pos = dest
        .find('@')
        .ok_or(format!("destination must be user@host. {}", usage))?;
    let user = dest[..at_pos].to_string();
    let host = dest[at_pos + 1..].to_string();

    if user.is_empty() || host.is_empty() {
        return Err(format!("destination must be user@host. {}", usage));
    }

    let command = args[1..].join(" ");
    if command.is_empty() {
        return Err(format!("interactive sessions are not supported. {}", usage));
    }

    Ok(ParsedRequest {
        dest: dest.to_string(),
        user,
        host,
        command,
    })
}

#[cfg(test)]
fn is_git_command(command: &str) -> bool {
    parse_git_command(command).is_some()
}

fn is_valid_git_repo(repo: &str) -> bool {
    !repo.is_empty()
        && repo.len() <= MAX_REPOSITORY_LEN
        && !repo.starts_with('-')
        && !repo.starts_with("//")
        && repo
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '-'))
        && repo
            .trim_start_matches('/')
            .split('/')
            .all(|segment| !segment.is_empty() && !matches!(segment, "." | ".."))
}

fn parse_git_wire_command(command: &str) -> Option<GitWireCommand> {
    let (service, rest) = command.split_once(' ')?;
    if !GIT_SERVICES.contains(&service) {
        return None;
    }

    let rest = rest.trim();
    if rest.is_empty() {
        return None;
    }

    let repo = if rest.starts_with('\'') {
        if !rest.ends_with('\'') || rest.len() < 2 {
            return None;
        }
        let repo = &rest[1..rest.len() - 1];
        if repo.contains('\'') {
            return None;
        }
        repo
    } else {
        if rest.chars().any(char::is_whitespace) {
            return None;
        }
        rest
    };

    if is_valid_git_repo(repo) {
        Some(GitWireCommand {
            service: service.to_string(),
            repository: repo.to_string(),
        })
    } else {
        None
    }
}

fn parse_git_command(command: &str) -> Option<(String, String)> {
    let parsed = parse_git_wire_command(command)?;
    let repository = parsed
        .repository
        .strip_prefix('/')
        .unwrap_or(&parsed.repository);
    let repository = repository.strip_suffix(".git").unwrap_or(repository);
    Some((parsed.service, repository.to_string()))
}

fn is_valid_managed_host(host: &str) -> bool {
    if host.is_empty() || host.len() > MAX_HOST_LEN || !host.is_ascii() {
        return false;
    }
    host.split('.').all(|label| {
        !label.is_empty()
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    })
}

fn managed_request_context(workspace_root: &Path, cwd: &str) -> Result<String, String> {
    let workspace_root = fs::canonicalize(workspace_root)
        .map_err(|error| format!("could not resolve workspace root: {error}"))?;
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
    let resolved = fs::canonicalize(workspace_root.join(relative))
        .map_err(|error| format!("could not resolve working directory: {error}"))?;
    if !resolved.starts_with(&workspace_root) {
        return Err("working directory escapes the mounted workspace".to_string());
    }
    let relative = resolved
        .strip_prefix(&workspace_root)
        .map_err(|_| "working directory escapes the mounted workspace".to_string())?;
    Ok(if relative.as_os_str().is_empty() {
        ".".to_string()
    } else {
        relative
            .to_str()
            .ok_or_else(|| "working directory is not UTF-8".to_string())?
            .to_string()
    })
}

#[cfg(test)]
fn extract_repo(command: &str) -> Option<String> {
    parse_git_command(command).map(|(_, repo)| repo)
}

fn parse_git_entry(entry: &str) -> (&str, Option<&str>) {
    match entry.find('/') {
        Some(pos) => (
            &entry[..pos],
            Some(entry[pos..].strip_prefix('/').unwrap_or(&entry[pos..])),
        ),
        None => (entry, None),
    }
}

fn check_git_rules(req: &ParsedRequest, rules: &[String]) -> bool {
    if req.user != "git" {
        return false;
    }
    let Some((_, repo)) = parse_git_command(&req.command) else {
        return false;
    };

    for entry in rules {
        let (host_pattern, repo_pattern) = parse_git_entry(entry);

        if !glob_match(host_pattern, &req.host) {
            continue;
        }

        if let Some(pattern) = repo_pattern {
            if glob_match(pattern, &repo) {
                return true;
            }
        } else {
            return true;
        }
    }

    false
}

fn check_command_rules(req: &ParsedRequest, rules: &[String]) -> bool {
    let full = format!("{} {}", req.dest, req.command);
    rules.contains(&full)
}

fn check_host_rules(req: &ParsedRequest, rules: &[String]) -> bool {
    rules.contains(&req.dest)
}

fn check_allowed(args: &[String], config: &Config) -> Result<(), String> {
    let req = parse_request(args)?;

    if check_git_rules(&req, &config.git) {
        return Ok(());
    }
    if check_command_rules(&req, &config.command) {
        return Ok(());
    }
    if check_host_rules(&req, &config.host) {
        return Ok(());
    }

    Err(format!(
        "denied: {} {} (ask the user to update ssh-proxy.json to allow this command)",
        req.dest, req.command
    ))
}

fn managed_fetch_args(source: &managed_fetch::Source) -> Vec<String> {
    vec![
        "-T".to_string(),
        "-o".to_string(),
        "BatchMode=yes".to_string(),
        "-o".to_string(),
        "ClearAllForwardings=yes".to_string(),
        "-o".to_string(),
        "ForwardAgent=no".to_string(),
        "-o".to_string(),
        "ForwardX11=no".to_string(),
        "-o".to_string(),
        "PermitLocalCommand=no".to_string(),
        "-o".to_string(),
        "RequestTTY=no".to_string(),
        "-o".to_string(),
        "StrictHostKeyChecking=yes".to_string(),
        "-o".to_string(),
        "UpdateHostKeys=no".to_string(),
        "-o".to_string(),
        "NumberOfPasswordPrompts=0".to_string(),
        format!("git@{}", source.host),
        format!("git-upload-pack '{}'", source.repository),
    ]
}

fn git_transport_args(args: &[String]) -> Result<&[String], String> {
    if args.first().map(String::as_str) != Some("-o") {
        return Ok(args);
    }
    if args.get(1).map(String::as_str) != Some("SendEnv=GIT_PROTOCOL") {
        return Err("SSH flags are not supported".to_string());
    }
    let transport_args = &args[2..];
    let request = parse_request(transport_args)?;
    if request.user != "git" || parse_git_wire_command(&request.command).is_none() {
        return Err("Git protocol negotiation is only accepted for Git SSH services".to_string());
    }
    Ok(transport_args)
}

fn authorize(
    request: &Request,
    config: &Config,
    managed: Option<&ManagedFetch>,
) -> Result<Authorization, String> {
    let transport_args = git_transport_args(&request.args)?;
    let parsed = parse_request(transport_args)?;
    let service = parsed.command.split_ascii_whitespace().next();

    if service == Some("git-upload-archive") && managed.is_some() {
        return Err("managed fetch mode does not permit git-upload-archive".to_string());
    }

    if service == Some("git-upload-pack")
        && let Some(managed) = managed
    {
        if parsed.user != "git" {
            return Err("managed fetches require the git SSH user".to_string());
        }
        if !is_valid_managed_host(&parsed.host) {
            return Err("managed fetch destination is not a valid DNS hostname".to_string());
        }
        let command = parse_git_wire_command(&parsed.command)
            .filter(|command| command.service == "git-upload-pack")
            .ok_or_else(|| "managed fetch command is not valid".to_string())?;
        let cwd = request.cwd.as_deref().ok_or_else(|| {
            "managed fetch request did not include a working directory".to_string()
        })?;
        let requested_from = managed_request_context(&managed.workspace_root, cwd)
            .map_err(|error| format!("managed fetch request context is invalid: {error}"))?;
        let source = managed_fetch::Source {
            host: parsed.host.to_ascii_lowercase(),
            repository: command.repository,
        };

        match managed_fetch::read_approval(&managed.state_dir, &source)? {
            Some(approval) => {
                if !managed_fetch::consume_once(&managed.state_dir, &approval)? {
                    return Err(
                        "the one-time fetch approval was already consumed; approve this source again"
                            .to_string(),
                    );
                }
                return Ok(Authorization::ManagedFetch(managed_fetch_args(&source)));
            }
            None => {
                let id =
                    managed_fetch::record_candidate(&managed.state_dir, &source, &requested_from)?;
                return Err(format!(
                    "fetch pending approval for {} (candidate {id}); open the T3 admin portal, approve it, and retry",
                    source.display()
                ));
            }
        }
    }

    check_allowed(transport_args, config)?;
    Ok(Authorization::Static)
}

fn log_safe(value: &str) -> String {
    value.chars().flat_map(char::escape_default).collect()
}

fn read_handshake_line(stream: &mut impl Read) -> Option<String> {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        match stream.read(&mut byte) {
            Ok(0) => return None,
            Ok(_) => {
                if byte[0] == b'\n' {
                    return String::from_utf8(line).ok();
                }
                line.push(byte[0]);
                if line.len() >= MAX_HANDSHAKE {
                    return None;
                }
            }
            Err(_) => return None,
        }
    }
}

fn write_frame(writer: &Mutex<impl Write>, frame_type: u8, data: &[u8]) -> std::io::Result<()> {
    let mut w = writer.lock().unwrap();
    w.write_all(&[frame_type])?;
    w.write_all(&(data.len() as u32).to_be_bytes())?;
    w.write_all(data)?;
    w.flush()
}

fn handle_connection(
    mut stream: std::os::unix::net::UnixStream,
    config: &Config,
    managed_fetch: Option<&ManagedFetch>,
    log: &Arc<Mutex<File>>,
) {
    if stream.set_read_timeout(Some(HANDSHAKE_TIMEOUT)).is_err() {
        return;
    }
    let line = match read_handshake_line(&mut stream) {
        Some(l) => l,
        None => return,
    };

    let req: Request = match serde_json::from_str(&line) {
        Ok(r) => r,
        Err(e) => {
            log_line(log, &format!("INVALID ({})", e));
            let resp = HandshakeResponse {
                status: "denied".to_string(),
                reason: Some(format!("invalid request: {}", e)),
            };
            let _ = serde_json::to_writer(&mut stream, &resp);
            let _ = stream.write_all(b"\n");
            return;
        }
    };

    let cmd_line = log_safe(&req.args.join(" "));

    let authorization = match authorize(&req, config, managed_fetch) {
        Ok(authorization) => authorization,
        Err(reason) => {
            log_line(log, &format!("DENIED  {}", cmd_line));
            let resp = HandshakeResponse {
                status: "denied".to_string(),
                reason: Some(reason),
            };
            let _ = serde_json::to_writer(&mut stream, &resp);
            let _ = stream.write_all(b"\n");
            return;
        }
    };

    log_line(log, &format!("ALLOWED {}", cmd_line));

    let resp = HandshakeResponse {
        status: "ok".to_string(),
        reason: None,
    };
    let _ = serde_json::to_writer(&mut stream, &resp);
    let _ = stream.write_all(b"\n");
    let _ = stream.set_read_timeout(None);

    let (ssh_args, session_limit) = match authorization {
        Authorization::Static => (req.args, None),
        Authorization::ManagedFetch(args) => (args, Some(MAX_SESSION_DURATION)),
    };
    let mut child = match Command::new("/usr/bin/ssh")
        .args(&ssh_args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            log_line(log, &format!("ERROR   {} ({})", cmd_line, e));
            let _ = write_frame(
                &Mutex::new(&stream),
                FRAME_STDERR,
                format!("ssh-proxy: failed to spawn ssh: {}\n", e).as_bytes(),
            );
            let _ = write_frame(&Mutex::new(&stream), FRAME_EXIT, &1i32.to_be_bytes());
            return;
        }
    };

    let ssh_stdin = child.stdin.take().unwrap();
    let ssh_stdout = child.stdout.take().unwrap();
    let ssh_stderr = child.stderr.take().unwrap();

    let write_stream = stream.try_clone().expect("failed to clone socket");
    let writer = Arc::new(Mutex::new(write_stream));

    let read_stream = stream.try_clone().expect("failed to clone socket");
    let thread_a = thread::spawn(move || {
        let mut reader = read_stream;
        let mut stdin = ssh_stdin;
        let mut buf = [0u8; MAX_FRAME];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if stdin.write_all(&buf[..n]).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        drop(stdin);
    });

    let writer_b = Arc::clone(&writer);
    let thread_b = thread::spawn(move || {
        let mut stdout = ssh_stdout;
        let mut buf = [0u8; MAX_FRAME];
        loop {
            match stdout.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if write_frame(&writer_b, FRAME_STDOUT, &buf[..n]).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    let writer_c = Arc::clone(&writer);
    let thread_c = thread::spawn(move || {
        let mut stderr = ssh_stderr;
        let mut buf = [0u8; MAX_FRAME];
        loop {
            match stderr.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if write_frame(&writer_c, FRAME_STDERR, &buf[..n]).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    let exit_code = if let Some(limit) = session_limit {
        let deadline = Instant::now() + limit;
        loop {
            match child.try_wait() {
                Ok(Some(status)) => break status.code().unwrap_or(255),
                Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(100)),
                Ok(None) => {
                    log_line(log, &format!("TIMEOUT {}", cmd_line));
                    let _ = child.kill();
                    let _ = child.wait();
                    break 124;
                }
                Err(_) => break 255,
            }
        }
    } else {
        child
            .wait()
            .map(|status| status.code().unwrap_or(255))
            .unwrap_or(255)
    };

    let _ = thread_b.join();
    let _ = thread_c.join();

    let _ = write_frame(&writer, FRAME_EXIT, &(exit_code as i32).to_be_bytes());

    drop(writer);
    let _ = stream.shutdown(std::net::Shutdown::Read);
    let _ = thread_a.join();

    log_line(log, &format!("EXIT    {} -> {}", cmd_line, exit_code));
}

pub fn run(
    socket_path: &str,
    log_path: &Path,
    config: &Config,
    managed_fetch: Option<&ManagedFetch>,
) {
    let path = Path::new(socket_path);
    let log_file = proxy_log::open(log_path).unwrap_or_else(|e| {
        eprintln!(
            "ssh-proxy: failed to open log {}: {}",
            log_path.display(),
            e
        );
        std::process::exit(1);
    });
    let log = Arc::new(Mutex::new(log_file));

    let bound = proxy_socket::bind(path).unwrap_or_else(|e| {
        eprintln!("ssh-proxy: failed to bind {}: {}", socket_path, e);
        std::process::exit(1);
    });
    let listener = bound.listener;
    let socket_identity = bound.identity;

    log_line(&log, &format!("listening on {}", socket_path));
    log_line(
        &log,
        &format!(
            "rules: git={:?} command={:?} host={:?}",
            config.git, config.command, config.host
        ),
    );
    if let Some(managed) = managed_fetch {
        log_line(
            &log,
            &format!(
                "managed fetch: workspace={} state={}",
                managed.workspace_root.display(),
                managed.state_dir.display()
            ),
        );
    }

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

    let config = Arc::new(config.clone());
    let managed_fetch = managed_fetch.cloned().map(Arc::new);
    let active_connections = Arc::new(AtomicUsize::new(0));

    for stream in listener.incoming() {
        match stream {
            Ok(mut stream) => {
                if active_connections.fetch_add(1, Ordering::Relaxed) >= MAX_CONNECTIONS {
                    active_connections.fetch_sub(1, Ordering::Relaxed);
                    log_line(&log, "DENIED  connection limit reached");
                    let response = HandshakeResponse {
                        status: "denied".to_string(),
                        reason: Some("too many concurrent SSH proxy connections".to_string()),
                    };
                    let _ = serde_json::to_writer(&mut stream, &response);
                    let _ = stream.write_all(b"\n");
                    continue;
                }
                let log = Arc::clone(&log);
                let config = Arc::clone(&config);
                let managed_fetch = managed_fetch.as_ref().map(Arc::clone);
                let guard = ConnectionGuard(Arc::clone(&active_connections));
                thread::spawn(move || {
                    let _guard = guard;
                    handle_connection(stream, &config, managed_fetch.as_deref(), &log);
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

    // ── glob_match ────────────────────────────────────────────────

    #[test]
    fn test_glob_exact() {
        assert!(glob_match("github.com", "github.com"));
        assert!(!glob_match("github.com", "xgithub.com"));
    }

    #[test]
    fn test_glob_star() {
        assert!(glob_match("*.com", "github.com"));
        assert!(!glob_match("*.com", "github.org"));
        assert!(glob_match("*", "anything"));
        assert!(glob_match("*", ""));
    }

    #[test]
    fn test_glob_multi_star() {
        assert!(glob_match("*.*", "github.com"));
        assert!(glob_match("a*b*", "axbx"));
        assert!(!glob_match("a*b*", "xaxbx"));
    }

    // ── parse_request ─────────────────────────────────────────────

    #[test]
    fn test_parse_basic() {
        let req = parse_request(&strs(&["git@github.com", "git-receive-pack '/repo'"])).unwrap();
        assert_eq!(req.dest, "git@github.com");
        assert_eq!(req.user, "git");
        assert_eq!(req.host, "github.com");
        assert_eq!(req.command, "git-receive-pack '/repo'");
    }

    #[test]
    fn test_parse_multi_arg_command() {
        let req =
            parse_request(&strs(&["user@host", "sudo", "systemctl", "restart", "app"])).unwrap();
        assert_eq!(req.command, "sudo systemctl restart app");
    }

    #[test]
    fn test_parse_denies_flags() {
        assert!(parse_request(&strs(&["-v", "git@host", "cmd"])).is_err());
        assert!(parse_request(&strs(&["-p", "22", "git@host", "cmd"])).is_err());
        assert!(parse_request(&strs(&["-L", "8080:localhost:80", "git@host", "cmd"])).is_err());
    }

    #[test]
    fn test_parse_denies_no_destination() {
        assert!(parse_request(&[]).is_err());
    }

    #[test]
    fn test_parse_denies_no_at() {
        assert!(parse_request(&strs(&["github.com", "cmd"])).is_err());
    }

    #[test]
    fn test_parse_denies_interactive() {
        assert!(parse_request(&strs(&["git@github.com"])).is_err());
    }

    // ── is_git_command ────────────────────────────────────────────

    #[test]
    fn test_is_git_command() {
        assert!(is_git_command("git-receive-pack '/repo.git'"));
        assert!(is_git_command("git-upload-pack '/repo.git'"));
        assert!(is_git_command("git-upload-archive '/repo.git'"));
        assert!(!is_git_command("git-lfs-authenticate '/repo' download"));
        assert!(!is_git_command("bash"));
        assert!(!is_git_command("git-receive-packFOO"));
    }

    // ── extract_repo ──────────────────────────────────────────────

    #[test]
    fn test_extract_repo() {
        // SCP-style (no leading slash): git@github.com:org/repo.git
        assert_eq!(
            extract_repo("git-receive-pack 'org/repo.git'"),
            Some("org/repo".to_string())
        );
        // SSH URL-style (leading slash): ssh://git@github.com/org/repo.git
        assert_eq!(
            extract_repo("git-receive-pack '/org/repo.git'"),
            Some("org/repo".to_string())
        );
        assert_eq!(
            extract_repo("git-upload-pack 'repo'"),
            Some("repo".to_string())
        );
        assert_eq!(
            extract_repo("git-upload-pack '/repo'"),
            Some("repo".to_string())
        );
        assert_eq!(extract_repo("git-receive-pack"), None);
    }

    // ── parse_git_entry ───────────────────────────────────────────

    #[test]
    fn test_parse_git_entry_host_only() {
        let (host, repo) = parse_git_entry("github.com");
        assert_eq!(host, "github.com");
        assert_eq!(repo, None);
    }

    #[test]
    fn test_parse_git_entry_with_repo() {
        let (host, repo) = parse_git_entry("github.com/org/*");
        assert_eq!(host, "github.com");
        assert_eq!(repo, Some("org/*"));
    }

    #[test]
    fn test_parse_git_entry_wildcard_host() {
        let (host, repo) = parse_git_entry("*.gitlab.com");
        assert_eq!(host, "*.gitlab.com");
        assert_eq!(repo, None);
    }

    // ── check_git_rules ───────────────────────────────────────────

    // SCP-style (most common): git@github.com:org/repo.git → no leading slash
    fn git_push(host: &str, repo: &str) -> Vec<String> {
        strs(&[
            &format!("git@{}", host),
            &format!("git-receive-pack '{}.git'", repo),
        ])
    }

    fn git_fetch(host: &str, repo: &str) -> Vec<String> {
        strs(&[
            &format!("git@{}", host),
            &format!("git-upload-pack '{}.git'", repo),
        ])
    }

    // SSH URL-style: ssh://git@github.com/org/repo.git → leading slash
    fn git_push_ssh_url(host: &str, repo: &str) -> Vec<String> {
        strs(&[
            &format!("git@{}", host),
            &format!("git-receive-pack '/{}.git'", repo),
        ])
    }

    #[test]
    fn test_git_allowed_basic() {
        let rules = strs(&["github.com"]);
        let req = parse_request(&git_push("github.com", "org/repo")).unwrap();
        assert!(check_git_rules(&req, &rules));
    }

    #[test]
    fn test_git_allowed_fetch() {
        let rules = strs(&["github.com"]);
        let req = parse_request(&git_fetch("github.com", "org/repo")).unwrap();
        assert!(check_git_rules(&req, &rules));
    }

    #[test]
    fn test_git_allowed_ssh_url_style() {
        // ssh://git@github.com/org/repo.git produces a leading slash
        let rules = strs(&["github.com"]);
        let req = parse_request(&git_push_ssh_url("github.com", "org/repo")).unwrap();
        assert!(check_git_rules(&req, &rules));
    }

    #[test]
    fn test_git_repo_match_both_styles() {
        // Same rule should match both SCP-style and SSH URL-style
        let rules = strs(&["github.com/org/*"]);
        let scp = parse_request(&git_push("github.com", "org/repo")).unwrap();
        let ssh_url = parse_request(&git_push_ssh_url("github.com", "org/repo")).unwrap();
        assert!(check_git_rules(&scp, &rules));
        assert!(check_git_rules(&ssh_url, &rules));
    }

    #[test]
    fn test_git_denied_wrong_host() {
        let rules = strs(&["github.com"]);
        let req = parse_request(&git_push("evil.com", "repo")).unwrap();
        assert!(!check_git_rules(&req, &rules));
    }

    #[test]
    fn test_git_denied_not_git_user() {
        let rules = strs(&["github.com"]);
        let args = strs(&["deploy@github.com", "git-receive-pack '/repo.git'"]);
        let req = parse_request(&args).unwrap();
        assert!(!check_git_rules(&req, &rules));
    }

    #[test]
    fn test_git_denied_not_git_command() {
        let rules = strs(&["github.com"]);
        let args = strs(&["git@github.com", "bash"]);
        let req = parse_request(&args).unwrap();
        assert!(!check_git_rules(&req, &rules));
    }

    #[test]
    fn test_git_wildcard_host() {
        let rules = strs(&["*.gitlab.com"]);
        let req = parse_request(&git_push("private.gitlab.com", "repo")).unwrap();
        assert!(check_git_rules(&req, &rules));
    }

    #[test]
    fn test_git_repo_restriction() {
        let rules = strs(&["github.com/myorg/*"]);
        let req = parse_request(&git_push("github.com", "myorg/repo")).unwrap();
        assert!(check_git_rules(&req, &rules));
    }

    #[test]
    fn test_git_repo_restriction_denied() {
        let rules = strs(&["github.com/myorg/*"]);
        let req = parse_request(&git_push("github.com", "other/repo")).unwrap();
        assert!(!check_git_rules(&req, &rules));
    }

    #[test]
    fn test_git_exact_repo() {
        let rules = strs(&["github.com/myorg/specific"]);
        let allowed = parse_request(&git_push("github.com", "myorg/specific")).unwrap();
        let denied = parse_request(&git_push("github.com", "myorg/other")).unwrap();
        assert!(check_git_rules(&allowed, &rules));
        assert!(!check_git_rules(&denied, &rules));
    }

    #[test]
    fn test_git_empty_rules() {
        let rules: Vec<String> = vec![];
        let req = parse_request(&git_push("github.com", "repo")).unwrap();
        assert!(!check_git_rules(&req, &rules));
    }

    // ── check_command_rules ───────────────────────────────────────

    #[test]
    fn test_command_exact_match() {
        let rules = strs(&["deploy@prod.example.com uptime"]);
        let req = parse_request(&strs(&["deploy@prod.example.com", "uptime"])).unwrap();
        assert!(check_command_rules(&req, &rules));
    }

    #[test]
    fn test_command_denied_different_command() {
        let rules = strs(&["deploy@prod.example.com uptime"]);
        let req = parse_request(&strs(&["deploy@prod.example.com", "bash"])).unwrap();
        assert!(!check_command_rules(&req, &rules));
    }

    #[test]
    fn test_command_denied_different_host() {
        let rules = strs(&["deploy@prod.example.com uptime"]);
        let req = parse_request(&strs(&["deploy@evil.com", "uptime"])).unwrap();
        assert!(!check_command_rules(&req, &rules));
    }

    #[test]
    fn test_command_multi_word() {
        let rules = strs(&["deploy@host sudo systemctl restart myapp"]);
        let req = parse_request(&strs(&[
            "deploy@host",
            "sudo",
            "systemctl",
            "restart",
            "myapp",
        ]))
        .unwrap();
        assert!(check_command_rules(&req, &rules));
    }

    // ── check_host_rules ──────────────────────────────────────────

    #[test]
    fn test_host_any_command() {
        let rules = strs(&["admin@staging.internal"]);
        let req = parse_request(&strs(&["admin@staging.internal", "uptime"])).unwrap();
        assert!(check_host_rules(&req, &rules));
        let req2 =
            parse_request(&strs(&["admin@staging.internal", "anything", "at", "all"])).unwrap();
        assert!(check_host_rules(&req2, &rules));
    }

    #[test]
    fn test_host_denied_wrong_dest() {
        let rules = strs(&["admin@staging.internal"]);
        let req = parse_request(&strs(&["admin@evil.com", "uptime"])).unwrap();
        assert!(!check_host_rules(&req, &rules));
    }

    #[test]
    fn test_host_denied_wrong_user() {
        let rules = strs(&["admin@staging.internal"]);
        let req = parse_request(&strs(&["root@staging.internal", "uptime"])).unwrap();
        assert!(!check_host_rules(&req, &rules));
    }

    // ── check_allowed (integration) ───────────────────────────────

    #[test]
    fn test_default_config_denies_everything() {
        let config = default_config();
        assert!(check_allowed(&git_push("github.com", "repo"), &config).is_err());
    }

    #[test]
    fn test_git_rule_allows_push() {
        let config = Config {
            git: strs(&["github.com"]),
            command: vec![],
            host: vec![],
        };
        assert!(check_allowed(&git_push("github.com", "repo"), &config).is_ok());
    }

    #[test]
    fn test_git_rule_denies_non_git() {
        let config = Config {
            git: strs(&["github.com"]),
            command: vec![],
            host: vec![],
        };
        let args = strs(&["git@github.com", "bash"]);
        assert!(check_allowed(&args, &config).is_err());
    }

    #[test]
    fn test_command_rule_allows_exact() {
        let config = Config {
            git: vec![],
            command: strs(&["deploy@host uptime"]),
            host: vec![],
        };
        let args = strs(&["deploy@host", "uptime"]);
        assert!(check_allowed(&args, &config).is_ok());
    }

    #[test]
    fn test_host_rule_allows_any_command() {
        let config = Config {
            git: vec![],
            command: vec![],
            host: strs(&["admin@box"]),
        };
        let args = strs(&["admin@box", "anything"]);
        assert!(check_allowed(&args, &config).is_ok());
    }

    #[test]
    fn test_flags_denied_even_with_host_rule() {
        let config = Config {
            git: vec![],
            command: vec![],
            host: strs(&["admin@box"]),
        };
        let args = strs(&["-v", "admin@box", "cmd"]);
        assert!(check_allowed(&args, &config).is_err());
    }

    #[test]
    fn test_interactive_denied_even_with_host_rule() {
        let config = Config {
            git: vec![],
            command: vec![],
            host: strs(&["admin@box"]),
        };
        let args = strs(&["admin@box"]);
        assert!(check_allowed(&args, &config).is_err());
    }

    // ── config serialization ──────────────────────────────────────

    #[test]
    fn test_config_round_trip() {
        let config = Config {
            git: strs(&["github.com", "gitlab.com/org/*"]),
            command: strs(&["deploy@host uptime"]),
            host: strs(&["admin@box"]),
        };
        let json = serde_json::to_string_pretty(&config).unwrap();
        let loaded: Config = serde_json::from_str(&json).unwrap();
        assert_eq!(config.git, loaded.git);
        assert_eq!(config.command, loaded.command);
        assert_eq!(config.host, loaded.host);
    }

    #[test]
    fn test_config_empty_fields_default() {
        let json = r#"{"git": ["github.com"]}"#;
        let config: Config = serde_json::from_str(json).unwrap();
        assert_eq!(config.git, vec!["github.com"]);
        assert!(config.command.is_empty());
        assert!(config.host.is_empty());
    }

    #[test]
    fn test_config_empty_object() {
        let json = "{}";
        let config: Config = serde_json::from_str(json).unwrap();
        assert!(config.git.is_empty());
        assert!(config.command.is_empty());
        assert!(config.host.is_empty());
    }

    // ── adversarial: flag injection ───────────────────────────────

    #[test]
    fn test_flag_before_dest() {
        assert!(parse_request(&strs(&["-v", "git@host", "cmd"])).is_err());
    }

    #[test]
    fn test_flag_after_dest() {
        assert!(parse_request(&strs(&["git@host", "-v", "cmd"])).is_err());
    }

    #[test]
    fn test_flag_at_end() {
        assert!(parse_request(&strs(&["git@host", "cmd", "-v"])).is_err());
    }

    #[test]
    fn test_port_forward_flag() {
        assert!(parse_request(&strs(&["-L", "8080:localhost:80", "git@host", "cmd"])).is_err());
    }

    #[test]
    fn test_reverse_tunnel_flag() {
        assert!(parse_request(&strs(&["-R", "8080:localhost:80", "git@host", "cmd"])).is_err());
    }

    #[test]
    fn test_socks_proxy_flag() {
        assert!(parse_request(&strs(&["-D", "1080", "git@host", "cmd"])).is_err());
    }

    #[test]
    fn test_jump_host_flag() {
        assert!(parse_request(&strs(&["-J", "jump@proxy", "git@host", "cmd"])).is_err());
    }

    #[test]
    fn test_identity_file_flag() {
        assert!(parse_request(&strs(&["-i", "/root/.ssh/id_rsa", "git@host", "cmd"])).is_err());
    }

    #[test]
    fn test_config_file_flag() {
        assert!(parse_request(&strs(&["-F", "/etc/ssh/config", "git@host", "cmd"])).is_err());
    }

    #[test]
    fn test_proxy_command_flag() {
        assert!(parse_request(&strs(&["-o", "ProxyCommand=nc %h %p", "git@host", "cmd"])).is_err());
    }

    #[test]
    fn test_combined_flags() {
        assert!(parse_request(&strs(&["-vvv", "git@host", "cmd"])).is_err());
    }

    #[test]
    fn test_double_dash() {
        assert!(parse_request(&strs(&["--", "git@host", "cmd"])).is_err());
    }

    #[test]
    fn test_single_dash() {
        assert!(parse_request(&strs(&["-", "git@host", "cmd"])).is_err());
    }

    #[test]
    fn test_long_option() {
        assert!(parse_request(&strs(&["--option", "git@host", "cmd"])).is_err());
    }

    #[test]
    fn test_flag_disguised_as_quoted_arg() {
        // Agent sends "-danger" as a separate arg — still caught
        assert!(parse_request(&strs(&["git@host", "-danger", "cmd"])).is_err());
    }

    #[test]
    fn test_flag_inside_command_string_ok() {
        // "ls -la" as a single arg is fine — the dash is inside the command
        let req = parse_request(&strs(&["user@host", "ls -la /tmp"])).unwrap();
        assert_eq!(req.command, "ls -la /tmp");
    }

    #[test]
    fn test_flag_inside_git_repo_path_ok() {
        // Repo path containing a dash is fine
        let req = parse_request(&strs(&[
            "git@github.com",
            "git-receive-pack 'org/my-repo.git'",
        ]))
        .unwrap();
        assert_eq!(req.command, "git-receive-pack 'org/my-repo.git'");
    }

    // ── adversarial: destination manipulation ─────────────────────

    #[test]
    fn test_no_at_sign() {
        assert!(parse_request(&strs(&["github.com", "cmd"])).is_err());
    }

    #[test]
    fn test_empty_user() {
        assert!(parse_request(&strs(&["@github.com", "cmd"])).is_err());
    }

    #[test]
    fn test_empty_host() {
        assert!(parse_request(&strs(&["git@", "cmd"])).is_err());
    }

    #[test]
    fn test_multiple_at_signs() {
        // "git@evil@github.com" — user is "git", host is "evil@github.com"
        // This is syntactically accepted but won't match any sensible rule
        let req = parse_request(&strs(&["git@evil@github.com", "cmd"])).unwrap();
        assert_eq!(req.user, "git");
        assert_eq!(req.host, "evil@github.com");
    }

    #[test]
    fn test_space_in_dest_as_separate_args() {
        // Trying to sneak host into command: ["git@github.com git-receive-pack", "evil"]
        // No @ in first arg's host portion would be weird, but let's check
        // Actually "git@github.com git-receive-pack" has @ so it parses as:
        // user="git", host="github.com git-receive-pack"
        let req = parse_request(&strs(&["git@github.com git-receive-pack", "evil"])).unwrap();
        assert_eq!(req.host, "github.com git-receive-pack");
        // This won't match any git rule because the host contains a space
        let rules = strs(&["github.com"]);
        assert!(!check_git_rules(&req, &rules));
    }

    // ── adversarial: git rule bypass attempts ─────────────────────

    #[test]
    fn test_git_non_git_user_with_git_command() {
        // Trying to use git commands with a non-git user
        let config = Config {
            git: strs(&["github.com"]),
            command: vec![],
            host: vec![],
        };
        let args = strs(&["deploy@github.com", "git-receive-pack '/repo.git'"]);
        assert!(check_allowed(&args, &config).is_err());
    }

    #[test]
    fn test_git_arbitrary_command_on_allowed_host() {
        // git user on allowed host but non-git command
        let config = Config {
            git: strs(&["github.com"]),
            command: vec![],
            host: vec![],
        };
        let args = strs(&["git@github.com", "bash"]);
        assert!(check_allowed(&args, &config).is_err());
    }

    #[test]
    fn test_git_shell_injection_in_repo() {
        let config = Config {
            git: strs(&["github.com"]),
            command: vec![],
            host: vec![],
        };
        let args = strs(&["git@github.com", "git-receive-pack 'repo; rm -rf /'"]);
        assert!(check_allowed(&args, &config).is_err());
    }

    #[test]
    fn test_git_denies_suffix_after_repo() {
        let config = Config {
            git: strs(&["github.com"]),
            command: vec![],
            host: vec![],
        };
        let args = strs(&["git@github.com", "git-upload-pack 'repo.git'; arbitrary"]);
        assert!(check_allowed(&args, &config).is_err());
    }

    #[test]
    fn test_git_repo_traversal() {
        // Trying to access a repo outside the allowed org
        let config = Config {
            git: strs(&["github.com/myorg/*"]),
            command: vec![],
            host: vec![],
        };
        let args = strs(&["git@github.com", "git-receive-pack '../other-org/repo.git'"]);
        let req = parse_request(&args).unwrap();
        assert!(!check_git_rules(&req, &config.git));
    }

    // ── adversarial: command rule bypass attempts ──────────────────

    #[test]
    fn test_command_extra_args() {
        // Trying to extend a command with extra args
        let config = Config {
            git: vec![],
            command: strs(&["deploy@host uptime"]),
            host: vec![],
        };
        let args = strs(&["deploy@host", "uptime; rm -rf /"]);
        assert!(check_allowed(&args, &config).is_err());
    }

    #[test]
    fn test_command_partial_match() {
        // "uptime" should not match "uptimex"
        let config = Config {
            git: vec![],
            command: strs(&["deploy@host uptime"]),
            host: vec![],
        };
        let args = strs(&["deploy@host", "uptimex"]);
        assert!(check_allowed(&args, &config).is_err());
    }

    #[test]
    fn test_command_prefix_match() {
        // "uptime" command should not match "uptime && bash"
        let config = Config {
            git: vec![],
            command: strs(&["deploy@host uptime"]),
            host: vec![],
        };
        let args = strs(&["deploy@host", "uptime && bash"]);
        assert!(check_allowed(&args, &config).is_err());
    }

    #[test]
    fn test_command_split_attack() {
        // Splitting the command across args to try boundary confusion
        let config = Config {
            git: vec![],
            command: strs(&["deploy@host sudo systemctl restart app"]),
            host: vec![],
        };
        // "sudo" "systemctl" "restart" "app" joins to same string
        let args = strs(&["deploy@host", "sudo", "systemctl", "restart", "app"]);
        assert!(check_allowed(&args, &config).is_ok());
        // But adding extra args fails
        let args2 = strs(&[
            "deploy@host",
            "sudo",
            "systemctl",
            "restart",
            "app",
            "&&",
            "bash",
        ]);
        assert!(check_allowed(&args2, &config).is_err());
    }

    // ── adversarial: host rule constraints ─────────────────────────

    #[test]
    fn test_host_wrong_user() {
        let config = Config {
            git: vec![],
            command: vec![],
            host: strs(&["deploy@host"]),
        };
        let args = strs(&["root@host", "cmd"]);
        assert!(check_allowed(&args, &config).is_err());
    }

    #[test]
    fn test_host_interactive_denied() {
        let config = Config {
            git: vec![],
            command: vec![],
            host: strs(&["admin@box"]),
        };
        let args = strs(&["admin@box"]);
        assert!(check_allowed(&args, &config).is_err());
    }

    #[test]
    fn test_host_flags_denied() {
        let config = Config {
            git: vec![],
            command: vec![],
            host: strs(&["admin@box"]),
        };
        assert!(
            check_allowed(
                &strs(&["-L", "8080:localhost:80", "admin@box", "cmd"]),
                &config
            )
            .is_err()
        );
        assert!(check_allowed(&strs(&["-D", "1080", "admin@box", "cmd"]), &config).is_err());
        assert!(
            check_allowed(
                &strs(&["-R", "80:internal:80", "admin@box", "cmd"]),
                &config
            )
            .is_err()
        );
    }

    // ── adversarial: empty and degenerate inputs ──────────────────

    #[test]
    fn test_empty_args() {
        assert!(parse_request(&[]).is_err());
    }

    #[test]
    fn test_empty_string_arg() {
        assert!(parse_request(&strs(&[""])).is_err());
    }

    #[test]
    fn test_only_spaces() {
        assert!(parse_request(&strs(&[" @ ", "cmd"])).is_ok());
        // Parses but won't match any rule
        let config = default_config();
        assert!(check_allowed(&strs(&[" @ ", "cmd"]), &config).is_err());
    }

    #[test]
    fn test_newline_in_arg() {
        let req = parse_request(&strs(&["git@host\nevil", "cmd"])).unwrap();
        assert_eq!(req.host, "host\nevil");
        // Won't match any rule
        let config = Config {
            git: strs(&["host"]),
            command: vec![],
            host: vec![],
        };
        assert!(check_allowed(&strs(&["git@host\nevil", "cmd"]), &config).is_err());
        assert_eq!(log_safe("git@host\nevil"), "git@host\\nevil");
    }

    #[test]
    fn test_null_byte_in_arg() {
        let req = parse_request(&strs(&["git@host\0evil", "cmd"])).unwrap();
        assert_eq!(req.host, "host\0evil");
    }

    fn managed_test_root(label: &str) -> PathBuf {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "claude-sandbox-ssh-managed-{label}-{}-{nonce}",
            std::process::id()
        ))
    }

    fn managed_request(host: &str, repository: &str) -> Request {
        Request {
            args: strs(&[
                &format!("git@{host}"),
                &format!("git-upload-pack '{repository}'"),
            ]),
            cwd: Some("/workspace/project".to_string()),
        }
    }

    fn managed_fixture(label: &str) -> (PathBuf, ManagedFetch) {
        let root = managed_test_root(label);
        let workspace = root.join("workspace");
        let state = root.join("state");
        fs::create_dir_all(workspace.join("project")).unwrap();
        let state = managed_fetch::prepare_state_dir(&state).unwrap();
        (
            root,
            ManagedFetch {
                workspace_root: workspace,
                state_dir: state,
            },
        )
    }

    #[test]
    fn managed_fetch_requires_exact_approval_and_reconstructs_ssh_args() {
        let (root, managed) = managed_fixture("approval");
        let config = default_config();
        let request = managed_request("GitHub.COM", "org/project.git");

        let denied = authorize(&request, &config, Some(&managed)).unwrap_err();
        assert!(denied.contains("pending approval"));
        let candidates = managed_fetch::list_candidates(&managed.state_dir).unwrap();
        assert_eq!(candidates.len(), 1);
        let source = candidates[0].1.source.clone();
        assert_eq!(source.host, "github.com");
        assert_eq!(source.repository, "org/project.git");
        assert_eq!(candidates[0].1.requested_from, "project");

        managed_fetch::approve(
            &managed.state_dir,
            &source,
            crate::managed_push::ApprovalScope::Persistent,
        )
        .unwrap();
        let allowed = authorize(&request, &config, Some(&managed)).unwrap();
        let Authorization::ManagedFetch(args) = allowed else {
            panic!("managed fetch used the static authorization path");
        };
        assert_eq!(args.last().unwrap(), "git-upload-pack 'org/project.git'");
        assert!(args.contains(&"git@github.com".to_string()));
        assert!(args.contains(&"StrictHostKeyChecking=yes".to_string()));
        assert!(args.contains(&"UpdateHostKeys=no".to_string()));
        assert!(!args.iter().any(|arg| arg.contains("receive-pack")));
        let mut protocol_request = request;
        protocol_request
            .args
            .splice(0..0, ["-o".to_string(), "SendEnv=GIT_PROTOCOL".to_string()]);
        assert!(matches!(
            authorize(&protocol_request, &config, Some(&managed)),
            Ok(Authorization::ManagedFetch(_))
        ));
        let push = Request {
            args: git_push("github.com", "org/project"),
            cwd: Some("/workspace/project".to_string()),
        };
        assert!(authorize(&push, &config, Some(&managed)).is_err());

        let other = managed_request("github.com", "org/other.git");
        assert!(authorize(&other, &config, Some(&managed)).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn managed_fetch_overrides_static_read_rules_without_changing_explicit_write_rules() {
        let (root, managed) = managed_fixture("read-only");
        let config = Config {
            git: strs(&["github.com"]),
            command: vec![],
            host: vec![],
        };
        let fetch = managed_request("github.com", "org/project.git");
        assert!(authorize(&fetch, &config, Some(&managed)).is_err());

        let archive = Request {
            args: strs(&["git@github.com", "git-upload-archive 'org/project.git'"]),
            cwd: Some("/workspace/project".to_string()),
        };
        assert!(authorize(&archive, &config, Some(&managed)).is_err());

        let push = Request {
            args: git_push("github.com", "org/project"),
            cwd: Some("/workspace/project".to_string()),
        };
        assert!(matches!(
            authorize(&push, &config, Some(&managed)),
            Ok(Authorization::Static)
        ));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn managed_fetch_rejects_invalid_host_repo_and_context() {
        let (root, managed) = managed_fixture("invalid");
        let config = default_config();
        for host in [
            "-github.com",
            "github..com",
            "github.com:22",
            "github.com\nevil",
        ] {
            assert!(
                authorize(
                    &managed_request(host, "org/repo.git"),
                    &config,
                    Some(&managed)
                )
                .is_err()
            );
        }
        for repository in [
            "../repo.git",
            "org/../repo.git",
            "org//repo.git",
            "//org/repo.git",
            "org/repo;evil",
        ] {
            assert!(
                authorize(
                    &managed_request("github.com", repository),
                    &config,
                    Some(&managed)
                )
                .is_err()
            );
        }
        let outside = Request {
            cwd: Some("/workspace/../outside".to_string()),
            ..managed_request("github.com", "org/repo.git")
        };
        assert!(authorize(&outside, &config, Some(&managed)).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn git_protocol_option_is_narrowly_scoped_to_structural_git_requests() {
        let config = Config {
            git: strs(&["github.com"]),
            command: vec![],
            host: strs(&["admin@example.com"]),
        };
        let valid = Request {
            args: strs(&[
                "-o",
                "SendEnv=GIT_PROTOCOL",
                "git@github.com",
                "git-upload-pack 'org/repo.git'",
            ]),
            cwd: None,
        };
        assert!(matches!(
            authorize(&valid, &config, None),
            Ok(Authorization::Static)
        ));

        for args in [
            strs(&[
                "-o",
                "ProxyCommand=evil",
                "git@github.com",
                "git-upload-pack 'org/repo.git'",
            ]),
            strs(&["-o", "SendEnv=GIT_PROTOCOL", "admin@example.com", "uptime"]),
            strs(&["-o", "SendEnv=GIT_PROTOCOL", "git@github.com", "bash"]),
        ] {
            assert!(authorize(&Request { args, cwd: None }, &config, None).is_err());
        }
    }
}
