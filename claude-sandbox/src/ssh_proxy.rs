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

use crate::managed_fetch::{self, Source};
use crate::{proxy_log, proxy_socket};

#[derive(Deserialize)]
struct Request {
    args: Vec<String>,
    #[serde(default)]
    cwd: Option<String>,
}

#[derive(Serialize)]
struct HandshakeResponse {
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
}

struct ParsedRequest {
    user: String,
    host: String,
    command: String,
}

/// Portal approvals for sources other than the launch repository's origin.
#[derive(Clone)]
pub struct PortalFetch {
    pub workspace_root: PathBuf,
    pub state_dir: PathBuf,
}

/// What the SSH bridge may fetch: the launch repository's origin, snapshotted
/// at launch, and (with the admin portal) sources approved there.
#[derive(Clone, Default)]
pub struct Policy {
    pub launch_origin: Option<Source>,
    pub portal: Option<PortalFetch>,
}

struct GitWireCommand {
    service: String,
    repository: String,
}

const GIT_SERVICES: &[&str] = &["git-receive-pack", "git-upload-pack", "git-upload-archive"];
const PLAIN_SSH_REFUSED: &str =
    "plain SSH commands are refused; the sandbox's SSH bridge only runs git fetches";

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

fn parse_request(args: &[String]) -> Result<ParsedRequest, String> {
    if args.iter().any(|arg| arg.starts_with('-')) {
        return Err("SSH flags are not supported".to_string());
    }
    let (destination, command) = args
        .split_first()
        .ok_or_else(|| PLAIN_SSH_REFUSED.to_string())?;
    let (user, host) = destination
        .split_once('@')
        .filter(|(user, host)| !user.is_empty() && !host.is_empty())
        .ok_or_else(|| PLAIN_SSH_REFUSED.to_string())?;
    let command = command.join(" ");
    if command.is_empty() {
        return Err(PLAIN_SSH_REFUSED.to_string());
    }
    Ok(ParsedRequest {
        user: user.to_string(),
        host: host.to_string(),
        command,
    })
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

    is_valid_git_repo(repo).then(|| GitWireCommand {
        service: service.to_string(),
        repository: repo.to_string(),
    })
}

fn is_valid_host(host: &str) -> bool {
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

fn git_source(user: &str, host: &str, repository: &str) -> Result<Source, String> {
    if user != "git" {
        return Err(format!(
            "only the 'git' SSH user is supported (got '{user}')"
        ));
    }
    if !is_valid_host(host) {
        return Err(format!("'{host}' is not a plain DNS hostname"));
    }
    if !is_valid_git_repo(repository) {
        return Err(format!("unsupported repository path '{repository}'"));
    }
    Ok(Source {
        host: host.to_ascii_lowercase(),
        repository: repository.to_string(),
    })
}

/// Map an `origin` URL to the source git requests when fetching it over SSH.
/// `Ok(None)` means the URL does not use SSH; `Err` means it does but cannot
/// be bridged. Only `git@host:path` and `ssh://git@host/path` are accepted:
/// the bridge always connects as `git` on the default port, so other users
/// and explicit ports are refused instead of silently rewritten.
pub fn origin_source(url: &str) -> Result<Option<Source>, String> {
    if let Some(rest) = url.strip_prefix("ssh://") {
        let (authority, path) = rest
            .find('/')
            .map(|index| rest.split_at(index))
            .ok_or_else(|| format!("{url} has no repository path"))?;
        let (user, host) = authority
            .rsplit_once('@')
            .ok_or_else(|| format!("{url} does not name the 'git' SSH user"))?;
        if host.contains(':') {
            return Err(format!(
                "{url} sets a port, which the SSH bridge does not support"
            ));
        }
        return git_source(user, host, path).map(Some);
    }
    if let Some((scheme, _)) = url.split_once("://") {
        if scheme.contains("ssh") {
            return Err(format!("{url}: only ssh:// URLs are supported"));
        }
        return Ok(None);
    }
    let Some((destination, path)) = url.split_once(':') else {
        return Ok(None);
    };
    if destination.contains('/') {
        return Ok(None);
    }
    let (user, host) = destination
        .rsplit_once('@')
        .ok_or_else(|| format!("{url} does not name the 'git' SSH user"))?;
    git_source(user, host, path).map(Some)
}

fn request_context(workspace_root: &Path, cwd: &str) -> Result<String, String> {
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

fn fetch_args(source: &Source) -> Vec<String> {
    let mut args = vec!["-T".to_string()];
    for option in [
        "BatchMode=yes",
        "ClearAllForwardings=yes",
        "ForwardAgent=no",
        "ForwardX11=no",
        "PermitLocalCommand=no",
        "RequestTTY=no",
        "StrictHostKeyChecking=yes",
        "UpdateHostKeys=no",
        "NumberOfPasswordPrompts=0",
    ] {
        args.push("-o".to_string());
        args.push(option.to_string());
    }
    args.push(format!("git@{}", source.host));
    args.push(format!("git-upload-pack '{}'", source.repository));
    args
}

// Git prefixes its SSH invocation with exactly this option for protocol v2.
fn strip_protocol_option(args: &[String]) -> &[String] {
    match args {
        [flag, value, rest @ ..] if flag == "-o" && value == "SendEnv=GIT_PROTOCOL" => rest,
        _ => args,
    }
}

fn authorize(request: &Request, policy: &Policy) -> Result<Vec<String>, String> {
    let parsed = parse_request(strip_protocol_option(&request.args))?;
    let command =
        parse_git_wire_command(&parsed.command).ok_or_else(|| PLAIN_SSH_REFUSED.to_string())?;
    match command.service.as_str() {
        "git-upload-pack" => {}
        "git-receive-pack" => {
            return Err(
                "pushes are not bridged over SSH; use plain 'git push', which runs through the push bridge"
                    .to_string(),
            );
        }
        _ => return Err(format!("{} is not bridged", command.service)),
    }
    let source = git_source(&parsed.user, &parsed.host, &command.repository)
        .map_err(|error| format!("fetch refused: {error}"))?;

    if policy.launch_origin.as_ref() == Some(&source) {
        return Ok(fetch_args(&source));
    }

    let Some(portal) = &policy.portal else {
        let allowed = policy
            .launch_origin
            .as_ref()
            .map(|origin| {
                format!(
                    "only the launch repository's origin ({}) is",
                    origin.display()
                )
            })
            .unwrap_or_else(|| "nothing is".to_string());
        return Err(format!(
            "fetch refused for {}: {allowed} fetchable through the host's SSH keys",
            source.display()
        ));
    };

    let cwd = request
        .cwd
        .as_deref()
        .ok_or_else(|| "fetch request did not include a working directory".to_string())?;
    let requested_from = request_context(&portal.workspace_root, cwd)
        .map_err(|error| format!("fetch request context is invalid: {error}"))?;
    match managed_fetch::read_approval(&portal.state_dir, &source)? {
        Some(approval) => {
            if !managed_fetch::consume_once(&portal.state_dir, &approval)? {
                return Err(
                    "the one-time fetch approval was already consumed; approve this source again"
                        .to_string(),
                );
            }
            Ok(fetch_args(&source))
        }
        None => {
            let id = managed_fetch::record_candidate(&portal.state_dir, &source, &requested_from)?;
            Err(format!(
                "fetch pending approval for {} (candidate {id}); open the admin portal, approve it, and retry",
                source.display()
            ))
        }
    }
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
    policy: &Policy,
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

    let ssh_args = match authorize(&req, policy) {
        Ok(args) => args,
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

    let deadline = Instant::now() + MAX_SESSION_DURATION;
    let exit_code = loop {
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
    };

    let _ = thread_b.join();
    let _ = thread_c.join();

    let _ = write_frame(&writer, FRAME_EXIT, &(exit_code as i32).to_be_bytes());

    drop(writer);
    let _ = stream.shutdown(std::net::Shutdown::Read);
    let _ = thread_a.join();

    log_line(log, &format!("EXIT    {} -> {}", cmd_line, exit_code));
}

pub fn run(socket_path: &str, log_path: &Path, policy: Policy) {
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
    if let Some(origin) = &policy.launch_origin {
        log_line(&log, &format!("launch origin: {}", origin.display()));
    }
    if let Some(portal) = &policy.portal {
        log_line(
            &log,
            &format!(
                "portal approvals: workspace={} state={}",
                portal.workspace_root.display(),
                portal.state_dir.display()
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

    let policy = Arc::new(policy);
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
                let policy = Arc::clone(&policy);
                let guard = ConnectionGuard(Arc::clone(&active_connections));
                thread::spawn(move || {
                    let _guard = guard;
                    handle_connection(stream, &policy, &log);
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

    fn source(host: &str, repository: &str) -> Source {
        Source {
            host: host.to_string(),
            repository: repository.to_string(),
        }
    }

    fn request(args: &[&str]) -> Request {
        Request {
            args: strs(args),
            cwd: Some("/workspace/project".to_string()),
        }
    }

    fn fetch(host: &str, repository: &str) -> Request {
        request(&[
            &format!("git@{host}"),
            &format!("git-upload-pack '{repository}'"),
        ])
    }

    fn launch_policy() -> Policy {
        Policy {
            launch_origin: Some(source("github.com", "org/project.git")),
            portal: None,
        }
    }

    fn test_root(label: &str) -> PathBuf {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "claude-sandbox-ssh-{label}-{}-{nonce}",
            std::process::id()
        ))
    }

    fn portal_policy(label: &str) -> (PathBuf, Policy) {
        let root = test_root(label);
        let workspace = root.join("workspace");
        fs::create_dir_all(workspace.join("project")).unwrap();
        let state_dir = managed_fetch::prepare_state_dir(&root.join("state")).unwrap();
        let policy = Policy {
            portal: Some(PortalFetch {
                workspace_root: workspace,
                state_dir,
            }),
            ..launch_policy()
        };
        (root, policy)
    }

    #[test]
    fn origin_urls_map_to_the_requested_wire_source() {
        assert_eq!(
            origin_source("git@GitHub.com:org/project.git").unwrap(),
            Some(source("github.com", "org/project.git"))
        );
        assert_eq!(
            origin_source("git@github.com:org/project").unwrap(),
            Some(source("github.com", "org/project"))
        );
        assert_eq!(
            origin_source("ssh://git@github.com/org/project.git").unwrap(),
            Some(source("github.com", "/org/project.git"))
        );
        for url in [
            "https://github.com/org/project.git",
            "https://github.com/sshd/project.git",
            "/srv/git/project.git",
            "../project",
            "./dir:with/colon",
        ] {
            assert_eq!(origin_source(url).unwrap(), None, "{url}");
        }
    }

    #[test]
    fn unsupported_ssh_origins_are_refused_strictly() {
        for url in [
            "ssh://git@github.com:22/org/project.git",
            "ssh://git@github.com:2222/org/project.git",
            "ssh://deploy@example.com/org/project.git",
            "ssh://github.com/org/project.git",
            "ssh://git@github.com",
            "git+ssh://git@github.com/org/project.git",
            "deploy@example.com:org/project.git",
            "github.com:org/project.git",
            "git@github.com:org/../secret.git",
            "git@github.com:~/project.git",
            "git@[::1]:org/project.git",
            "git@-oProxyCommand=evil:org/project.git",
        ] {
            assert!(origin_source(url).is_err(), "{url}");
        }
    }

    #[test]
    fn launch_origin_is_fetchable_with_reconstructed_arguments() {
        let policy = launch_policy();
        let args = authorize(&fetch("GitHub.com", "org/project.git"), &policy).unwrap();
        assert_eq!(args.last().unwrap(), "git-upload-pack 'org/project.git'");
        assert!(args.contains(&"git@github.com".to_string()));
        assert!(args.contains(&"StrictHostKeyChecking=yes".to_string()));

        let protocol = request(&[
            "-o",
            "SendEnv=GIT_PROTOCOL",
            "git@github.com",
            "git-upload-pack 'org/project.git'",
        ]);
        assert!(authorize(&protocol, &policy).is_ok());
    }

    #[test]
    fn other_sources_are_denied_without_the_portal() {
        let policy = launch_policy();
        for other in [
            fetch("github.com", "org/other.git"),
            fetch("github.com", "/org/project.git"),
            fetch("github.com", "org/project"),
            fetch("gitlab.com", "org/project.git"),
        ] {
            let error = authorize(&other, &policy).unwrap_err();
            assert!(
                error.contains("only the launch repository's origin"),
                "{error}"
            );
        }
        let error =
            authorize(&fetch("github.com", "org/project.git"), &Policy::default()).unwrap_err();
        assert!(error.contains("nothing is fetchable"), "{error}");
    }

    #[test]
    fn pushes_archives_and_plain_ssh_are_always_refused() {
        let (root, policy) = portal_policy("refused");
        for args in [
            vec!["git@github.com", "git-receive-pack 'org/project.git'"],
            vec!["git@github.com", "git-upload-archive 'org/project.git'"],
            vec!["git@github.com", "bash"],
            vec!["user@example.com", "uptime"],
            vec!["git@github.com"],
            vec!["github.com", "git-upload-pack 'org/project.git'"],
            vec!["deploy@github.com", "git-upload-pack 'org/project.git'"],
            vec![
                "-p",
                "22",
                "git@github.com",
                "git-upload-pack 'org/project.git'",
            ],
            vec![
                "-o",
                "ProxyCommand=evil",
                "git@github.com",
                "git-upload-pack 'org/project.git'",
            ],
            vec![
                "-o",
                "SendEnv=GIT_PROTOCOL",
                "-o",
                "SendEnv=GIT_PROTOCOL",
                "git@github.com",
                "git-upload-pack 'org/project.git'",
            ],
            vec!["git@github.com", "git-upload-pack 'org/project.git'; evil"],
            vec!["git@github.com", "git-upload-pack 'org/project.git'", "-v"],
        ] {
            for policy in [&launch_policy(), &policy] {
                assert!(authorize(&request(&args), policy).is_err(), "{args:?}");
            }
        }
        assert!(
            managed_fetch::list_candidates(&policy.portal.unwrap().state_dir)
                .unwrap()
                .is_empty()
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn portal_records_pending_sources_and_honours_approvals() {
        let (root, policy) = portal_policy("portal");
        let portal = policy.portal.clone().unwrap();
        let request = fetch("GitHub.COM", "org/dependency.git");

        let denied = authorize(&request, &policy).unwrap_err();
        assert!(denied.contains("pending approval"), "{denied}");
        assert!(denied.contains("admin portal"), "{denied}");
        let candidates = managed_fetch::list_candidates(&portal.state_dir).unwrap();
        assert_eq!(candidates.len(), 1);
        let pending = candidates[0].1.source.clone();
        assert_eq!(pending, source("github.com", "org/dependency.git"));
        assert_eq!(candidates[0].1.requested_from, "project");

        managed_fetch::approve(
            &portal.state_dir,
            &pending,
            crate::managed_push::ApprovalScope::Once,
        )
        .unwrap();
        let args = authorize(&request, &policy).unwrap();
        assert_eq!(args.last().unwrap(), "git-upload-pack 'org/dependency.git'");
        assert!(authorize(&request, &policy).is_err());

        assert!(authorize(&fetch("github.com", "org/project.git"), &policy).is_ok());
        assert!(
            managed_fetch::list_candidates(&portal.state_dir)
                .unwrap()
                .iter()
                .all(|(_, candidate)| candidate.source.repository != "org/project.git")
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn portal_rejects_invalid_hosts_paths_and_context() {
        let (root, policy) = portal_policy("invalid");
        for host in [
            "-github.com",
            "github..com",
            "github.com:22",
            "github.com\nevil",
        ] {
            assert!(authorize(&fetch(host, "org/repo.git"), &policy).is_err());
        }
        for repository in [
            "../repo.git",
            "org/../repo.git",
            "org//repo.git",
            "//org/repo.git",
            "org/repo;evil",
        ] {
            assert!(authorize(&fetch("github.com", repository), &policy).is_err());
        }
        let outside = Request {
            cwd: Some("/workspace/../outside".to_string()),
            ..fetch("github.com", "org/repo.git")
        };
        assert!(authorize(&outside, &policy).is_err());
        let no_cwd = Request {
            cwd: None,
            ..fetch("github.com", "org/repo.git")
        };
        assert!(authorize(&no_cwd, &policy).is_err());
        assert!(
            managed_fetch::list_candidates(&policy.portal.unwrap().state_dir)
                .unwrap()
                .is_empty()
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn log_lines_escape_control_characters() {
        assert_eq!(log_safe("git@host\nevil"), "git@host\\nevil");
    }
}
