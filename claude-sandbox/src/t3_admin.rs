use base64::Engine;
use serde::Deserialize;
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use std::{process, thread};

use crate::managed_fetch;
use crate::managed_gh;
use crate::managed_push::{self, ApprovalScope};
use crate::novnc_assets;
use crate::portal_ui::{self, Nav};
use crate::usage_api;
use crate::usage_collector;
use crate::usage_dashboard;

const MAX_REQUEST_BYTES: usize = 16_384;
const MAX_BODY_BYTES: usize = 8_192;
const MAX_LOGIN_ATTEMPTS: u32 = 5;
const LOGIN_LOCKOUT: Duration = Duration::from_secs(60);
const MAX_NOTICES: usize = 32;
const NOTICE_TTL: Duration = Duration::from_secs(300);

struct Config {
    portal_port: u16,
    t3_port: u16,
    container_name: String,
    t3_base_dir: String,
    workspace_root: PathBuf,
    state_dir: PathBuf,
    fetch_state_dir: PathBuf,
    gh_state_dir: PathBuf,
    usage_state_dir: PathBuf,
    pin: String,
    csrf_token: String,
    session_token: String,
    restart_tx: mpsc::Sender<()>,
    restart_queued: AtomicBool,
    gh_grant_lock: Mutex<()>,
    failed_logins: Mutex<HashMap<String, LoginFailures>>,
    notices: Mutex<HashMap<String, (Instant, Notice)>>,
    display_command: fn(&str) -> Command,
}

#[derive(Clone, Copy)]
struct LoginFailures {
    count: u32,
    blocked_until: Option<Instant>,
}

struct Request {
    method: String,
    path: String,
    query: String,
    headers: HashMap<String, String>,
    body: Vec<u8>,
    peer: SocketAddr,
}

#[derive(Deserialize)]
struct PairingResponse {
    #[serde(rename = "pairUrl")]
    pair_url: String,
}

pub struct RunOptions<'a> {
    pub portal_port: u16,
    pub t3_port: u16,
    pub container_name: &'a str,
    pub t3_base_dir: &'a str,
    pub workspace_root: &'a Path,
    pub state_dir: &'a Path,
    pub fetch_state_dir: &'a Path,
    pub gh_state_dir: &'a Path,
    pub usage_state_dir: &'a Path,
}

pub fn run(options: RunOptions<'_>) {
    let pin = std::env::var("CLAUDE_SANDBOX_PORTAL_PIN").unwrap_or_default();
    if !valid_pin(&pin) {
        eprintln!("t3-admin: CLAUDE_SANDBOX_PORTAL_PIN must contain 4 to 12 digits");
        process::exit(2);
    }
    let (restart_tx, restart_rx) = mpsc::channel();
    let config = Arc::new(Config {
        portal_port: options.portal_port,
        t3_port: options.t3_port,
        container_name: options.container_name.to_string(),
        t3_base_dir: options.t3_base_dir.to_string(),
        workspace_root: options.workspace_root.to_path_buf(),
        state_dir: options.state_dir.to_path_buf(),
        fetch_state_dir: options.fetch_state_dir.to_path_buf(),
        gh_state_dir: options.gh_state_dir.to_path_buf(),
        usage_state_dir: options.usage_state_dir.to_path_buf(),
        pin,
        csrf_token: random_token(24),
        session_token: random_token(32),
        restart_tx,
        restart_queued: AtomicBool::new(false),
        gh_grant_lock: Mutex::new(()),
        failed_logins: Mutex::new(HashMap::new()),
        notices: Mutex::new(HashMap::new()),
        display_command,
    });

    usage_collector::start(
        options.container_name.to_string(),
        options.usage_state_dir.to_path_buf(),
    );

    let parent_pid = std::os::unix::process::parent_id();
    thread::spawn(move || {
        loop {
            thread::sleep(Duration::from_secs(2));
            if std::os::unix::process::parent_id() != parent_pid {
                process::exit(0);
            }
        }
    });

    let listener = bind_listener(options.portal_port);
    loop {
        if restart_rx.try_recv().is_ok() {
            match stop_sandbox(&config.container_name) {
                Ok(()) => process::exit(0),
                Err(error) => {
                    eprintln!("t3-admin: sandbox restart failed: {error}");
                    while restart_rx.try_recv().is_ok() {}
                    config.restart_queued.store(false, Ordering::Release);
                }
            }
        }

        match listener.accept() {
            Ok((stream, _)) => {
                let config = Arc::clone(&config);
                thread::spawn(move || handle_connection(stream, &config));
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(50));
            }
            Err(error) => {
                eprintln!("t3-admin: connection error: {error}");
                thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

fn bind_listener(port: u16) -> TcpListener {
    let listener = TcpListener::bind(("0.0.0.0", port)).unwrap_or_else(|error| {
        eprintln!("t3-admin: failed to bind port {port}: {error}");
        process::exit(1);
    });
    listener.set_nonblocking(true).unwrap_or_else(|error| {
        eprintln!("t3-admin: failed to configure port {port}: {error}");
        process::exit(1);
    });
    listener
}

fn sandbox_stop_command(container_name: &str) -> Command {
    let mut command = Command::new("podman");
    command
        .args(["stop", "--time=10", container_name])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    command
}

// x11vnc needs a real socket on stdin (libvncserver peeks at it), so socat
// bridges the exec pipes to a socketpair. x11vnc can deadlock while exiting
// after the client goes away, so the session group is killed once socat ends.
const DISPLAY_SESSION_SCRIPT: &str = "export DISPLAY=:99; . /run/claude-display.env 2>/dev/null; socat STDIO EXEC:\"x11vnc -inetd -q -nopw -once -shared -xkb\"; pkill -KILL -g 0 -x x11vnc; exit 0";

fn display_command(container_name: &str) -> Command {
    let mut command = Command::new("podman");
    command
        .args([
            "exec",
            "-i",
            container_name,
            "setsid",
            "-w",
            "sh",
            "-c",
            DISPLAY_SESSION_SCRIPT,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    command
}

fn stop_sandbox(container_name: &str) -> Result<(), String> {
    let status = sandbox_stop_command(container_name)
        .status()
        .map_err(|error| format!("could not start podman stop: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("podman stop exited with {status}"))
    }
}

fn header_has_token(value: Option<&String>, expected: &str) -> bool {
    value.is_some_and(|value| {
        value
            .split(',')
            .any(|token| token.trim().eq_ignore_ascii_case(expected))
    })
}

fn handle_display_socket(
    stream: &mut TcpStream,
    request: &Request,
    config: &Config,
) -> Result<(), String> {
    let valid_upgrade = request
        .headers
        .get("upgrade")
        .is_some_and(|value| value.eq_ignore_ascii_case("websocket"))
        && header_has_token(request.headers.get("connection"), "upgrade")
        && request
            .headers
            .get("sec-websocket-version")
            .is_some_and(|value| value == "13")
        && request.headers.contains_key("sec-websocket-key");
    if !valid_upgrade {
        send_html(
            stream,
            400,
            &render_error(
                "Invalid WebSocket request",
                "Required WebSocket headers are missing.",
            ),
            &[],
        );
        return Ok(());
    }

    let valid_origin = request.headers.get("host").is_some_and(|host| {
        request.headers.get("origin").is_some_and(|origin| {
            origin.eq_ignore_ascii_case(&format!("http://{host}"))
                || origin.eq_ignore_ascii_case(&format!("https://{host}"))
        })
    });
    if !valid_origin {
        send_html(
            stream,
            403,
            &render_error(
                "Forbidden",
                "The WebSocket origin does not match this admin portal.",
            ),
            &[],
        );
        return Ok(());
    }

    stream
        .set_read_timeout(None)
        .map_err(|error| format!("could not clear socket timeout: {error}"))?;
    let accept = crate::websocket::accept_key(&request.headers["sec-websocket-key"]);
    write!(
        stream,
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n"
    )
    .and_then(|()| stream.flush())
    .map_err(|error| format!("could not complete WebSocket handshake: {error}"))?;

    let child = (config.display_command)(&config.container_name)
        .spawn()
        .map_err(|error| format!("could not start x11vnc: {error}"))?;
    relay_display_session(stream, child)
}

fn send_frame(
    writer: &Mutex<TcpStream>,
    opcode: crate::websocket::Opcode,
    payload: &[u8],
) -> std::io::Result<()> {
    let mut writer = writer
        .lock()
        .map_err(|_| std::io::Error::other("display socket lock was poisoned"))?;
    crate::websocket::write_frame(&mut *writer, opcode, payload)?;
    writer.flush()
}

fn send_close_once(
    writer: &Mutex<TcpStream>,
    close_sent: &AtomicBool,
    payload: &[u8],
) -> std::io::Result<()> {
    if close_sent.swap(true, Ordering::AcqRel) {
        Ok(())
    } else {
        send_frame(writer, crate::websocket::Opcode::Close, payload)
    }
}

fn relay_display_session(
    stream: &mut TcpStream,
    mut child: std::process::Child,
) -> Result<(), String> {
    let writer_stream = match stream.try_clone() {
        Ok(stream) => stream,
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("could not clone display socket: {error}"));
        }
    };
    let Some(mut child_stdin) = child.stdin.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return Err("x11vnc stdin was not piped".to_string());
    };
    let Some(mut child_stdout) = child.stdout.take() else {
        drop(child_stdin);
        let _ = child.kill();
        let _ = child.wait();
        return Err("x11vnc stdout was not piped".to_string());
    };
    let writer = Arc::new(Mutex::new(writer_stream));
    let close_sent = Arc::new(AtomicBool::new(false));
    let pump_writer = Arc::clone(&writer);
    let pump_close_sent = Arc::clone(&close_sent);
    let pump = thread::spawn(move || -> std::io::Result<()> {
        let mut buffer = [0_u8; 64 * 1024];
        let result = loop {
            match child_stdout.read(&mut buffer) {
                Ok(0) => break send_close_once(&pump_writer, &pump_close_sent, &[]),
                Ok(count) => {
                    if let Err(error) = send_frame(
                        &pump_writer,
                        crate::websocket::Opcode::Binary,
                        &buffer[..count],
                    ) {
                        break Err(error);
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => break Err(error),
            }
        };
        if let Ok(socket) = pump_writer.lock() {
            let _ = socket.shutdown(Shutdown::Both);
        }
        result
    });

    let mut session_error = None;
    loop {
        let frame = match crate::websocket::read_frame(stream) {
            Ok(frame) => frame,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::UnexpectedEof
                        | std::io::ErrorKind::ConnectionReset
                        | std::io::ErrorKind::ConnectionAborted
                        | std::io::ErrorKind::BrokenPipe
                ) =>
            {
                break;
            }
            Err(error) => {
                session_error = Some(format!("could not read WebSocket frame: {error}"));
                break;
            }
        };
        match frame.opcode {
            crate::websocket::Opcode::Binary | crate::websocket::Opcode::Continuation => {
                if let Err(error) = child_stdin
                    .write_all(&frame.payload)
                    .and_then(|()| child_stdin.flush())
                {
                    session_error = Some(format!("could not write to x11vnc: {error}"));
                    break;
                }
            }
            crate::websocket::Opcode::Ping => {
                if let Err(error) =
                    send_frame(&writer, crate::websocket::Opcode::Pong, &frame.payload)
                {
                    session_error = Some(format!("could not send WebSocket pong: {error}"));
                    break;
                }
            }
            crate::websocket::Opcode::Close => {
                if let Err(error) = send_close_once(&writer, &close_sent, &frame.payload) {
                    session_error = Some(format!("could not send WebSocket close: {error}"));
                }
                break;
            }
            crate::websocket::Opcode::Text | crate::websocket::Opcode::Pong => {}
        }
    }

    drop(child_stdin);
    if let Err(error) = finish_display_child(&mut child) {
        session_error.get_or_insert(error);
    }
    let _ = stream.shutdown(Shutdown::Both);
    match pump.join() {
        Ok(Ok(())) => {}
        Ok(Err(error))
            if matches!(
                error.kind(),
                std::io::ErrorKind::BrokenPipe
                    | std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::ConnectionAborted
            ) => {}
        Ok(Err(error)) => {
            session_error.get_or_insert_with(|| format!("display output pump failed: {error}"));
        }
        Err(_) => {
            session_error.get_or_insert_with(|| "display output pump panicked".to_string());
        }
    }
    session_error.map_or(Ok(()), Err)
}

fn finish_display_child(child: &mut std::process::Child) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut poll_error = None;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return Ok(()),
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(20)),
            Ok(None) => break,
            Err(error) => {
                poll_error = Some(error);
                break;
            }
        }
    }
    let kill_result = child.kill();
    match child.wait() {
        Ok(_) => poll_error.map_or(Ok(()), |error| {
            Err(format!("could not poll x11vnc before stopping it: {error}"))
        }),
        Err(wait_error) => Err(match kill_result {
            Ok(()) => format!("could not reap x11vnc: {wait_error}"),
            Err(kill_error) => {
                format!("could not stop x11vnc: {kill_error}; could not reap it: {wait_error}")
            }
        }),
    }
}

fn handle_connection(mut stream: TcpStream, config: &Config) {
    let request = match read_request(&mut stream) {
        Ok(request) => request,
        Err(error) => {
            send_html(
                &mut stream,
                400,
                &render_error("Invalid request", &error),
                &[],
            );
            return;
        }
    };

    if request.method == "GET" && request.path == "/api/usage" {
        let (usage, available) = usage_api::collect(&config.usage_state_dir);
        send_json(&mut stream, if available { 200 } else { 503 }, &usage);
        return;
    }

    if request.method == "GET" && request.path == "/usage" {
        send_usage_dashboard(&mut stream);
        return;
    }

    if request.method == "GET" && request.path == "/assets/portal.js" {
        send_portal_script(&mut stream);
        return;
    }

    if request.method == "POST" && request.path == "/login" {
        handle_login(&mut stream, &request, config);
        return;
    }

    if request.method == "GET" && request.path == "/display/socket" {
        if !is_authorized(&request, config) {
            redirect(&mut stream, "/");
        } else if let Err(error) = handle_display_socket(&mut stream, &request, config) {
            eprintln!("t3-admin: display session error: {error}");
        }
        return;
    }

    if request.method == "GET" && request.path == "/display" {
        if !is_authorized(&request, config) {
            redirect(&mut stream, "/");
        } else {
            send_display_page(&mut stream);
        }
        return;
    }

    if request.method == "GET" && request.path.starts_with("/display/novnc/") {
        if !is_authorized(&request, config) {
            redirect(&mut stream, "/");
        } else {
            send_novnc_asset(&mut stream, &request.path[15..]);
        }
        return;
    }

    if request.method == "GET"
        && let Some(page) = parse_page(&request.path)
    {
        let body = if !is_authorized(&request, config) {
            render_lock(&login_target(Some(&request.path)), None)
        } else {
            let notice = take_notice(config, &request.query);
            match page {
                Page::Access => render_access(config, notice.as_ref()),
                Page::System => render_system(config, None, notice.as_ref()),
                page => render_inbox(config, Some(&page), notice.as_ref()),
            }
        };
        send_html(&mut stream, 200, &body, &[]);
        return;
    }

    if !is_authorized(&request, config) {
        redirect(&mut stream, "/");
        return;
    }

    let form = match parse_form(&request.body) {
        Ok(form) => form,
        Err(error) => {
            send_html(
                &mut stream,
                400,
                &render_inbox(config, None, Some(&Notice::error(error))),
                &[],
            );
            return;
        }
    };
    if request.method != "POST"
        || !safe_equal(
            form.get("csrf").map(String::as_str).unwrap_or(""),
            &config.csrf_token,
        )
    {
        send_html(
            &mut stream,
            403,
            &render_inbox(
                config,
                None,
                Some(&Notice::error("The form expired. Reload and try again.")),
            ),
            &[],
        );
        return;
    }

    match request.path.as_str() {
        "/restart" => {
            let queued = !config.restart_queued.swap(true, Ordering::AcqRel);
            let notice = if queued {
                "Restart requested. The admin page will be unavailable briefly."
            } else {
                "A sandbox restart is already in progress."
            };
            send_html(
                &mut stream,
                200,
                &render_system(config, None, Some(&Notice::ok(notice))),
                &[],
            );
            let _ = stream.flush();
            if queued {
                let _ = config.restart_tx.send(());
            }
            return;
        }
        "/pair" => {
            match create_pairing_link(config, request.headers.get("host")) {
                Ok(url) => send_html(
                    &mut stream,
                    200,
                    &render_system(config, Some(&url), None),
                    &[],
                ),
                Err(error) => send_html(
                    &mut stream,
                    400,
                    &render_system(config, None, Some(&Notice::error(error))),
                    &[],
                ),
            }
            return;
        }
        "/lock" => {
            redirect_with_headers(
                &mut stream,
                "/",
                &[(
                    "Set-Cookie",
                    "t3_admin_session=; HttpOnly; SameSite=Lax; Path=/; Max-Age=0".to_string(),
                )],
            );
            return;
        }
        _ => {}
    }

    let field = |name: &str| form.get(name).map(String::as_str).unwrap_or("");
    let decided = match request.path.as_str() {
        "/approve" | "/dismiss" => RequestKey::parse(&format!("push-{}", field("id"))),
        "/approve-fetch" | "/dismiss-fetch" => RequestKey::parse(&format!("fetch-{}", field("id"))),
        "/approve-gh" | "/dismiss-gh" | "/grant-gh" => {
            RequestKey::parse(&format!("gh-{}", field("handle")))
        }
        _ => None,
    };
    let before: Vec<RequestKey> = if decided.is_some() {
        pending_requests(config).iter().map(Pending::key).collect()
    } else {
        Vec::new()
    };

    let result = match request.path.as_str() {
        "/approve" => approve_candidate(config, &form),
        "/dismiss" => dismiss_candidate(config, &form),
        "/revoke" => revoke_approval(config, &form),
        "/approve-fetch" => approve_fetch_candidate(config, &form),
        "/dismiss-fetch" => dismiss_fetch_candidate(config, &form),
        "/revoke-fetch" => revoke_fetch_approval(config, &form),
        "/approve-gh" => approve_gh_action(config, &form),
        "/dismiss-gh" => dismiss_gh_action(config, &form),
        "/revoke-gh" => revoke_gh_action(config, &form),
        "/grant-gh" => grant_gh_action(config, &form),
        "/revoke-gh-grant" => revoke_gh_grant(config, &form),
        _ => {
            redirect(&mut stream, "/");
            return;
        }
    };

    match (result, decided) {
        (Ok(message), Some(decided)) => {
            let token = store_notice(config, Notice::ok(message));
            redirect(
                &mut stream,
                &format!("{}?n={token}", next_after(config, &before, &decided)),
            );
        }
        (Ok(message), None) => {
            let token = store_notice(config, Notice::ok(message));
            redirect(&mut stream, &format!("/access?n={token}"));
        }
        (Err(error), Some(decided)) => send_html(
            &mut stream,
            400,
            &render_inbox(
                config,
                Some(&Page::Request(Some(decided))),
                Some(&Notice::error(error)),
            ),
            &[],
        ),
        (Err(error), None) if request.path.starts_with("/revoke") => send_html(
            &mut stream,
            400,
            &render_access(config, Some(&Notice::error(error))),
            &[],
        ),
        (Err(error), None) => send_html(
            &mut stream,
            400,
            &render_inbox(config, None, Some(&Notice::error(error))),
            &[],
        ),
    }
}

fn handle_login(stream: &mut TcpStream, request: &Request, config: &Config) {
    let address = request.peer.ip().to_string();
    let form = parse_form(&request.body).unwrap_or_default();
    let next = login_target(form.get("next").map(String::as_str));
    {
        let mut failures = config.failed_logins.lock().unwrap();
        if let Some(state) = failures.get(&address).copied() {
            if state
                .blocked_until
                .is_some_and(|until| until > Instant::now())
            {
                send_html(
                    stream,
                    429,
                    &render_lock(
                        &next,
                        Some("Too many attempts. Wait one minute and try again."),
                    ),
                    &[],
                );
                return;
            }
            if state.blocked_until.is_some() {
                failures.remove(&address);
            }
        }
    }

    if !safe_equal(
        form.get("pin").map(String::as_str).unwrap_or(""),
        &config.pin,
    ) {
        let mut failures = config.failed_logins.lock().unwrap();
        let previous = failures.get(&address).copied().unwrap_or(LoginFailures {
            count: 0,
            blocked_until: None,
        });
        let count = previous.count + 1;
        failures.insert(
            address,
            if count >= MAX_LOGIN_ATTEMPTS {
                LoginFailures {
                    count: 0,
                    blocked_until: Some(Instant::now() + LOGIN_LOCKOUT),
                }
            } else {
                LoginFailures {
                    count,
                    blocked_until: None,
                }
            },
        );
        send_html(
            stream,
            401,
            &render_lock(&next, Some("That PIN is not correct.")),
            &[],
        );
        return;
    }

    config.failed_logins.lock().unwrap().remove(&address);
    // Lax rather than Strict so a deep link opened from the agent's chat (a
    // cross-site top-level navigation) arrives signed in. Every state change
    // is a POST that also needs the CSRF token.
    redirect_with_headers(
        stream,
        &next,
        &[(
            "Set-Cookie",
            format!(
                "t3_admin_session={}; HttpOnly; SameSite=Lax; Path=/",
                config.session_token
            ),
        )],
    );
}

fn approve_candidate(config: &Config, form: &HashMap<String, String>) -> Result<String, String> {
    let id = form.get("id").ok_or("Candidate identifier is missing")?;
    let scope = match form.get("scope").map(String::as_str) {
        Some("once") => ApprovalScope::Once,
        Some("persistent") => ApprovalScope::Persistent,
        _ => return Err("Invalid approval scope".to_string()),
    };
    let candidate = managed_push::read_candidate(&config.state_dir, id)?;
    let (_, current) = managed_push::resolve_relative_repository(
        &config.workspace_root,
        &candidate.repository.relative_path,
    )?;
    if current.origin != candidate.repository.origin {
        managed_push::record_candidate(
            &config.state_dir,
            &current,
            candidate.previously_approved_origin,
        )?;
        managed_push::remove_candidate(&config.state_dir, id)?;
        return Err(
            "The repository origin changed. Review the new candidate before approving.".to_string(),
        );
    }
    managed_push::approve(&config.state_dir, &current, scope)?;
    managed_push::remove_candidate(&config.state_dir, id)?;
    Ok(match scope {
        ApprovalScope::Once => format!(
            "Approved the next push from {}. The agent can retry now.",
            current.relative_path
        ),
        ApprovalScope::Persistent => {
            format!(
                "Always allowing pushes from {}. The agent can retry now.",
                current.relative_path
            )
        }
    })
}

fn dismiss_candidate(config: &Config, form: &HashMap<String, String>) -> Result<String, String> {
    let id = form.get("id").ok_or("Candidate identifier is missing")?;
    managed_push::remove_candidate(&config.state_dir, id)?;
    Ok("Pending request dismissed.".to_string())
}

fn revoke_approval(config: &Config, form: &HashMap<String, String>) -> Result<String, String> {
    let id = form.get("id").ok_or("Approval identifier is missing")?;
    let approvals = managed_push::list_approvals(&config.state_dir)?;
    let (_, approval) = approvals
        .into_iter()
        .find(|(approval_id, _)| approval_id == id)
        .ok_or("Approval no longer exists")?;
    managed_push::revoke(&config.state_dir, &approval.repository.relative_path)?;
    Ok(format!(
        "Revoked pushes from {}.",
        approval.repository.relative_path
    ))
}

fn approve_fetch_candidate(
    config: &Config,
    form: &HashMap<String, String>,
) -> Result<String, String> {
    let id = form.get("id").ok_or("Candidate identifier is missing")?;
    let scope = match form.get("scope").map(String::as_str) {
        Some("once") => ApprovalScope::Once,
        Some("persistent") => ApprovalScope::Persistent,
        _ => return Err("Invalid approval scope".to_string()),
    };
    let candidate = managed_fetch::read_candidate(&config.fetch_state_dir, id)?;
    managed_fetch::approve(&config.fetch_state_dir, &candidate.source, scope)?;
    managed_fetch::remove_candidate(&config.fetch_state_dir, id)?;
    Ok(match scope {
        ApprovalScope::Once => format!(
            "Approved one fetch from {}. The agent can retry now.",
            candidate.source.display()
        ),
        ApprovalScope::Persistent => {
            format!(
                "Always allowing fetches from {}. The agent can retry now.",
                candidate.source.display()
            )
        }
    })
}

fn dismiss_fetch_candidate(
    config: &Config,
    form: &HashMap<String, String>,
) -> Result<String, String> {
    let id = form.get("id").ok_or("Candidate identifier is missing")?;
    managed_fetch::remove_candidate(&config.fetch_state_dir, id)?;
    Ok("Pending fetch request dismissed.".to_string())
}

fn revoke_fetch_approval(
    config: &Config,
    form: &HashMap<String, String>,
) -> Result<String, String> {
    let id = form.get("id").ok_or("Approval identifier is missing")?;
    let approvals = managed_fetch::list_approvals(&config.fetch_state_dir)?;
    let (_, approval) = approvals
        .into_iter()
        .find(|(approval_id, _)| approval_id == id)
        .ok_or("Approval no longer exists")?;
    managed_fetch::revoke(&config.fetch_state_dir, &approval.source)?;
    Ok(format!(
        "Revoked fetches from {}.",
        approval.source.display()
    ))
}

fn approve_gh_action(config: &Config, form: &HashMap<String, String>) -> Result<String, String> {
    let handle = form.get("handle").ok_or("Action handle is missing")?;
    let _guard = config.gh_grant_lock.lock().unwrap();
    managed_gh::remove_covered_candidates(&config.gh_state_dir)?;
    let candidate = managed_gh::list_candidates(&config.gh_state_dir)?
        .into_iter()
        .find(|(candidate_handle, _)| candidate_handle == handle)
        .map(|(_, candidate)| candidate)
        .ok_or("GitHub action is no longer pending")?;
    if gh_display(&candidate.action, &candidate.context).is_none() {
        return Err("GitHub action cannot be displayed and cannot be approved".to_string());
    }
    managed_gh::approve(&config.gh_state_dir, handle)?;
    Ok(format!(
        "Approved this GitHub action once. The agent can retry the same command within {} minutes.",
        managed_gh::APPROVAL_TTL / 60
    ))
}

fn dismiss_gh_action(config: &Config, form: &HashMap<String, String>) -> Result<String, String> {
    let handle = form.get("handle").ok_or("Action handle is missing")?;
    if managed_gh::dismiss(&config.gh_state_dir, handle)? {
        Ok("Pending GitHub action dismissed.".to_string())
    } else {
        Err("GitHub action is no longer pending".to_string())
    }
}

fn revoke_gh_action(config: &Config, form: &HashMap<String, String>) -> Result<String, String> {
    let handle = form.get("handle").ok_or("Action handle is missing")?;
    if managed_gh::revoke(&config.gh_state_dir, handle)? {
        Ok("GitHub action approval revoked.".to_string())
    } else {
        Err("GitHub action approval was already used or expired".to_string())
    }
}

fn grant_gh_action(config: &Config, form: &HashMap<String, String>) -> Result<String, String> {
    let handle = form.get("handle").ok_or("Action handle is missing")?;
    let scope = match form.get("scope").map(String::as_str) {
        Some("command") => managed_gh::RequestedGrantScope::Command,
        Some("repository") => managed_gh::RequestedGrantScope::Repository,
        _ => return Err("Invalid grant scope".to_string()),
    };
    let _guard = config.gh_grant_lock.lock().unwrap();
    managed_gh::remove_covered_candidates(&config.gh_state_dir)?;
    let candidate = managed_gh::list_candidates(&config.gh_state_dir)?
        .into_iter()
        .find(|(candidate_handle, _)| candidate_handle == handle)
        .map(|(_, candidate)| candidate)
        .ok_or("GitHub action is no longer pending")?;
    if gh_display(&candidate.action, &candidate.context).is_none() {
        return Err("GitHub action cannot be displayed and cannot be granted".to_string());
    }
    managed_gh::grant_candidate(&config.gh_state_dir, handle, scope)?;
    Ok("GitHub repository grant saved. The agent can retry now.".to_string())
}

fn revoke_gh_grant(config: &Config, form: &HashMap<String, String>) -> Result<String, String> {
    let handle = form.get("handle").ok_or("Grant handle is missing")?;
    let _guard = config.gh_grant_lock.lock().unwrap();
    if managed_gh::revoke_grant(&config.gh_state_dir, handle)? {
        Ok("GitHub repository grant revoked.".to_string())
    } else {
        Err("GitHub repository grant no longer exists".to_string())
    }
}

fn create_pairing_link(config: &Config, host: Option<&String>) -> Result<String, String> {
    let hostname = host
        .and_then(|value| parse_hostname(value))
        .unwrap_or_else(|| "localhost".to_string());
    let base_url = if hostname.contains(':') {
        format!("http://[{hostname}]:{}/", config.t3_port)
    } else {
        format!("http://{hostname}:{}/", config.t3_port)
    };
    let output = Command::new("podman")
        .args([
            "exec",
            &config.container_name,
            "t3",
            "auth",
            "pairing",
            "create",
        ])
        .args(["--base-dir", &config.t3_base_dir])
        .args([
            "--base-url",
            &base_url,
            "--ttl",
            "5m",
            "--label",
            "pair-admin",
            "--json",
        ])
        .output()
        .map_err(|error| format!("Could not reach the T3 container: {error}"))?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    let response: PairingResponse = serde_json::from_slice(&output.stdout)
        .map_err(|_| "T3 returned an invalid pairing response".to_string())?;
    Ok(response.pair_url)
}

fn parse_hostname(host: &str) -> Option<String> {
    let hostname = if let Some(rest) = host.strip_prefix('[') {
        rest.split_once(']')?.0
    } else {
        host.split(':').next()?
    };
    if hostname.is_empty()
        || !hostname
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b':' | b'_'))
    {
        return None;
    }
    Some(hostname.to_string())
}

struct Notice {
    error: bool,
    text: String,
}

impl Notice {
    fn ok(text: impl Into<String>) -> Self {
        Self {
            error: false,
            text: text.into(),
        }
    }

    fn error(text: impl Into<String>) -> Self {
        Self {
            error: true,
            text: text.into(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RequestKind {
    Push,
    Fetch,
    Gh,
}

impl RequestKind {
    fn slug(self) -> &'static str {
        match self {
            Self::Push => "push",
            Self::Fetch => "fetch",
            Self::Gh => "gh",
        }
    }
}

/// The deep link to one pending request, as the proxies hand it to the agent.
pub(crate) fn request_url(base: &str, kind: RequestKind, id: &str) -> String {
    format!("{}/r/{}-{id}", base.trim_end_matches('/'), kind.slug())
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RequestKey {
    kind: RequestKind,
    id: String,
}

impl RequestKey {
    fn new(kind: RequestKind, id: &str) -> Self {
        Self {
            kind,
            id: id.to_string(),
        }
    }

    fn parse(value: &str) -> Option<Self> {
        let (kind, id) = value.split_once('-')?;
        let kind = match kind {
            "push" => RequestKind::Push,
            "fetch" => RequestKind::Fetch,
            "gh" => RequestKind::Gh,
            _ => return None,
        };
        (id.len() == 16
            && id
                .bytes()
                .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f')))
        .then(|| Self::new(kind, id))
    }

    fn path(&self) -> String {
        format!("/r/{}-{}", self.kind.slug(), self.id)
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Page {
    Inbox,
    Request(Option<RequestKey>),
    Access,
    System,
}

fn parse_page(path: &str) -> Option<Page> {
    match path {
        "/" => Some(Page::Inbox),
        "/access" => Some(Page::Access),
        "/system" => Some(Page::System),
        _ => path
            .strip_prefix("/r/")
            .map(|key| Page::Request(RequestKey::parse(key))),
    }
}

/// Where a successful unlock continues: only known portal pages, rebuilt
/// from the parsed value so nothing the client sent is echoed into Location.
fn login_target(next: Option<&str>) -> String {
    match next.and_then(parse_page) {
        Some(Page::Access) => "/access".to_string(),
        Some(Page::System) => "/system".to_string(),
        Some(Page::Request(Some(key))) => key.path(),
        _ => "/".to_string(),
    }
}

enum Pending {
    Push(String, managed_push::Candidate),
    Fetch(String, managed_fetch::Candidate),
    Gh(String, managed_gh::Candidate),
}

impl Pending {
    fn key(&self) -> RequestKey {
        match self {
            Self::Push(id, _) => RequestKey::new(RequestKind::Push, id),
            Self::Fetch(id, _) => RequestKey::new(RequestKind::Fetch, id),
            Self::Gh(handle, _) => RequestKey::new(RequestKind::Gh, handle),
        }
    }

    fn requested_at(&self) -> u64 {
        match self {
            Self::Push(_, candidate) => candidate.requested_at,
            Self::Fetch(_, candidate) => candidate.requested_at,
            Self::Gh(_, candidate) => candidate.context.requested_at,
        }
    }

    fn needs_attention(&self) -> bool {
        matches!(self, Self::Push(_, candidate) if candidate.previously_approved_origin.is_some())
    }
}

/// Every pending request, the ones needing extra care first, then newest first.
fn pending_requests(config: &Config) -> Vec<Pending> {
    let mut pending: Vec<Pending> = managed_push::list_candidates(&config.state_dir)
        .unwrap_or_default()
        .into_iter()
        .take(100)
        .map(|(id, candidate)| Pending::Push(id, candidate))
        .collect();
    pending.extend(
        managed_fetch::list_candidates(&config.fetch_state_dir)
            .unwrap_or_default()
            .into_iter()
            .map(|(id, candidate)| Pending::Fetch(id, candidate)),
    );
    let gh = {
        let _guard = config.gh_grant_lock.lock().unwrap();
        managed_gh::remove_covered_candidates(&config.gh_state_dir)
            .and_then(|()| managed_gh::list_candidates(&config.gh_state_dir))
            .unwrap_or_default()
    };
    pending.extend(
        gh.into_iter()
            .map(|(handle, candidate)| Pending::Gh(handle, candidate)),
    );
    pending.sort_by_key(|request| {
        (
            !request.needs_attention(),
            std::cmp::Reverse(request.requested_at()),
        )
    });
    pending
}

/// After a decision, continue with the request that followed the decided one.
fn next_after(config: &Config, before: &[RequestKey], decided: &RequestKey) -> String {
    let now: Vec<RequestKey> = pending_requests(config).iter().map(Pending::key).collect();
    let start = before
        .iter()
        .position(|key| key == decided)
        .map_or(0, |index| index + 1);
    before
        .iter()
        .cycle()
        .skip(start)
        .take(before.len())
        .find(|key| now.contains(key))
        .or_else(|| now.first())
        .map_or_else(|| "/".to_string(), RequestKey::path)
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn short_age(timestamp: u64) -> String {
    let seconds = unix_now().saturating_sub(timestamp);
    match seconds {
        0..60 => "now".to_string(),
        60..3_600 => format!("{}m", seconds / 60),
        3_600..86_400 => format!("{}h", seconds / 3_600),
        _ => format!("{}d", seconds / 86_400),
    }
}

fn duration_text(seconds: u64) -> String {
    if seconds >= 3_600 {
        format!("{}h {}m", seconds / 3_600, seconds % 3_600 / 60)
    } else {
        format!("{}m {}s", seconds / 60, seconds % 60)
    }
}

fn ago_text(timestamp: u64) -> String {
    match short_age(timestamp).as_str() {
        "now" => "just now".to_string(),
        age => format!("{age} ago"),
    }
}

fn requested_text(timestamp: u64) -> String {
    format!("requested {}", ago_text(timestamp))
}

fn date_text(timestamp: u64) -> String {
    let full = usage_api::format_epoch(i64::try_from(timestamp).unwrap_or(i64::MAX));
    format!(
        r#"<span title="{}">since {}</span>"#,
        escape_html(&full),
        escape_html(full.get(..10).unwrap_or(&full))
    )
}

fn plural(count: usize, one: &str, many: &str) -> String {
    format!("{count} {}", if count == 1 { one } else { many })
}

fn hidden(name: &str, value: &str) -> String {
    format!(
        r#"<input type="hidden" name="{name}" value="{}">"#,
        escape_html(value)
    )
}

/// Keeps a decision's notice for the page the redirect lands on. Each notice
/// is keyed by its own token in that redirect, so tabs never see each
/// other's notices.
fn store_notice(config: &Config, notice: Notice) -> String {
    let token = random_token(12);
    let mut notices = config.notices.lock().unwrap();
    notices.retain(|_, (stored, _)| stored.elapsed() < NOTICE_TTL);
    while notices.len() >= MAX_NOTICES {
        let Some(oldest) = notices
            .iter()
            .min_by_key(|(_, (stored, _))| *stored)
            .map(|(token, _)| token.clone())
        else {
            break;
        };
        notices.remove(&oldest);
    }
    notices.insert(token.clone(), (Instant::now(), notice));
    token
}

fn take_notice(config: &Config, query: &str) -> Option<Notice> {
    let token = query
        .split('&')
        .find_map(|pair| pair.strip_prefix("n="))
        .filter(|token| {
            token.len() == 16
                && token
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
        })?;
    let (stored, notice) = config.notices.lock().unwrap().remove(token)?;
    (stored.elapsed() < NOTICE_TTL).then_some(notice)
}

fn flash_html(notice: Option<&Notice>) -> String {
    notice
        .map(|notice| {
            let (class, icon) = if notice.error {
                (" error", portal_ui::ICON_WARN)
            } else {
                ("", portal_ui::ICON_CHECK)
            };
            format!(
                r#"<div class="flash{class}" role="status">{icon}<span>{}</span></div>"#,
                escape_html(&notice.text)
            )
        })
        .unwrap_or_default()
}

fn app_page(config: &Config, nav: Nav, pending: usize, title: &str, main: &str) -> String {
    portal_ui::document(
        &escape_html(title),
        "app",
        &format!(
            "{}{main}",
            portal_ui::top_bar(
                nav,
                pending,
                config.portal_port,
                &escape_html(&config.csrf_token)
            )
        ),
    )
}

fn render_lock(next: &str, notice: Option<&str>) -> String {
    let (title, text) = if next.starts_with("/r/") {
        (
            "Unlock to review this request",
            "Enter your portal PIN. You'll go straight to the request.",
        )
    } else {
        (
            "Sandbox Admin is locked",
            "Enter your portal PIN to review pending requests and access.",
        )
    };
    let notice = flash_html(notice.map(Notice::error).as_ref());
    portal_ui::document(
        "Unlock",
        "",
        &format!(
            r#"<main class="lock"><div class="floating">{}</div><div class="card"><span class="mark">{}</span><h1>{title}</h1><p>{text}</p>{notice}<form method="post" action="/login">{}<input class="pin" name="pin" type="password" inputmode="numeric" autocomplete="current-password" pattern="[0-9]{{4,12}}" minlength="4" maxlength="12" required autofocus aria-label="Admin PIN" placeholder="PIN"><button class="btn primary">Unlock</button></form><p class="foot"><a class="link" href="/usage">Public usage dashboard ↗</a></p></div></main>"#,
            portal_ui::theme_toggle(),
            portal_ui::mark(),
            hidden("next", next),
        ),
    )
}

fn render_inbox(config: &Config, selected: Option<&Page>, notice: Option<&Notice>) -> String {
    let pending = pending_requests(config);
    let keys: Vec<RequestKey> = pending.iter().map(Pending::key).collect();
    let (mode, index) = match selected {
        Some(Page::Request(Some(key))) => ("at-item", keys.iter().position(|item| item == key)),
        Some(Page::Request(None)) => ("at-item", None),
        _ => ("at-root", (!pending.is_empty()).then_some(0)),
    };
    let detail = match index {
        Some(index) => render_detail(config, &pending[index], index, &keys),
        None if mode == "at-item" => render_state(
            portal_ui::ICON_INBOX,
            true,
            "This request is no longer pending",
            "It was approved, dismissed or expired. Nothing is left to decide here.",
            &format!(
                r#"<a class="btn primary" href="/">Go to inbox ({} waiting)</a>"#,
                pending.len()
            ),
        ),
        None => render_state(
            portal_ui::ICON_CHECK,
            false,
            "All clear",
            "Nothing is waiting for you. Pushes, fetches and GitHub actions that need approval show up here, and the agent's message links straight to them.",
            r#"<a class="btn" href="/access">Review standing access</a>"#,
        ),
    };
    let flash = flash_html(notice);
    let no_flash = if flash.is_empty() { " no-flash" } else { "" };
    let main = format!(
        r#"<main class="inbox {mode}{no_flash}">{flash}{}{detail}</main>"#,
        render_list(&pending, index)
    );
    app_page(config, Nav::Inbox, pending.len(), "Inbox", &main)
}

fn render_state(icon: &str, muted: bool, title: &str, text: &str, action: &str) -> String {
    let class = if muted { " gray" } else { "" };
    format!(
        r#"<section class="card detail"><div class="detail-body center"><div class="state"><div class="big{class}">{icon}</div><h2>{title}</h2><p>{text}</p>{action}</div></div></section>"#
    )
}

fn render_list(pending: &[Pending], selected: Option<usize>) -> String {
    let mut html = format!(
        r#"<section class="card list"><div class="list-head"><h2>Inbox</h2><span>{}</span></div>"#,
        if pending.is_empty() {
            "nothing waiting".to_string()
        } else {
            format!("{} waiting · newest first", pending.len())
        }
    );
    if pending.is_empty() {
        html.push_str(r#"<div class="empty"><b>Nothing is waiting</b>A denied push, fetch or GitHub action appears here automatically.</div>"#);
    }
    let mut group = None;
    for (index, request) in pending.iter().enumerate() {
        let attention = request.needs_attention();
        if group != Some(attention) {
            group = Some(attention);
            html.push_str(&format!(
                r#"<div class="group">{}</div>"#,
                if attention {
                    "Needs attention"
                } else {
                    "Waiting"
                }
            ));
        }
        let (kind, icon, title, place, chips) = list_summary(request);
        let on = if selected == Some(index) { " on" } else { "" };
        html.push_str(&format!(
            r#"<a class="item{on}" href="{}"><div class="ico {kind}">{icon}</div><div><div class="t">{title}</div><div class="s"><span>{place}</span>{chips}</div></div><div class="age">{}</div></a>"#,
            request.key().path(),
            short_age(request.requested_at())
        ));
    }
    html.push_str("</section>");
    html
}

fn list_summary(request: &Pending) -> (&'static str, &'static str, String, String, String) {
    match request {
        Pending::Push(_, candidate) => {
            let repository = &candidate.repository;
            let title = repository.branch.as_deref().map_or_else(
                || "Push from a detached HEAD".to_string(),
                |branch| format!("Push to <code>{}</code>", escape_html(branch)),
            );
            let chips = if candidate.previously_approved_origin.is_some() {
                r#"<span class="chip red">Origin changed</span>"#.to_string()
            } else {
                String::new()
            };
            (
                "push",
                portal_ui::ICON_PUSH,
                title,
                escape_html(&repository.relative_path),
                chips,
            )
        }
        Pending::Fetch(_, candidate) => (
            "fetch",
            portal_ui::ICON_FETCH,
            format!(
                "Read <code>{}</code>",
                escape_html(&candidate.source.repository)
            ),
            format!(
                "{} · from {}",
                escape_html(&candidate.source.host),
                escape_html(&candidate.requested_from)
            ),
            r#"<span class="chip gray">Read only</span>"#.to_string(),
        ),
        Pending::Gh(_, candidate) => {
            let action = &candidate.action;
            let context = &candidate.context;
            let title = match gh_display(action, context) {
                Some(display) => {
                    let quoted = display
                        .values
                        .iter()
                        .find(|(flag, _)| *flag == "title")
                        .and_then(|(_, value)| *value)
                        .map(str::to_string)
                        .or_else(|| context.item_title.clone())
                        .map(|title| format!(" “{}”", escape_html(&visible_context(&title))))
                        .unwrap_or_default();
                    let operation = if action.args.get(1).map(String::as_str) == Some("create")
                        && action.args.first().map(String::as_str) == Some("pr")
                    {
                        "Open pull request".to_string()
                    } else {
                        display.operation
                    };
                    format!("{}{quoted}", escape_html(&operation))
                }
                None => "GitHub action that cannot be displayed".to_string(),
            };
            let chips = if context.repository_visibility.eq_ignore_ascii_case("public") {
                r#"<span class="chip gray">Public</span>"#.to_string()
            } else {
                String::new()
            };
            (
                "gh",
                portal_ui::ICON_GH,
                title,
                escape_html(&action.repository.selector()),
                chips,
            )
        }
    }
}

fn position_links(index: usize, keys: &[RequestKey]) -> String {
    let link = |target: Option<&RequestKey>, icon: &str, label: &str| match target {
        Some(key) => format!(
            r#"<a class="pill-btn" href="{}" aria-label="{label}" title="{label}">{icon}</a>"#,
            key.path()
        ),
        None => format!(r#"<span class="pill-btn off" aria-hidden="true">{icon}</span>"#),
    };
    format!(
        r#"{}<span class="pos">{} of {}</span>{}"#,
        link(
            index.checked_sub(1).and_then(|previous| keys.get(previous)),
            portal_ui::ICON_PREV,
            "Previous request"
        ),
        index + 1,
        keys.len(),
        link(keys.get(index + 1), portal_ui::ICON_NEXT, "Next request")
    )
}

struct Detail<'a> {
    kind: RequestKind,
    requested_at: u64,
    requested_from: Option<&'a str>,
    key: &'a RequestKey,
    position: &'a str,
    title: &'a str,
    sub: &'a str,
    body: &'a str,
    actions: &'a str,
}

fn detail_frame(detail: &Detail<'_>) -> String {
    let chip = match detail.kind {
        RequestKind::Push => r#"<span class="chip push">Push</span>"#,
        RequestKind::Fetch => r#"<span class="chip fetch">Fetch</span>"#,
        RequestKind::Gh => r#"<span class="chip gh">GitHub</span>"#,
    };
    format!(
        r#"<section class="card detail"><div class="detail-body"><a class="back" href="/">← Inbox</a><div class="crumbs"><div class="links">{chip}<span>{}</span></div><div class="links"><button type="button" class="pill-btn" data-copy="{}" title="Copy a link to this request">{}<span>Copy link</span></button>{}</div></div><h1>{}</h1><div class="sub">{}</div>{}</div><div class="actions">{}</div></section>"#,
        detail.requested_from.map_or_else(
            || requested_text(detail.requested_at),
            |from| format!(
                "{} from <code>{}</code>",
                requested_text(detail.requested_at),
                escape_html(&visible_context(from))
            )
        ),
        detail.key.path(),
        portal_ui::ICON_LINK,
        detail.position,
        detail.title,
        detail.sub,
        detail.body,
        detail.actions
    )
}

fn callout(class: &str, icon: &str, html: &str) -> String {
    format!(r#"<div class="callout {class}">{icon}<div>{html}</div></div>"#)
}

fn fact(label: &str, value: &str, wide: bool) -> String {
    let class = if wide { r#" class="wide""# } else { "" };
    format!("<div{class}><span>{label}</span>{value}</div>")
}

fn render_detail(config: &Config, request: &Pending, index: usize, keys: &[RequestKey]) -> String {
    let position = position_links(index, keys);
    let csrf = &config.csrf_token;
    match request {
        Pending::Push(id, candidate) => render_push_detail(id, candidate, csrf, &position),
        Pending::Fetch(id, candidate) => render_fetch_detail(id, candidate, csrf, &position),
        Pending::Gh(handle, candidate) => render_gh_detail(
            handle,
            &candidate.action,
            &candidate.context,
            csrf,
            &position,
        ),
    }
}

fn decision_actions(approve: &str, dismiss: &str, id: &str, csrf: &str) -> String {
    format!(
        r#"<form method="post" action="{approve}">{}{}<button class="btn primary" name="scope" value="once">{}Approve once</button></form><form method="post" action="{dismiss}">{}{}<button class="btn">Dismiss</button></form><form method="post" action="{approve}">{}{}<button class="btn ghost" name="scope" value="persistent">Always allow</button></form>"#,
        hidden("csrf", csrf),
        hidden("id", id),
        portal_ui::ICON_CHECK,
        hidden("csrf", csrf),
        hidden("id", id),
        hidden("csrf", csrf),
        hidden("id", id),
    )
}

fn render_push_detail(
    id: &str,
    candidate: &managed_push::Candidate,
    csrf: &str,
    position: &str,
) -> String {
    let repository = &candidate.repository;
    let title = repository.branch.as_deref().map_or_else(
        || "Push from a detached HEAD".to_string(),
        |branch| format!("Push to <code>{}</code>", escape_html(branch)),
    );
    let mut body = String::new();
    if let Some(previous) = &candidate.previously_approved_origin {
        body.push_str(&callout(
            "",
            portal_ui::ICON_WARN,
            &format!(
                r#"<b>Origin changed since this repository was approved.</b> Check the new origin before approving.<div class="rows"><span>Previously approved</span><code>{}</code><span>Now</span><code>{}</code></div>"#,
                escape_html(previous),
                escape_html(&repository.origin)
            ),
        ));
    }
    body.push_str(&format!(
        r#"<div class="facts">{}{}</div>"#,
        fact(
            "Branch",
            &format!(
                "<code>{}</code>",
                escape_html(repository.branch.as_deref().unwrap_or("detached HEAD"))
            ),
            false
        ),
        fact(
            "Origin",
            &format!("<code>{}</code>", escape_html(&repository.origin)),
            true
        ),
    ));
    body.push_str(&callout(
        "info",
        portal_ui::ICON_INFO,
        "<b>Approve once</b> lets the next push from this repository through. <b>Always allow</b> keeps allowing pushes while the origin stays the same; a changed origin suspends it and asks again.",
    ));
    detail_frame(&Detail {
        kind: RequestKind::Push,
        requested_at: candidate.requested_at,
        requested_from: None,
        key: &RequestKey::new(RequestKind::Push, id),
        position,
        title: &title,
        sub: &format!("<code>{}</code>", escape_html(&repository.relative_path)),
        body: &body,
        actions: &decision_actions("/approve", "/dismiss", id, csrf),
    })
}

fn render_fetch_detail(
    id: &str,
    candidate: &managed_fetch::Candidate,
    csrf: &str,
    position: &str,
) -> String {
    let source = escape_html(&candidate.source.display());
    let body = format!(
        r#"{}<div class="facts">{}{}{}</div>"#,
        callout(
            "info",
            portal_ui::ICON_INFO,
            "Approval grants this sandbox read access to the exact SSH repository. The requesting checkout is shown for context and is not an isolation boundary inside a shared container.",
        ),
        fact(
            "Requested from",
            &format!(
                "<strong>{}</strong>",
                escape_html(&candidate.requested_from)
            ),
            false
        ),
        fact("Access", "<strong>Read only</strong>", false),
        fact("Source", &format!("<code>{source}</code>"), true),
    );
    detail_frame(&Detail {
        kind: RequestKind::Fetch,
        requested_at: candidate.requested_at,
        requested_from: None,
        key: &RequestKey::new(RequestKind::Fetch, id),
        position,
        title: "Read a private repository",
        sub: &format!("<code>{source}</code>"),
        body: &body,
        actions: &decision_actions("/approve-fetch", "/dismiss-fetch", id, csrf),
    })
}

fn gh_value_block(flag: &str, value: &str) -> String {
    let label = match flag {
        "title" => "Title",
        "body" => "Body",
        "comment" => "Comment",
        "reason" => "Reason",
        "head" => "Head",
        "base" => "Base",
        _ => "Value",
    };
    let lines: Vec<&str> = value.split('\n').collect();
    let mut rendered = String::new();
    for (index, line) in lines.iter().enumerate() {
        let trailing = line
            .chars()
            .rev()
            .take_while(|character| *character == ' ')
            .count();
        let visible_end = line.len() - trailing;
        let mut content = String::new();
        for character in line[..visible_end].chars() {
            if character == '\t' {
                content.push_str("<span class=whitespace>→</span>\t");
            } else {
                content.push_str(&escape_html(&character.to_string()));
            }
        }
        for _ in 0..trailing {
            content.push_str("<span class=whitespace>·</span>");
        }
        rendered.push_str(&format!(
            "<span><i>{}</i><em>{content}</em></span>",
            index + 1
        ));
    }
    format!(
        r#"<div class="text"><div class="lbl"><b>{label}</b><span>{} · {}</span></div><pre>{rendered}</pre></div>"#,
        plural(value.len(), "byte", "bytes"),
        plural(lines.len(), "line", "lines"),
    )
}

fn gh_bare_flag(flag: &str) -> String {
    let label = match flag {
        "draft" => "Draft",
        "undo" => "Undo",
        _ => "Flag",
    };
    format!(r#"<div class="flag"><b>{label}</b><span>enabled</span></div>"#)
}

fn gh_values(display: &GhDisplay<'_>) -> String {
    display
        .values
        .iter()
        .map(|(flag, value)| {
            value.map_or_else(|| gh_bare_flag(flag), |value| gh_value_block(flag, value))
        })
        .collect()
}

fn render_gh_grant_controls(
    handle: &str,
    action: &managed_gh::Action,
    context: &managed_gh::Context,
    csrf: &str,
) -> String {
    let Some(kind) = managed_gh::command_kind(action) else {
        return String::new();
    };
    let repository = escape_html(&action.repository.selector());
    let public_warning = if context.repository_visibility.eq_ignore_ascii_case("public") {
        " This repository is public."
    } else {
        ""
    };
    format!(
        r#"<details class="allow"><summary class="btn ghost">Always allow… {}</summary><div class="allow-pop"><p>Future matching commands from this sandbox to {repository} will run without review. The agent chooses the text that is published under your account.{public_warning}</p><form method="post" action="/grant-gh">{}{}<button class="btn" name="scope" value="command">Always allow {} in {repository}</button><button class="btn" name="scope" value="repository">Always allow all issue and pull request actions in {repository}</button></form></div></details>"#,
        portal_ui::ICON_CHEVRON,
        hidden("csrf", csrf),
        hidden("handle", handle),
        escape_html(kind.phrase),
    )
}

fn gh_facts(action: &managed_gh::Action, context: &managed_gh::Context) -> String {
    let public = context.repository_visibility.eq_ignore_ascii_case("public");
    let mut facts = String::from(r#"<div class="facts">"#);
    facts.push_str(&fact(
        "Visibility",
        &format!(
            r#"<strong class="{}">{}</strong>"#,
            if public { "public" } else { "" },
            escape_html(&context.repository_visibility)
        ),
        false,
    ));
    if let Some(commits) = context.commits {
        facts.push_str(&fact(
            "Commits",
            &format!("<strong>{commits}</strong>"),
            false,
        ));
    }
    if let Some(files) = context.changed_files {
        facts.push_str(&fact(
            "Changed files",
            &format!("<strong>{files}</strong>"),
            false,
        ));
    }
    if let Some(item) = &action.item {
        facts.push_str(&fact(
            "Item",
            &format!(
                "<strong>#{} · {} · {}</strong>",
                item.number,
                escape_html(context.item_state.as_deref().unwrap_or("unknown")),
                escape_html(&visible_context(
                    context.item_title.as_deref().unwrap_or("untitled")
                ))
            ),
            true,
        ));
    }
    if let Some(head_sha) = &action.head_sha {
        facts.push_str(&fact(
            "Head commit when requested",
            &format!("<code>{}</code>", escape_html(head_sha)),
            true,
        ));
    }
    facts.push_str("</div>");
    facts
}

fn gh_branch_notice(action: &managed_gh::Action, display: &GhDisplay<'_>) -> String {
    if action.args.first().map(String::as_str) != Some("pr")
        || action.args.get(1).map(String::as_str) != Some("create")
    {
        return String::new();
    }
    let head = display
        .values
        .iter()
        .find(|(flag, _)| *flag == "head")
        .and_then(|(_, value)| *value)
        .expect("displayable pr create has a head");
    callout(
        "warn",
        portal_ui::ICON_INFO,
        &format!(
            "The pull request follows branch {}; commits pushed to it later, including between approval and creation, become part of it.",
            escape_html(head)
        ),
    )
}

fn render_gh_detail(
    handle: &str,
    action: &managed_gh::Action,
    context: &managed_gh::Context,
    csrf: &str,
    position: &str,
) -> String {
    let key = RequestKey::new(RequestKind::Gh, handle);
    let repository = escape_html(&action.repository.selector());
    let dismiss = format!(
        r#"<form method="post" action="/dismiss-gh">{}{}<button class="btn">Dismiss</button></form>"#,
        hidden("csrf", csrf),
        hidden("handle", handle)
    );
    let Some(display) = gh_display(action, context) else {
        return detail_frame(&Detail {
            kind: RequestKind::Gh,
            requested_at: context.requested_at,
            requested_from: Some(&context.requested_from),
            key: &key,
            position,
            title: "Cannot be displayed",
            sub: &format!("<code>{repository}</code>"),
            body: &callout(
                "warn",
                portal_ui::ICON_WARN,
                "This GitHub action uses arguments the portal cannot show in full, so it cannot be approved here. Dismiss it and ask the agent to retry with supported options.",
            ),
            actions: &dismiss,
        });
    };
    let body = gh_review(action, context, &display);
    let actions = format!(
        r#"<form method="post" action="/approve-gh">{}{}<button class="btn primary">{}Approve once</button></form>{dismiss}{}"#,
        hidden("csrf", csrf),
        hidden("handle", handle),
        portal_ui::ICON_CHECK,
        render_gh_grant_controls(handle, action, context, csrf)
    );
    detail_frame(&Detail {
        kind: RequestKind::Gh,
        requested_at: context.requested_at,
        requested_from: Some(&context.requested_from),
        key: &key,
        position,
        title: &escape_html(&display.operation),
        sub: &format!("<code>{repository}</code>"),
        body: &body,
        actions: &actions,
    })
}

/// Everything needed to judge a GitHub action: warnings, identities and the
/// exact text, shared by pending requests and unused approvals.
fn gh_review(
    action: &managed_gh::Action,
    context: &managed_gh::Context,
    display: &GhDisplay<'_>,
) -> String {
    let mut body = String::new();
    if context.repository_visibility.eq_ignore_ascii_case("public") {
        body.push_str(&callout(
            "",
            portal_ui::ICON_WARN,
            "<b>Public repository.</b> This acts under your GitHub account, and anything published below is visible to everyone exactly as written.",
        ));
    }
    body.push_str(&gh_facts(action, context));
    body.push_str(&gh_branch_notice(action, display));
    if !display.values.is_empty() {
        body.push_str(r#"<div class="sec">What will be published <small>Tabs (→) and trailing spaces (·) are marked</small></div>"#);
        body.push_str(&gh_values(display));
    }
    body
}

fn revoke_button(action: &str, field: &str, value: &str, csrf: &str) -> String {
    format!(
        r#"<form method="post" action="{action}">{}{}<button class="btn sm danger">Revoke</button></form>"#,
        hidden("csrf", csrf),
        hidden(field, value)
    )
}

fn access_block(icon_class: &str, icon: &str, title: &str, summary: &str, rows: &str) -> String {
    format!(
        r#"<section class="card block"><div class="block-h"><div class="ico {icon_class}">{icon}</div><h3>{title}</h3><span>{summary}</span></div>{rows}</section>"#
    )
}

fn render_access(config: &Config, notice: Option<&Notice>) -> String {
    let csrf = &config.csrf_token;
    let mut push_rows = String::new();
    let push = managed_push::list_approvals(&config.state_dir).unwrap_or_default();
    for (id, approval) in &push {
        let current_origin = managed_push::resolve_relative_repository(
            &config.workspace_root,
            &approval.repository.relative_path,
        )
        .ok()
        .map(|(_, repository)| repository.origin);
        let scope = match approval.scope {
            ApprovalScope::Once => r#"<span class="chip gray">Next push only</span>"#,
            ApprovalScope::Persistent => r#"<span class="chip green">Always</span>"#,
        };
        let origin = escape_html(&approval.repository.origin);
        let status = match current_origin {
            Some(current) if current == approval.repository.origin => {
                format!("{scope} <code>{origin}</code>")
            }
            Some(current) => format!(
                r#"<span class="chip red">Suspended</span> <span>approved for <code>{origin}</code>, origin is now <code>{}</code></span>"#,
                escape_html(&current)
            ),
            None => format!(
                r#"<span class="chip red">Suspended</span> <span>repository unavailable; approved for <code>{origin}</code></span>"#
            ),
        };
        push_rows.push_str(&format!(
            r#"<div class="row"><div><code>{}</code></div><div class="muted">{status}</div><div class="when">{}</div>{}</div>"#,
            escape_html(&approval.repository.relative_path),
            date_text(approval.approved_at),
            revoke_button("/revoke", "id", id, csrf)
        ));
    }
    if push.is_empty() {
        push_rows.push_str(r#"<div class="none">No repositories are approved.</div>"#);
    }

    let mut fetch_rows = String::new();
    let fetch = managed_fetch::list_approvals(&config.fetch_state_dir).unwrap_or_default();
    for (id, approval) in &fetch {
        let scope = match approval.scope {
            ApprovalScope::Once => r#"<span class="chip gray">Next fetch only</span>"#,
            ApprovalScope::Persistent => r#"<span class="chip green">Always</span>"#,
        };
        fetch_rows.push_str(&format!(
            r#"<div class="row"><div><code>{}</code></div><div class="muted">{scope} read only</div><div class="when">{}</div>{}</div>"#,
            escape_html(&approval.source.display()),
            date_text(approval.approved_at),
            revoke_button("/revoke-fetch", "id", id, csrf)
        ));
    }
    if fetch.is_empty() {
        fetch_rows.push_str(
            r#"<div class="none">No private SSH repositories are approved for reading.</div>"#,
        );
    }

    let mut grant_rows = String::new();
    let grants = managed_gh::list_grants(&config.gh_state_dir).unwrap_or_default();
    for (handle, grant) in &grants {
        let scope = match &grant.scope {
            managed_gh::GrantScope::All => "All issue and pull request actions".to_string(),
            managed_gh::GrantScope::Commands(commands) => commands
                .iter()
                .map(|command| {
                    managed_gh::COMMAND_KINDS
                        .iter()
                        .find(|kind| kind.command == command)
                        .map_or(command.as_str(), |kind| kind.phrase)
                })
                .collect::<Vec<_>>()
                .join(", "),
        };
        let visibility = if grant.visibility.eq_ignore_ascii_case("public") {
            "red"
        } else {
            "gray"
        };
        grant_rows.push_str(&format!(
            r#"<div class="row"><div><code>{}</code> <span class="chip {visibility}">{}</span></div><div class="muted">{}</div><div class="when">{}</div>{}</div>"#,
            escape_html(&grant.repository.selector()),
            escape_html(&grant.visibility),
            escape_html(&scope),
            date_text(grant.granted_at),
            revoke_button("/revoke-gh-grant", "handle", handle, csrf)
        ));
    }
    if grants.is_empty() {
        grant_rows
            .push_str(r#"<div class="none">No GitHub repositories are always allowed.</div>"#);
    }

    let mut approval_rows = String::new();
    let approvals = managed_gh::list_approvals(&config.gh_state_dir).unwrap_or_default();
    for (handle, approval) in &approvals {
        let display = gh_display(&approval.action, &approval.context);
        let mut operation = display.as_ref().map_or_else(
            || "Cannot be displayed".to_string(),
            |display| escape_html(&display.operation),
        );
        if let Some(head_sha) = &approval.action.head_sha {
            operation.push_str(&format!(
                " <code>{}</code>",
                escape_html(head_sha.get(..12).unwrap_or(head_sha))
            ));
        }
        let values = display.as_ref().map_or_else(String::new, |display| {
            format!(
                r#"<details><summary>Show what was approved</summary><p class="note">Requested from <code>{}</code>, approved {}</p>{}</details>"#,
                escape_html(&visible_context(&approval.context.requested_from)),
                ago_text(approval.approved_at),
                gh_review(&approval.action, &approval.context, display)
            )
        });
        approval_rows.push_str(&format!(
            r#"<div class="row"><div><code>{}</code></div><div class="muted">{operation}</div><div class="when"><span class="chip amber">{} left</span></div>{}{values}</div>"#,
            escape_html(&approval.action.repository.selector()),
            duration_text(managed_gh::remaining_seconds(approval)),
            revoke_button("/revoke-gh", "handle", handle, csrf)
        ));
    }
    if approvals.is_empty() {
        approval_rows.push_str(r#"<div class="none">No unused GitHub action approvals.</div>"#);
    }

    let main = format!(
        r#"<main class="page">{}<div class="page-h"><h1>Access</h1><p>Everything the sandbox may do without asking you again. Revoking takes effect on its next attempt.</p></div>{}{}{}{}</main>"#,
        flash_html(notice),
        access_block(
            "push",
            portal_ui::ICON_PUSH,
            "Git push",
            &plural(push.len(), "repository", "repositories"),
            &push_rows
        ),
        access_block(
            "fetch",
            portal_ui::ICON_FETCH,
            "Private fetch sources",
            &format!("{} · read only", plural(fetch.len(), "source", "sources")),
            &fetch_rows
        ),
        access_block(
            "gh",
            portal_ui::ICON_GH,
            "Always-allowed GitHub repositories",
            &plural(grants.len(), "repository", "repositories"),
            &grant_rows
        ),
        access_block(
            "gh",
            portal_ui::ICON_GH,
            "Unused one-time GitHub approvals",
            &format!(
                "expire {} minutes after approval",
                managed_gh::APPROVAL_TTL / 60
            ),
            &approval_rows
        ),
    );
    app_page(
        config,
        Nav::Access,
        pending_requests(config).len(),
        "Access",
        &main,
    )
}

fn render_pairing_controls(csrf_token: &str, pair_url: Option<&str>) -> String {
    let result = pair_url
        .map(|url| {
            let url = escape_html(url);
            format!(
                r#"<label class="field">Pair another client<input class="pair-url" type="url" value="{url}" readonly><small>Copy this link to the client you want to pair. Creating or copying it does not pair this browser. The link must use an address that client can reach.</small></label><a class="btn" href="{url}">Pair this browser ↗</a>"#
            )
        })
        .unwrap_or_default();
    format!(
        r#"<p>Create a five-minute, single-use client credential.</p><form method="post" action="/pair">{}<button class="btn primary">Create pairing link</button></form>{result}"#,
        hidden("csrf", csrf_token)
    )
}

fn render_restart_controls(csrf_token: &str) -> String {
    format!(
        r#"<p>Stop the current T3 container. When this launcher is supervised with a restart policy, the service starts a fresh sandbox. Pending requests and approvals are kept on the host; you unlock the portal again afterwards.</p><details class="danger-zone"><summary>Restart sandbox…</summary><p>Everything running in the container stops.</p><form method="post" action="/restart">{}<button class="btn danger">Confirm restart</button></form></details>"#,
        hidden("csrf", csrf_token)
    )
}

fn render_system(config: &Config, pair_url: Option<&str>, notice: Option<&Notice>) -> String {
    let main = format!(
        r#"<main class="page">{}<div class="page-h"><h1>System</h1><p>Client pairing, the live display and the sandbox itself.</p></div><section class="card block"><div class="block-h"><h3>Client pairing</h3></div><div class="block-b">{}</div></section><section class="card block"><div class="block-h"><h3>Live display and usage</h3></div><a class="linkrow" href="/display" target="_blank" rel="noopener"><b>Display ↗</b><span>View and control the sandbox's virtual X display in this browser.</span></a><a class="linkrow" href="/usage" target="_blank" rel="noopener"><b>Usage ↗</b><span>Public plan usage dashboard.</span></a></section><section class="card block"><div class="block-h"><h3>Sandbox lifecycle</h3></div><div class="block-b">{}</div></section></main>"#,
        flash_html(notice),
        render_pairing_controls(&config.csrf_token, pair_url),
        render_restart_controls(&config.csrf_token),
    );
    app_page(
        config,
        Nav::System,
        pending_requests(config).len(),
        "System",
        &main,
    )
}

const DISPLAY_PAGE: &str = r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><meta name="color-scheme" content="dark"><title>T3 Code · Display</title><style>
:root{--ink:#f4f1e8;--muted:#a4a49b;--line:#353630;--acid:#d8ff4f;--danger:#ff8c7e;--bg:#11120f}*{box-sizing:border-box}body{margin:0;min-height:100vh;color:var(--ink);background:radial-gradient(circle at 85% 10%,#293315 0,transparent 30%),var(--bg);font:14px/1.55 "IBM Plex Mono","Courier New",monospace}main{width:min(1400px,calc(100% - 32px));margin:auto;padding:24px 0}header{border-top:1px solid var(--acid);padding-top:16px;margin-bottom:20px;display:flex;align-items:baseline;justify-content:space-between;gap:20px}h1{font:400 clamp(32px,6vw,56px)/.95 Georgia,serif;letter-spacing:-.05em;margin:12px 0}a{color:var(--ink);text-underline-offset:4px}.toolbar{display:flex;align-items:center;gap:18px;flex-wrap:wrap;border:1px solid var(--line);padding:12px 14px;background:#11120fd9}.toolbar label{display:flex;align-items:center;gap:7px}.status{color:var(--muted);margin-right:auto}.status[data-connected="true"]{color:var(--acid)}button{border:0;background:var(--acid);color:#15170d;padding:10px 13px;font:700 11px/1 monospace;text-transform:uppercase;cursor:pointer}#screen{width:100%;height:calc(100vh - 190px);min-height:360px;margin-top:14px;border:1px solid var(--line);background:var(--bg);overflow:hidden}#screen canvas{outline:none}
</style></head><body><main><header><div><small>Private control plane</small><h1>Live display.</h1></div><a href="/">← Admin</a></header><div class="toolbar"><span id="status" class="status">Disconnected</span><label><input id="view-only" type="checkbox"> View only</label><label><input id="fit" type="checkbox" checked> Fit to window</label><button id="reconnect" type="button">Reconnect</button></div><div id="screen"></div></main><script type="module">
import RFB from '/display/novnc/core/rfb.js';
const screen = document.querySelector('#screen');
const status = document.querySelector('#status');
const viewOnly = document.querySelector('#view-only');
const fit = document.querySelector('#fit');
const socketUrl = (location.protocol === 'https:' ? 'wss' : 'ws') + '://' + location.host + '/display/socket';
let rfb;
let reconnectRequested = false;
function updateStatus(message, connected = false) {
  status.textContent = message;
  status.dataset.connected = String(connected);
}
function connect() {
  updateStatus('Connecting…');
  const connection = new RFB(screen, socketUrl);
  rfb = connection;
  connection.viewOnly = viewOnly.checked;
  connection.scaleViewport = fit.checked;
  connection.focusOnClick = true;
  connection.resizeSession = false;
  connection.background = getComputedStyle(document.documentElement).getPropertyValue('--bg').trim();
  connection.addEventListener('connect', () => {
    updateStatus('Connected', true);
    connection.focus({ preventScroll: true });
  });
  connection.addEventListener('disconnect', event => {
    if (rfb !== connection) return;
    rfb = null;
    updateStatus(event.detail.clean ? 'Disconnected' : 'Connection lost');
    if (reconnectRequested) {
      reconnectRequested = false;
      connect();
    }
  });
}
viewOnly.addEventListener('change', () => { if (rfb) rfb.viewOnly = viewOnly.checked; });
fit.addEventListener('change', () => { if (rfb) rfb.scaleViewport = fit.checked; });
document.querySelector('#reconnect').addEventListener('click', () => {
  if (!rfb) {
    connect();
    return;
  }
  reconnectRequested = true;
  updateStatus('Reconnecting…');
  rfb.disconnect();
});
connect();
</script></body></html>"#;

fn send_display_page(stream: &mut TcpStream) {
    let headers = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\nContent-Security-Policy: default-src 'none'; style-src 'unsafe-inline'; script-src 'self' 'unsafe-inline'; connect-src 'self' ws: wss:; img-src data:; base-uri 'none'; frame-ancestors 'none'\r\nReferrer-Policy: no-referrer\r\nX-Content-Type-Options: nosniff\r\nConnection: close\r\n\r\n",
        DISPLAY_PAGE.len()
    );
    let _ = stream.write_all(headers.as_bytes());
    let _ = stream.write_all(DISPLAY_PAGE.as_bytes());
}

fn send_novnc_asset(stream: &mut TcpStream, path: &str) {
    let Some(contents) = (!path.contains(".."))
        .then(|| novnc_assets::asset(path))
        .flatten()
    else {
        send_html(
            stream,
            404,
            &render_error("Not found", "The requested noVNC asset does not exist."),
            &[],
        );
        return;
    };
    let headers = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/javascript; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: private, max-age=3600\r\nX-Content-Type-Options: nosniff\r\nConnection: close\r\n\r\n",
        contents.len()
    );
    let _ = stream.write_all(headers.as_bytes());
    let _ = stream.write_all(contents);
}

fn visible_context(value: &str) -> String {
    let mut visible = String::new();
    for character in value.chars() {
        if crate::gh_proxy::refused_text_code_point(character.encode_utf8(&mut [0; 4])).is_some() {
            visible.push_str(&format!("U+{:04X}", character as u32));
        } else {
            visible.push(character);
        }
    }
    visible
}

struct GhDisplay<'a> {
    operation: String,
    values: Vec<(&'a str, Option<&'a str>)>,
}

fn gh_display<'a>(
    action: &'a managed_gh::Action,
    context: &managed_gh::Context,
) -> Option<GhDisplay<'a>> {
    let group = action.args.first()?.as_str();
    let subcommand = action.args.get(1)?.as_str();
    let mut index = 2;
    let number = match &action.item {
        Some(item) => {
            if action.args.get(index)? != &item.number.to_string() {
                return None;
            }
            index += 1;
            Some(item.number)
        }
        None => None,
    };
    let allowed: &[(&str, bool)] = match (group, subcommand) {
        ("issue", "create") => &[("title", false), ("body", false)],
        ("issue", "comment") | ("pr", "comment") => &[("body", false)],
        ("issue", "close") => &[("comment", false), ("reason", false)],
        ("issue", "edit") => &[("title", false), ("body", false)],
        ("pr", "create") => &[
            ("title", false),
            ("body", false),
            ("head", false),
            ("base", false),
            ("draft", true),
        ],
        ("pr", "edit") => &[("title", false), ("body", false), ("base", false)],
        ("pr", "ready") => &[("undo", true)],
        ("pr", "close") | ("pr", "reopen") | ("issue", "reopen") => &[("comment", false)],
        _ => return None,
    };
    let creates = matches!((group, subcommand), ("issue", "create") | ("pr", "create"));
    if creates != number.is_none() {
        return None;
    }
    if let Some(item) = &action.item {
        let expected = if group == "pr" {
            managed_gh::ItemKind::PullRequest
        } else {
            managed_gh::ItemKind::Issue
        };
        if item.kind != expected {
            return None;
        }
    }
    if (group, subcommand) == ("pr", "create")
        && (action.head_sha.is_none()
            || context.commits.is_none()
            || context.changed_files.is_none())
    {
        return None;
    }
    let mut values = Vec::new();
    for arg in &action.args[index..] {
        let (name, value) = if let Some((name, value)) = arg.strip_prefix("--")?.split_once('=') {
            (name, Some(value))
        } else {
            (arg.strip_prefix("--")?, None)
        };
        let (_, bare) = allowed.iter().find(|(allowed, _)| *allowed == name)?;
        if *bare != value.is_none() || values.iter().any(|(seen, _)| *seen == name) {
            return None;
        }
        values.push((name, value));
    }
    let flag = |name: &str| {
        values
            .iter()
            .find(|(flag, _)| *flag == name)
            .and_then(|(_, value)| *value)
    };
    let required: &[&str] = match (group, subcommand) {
        ("issue", "create") => &["title", "body"],
        ("issue", "comment") | ("pr", "comment") => &["body"],
        ("pr", "create") => &["title", "body", "head", "base"],
        _ => &[],
    };
    if required.iter().any(|name| flag(name).is_none())
        || (subcommand == "edit" && values.is_empty())
    {
        return None;
    }
    let number = number.unwrap_or_default();
    let operation = match (group, subcommand) {
        ("issue", "create") => "Create issue".to_string(),
        ("issue", "comment") => format!("Comment on issue #{number}"),
        ("pr", "comment") => format!("Comment on pull request #{number}"),
        ("issue", "close") => flag("reason").map_or_else(
            || format!("Close issue #{number}"),
            |reason| format!("Close issue #{number} as {reason}"),
        ),
        ("issue", "edit") => format!("Edit issue #{number}"),
        ("pr", "create") => {
            let draft = if values.iter().any(|(flag, _)| *flag == "draft") {
                " as draft"
            } else {
                ""
            };
            format!(
                "Open pull request from {} into {}{draft}",
                flag("head")?,
                flag("base")?
            )
        }
        ("pr", "edit") => format!("Edit pull request #{number}"),
        ("pr", "ready") if values.iter().any(|(flag, _)| *flag == "undo") => {
            format!("Convert pull request #{number} back to draft")
        }
        ("pr", "ready") => format!("Mark pull request #{number} ready for review"),
        ("pr", "close") => format!("Close pull request #{number}"),
        ("pr", "reopen") => format!("Reopen pull request #{number}"),
        ("issue", "reopen") => format!("Reopen issue #{number}"),
        _ => return None,
    };
    Some(GhDisplay { operation, values })
}

fn render_error(title: &str, message: &str) -> String {
    format!(
        "<!doctype html><meta charset=utf-8><title>{}</title><h1>{}</h1><p>{}</p>",
        escape_html(title),
        escape_html(title),
        escape_html(message)
    )
}

fn read_request(stream: &mut TcpStream) -> Result<Request, String> {
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .map_err(|error| error.to_string())?;
    let peer = stream.peer_addr().map_err(|error| error.to_string())?;
    let mut reader = BufReader::new(stream);
    let mut total = 0;
    let mut request_line = String::new();
    total += reader
        .read_line(&mut request_line)
        .map_err(|error| error.to_string())?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().ok_or("missing request method")?.to_string();
    let path = parts.next().ok_or("missing request path")?.to_string();
    if !matches!(method.as_str(), "GET" | "POST") || !path.starts_with('/') {
        return Err("unsupported request".to_string());
    }
    let mut headers = HashMap::new();
    loop {
        let mut line = String::new();
        let count = reader
            .read_line(&mut line)
            .map_err(|error| error.to_string())?;
        total += count;
        if total > MAX_REQUEST_BYTES {
            return Err("request headers are too large".to_string());
        }
        if count == 0 || line == "\r\n" || line == "\n" {
            break;
        }
        let (name, value) = line.split_once(':').ok_or("invalid request header")?;
        headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
    }
    let content_length = headers
        .get("content-length")
        .map(|value| value.parse::<usize>().map_err(|_| "invalid content length"))
        .transpose()?
        .unwrap_or(0);
    if content_length > MAX_BODY_BYTES {
        return Err("request body is too large".to_string());
    }
    let mut body = vec![0; content_length];
    reader
        .read_exact(&mut body)
        .map_err(|error| error.to_string())?;
    let (path, query) = path.split_once('?').unwrap_or((&path, ""));
    Ok(Request {
        method,
        query: query.to_string(),
        path: path.to_string(),
        headers,
        body,
        peer,
    })
}

fn parse_form(body: &[u8]) -> Result<HashMap<String, String>, String> {
    let body = std::str::from_utf8(body).map_err(|_| "form is not UTF-8")?;
    let mut values = HashMap::new();
    for field in body.split('&').filter(|value| !value.is_empty()) {
        let (key, value) = field.split_once('=').unwrap_or((field, ""));
        values.insert(percent_decode(key)?, percent_decode(value)?);
    }
    Ok(values)
}

fn percent_decode(value: &str) -> Result<String, String> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => decoded.push(b' '),
            b'%' if index + 2 < bytes.len() => {
                let high = hex_value(bytes[index + 1]).ok_or("invalid form escape")?;
                let low = hex_value(bytes[index + 2]).ok_or("invalid form escape")?;
                decoded.push(high * 16 + low);
                index += 2;
            }
            b'%' => return Err("incomplete form escape".to_string()),
            byte => decoded.push(byte),
        }
        index += 1;
    }
    String::from_utf8(decoded).map_err(|_| "form value is not UTF-8".to_string())
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn is_authorized(request: &Request, config: &Config) -> bool {
    request
        .headers
        .get("cookie")
        .and_then(|cookies| {
            cookies.split(';').find_map(|cookie| {
                let (name, value) = cookie.trim().split_once('=')?;
                (name == "t3_admin_session").then_some(value)
            })
        })
        .is_some_and(|value| safe_equal(value, &config.session_token))
}

fn send_html(stream: &mut TcpStream, status: u16, body: &str, extra_headers: &[(&str, String)]) {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        429 => "Too Many Requests",
        _ => "Error",
    };
    let mut headers = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\nContent-Security-Policy: default-src 'none'; style-src 'unsafe-inline'; script-src 'self'; form-action 'self'; base-uri 'none'; frame-ancestors 'none'\r\nReferrer-Policy: no-referrer\r\nX-Content-Type-Options: nosniff\r\nConnection: close\r\n",
        body.len()
    );
    for (name, value) in extra_headers {
        headers.push_str(name);
        headers.push_str(": ");
        headers.push_str(value);
        headers.push_str("\r\n");
    }
    headers.push_str("\r\n");
    let _ = stream.write_all(headers.as_bytes());
    let _ = stream.write_all(body.as_bytes());
}

fn send_json(stream: &mut TcpStream, status: u16, body: &impl serde::Serialize) {
    let body = serde_json::to_vec(body).unwrap_or_else(|_| b"{\"schema_version\":1}".to_vec());
    let reason = match status {
        200 => "OK",
        503 => "Service Unavailable",
        _ => "Error",
    };
    let headers = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(headers.as_bytes());
    let _ = stream.write_all(&body);
}

fn send_usage_dashboard(stream: &mut TcpStream) {
    let body = usage_dashboard::PAGE;
    let headers = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\nContent-Security-Policy: default-src 'none'; style-src 'unsafe-inline'; script-src 'unsafe-inline'; connect-src 'self'; base-uri 'none'; frame-ancestors 'none'\r\nReferrer-Policy: no-referrer\r\nX-Content-Type-Options: nosniff\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(headers.as_bytes());
    let _ = stream.write_all(body.as_bytes());
}

fn send_portal_script(stream: &mut TcpStream) {
    let body = portal_ui::SCRIPT;
    let headers = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/javascript; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-cache\r\nX-Content-Type-Options: nosniff\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(headers.as_bytes());
    let _ = stream.write_all(body.as_bytes());
}

fn redirect(stream: &mut TcpStream, location: &str) {
    redirect_with_headers(stream, location, &[]);
}

fn redirect_with_headers(stream: &mut TcpStream, location: &str, extra: &[(&str, String)]) {
    let mut headers = format!(
        "HTTP/1.1 303 See Other\r\nLocation: {location}\r\nCache-Control: no-store\r\nContent-Length: 0\r\nConnection: close\r\n"
    );
    for (name, value) in extra {
        headers.push_str(name);
        headers.push_str(": ");
        headers.push_str(value);
        headers.push_str("\r\n");
    }
    headers.push_str("\r\n");
    let _ = stream.write_all(headers.as_bytes());
}

fn random_token(bytes: usize) -> String {
    let mut random = vec![0; bytes];
    File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut random))
        .expect("could not read /dev/urandom");
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(random)
}

fn safe_equal(left: &str, right: &str) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.bytes()
        .zip(right.bytes())
        .fold(0_u8, |difference, (left, right)| {
            difference | (left ^ right)
        })
        == 0
}

fn valid_pin(pin: &str) -> bool {
    (4..=12).contains(&pin.len()) && pin.bytes().all(|byte| byte.is_ascii_digit())
}

fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::net::Shutdown;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn decodes_forms() {
        let values = parse_form(b"scope=once&path=hello%2Fworld+again").unwrap();
        assert_eq!(values.get("scope").unwrap(), "once");
        assert_eq!(values.get("path").unwrap(), "hello/world again");
    }

    #[test]
    fn validates_hostnames() {
        assert_eq!(
            parse_hostname("example.test:3774").as_deref(),
            Some("example.test")
        );
        assert_eq!(parse_hostname("[::1]:3774").as_deref(), Some("::1"));
        assert!(parse_hostname("bad/host:3774").is_none());
    }

    #[test]
    fn validates_pins() {
        assert!(valid_pin("1234"));
        assert!(valid_pin("123456789012"));
        assert!(!valid_pin("123"));
        assert!(!valid_pin("123x"));
    }

    #[test]
    fn renders_transferable_pairing_link_without_opening_it() {
        let html = render_pairing_controls(
            "csrf-token",
            Some("http://example.test:3773/pair?first=1&second=\"two\""),
        );

        assert!(html.contains("Pair another client"));
        assert!(html.contains("readonly"));
        assert!(html.contains(
            "value=\"http://example.test:3773/pair?first=1&amp;second=&quot;two&quot;\""
        ));
        assert!(
            html.contains(
                "href=\"http://example.test:3773/pair?first=1&amp;second=&quot;two&quot;\""
            )
        );
        assert!(html.contains("Creating or copying it does not pair this browser."));
    }

    #[test]
    fn hides_pairing_result_before_a_link_is_created() {
        let html = render_pairing_controls("csrf-token", None);

        assert!(html.contains("Create pairing link"));
        assert!(!html.contains("Pair another client"));
        assert!(!html.contains("Pair this browser"));
    }

    #[test]
    fn login_only_continues_to_portal_pages() {
        assert_eq!(
            login_target(Some("/r/gh-0123456789abcdef")),
            "/r/gh-0123456789abcdef"
        );
        assert_eq!(login_target(Some("/access")), "/access");
        for rejected in [
            None,
            Some("//evil.test/r/gh-0123456789abcdef"),
            Some("https://evil.test/"),
            Some("/r/gh-0123"),
            Some("/r/gh-0123456789ABCDEF"),
            Some("/r/ssh-0123456789abcdef"),
            Some("/r/gh-0123456789abcdef/extra"),
            Some("/display"),
        ] {
            assert_eq!(login_target(rejected), "/", "{rejected:?}");
        }
    }

    #[test]
    fn locked_deep_link_unlocks_straight_to_the_request() {
        let workspace = temporary_workspace("deep-link");
        fs::create_dir_all(&workspace).unwrap();
        let config = test_config(&workspace);

        let locked = make_request(
            &config,
            "GET /r/gh-0123456789abcdef HTTP/1.1\r\nHost: localhost\r\n\r\n",
        );
        assert!(locked.starts_with("HTTP/1.1 200 OK"));
        assert!(locked.contains("Unlock to review this request"));
        assert!(locked.contains(r#"name="next" value="/r/gh-0123456789abcdef""#));
        assert!(locked.contains("script-src 'self'"));

        let login = |next: &str| {
            let body = format!("pin=1234&next={next}");
            make_request(
                &config,
                &format!(
                    "POST /login HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\n\r\n{body}",
                    body.len()
                ),
            )
        };
        let unlocked = login("%2Fr%2Fgh-0123456789abcdef");
        assert!(unlocked.contains("Location: /r/gh-0123456789abcdef\r\n"));
        assert!(unlocked.contains("SameSite=Lax"));
        assert!(login("%2F%2Fevil.test").contains("Location: /\r\n"));
        fs::remove_dir_all(workspace).unwrap();
    }

    #[test]
    fn decisions_continue_with_the_next_request_and_keep_notices_per_redirect() {
        let workspace = temporary_workspace("decision-flow");
        fs::create_dir_all(&workspace).unwrap();
        let mut config = test_config(&workspace);
        config.fetch_state_dir = managed_fetch::prepare_state_dir(&config.fetch_state_dir).unwrap();
        for repository in ["org/first.git", "org/second.git"] {
            let source = managed_fetch::Source {
                host: "github.com".to_string(),
                repository: repository.to_string(),
            };
            managed_fetch::record_candidate(&config.fetch_state_dir, &source, "app").unwrap();
        }
        let order: Vec<RequestKey> = pending_requests(&config).iter().map(Pending::key).collect();
        let approve = |key: &RequestKey| {
            let body = format!("csrf=csrf&scope=once&id={}", key.id);
            let response = make_request(
                &config,
                &format!(
                    "POST /approve-fetch HTTP/1.1\r\nHost: localhost\r\nCookie: t3_admin_session=session\r\nContent-Length: {}\r\n\r\n{body}",
                    body.len()
                ),
            );
            assert!(response.starts_with("HTTP/1.1 303 See Other"));
            response
                .lines()
                .find_map(|line| line.strip_prefix("Location: "))
                .unwrap()
                .to_string()
        };
        let page = |path: &str| {
            make_request(
                &config,
                &format!(
                    "GET {path} HTTP/1.1\r\nHost: localhost\r\nCookie: t3_admin_session=session\r\n\r\n"
                ),
            )
        };
        let notice = |key: &RequestKey| {
            let source = managed_fetch::list_candidates(&config.fetch_state_dir)
                .unwrap()
                .into_iter()
                .find(|(id, _)| *id == key.id)
                .unwrap()
                .1
                .source
                .display();
            format!("Approved one fetch from {source}.")
        };
        let (first_notice, second_notice) = (notice(&order[0]), notice(&order[1]));

        let first = approve(&order[0]);
        assert!(first.starts_with(&format!("{}?n=", order[1].path())));
        let second = approve(&order[1]);
        assert!(second.starts_with("/?n="));

        let second_page = page(&second);
        assert!(second_page.contains(&second_notice));
        assert!(!second_page.contains(&first_notice));
        let first_page = page(&first);
        assert!(first_page.contains(&first_notice));
        assert!(first_page.contains("This request is no longer pending"));
        assert!(!page(&first).contains(&first_notice));
        assert!(!page("/?n=../../etc/passwd").contains(r#"class="flash"#));
        fs::remove_dir_all(workspace).unwrap();
    }

    #[test]
    fn lock_requires_the_csrf_token() {
        let workspace = temporary_workspace("lock-csrf");
        fs::create_dir_all(&workspace).unwrap();
        let config = test_config(&workspace);
        let lock = |body: &str| {
            make_request(
                &config,
                &format!(
                    "POST /lock HTTP/1.1\r\nHost: localhost\r\nCookie: t3_admin_session=session\r\nContent-Length: {}\r\n\r\n{body}",
                    body.len()
                ),
            )
        };
        let rejected = lock("csrf=wrong");
        assert!(rejected.starts_with("HTTP/1.1 403 Forbidden"));
        assert!(!rejected.contains("Set-Cookie"));
        let locked = lock("csrf=csrf");
        assert!(locked.starts_with("HTTP/1.1 303 See Other"));
        assert!(
            locked.contains(
                "Set-Cookie: t3_admin_session=; HttpOnly; SameSite=Lax; Path=/; Max-Age=0"
            )
        );
        fs::remove_dir_all(workspace).unwrap();
    }

    #[test]
    fn unused_gh_approvals_keep_their_review_context() {
        let workspace = temporary_workspace("gh-approval-context");
        fs::create_dir_all(&workspace).unwrap();
        let mut config = test_config(&workspace);
        config.gh_state_dir = managed_gh::prepare_state_dir(&config.gh_state_dir).unwrap();
        let mut context = gh_context();
        context.commits = Some(2);
        context.changed_files = Some(3);
        for head_sha in [
            "1111111111111111111111111111111111111111",
            "2222222222222222222222222222222222222222",
        ] {
            let action = managed_gh::Action {
                repository: managed_gh::Repository {
                    host: "github.com".to_string(),
                    owner: "other".to_string(),
                    name: "project".to_string(),
                },
                repository_id: 42,
                item: None,
                head_sha: Some(head_sha.to_string()),
                args: vec![
                    "pr".to_string(),
                    "create".to_string(),
                    "--title=Same".to_string(),
                    "--body=Same".to_string(),
                    "--head=topic".to_string(),
                    "--base=main".to_string(),
                ],
            };
            let handle =
                managed_gh::record_candidate(&config.gh_state_dir, &action, &context).unwrap();
            managed_gh::approve(&config.gh_state_dir, &handle).unwrap();
        }
        let html = render_access(&config, None);
        assert!(html.contains("<code>1111111111111111111111111111111111111111</code>"));
        assert!(html.contains("<code>2222222222222222222222222222222222222222</code>"));
        assert_eq!(
            html.matches("The pull request follows branch topic")
                .count(),
            2
        );
        assert_eq!(html.matches("<b>Public repository.</b>").count(), 2);
        assert!(html.contains("<span>Changed files</span><strong>3</strong>"));
        assert!(html.contains("Requested from <code>services/&lt;api&gt;</code>"));
        fs::remove_dir_all(workspace).unwrap();
    }

    #[test]
    fn manages_fetch_approvals() {
        let workspace = temporary_workspace("fetch-approval");
        std::fs::create_dir_all(&workspace).unwrap();
        let mut config = test_config(&workspace);
        config.fetch_state_dir = managed_fetch::prepare_state_dir(&config.fetch_state_dir).unwrap();
        let source = managed_fetch::Source {
            host: "github.com".to_string(),
            repository: "org/private.git".to_string(),
        };
        let id = managed_fetch::record_candidate(&config.fetch_state_dir, &source, "services/api")
            .unwrap();

        let html = render_inbox(&config, None, None);
        assert!(html.contains("Read a private repository"));
        assert!(html.contains("git@github.com:org/private.git"));
        assert!(html.contains("services/api"));

        let form = HashMap::from([
            ("id".to_string(), id),
            ("scope".to_string(), "persistent".to_string()),
        ]);
        approve_fetch_candidate(&config, &form).unwrap();
        let html = render_access(&config, None);
        assert!(html.contains("Private fetch sources"));
        assert!(html.contains("read only"));
        assert!(
            managed_fetch::read_approval(&config.fetch_state_dir, &source)
                .unwrap()
                .is_some()
        );
        let approval_id = managed_fetch::list_approvals(&config.fetch_state_dir)
            .unwrap()
            .remove(0)
            .0;
        revoke_fetch_approval(&config, &HashMap::from([("id".to_string(), approval_id)])).unwrap();
        assert!(
            managed_fetch::read_approval(&config.fetch_state_dir, &source)
                .unwrap()
                .is_none()
        );

        std::fs::remove_dir_all(workspace).unwrap();
    }

    fn gh_action(body: &str) -> managed_gh::Action {
        managed_gh::Action {
            repository: managed_gh::Repository {
                host: "github.com".to_string(),
                owner: "other".to_string(),
                name: "project".to_string(),
            },
            repository_id: 42,
            item: Some(managed_gh::Item {
                number: 7,
                kind: managed_gh::ItemKind::Issue,
                node_id: "ITEM_7".to_string(),
            }),
            head_sha: None,
            args: vec![
                "issue".to_string(),
                "comment".to_string(),
                "7".to_string(),
                format!("--body={body}"),
            ],
        }
    }

    fn gh_context() -> managed_gh::Context {
        managed_gh::Context {
            repository_visibility: "public".to_string(),
            item_title: Some("Hostile <title>".to_string()),
            item_state: Some("open".to_string()),
            commits: None,
            changed_files: None,
            requested_from: "services/<api>".to_string(),
            requested_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs(),
        }
    }

    #[test]
    fn managed_gh_routes_require_csrf() {
        let workspace = temporary_workspace("gh-csrf");
        fs::create_dir_all(&workspace).unwrap();
        let config = test_config(&workspace);
        for path in ["/approve-gh", "/grant-gh", "/revoke-gh-grant"] {
            let body = "csrf=wrong&handle=0123456789abcdef&scope=command";
            let response = make_request(
                &config,
                &format!(
                    "POST {path} HTTP/1.1\r\nHost: localhost\r\nCookie: t3_admin_session=session\r\nContent-Length: {}\r\n\r\n{body}",
                    body.len()
                ),
            );
            assert!(response.starts_with("HTTP/1.1 403 Forbidden"));
        }
        fs::remove_dir_all(workspace).unwrap();
    }

    #[test]
    fn managed_gh_grant_routes_require_session() {
        let workspace = temporary_workspace("gh-grant-session");
        fs::create_dir_all(&workspace).unwrap();
        let config = test_config(&workspace);
        for path in ["/grant-gh", "/revoke-gh-grant"] {
            let body = "csrf=csrf&handle=0123456789abcdef&scope=command";
            let response = make_request(
                &config,
                &format!(
                    "POST {path} HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\n\r\n{body}",
                    body.len()
                ),
            );
            assert!(response.starts_with("HTTP/1.1 303 See Other"));
        }
        fs::remove_dir_all(workspace).unwrap();
    }

    #[test]
    fn managed_gh_grant_rejects_invalid_scope() {
        let workspace = temporary_workspace("gh-grant-scope");
        fs::create_dir_all(&workspace).unwrap();
        let config = test_config(&workspace);
        let body = "csrf=csrf&handle=0123456789abcdef&scope=invalid";
        let response = make_request(
            &config,
            &format!(
                "POST /grant-gh HTTP/1.1\r\nHost: localhost\r\nCookie: t3_admin_session=session\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            ),
        );
        assert!(response.starts_with("HTTP/1.1 400 Bad Request"));
        assert!(response.contains("Invalid grant scope"));
        fs::remove_dir_all(workspace).unwrap();
    }

    #[test]
    fn managed_gh_page_escapes_hostile_action_text() {
        let workspace = temporary_workspace("gh-html");
        fs::create_dir_all(&workspace).unwrap();
        let mut config = test_config(&workspace);
        config.gh_state_dir = managed_gh::prepare_state_dir(&config.gh_state_dir).unwrap();
        managed_gh::record_candidate(
            &config.gh_state_dir,
            &gh_action("<script>alert('x')</script>&"),
            &gh_context(),
        )
        .unwrap();
        let html = render_inbox(&config, None, None);
        assert!(!html.contains("<script>alert('x')</script>"));
        assert!(html.contains("&lt;script&gt;alert(&#39;x&#39;)&lt;/script&gt;&amp;"));
        assert!(html.contains("Hostile &lt;title&gt;"));
        assert!(html.contains("services/&lt;api&gt;"));
        fs::remove_dir_all(workspace).unwrap();
    }

    #[test]
    fn undisplayable_gh_action_cannot_be_approved() {
        let workspace = temporary_workspace("gh-undisplayable");
        fs::create_dir_all(&workspace).unwrap();
        let mut config = test_config(&workspace);
        config.gh_state_dir = managed_gh::prepare_state_dir(&config.gh_state_dir).unwrap();
        let mut action = gh_action("hello");
        action.args.push("--unaccounted".to_string());
        let handle =
            managed_gh::record_candidate(&config.gh_state_dir, &action, &gh_context()).unwrap();
        let html = render_gh_detail(&handle, &action, &gh_context(), "csrf", "");
        assert!(html.contains("Cannot be displayed"));
        assert!(!html.contains("Approve once"));
        assert!(html.contains("Dismiss"));
        assert!(
            approve_gh_action(
                &config,
                &HashMap::from([("handle".to_string(), handle.clone())])
            )
            .unwrap_err()
            .contains("cannot be displayed")
        );
        assert!(
            grant_gh_action(
                &config,
                &HashMap::from([
                    ("handle".to_string(), handle.clone()),
                    ("scope".to_string(), "command".to_string()),
                ])
            )
            .unwrap_err()
            .contains("cannot be displayed")
        );
        let body = format!("csrf=csrf&handle={handle}");
        let response = make_request(
            &config,
            &format!(
                "POST /approve-gh HTTP/1.1\r\nHost: localhost\r\nCookie: t3_admin_session=session\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            ),
        );
        assert!(response.starts_with("HTTP/1.1 400 Bad Request"));
        fs::remove_dir_all(workspace).unwrap();
    }

    #[test]
    fn gh_grant_warning_and_buttons_escape_the_stored_repository() {
        let mut action = gh_action("hello");
        action.repository.host = "github.com<script>".to_string();
        action.repository.owner = "other&owner".to_string();
        action.repository.name = "project\"name".to_string();
        let html = render_gh_detail("0123456789abcdef", &action, &gh_context(), "csrf", "");
        let escaped = "github.com&lt;script&gt;/other&amp;owner/project&quot;name";
        assert!(html.contains(&format!(
            "Future matching commands from this sandbox to {escaped} will run without review."
        )));
        assert!(html.contains("This repository is public."));
        assert!(html.contains(&format!("Always allow issue comments in {escaped}")));
        assert!(html.contains(&format!(
            "Always allow all issue and pull request actions in {escaped}"
        )));
        assert!(!html.contains("github.com<script>"));
    }

    #[test]
    fn saved_gh_grant_renders_in_its_own_section_and_can_be_revoked() {
        let workspace = temporary_workspace("gh-grant-list");
        fs::create_dir_all(&workspace).unwrap();
        let mut config = test_config(&workspace);
        config.gh_state_dir = managed_gh::prepare_state_dir(&config.gh_state_dir).unwrap();
        let candidate =
            managed_gh::record_candidate(&config.gh_state_dir, &gh_action("hello"), &gh_context())
                .unwrap();
        grant_gh_action(
            &config,
            &HashMap::from([
                ("handle".to_string(), candidate),
                ("scope".to_string(), "command".to_string()),
            ]),
        )
        .unwrap();

        let html = render_access(&config, None);
        assert!(html.contains("Always-allowed GitHub repositories"));
        assert!(html.contains("github.com/other/project"));
        assert!(html.contains("issue comments"));
        assert!(html.contains("/revoke-gh-grant"));

        let (grant_handle, grant) = managed_gh::list_grants(&config.gh_state_dir)
            .unwrap()
            .remove(0);
        assert!(html.contains(&usage_api::format_epoch(
            i64::try_from(grant.granted_at).unwrap()
        )));
        revoke_gh_grant(
            &config,
            &HashMap::from([("handle".to_string(), grant_handle)]),
        )
        .unwrap();
        assert!(
            managed_gh::list_grants(&config.gh_state_dir)
                .unwrap()
                .is_empty()
        );
        fs::remove_dir_all(workspace).unwrap();
    }

    #[test]
    fn covered_gh_leftovers_are_removed_before_render_or_authorization() {
        let workspace = temporary_workspace("gh-covered-leftovers");
        fs::create_dir_all(&workspace).unwrap();
        let mut config = test_config(&workspace);
        config.gh_state_dir = managed_gh::prepare_state_dir(&config.gh_state_dir).unwrap();
        let first =
            managed_gh::record_candidate(&config.gh_state_dir, &gh_action("first"), &gh_context())
                .unwrap();
        managed_gh::grant_candidate(
            &config.gh_state_dir,
            &first,
            managed_gh::RequestedGrantScope::Repository,
        )
        .unwrap();

        managed_gh::record_candidate(
            &config.gh_state_dir,
            &gh_action("covered-render-leftover"),
            &gh_context(),
        )
        .unwrap();
        let html = render_inbox(&config, None, None);
        assert!(!html.contains("covered-render-leftover"));
        assert!(
            managed_gh::list_candidates(&config.gh_state_dir)
                .unwrap()
                .is_empty()
        );

        let approve_handle = managed_gh::record_candidate(
            &config.gh_state_dir,
            &gh_action("covered-approve-leftover"),
            &gh_context(),
        )
        .unwrap();
        assert_eq!(
            approve_gh_action(
                &config,
                &HashMap::from([("handle".to_string(), approve_handle)])
            )
            .unwrap_err(),
            "GitHub action is no longer pending"
        );

        let grant_handle = managed_gh::record_candidate(
            &config.gh_state_dir,
            &gh_action("covered-grant-leftover"),
            &gh_context(),
        )
        .unwrap();
        assert_eq!(
            grant_gh_action(
                &config,
                &HashMap::from([
                    ("handle".to_string(), grant_handle),
                    ("scope".to_string(), "command".to_string()),
                ])
            )
            .unwrap_err(),
            "GitHub action is no longer pending"
        );
        fs::remove_dir_all(workspace).unwrap();
    }

    #[test]
    fn bare_flags_and_whitespace_markers_are_explicitly_rendered() {
        let action = managed_gh::Action {
            repository: managed_gh::Repository {
                host: "github.com".to_string(),
                owner: "other".to_string(),
                name: "project".to_string(),
            },
            repository_id: 42,
            item: None,
            head_sha: Some("abc".to_string()),
            args: vec![
                "pr".to_string(),
                "create".to_string(),
                "--title=x".to_string(),
                "--body=line\t \nnext".to_string(),
                "--head=topic".to_string(),
                "--base=main".to_string(),
                "--draft".to_string(),
            ],
        };
        let mut context = gh_context();
        context.commits = Some(2);
        context.changed_files = Some(3);
        let html = render_gh_detail("0123456789abcdef", &action, &context, "csrf", "");
        assert!(html.contains("Open pull request from topic into main as draft"));
        assert!(html.contains("Head commit when requested"));
        assert!(html.contains(
            "The pull request follows branch topic; commits pushed to it later, including between approval and creation, become part of it."
        ));
        assert!(html.contains("<b>Draft</b><span>enabled</span>"));
        assert!(html.contains("class=whitespace>→"));
        assert!(html.contains("class=whitespace>·"));
    }

    #[test]
    fn serves_usage_without_authentication_and_does_not_enable_cors() {
        let workspace = temporary_workspace("empty");
        std::fs::create_dir(&workspace).unwrap();
        let config = test_config(&workspace);

        let response = make_request(
            &config,
            "GET /api/usage?format=json HTTP/1.1\r\nHost: localhost\r\n\r\n",
        );
        std::fs::remove_dir(&workspace).unwrap();

        assert!(
            response.starts_with("HTTP/1.1 200 OK\r\n")
                || response.starts_with("HTTP/1.1 503 Service Unavailable\r\n")
        );
        assert!(response.contains("Content-Type: application/json; charset=utf-8\r\n"));
        assert!(response.contains("Cache-Control: no-store\r\n"));
        assert!(response.contains("X-Content-Type-Options: nosniff\r\n"));
        assert!(!response.contains("Access-Control-Allow-Origin"));
        assert!(!response.contains("Set-Cookie"));
        assert!(response.contains("\"schema_version\":1"));
        assert!(response.contains("\"anthropic\""));
        assert!(response.contains("\"openai\""));
        assert!(response.contains("\"ollama\""));
    }

    #[test]
    fn serves_usage_dashboard_without_authentication() {
        let workspace = temporary_workspace("dashboard");
        std::fs::create_dir(&workspace).unwrap();
        let config = test_config(&workspace);

        let response = make_request(
            &config,
            "GET /usage?view=human HTTP/1.1\r\nHost: localhost\r\n\r\n",
        );
        let admin = make_request(&config, "GET / HTTP/1.1\r\nHost: localhost\r\n\r\n");
        std::fs::remove_dir(&workspace).unwrap();

        assert!(response.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(response.contains("Content-Type: text/html; charset=utf-8\r\n"));
        assert!(response.contains("connect-src 'self'"));
        assert!(response.contains("Plan usage · Claude Sandbox"));
        assert!(response.contains("href=\"/api/usage\""));
        assert!(response.contains("fetch(\"/api/usage\""));
        assert!(!response.contains("Set-Cookie"));
        assert!(admin.contains("href=\"/usage\""));
    }

    #[test]
    fn display_page_and_assets_are_authenticated() {
        let workspace = temporary_workspace("display-routes");
        std::fs::create_dir(&workspace).unwrap();
        let config = test_config(&workspace);

        let unauthorized =
            make_request(&config, "GET /display HTTP/1.1\r\nHost: localhost\r\n\r\n");
        assert!(unauthorized.starts_with("HTTP/1.1 303 See Other\r\n"));

        let page = make_request(
            &config,
            "GET /display HTTP/1.1\r\nHost: localhost\r\nCookie: t3_admin_session=session\r\n\r\n",
        );
        assert!(page.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(page.contains("/display/novnc/core/rfb.js"));
        assert!(page.contains("/display/socket"));
        assert!(page.contains("connect-src 'self' ws: wss:"));
        assert!(page.contains("img-src data:"));

        let asset = make_request(
            &config,
            "GET /display/novnc/core/rfb.js HTTP/1.1\r\nHost: localhost\r\nCookie: t3_admin_session=session\r\n\r\n",
        );
        assert!(asset.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(asset.contains("Content-Type: text/javascript; charset=utf-8\r\n"));
        assert!(asset.contains("export default class RFB"));

        let unknown = make_request(
            &config,
            "GET /display/novnc/core/missing.js HTTP/1.1\r\nHost: localhost\r\nCookie: t3_admin_session=session\r\n\r\n",
        );
        assert!(unknown.starts_with("HTTP/1.1 404 Not Found\r\n"));

        let asset_unauthorized = make_request(
            &config,
            "GET /display/novnc/core/rfb.js HTTP/1.1\r\nHost: localhost\r\n\r\n",
        );
        assert!(asset_unauthorized.starts_with("HTTP/1.1 303 See Other\r\n"));

        let admin = make_request(
            &config,
            "GET / HTTP/1.1\r\nHost: localhost\r\nCookie: t3_admin_session=session\r\n\r\n",
        );
        assert!(admin.contains("href=\"/display\""));
        std::fs::remove_dir(&workspace).unwrap();
    }

    #[test]
    fn display_socket_validates_auth_origin_and_upgrade_headers() {
        let workspace = temporary_workspace("display-socket-validation");
        std::fs::create_dir(&workspace).unwrap();
        let config = test_config(&workspace);
        let upgrade = "Upgrade: websocket\r\nConnection: keep-alive, Upgrade\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n";

        let unauthorized = make_request(
            &config,
            &format!(
                "GET /display/socket HTTP/1.1\r\nHost: localhost\r\nOrigin: http://localhost\r\n{upgrade}\r\n"
            ),
        );
        assert!(unauthorized.starts_with("HTTP/1.1 303 See Other\r\n"));

        for origin in ["", "Origin: http://elsewhere.test\r\n"] {
            let forbidden = make_request(
                &config,
                &format!(
                    "GET /display/socket HTTP/1.1\r\nHost: localhost\r\nCookie: t3_admin_session=session\r\n{origin}{upgrade}\r\n"
                ),
            );
            assert!(forbidden.starts_with("HTTP/1.1 403 Forbidden\r\n"));
        }

        let missing_host = make_request(
            &config,
            &format!(
                "GET /display/socket HTTP/1.1\r\nOrigin: http://localhost\r\nCookie: t3_admin_session=session\r\n{upgrade}\r\n"
            ),
        );
        assert!(missing_host.starts_with("HTTP/1.1 403 Forbidden\r\n"));

        let bad_upgrade = make_request(
            &config,
            "GET /display/socket HTTP/1.1\r\nHost: localhost\r\nOrigin: http://localhost\r\nCookie: t3_admin_session=session\r\n\r\n",
        );
        assert!(bad_upgrade.starts_with("HTTP/1.1 400 Bad Request\r\n"));
        std::fs::remove_dir(&workspace).unwrap();
    }

    #[test]
    fn display_socket_relays_bytes_and_closes_cleanly() {
        let workspace = temporary_workspace("display-socket-relay");
        std::fs::create_dir(&workspace).unwrap();
        let config = Arc::new(test_config(&workspace));
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let (done_tx, done_rx) = mpsc::channel();
        let server_config = Arc::clone(&config);
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            handle_connection(stream, &server_config);
            done_tx.send(()).unwrap();
        });

        let mut client = TcpStream::connect(address).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        client
            .write_all(
                b"GET /display/socket HTTP/1.1\r\nHost: localhost\r\nOrigin: http://localhost\r\nCookie: t3_admin_session=session\r\nUpgrade: WebSocket\r\nConnection: keep-alive, upgrade\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n",
            )
            .unwrap();
        let response = read_http_headers(&mut client);
        assert!(response.starts_with("HTTP/1.1 101 Switching Protocols\r\n"));
        assert!(response.contains("Sec-WebSocket-Accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo=\r\n"));

        client
            .write_all(&masked_client_frame(
                crate::websocket::Opcode::Binary,
                b"hello",
            ))
            .unwrap();
        let (opcode, payload) = read_server_frame(&mut client);
        assert_eq!(opcode, crate::websocket::Opcode::Binary);
        assert_eq!(payload, b"hello");

        client
            .write_all(&masked_client_frame(
                crate::websocket::Opcode::Ping,
                b"still here",
            ))
            .unwrap();
        let (opcode, payload) = read_server_frame(&mut client);
        assert_eq!(opcode, crate::websocket::Opcode::Pong);
        assert_eq!(payload, b"still here");

        client
            .write_all(&masked_client_frame(crate::websocket::Opcode::Close, &[]))
            .unwrap();
        let (opcode, payload) = read_server_frame(&mut client);
        assert_eq!(opcode, crate::websocket::Opcode::Close);
        assert!(payload.is_empty());

        done_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        server.join().unwrap();
        drop(config);
        std::fs::remove_dir(&workspace).unwrap();
    }

    #[test]
    fn display_command_is_fixed_and_gives_x11vnc_a_socket() {
        let command = display_command("sandbox-t3-123");
        assert_eq!(command.get_program(), "podman");
        assert_eq!(
            command
                .get_args()
                .map(|argument| argument.to_string_lossy().into_owned())
                .collect::<Vec<_>>(),
            [
                "exec",
                "-i",
                "sandbox-t3-123",
                "setsid",
                "-w",
                "sh",
                "-c",
                DISPLAY_SESSION_SCRIPT,
            ]
        );
    }

    #[test]
    fn serves_sanitized_cached_usage() {
        let workspace = temporary_workspace("cached");
        let config = test_config(&workspace);
        std::fs::create_dir_all(&config.usage_state_dir).unwrap();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let cache = serde_json::json!({
            "schema_version": 1,
            "providers": {
                "anthropic": null,
                "openai": {
                    "observed_at": now,
                    "private_plan": "private-plan",
                    "buckets": [{
                        "period": "weekly",
                        "label": "GPT-5 Codex",
                        "window": "secondary",
                        "used_percent": 42,
                        "resets_at": now + 3600
                    }]
                },
                "ollama": null
            }
        });
        std::fs::write(
            config.usage_state_dir.join(usage_api::CACHE_FILE),
            serde_json::to_vec(&cache).unwrap(),
        )
        .unwrap();

        let response = make_request(
            &config,
            "GET /api/usage HTTP/1.1\r\nHost: localhost\r\n\r\n",
        );
        std::fs::remove_dir_all(&workspace).unwrap();

        assert!(response.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(response.contains("\"openai\":{\"freshness\":\"fresh\""));
        assert!(response.contains("\"updated_at\":\""));
        assert!(response.contains("\"used_percent\":42"));
        assert!(response.contains("\"label\":\"GPT-5 Codex\""));
        assert!(response.contains("\"window\":\"secondary\""));
        assert!(!response.contains("\"scope\":\"model\""));
        assert!(!response.contains("private-plan"));
    }

    #[test]
    fn restart_control_is_authenticated_and_csrf_protected() {
        let workspace = temporary_workspace("restart");
        std::fs::create_dir(&workspace).unwrap();
        let (config, restart_rx) = test_config_with_restart(&workspace);

        let locked = render_lock("/", None);
        let unlocked = render_system(&config, None, None);
        assert!(!locked.contains("action=\"/restart\""));
        assert!(unlocked.contains("action=\"/restart\""));
        assert!(unlocked.contains("Confirm restart"));

        let unauthorized = make_request(
            &config,
            "POST /restart HTTP/1.1\r\nHost: localhost\r\nContent-Length: 9\r\n\r\ncsrf=csrf",
        );
        assert!(unauthorized.starts_with("HTTP/1.1 303 See Other"));
        assert!(restart_rx.try_recv().is_err());

        let bad_csrf = make_request(
            &config,
            "POST /restart HTTP/1.1\r\nHost: localhost\r\nCookie: t3_admin_session=session\r\nContent-Length: 8\r\n\r\ncsrf=bad",
        );
        assert!(bad_csrf.starts_with("HTTP/1.1 403 Forbidden"));
        assert!(restart_rx.try_recv().is_err());

        let get = make_request(
            &config,
            "GET /restart HTTP/1.1\r\nHost: localhost\r\nCookie: t3_admin_session=session\r\n\r\n",
        );
        assert!(get.starts_with("HTTP/1.1 403 Forbidden"));
        assert!(restart_rx.try_recv().is_err());

        let accepted = make_request(
            &config,
            "POST /restart HTTP/1.1\r\nHost: localhost\r\nCookie: t3_admin_session=session\r\nContent-Length: 9\r\n\r\ncsrf=csrf",
        );
        assert!(accepted.starts_with("HTTP/1.1 200 OK"));
        assert!(accepted.contains("Restart requested"));
        restart_rx.try_recv().unwrap();

        let repeated = make_request(
            &config,
            "POST /restart HTTP/1.1\r\nHost: localhost\r\nCookie: t3_admin_session=session\r\nContent-Length: 9\r\n\r\ncsrf=csrf",
        );
        assert!(repeated.contains("already in progress"));
        assert!(restart_rx.try_recv().is_err());

        std::fs::remove_dir(&workspace).unwrap();
    }

    #[test]
    fn sandbox_stop_command_has_a_fixed_target() {
        let command = sandbox_stop_command("sandbox-t3-123");
        assert_eq!(command.get_program(), "podman");
        assert_eq!(
            command
                .get_args()
                .map(|argument| argument.to_string_lossy().into_owned())
                .collect::<Vec<_>>(),
            ["stop", "--time=10", "sandbox-t3-123"]
        );
    }

    fn temporary_workspace(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "claude-sandbox-usage-api-{label}-{}-{}",
            process::id(),
            random_token(8)
        ))
    }

    fn test_config(workspace: &Path) -> Config {
        test_config_with_restart(workspace).0
    }

    fn test_config_with_restart(workspace: &Path) -> (Config, mpsc::Receiver<()>) {
        let (restart_tx, restart_rx) = mpsc::channel();
        let config = Config {
            portal_port: 0,
            t3_port: 0,
            container_name: "unused".to_string(),
            t3_base_dir: "unused".to_string(),
            workspace_root: workspace.to_path_buf(),
            state_dir: workspace.to_path_buf(),
            fetch_state_dir: workspace.join("fetch"),
            gh_state_dir: workspace.join("gh"),
            usage_state_dir: workspace.join("usage"),
            pin: "1234".to_string(),
            csrf_token: "csrf".to_string(),
            session_token: "session".to_string(),
            restart_tx,
            restart_queued: AtomicBool::new(false),
            gh_grant_lock: Mutex::new(()),
            failed_logins: Mutex::new(HashMap::new()),
            notices: Mutex::new(HashMap::new()),
            display_command: echo_display_command,
        };
        (config, restart_rx)
    }

    fn echo_display_command(_container_name: &str) -> Command {
        let mut command = Command::new("sh");
        command
            .args(["-c", "exec cat"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        command
    }

    fn make_request(config: &Config, request: &str) -> String {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let request = request.to_string();
        let client = thread::spawn(move || {
            let mut stream = TcpStream::connect(address).unwrap();
            stream.write_all(request.as_bytes()).unwrap();
            stream.shutdown(Shutdown::Write).unwrap();
            let mut response = String::new();
            stream.read_to_string(&mut response).unwrap();
            response
        });
        let (stream, _) = listener.accept().unwrap();
        handle_connection(stream, config);
        client.join().unwrap()
    }

    fn read_http_headers(stream: &mut TcpStream) -> String {
        let mut response = Vec::new();
        while !response.ends_with(b"\r\n\r\n") {
            assert!(response.len() < MAX_REQUEST_BYTES);
            let mut byte = [0];
            stream.read_exact(&mut byte).unwrap();
            response.push(byte[0]);
        }
        String::from_utf8(response).unwrap()
    }

    fn masked_client_frame(opcode: crate::websocket::Opcode, payload: &[u8]) -> Vec<u8> {
        assert!(payload.len() <= 125);
        let mask = [0x12, 0x34, 0x56, 0x78];
        let mut frame = vec![0x80 | opcode as u8, 0x80 | payload.len() as u8];
        frame.extend_from_slice(&mask);
        frame.extend(
            payload
                .iter()
                .enumerate()
                .map(|(index, byte)| byte ^ mask[index % mask.len()]),
        );
        frame
    }

    fn read_server_frame(stream: &mut TcpStream) -> (crate::websocket::Opcode, Vec<u8>) {
        let mut header = [0; 2];
        stream.read_exact(&mut header).unwrap();
        assert_eq!(header[0] & 0x80, 0x80);
        assert_eq!(header[1] & 0x80, 0, "server frames must be unmasked");
        let opcode = match header[0] & 0x0f {
            0 => crate::websocket::Opcode::Continuation,
            1 => crate::websocket::Opcode::Text,
            2 => crate::websocket::Opcode::Binary,
            8 => crate::websocket::Opcode::Close,
            9 => crate::websocket::Opcode::Ping,
            10 => crate::websocket::Opcode::Pong,
            value => panic!("unexpected opcode {value}"),
        };
        let length = match header[1] & 0x7f {
            126 => {
                let mut bytes = [0; 2];
                stream.read_exact(&mut bytes).unwrap();
                u16::from_be_bytes(bytes) as usize
            }
            127 => {
                let mut bytes = [0; 8];
                stream.read_exact(&mut bytes).unwrap();
                usize::try_from(u64::from_be_bytes(bytes)).unwrap()
            }
            length => length as usize,
        };
        let mut payload = vec![0; length];
        stream.read_exact(&mut payload).unwrap();
        (opcode, payload)
    }
}
