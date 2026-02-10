use base64::Engine;
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions, Permissions};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};
use std::{env, fs, process, thread};

#[derive(Deserialize)]
struct Request {
    command: String,
}

#[derive(Serialize)]
struct Response {
    exit_code: i32,
    stdout_b64: String,
    stderr: String,
}

const MAX_AGE_SECS: u64 = 120;

use crate::logging::log_line;

fn screenshots_dir() -> PathBuf {
    if let Ok(d) = env::var("CLIPBOARD_SCREENSHOTS_DIR") {
        return PathBuf::from(d);
    }
    let home = env::var("HOME").unwrap_or_else(|_| "/root".to_string());
    PathBuf::from(home).join("Pictures/Screenshots")
}

fn find_newest_screenshot(dir: &Path) -> Result<Vec<u8>, String> {
    let entries = fs::read_dir(dir).map_err(|e| format!("cannot read {}: {}", dir.display(), e))?;

    let now = SystemTime::now();
    let max_age = Duration::from_secs(MAX_AGE_SECS);

    let mut newest: Option<(SystemTime, PathBuf)> = None;

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }

        let meta = match fs::metadata(&path) {
            Ok(m) => m,
            Err(_) => continue,
        };

        let mtime = match meta.modified() {
            Ok(t) => t,
            Err(_) => continue,
        };

        let age = now.duration_since(mtime).unwrap_or(Duration::MAX);
        if age > max_age {
            continue;
        }

        if newest.as_ref().map_or(true, |(best, _)| mtime > *best) {
            newest = Some((mtime, path));
        }
    }

    let (_, path) = newest.ok_or_else(|| {
        format!(
            "no screenshot younger than {}s in {}",
            MAX_AGE_SECS,
            dir.display()
        )
    })?;

    fs::read(&path).map_err(|e| format!("failed to read {}: {}", path.display(), e))
}

fn handle_request(req: Request, log: &Arc<Mutex<File>>) -> Response {
    if req.command != "read_image" {
        log_line(log, &format!("DENIED  unknown command: {}", req.command));
        return Response {
            exit_code: 1,
            stdout_b64: String::new(),
            stderr: format!("clipboard-proxy: unknown command: {}", req.command),
        };
    }

    log_line(log, "REQUEST read_image");

    let dir = screenshots_dir();
    match find_newest_screenshot(&dir) {
        Ok(bytes) => {
            let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);
            log_line(
                log,
                &format!("OK      read_image ({} bytes, {} b64)", bytes.len(), encoded.len()),
            );
            Response {
                exit_code: 0,
                stdout_b64: encoded,
                stderr: String::new(),
            }
        }
        Err(msg) => {
            log_line(log, &format!("ERROR   read_image: {}", msg));
            Response {
                exit_code: 1,
                stdout_b64: String::new(),
                stderr: format!("clipboard-proxy: {}", msg),
            }
        }
    }
}

pub fn run(socket_path: &str) {
    let path = Path::new(socket_path);

    if path.exists() {
        let _ = fs::remove_file(path);
    }

    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
        let _ = fs::set_permissions(parent, Permissions::from_mode(0o700));
    }

    let listener = UnixListener::bind(path).unwrap_or_else(|e| {
        eprintln!("clipboard-proxy: failed to bind {}: {}", socket_path, e);
        std::process::exit(1);
    });

    let log_path = path.with_file_name("clipboard-proxy.log");
    let log_file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .unwrap_or_else(|e| {
            eprintln!(
                "clipboard-proxy: failed to open log {}: {}",
                log_path.display(),
                e
            );
            std::process::exit(1);
        });
    let log = Arc::new(Mutex::new(log_file));

    log_line(&log, &format!("listening on {}", socket_path));

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
                                    stdout_b64: String::new(),
                                    stderr: format!("clipboard-proxy: invalid request: {}", e),
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
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn make_temp_dir() -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = env::temp_dir().join(format!(
            "clipboard-proxy-test-{}-{}",
            std::process::id(),
            n
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn test_empty_dir() {
        let dir = make_temp_dir();
        let result = find_newest_screenshot(&dir);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("no screenshot"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_only_old_files() {
        let dir = make_temp_dir();
        let path = dir.join("old.png");
        fs::write(&path, b"PNG old").unwrap();

        // Set mtime to 5 minutes ago
        let old_time = filetime::FileTime::from_system_time(
            SystemTime::now() - Duration::from_secs(300),
        );
        filetime::set_file_mtime(&path, old_time).unwrap();

        let result = find_newest_screenshot(&dir);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("no screenshot"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_picks_newest() {
        let dir = make_temp_dir();

        // Create an older-but-still-recent file (30s ago)
        let older = dir.join("older.png");
        fs::write(&older, b"PNG older").unwrap();
        let older_time = filetime::FileTime::from_system_time(
            SystemTime::now() - Duration::from_secs(30),
        );
        filetime::set_file_mtime(&older, older_time).unwrap();

        // Create the newest file (just now)
        let newest = dir.join("newest.png");
        fs::write(&newest, b"PNG newest").unwrap();

        let result = find_newest_screenshot(&dir).unwrap();
        assert_eq!(result, b"PNG newest");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_skips_directories() {
        let dir = make_temp_dir();
        fs::create_dir_all(dir.join("subdir")).unwrap();

        let file = dir.join("screenshot.png");
        fs::write(&file, b"PNG data").unwrap();

        let result = find_newest_screenshot(&dir).unwrap();
        assert_eq!(result, b"PNG data");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_handle_unknown_command() {
        let dir = make_temp_dir();
        let log_path = dir.join("test.log");
        let log_file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .unwrap();
        let log = Arc::new(Mutex::new(log_file));

        let req = Request {
            command: "unknown".to_string(),
        };
        let resp = handle_request(req, &log);
        assert_eq!(resp.exit_code, 1);
        assert!(resp.stderr.contains("unknown command"));
        let _ = fs::remove_dir_all(&dir);
    }
}
