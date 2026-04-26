use crate::logging::log_line;
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions, Permissions};
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use std::{fs, process, thread};

#[derive(Deserialize)]
struct Request {
    args: Vec<String>,
}

#[derive(Deserialize, Serialize, Clone)]
pub struct Config {
    pub allow: Vec<String>,
}

#[derive(Serialize)]
struct HandshakeResponse {
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
}

const FRAME_EXIT: u8 = 0;
const FRAME_STDOUT: u8 = 1;
const FRAME_STDERR: u8 = 2;
const MAX_FRAME: usize = 65536;

pub fn default_config() -> Config {
    Config {
        allow: vec![
            "git@github.com git-receive-pack *".to_string(),
            "git@github.com git-upload-pack *".to_string(),
        ],
    }
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

fn check_allowed(args: &[String], allow: &[String]) -> bool {
    let cmd_line = args.join(" ");
    allow.iter().any(|pattern| glob_match(pattern, &cmd_line))
}

fn read_handshake_line(stream: &mut impl Read) -> Option<String> {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    let limit = 1_048_576;
    loop {
        match stream.read(&mut byte) {
            Ok(0) => return None,
            Ok(_) => {
                if byte[0] == b'\n' {
                    return String::from_utf8(line).ok();
                }
                line.push(byte[0]);
                if line.len() >= limit {
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
    log: &Arc<Mutex<File>>,
) {
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

    let cmd_line = req.args.join(" ");

    if !check_allowed(&req.args, &config.allow) {
        log_line(log, &format!("DENIED  {}", cmd_line));
        let resp = HandshakeResponse {
            status: "denied".to_string(),
            reason: Some("no matching allow pattern".to_string()),
        };
        let _ = serde_json::to_writer(&mut stream, &resp);
        let _ = stream.write_all(b"\n");
        return;
    }

    log_line(log, &format!("ALLOWED {}", cmd_line));

    let resp = HandshakeResponse {
        status: "ok".to_string(),
        reason: None,
    };
    let _ = serde_json::to_writer(&mut stream, &resp);
    let _ = stream.write_all(b"\n");

    let mut child = match Command::new("/usr/bin/ssh")
        .args(&req.args)
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

    // Thread A: socket -> ssh stdin
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

    // Thread B: ssh stdout -> framed socket
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

    // Thread C: ssh stderr -> framed socket
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

    let status = child.wait().unwrap();
    let exit_code = status.code().unwrap_or(255);

    let _ = thread_b.join();
    let _ = thread_c.join();

    let _ = write_frame(&writer, FRAME_EXIT, &(exit_code as i32).to_be_bytes());

    drop(writer);
    let _ = thread_a.join();

    log_line(log, &format!("EXIT    {} -> {}", cmd_line, exit_code));
}

pub fn run(socket_path: &str, config: &Config) {
    let path = Path::new(socket_path);

    if path.exists() {
        let _ = fs::remove_file(path);
    }

    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
        let _ = fs::set_permissions(parent, Permissions::from_mode(0o700));
    }

    let listener = UnixListener::bind(path).unwrap_or_else(|e| {
        eprintln!("ssh-proxy: failed to bind {}: {}", socket_path, e);
        std::process::exit(1);
    });

    let log_path = path.with_file_name("ssh-proxy.log");
    let log_file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .unwrap_or_else(|e| {
            eprintln!(
                "ssh-proxy: failed to open log {}: {}",
                log_path.display(),
                e
            );
            std::process::exit(1);
        });
    let log = Arc::new(Mutex::new(log_file));

    log_line(&log, &format!("listening on {}", socket_path));
    log_line(&log, &format!("allow patterns: {:?}", config.allow));

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

    let config = Arc::new(config.clone());

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let log = Arc::clone(&log);
                let config = Arc::clone(&config);
                thread::spawn(move || {
                    handle_connection(stream, &config, &log);
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

    // ── glob_match ────────────────────────────────────────────────

    #[test]
    fn test_exact_match() {
        assert!(glob_match("github.com", "github.com"));
    }

    #[test]
    fn test_exact_no_match() {
        assert!(!glob_match("github.com", "xgithub.com"));
    }

    #[test]
    fn test_star_suffix() {
        assert!(glob_match("*.com", "github.com"));
        assert!(glob_match("*.com", "x.com"));
        assert!(!glob_match("*.com", "github.org"));
    }

    #[test]
    fn test_star_prefix() {
        assert!(glob_match(
            "git-receive-pack *",
            "git-receive-pack '/repo.git'"
        ));
    }

    #[test]
    fn test_star_middle() {
        assert!(glob_match(
            "git@*.com git-*",
            "git@github.com git-receive-pack '/repo'"
        ));
    }

    #[test]
    fn test_star_matches_anything() {
        assert!(glob_match("*", "anything at all"));
        assert!(glob_match("*", ""));
    }

    #[test]
    fn test_empty_pattern_empty_text() {
        assert!(glob_match("", ""));
    }

    #[test]
    fn test_empty_pattern_nonempty_text() {
        assert!(!glob_match("", "x"));
    }

    #[test]
    fn test_star_foo_star() {
        assert!(glob_match("*foo*", "xfooy"));
        assert!(glob_match("*foo*", "foo"));
        assert!(glob_match("*foo*", "foobar"));
        assert!(!glob_match("*foo*", "bar"));
    }

    #[test]
    fn test_multiple_stars() {
        assert!(glob_match(
            "*@* git-*",
            "git@github.com git-upload-pack '/repo'"
        ));
    }

    #[test]
    fn test_trailing_stars() {
        assert!(glob_match("a*b*", "axbx"));
        assert!(!glob_match("a*b*", "xaxbx"));
    }

    // ── check_allowed ─────────────────────────────────────────────

    fn strs(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| x.to_string()).collect()
    }

    fn default_allow() -> Vec<String> {
        default_config().allow
    }

    #[test]
    fn test_allowed_push() {
        assert!(check_allowed(
            &strs(&["git@github.com", "git-receive-pack '/repo'"]),
            &default_allow(),
        ));
    }

    #[test]
    fn test_allowed_fetch() {
        assert!(check_allowed(
            &strs(&["git@github.com", "git-upload-pack '/repo'"]),
            &default_allow(),
        ));
    }

    #[test]
    fn test_denied_bash() {
        assert!(!check_allowed(
            &strs(&["git@github.com", "bash"]),
            &default_allow(),
        ));
    }

    #[test]
    fn test_denied_flag_prefix() {
        assert!(!check_allowed(
            &strs(&["-v", "git@github.com", "git-receive-pack '/repo'"]),
            &default_allow()
        ));
    }

    #[test]
    fn test_denied_wrong_host() {
        assert!(!check_allowed(
            &strs(&["git@evil.com", "git-receive-pack '/repo'"]),
            &default_allow()
        ));
    }

    #[test]
    fn test_denied_interactive_shell() {
        assert!(!check_allowed(&strs(&["git@github.com"]), &default_allow(),));
    }

    #[test]
    fn test_denied_port_forward() {
        assert!(!check_allowed(
            &strs(&[
                "-L",
                "8080:localhost:80",
                "git@github.com",
                "git-receive-pack '/repo'"
            ]),
            &default_allow()
        ));
    }

    #[test]
    fn test_custom_pattern_with_flag() {
        let allow = strs(&["-v git@deploy.* git-*"]);
        assert!(check_allowed(
            &strs(&["-v", "git@deploy.internal", "git-receive-pack '/repo'"]),
            &allow,
        ));
        assert!(!check_allowed(
            &strs(&["git@deploy.internal", "git-receive-pack '/repo'"]),
            &allow,
        ));
    }

    #[test]
    fn test_wildcard_host() {
        let allow = strs(&["git@*.gitlab.com git-upload-pack *"]);
        assert!(check_allowed(
            &strs(&["git@private.gitlab.com", "git-upload-pack '/repo'"]),
            &allow,
        ));
        assert!(!check_allowed(
            &strs(&["git@github.com", "git-upload-pack '/repo'"]),
            &allow
        ));
    }

    // ── config serialization ──────────────────────────────────────

    #[test]
    fn test_config_round_trip() {
        let config = default_config();
        let json = serde_json::to_string_pretty(&config).unwrap();
        let loaded: Config = serde_json::from_str(&json).unwrap();
        assert_eq!(config.allow, loaded.allow);
    }
}
