use base64::Engine;
use serde::Deserialize;
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};
use std::{process, thread};

use crate::managed_fetch;
use crate::managed_push::{self, ApprovalScope};
use crate::usage_api;
use crate::usage_collector;
use crate::usage_dashboard;

const MAX_REQUEST_BYTES: usize = 16_384;
const MAX_BODY_BYTES: usize = 8_192;
const MAX_LOGIN_ATTEMPTS: u32 = 5;
const LOGIN_LOCKOUT: Duration = Duration::from_secs(60);

struct Config {
    portal_port: u16,
    t3_port: u16,
    container_name: String,
    t3_base_dir: String,
    workspace_root: PathBuf,
    state_dir: PathBuf,
    fetch_state_dir: PathBuf,
    usage_state_dir: PathBuf,
    managed_push: bool,
    managed_fetch: bool,
    pin: String,
    csrf_token: String,
    session_token: String,
    restart_tx: mpsc::Sender<()>,
    restart_queued: AtomicBool,
    failed_logins: Mutex<HashMap<String, LoginFailures>>,
}

#[derive(Clone, Copy)]
struct LoginFailures {
    count: u32,
    blocked_until: Option<Instant>,
}

struct Request {
    method: String,
    path: String,
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
    pub usage_state_dir: &'a Path,
    pub managed_push: bool,
    pub managed_fetch: bool,
}

pub fn run(options: RunOptions<'_>) {
    let pin = std::env::var("T3CODE_PAIR_ADMIN_PIN").unwrap_or_default();
    if !valid_pin(&pin) {
        eprintln!("t3-admin: T3CODE_PAIR_ADMIN_PIN must contain 4 to 12 digits");
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
        usage_state_dir: options.usage_state_dir.to_path_buf(),
        managed_push: options.managed_push,
        managed_fetch: options.managed_fetch,
        pin,
        csrf_token: random_token(24),
        session_token: random_token(32),
        restart_tx,
        restart_queued: AtomicBool::new(false),
        failed_logins: Mutex::new(HashMap::new()),
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

    if request.method == "GET" && request.path == "/" {
        let authorized = is_authorized(&request, config);
        send_html(
            &mut stream,
            200,
            &render_page(config, !authorized, None, None),
            &[],
        );
        return;
    }

    if request.method == "POST" && request.path == "/login" {
        handle_login(&mut stream, &request, config);
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
                &render_page(config, false, None, Some(&error)),
                &[],
            );
            return;
        }
    };
    if !safe_equal(
        form.get("csrf").map(String::as_str).unwrap_or(""),
        &config.csrf_token,
    ) {
        send_html(
            &mut stream,
            403,
            &render_page(
                config,
                false,
                None,
                Some("The form expired. Reload and try again."),
            ),
            &[],
        );
        return;
    }

    if request.method == "POST" && request.path == "/restart" {
        let queued = !config.restart_queued.swap(true, Ordering::AcqRel);
        let notice = if queued {
            "Restart requested. The admin page will be unavailable briefly."
        } else {
            "A sandbox restart is already in progress."
        };
        send_html(
            &mut stream,
            200,
            &render_page(config, false, None, Some(notice)),
            &[],
        );
        let _ = stream.flush();
        if queued {
            let _ = config.restart_tx.send(());
        }
        return;
    }

    let result = match (request.method.as_str(), request.path.as_str()) {
        ("POST", "/pair") => create_pairing_link(config, request.headers.get("host"))
            .map(|url| format!("PAIR:{url}")),
        ("POST", "/approve") if config.managed_push => approve_candidate(config, &form),
        ("POST", "/dismiss") if config.managed_push => dismiss_candidate(config, &form),
        ("POST", "/revoke") if config.managed_push => revoke_approval(config, &form),
        ("POST", "/approve-fetch") if config.managed_fetch => {
            approve_fetch_candidate(config, &form)
        }
        ("POST", "/dismiss-fetch") if config.managed_fetch => {
            dismiss_fetch_candidate(config, &form)
        }
        ("POST", "/revoke-fetch") if config.managed_fetch => revoke_fetch_approval(config, &form),
        _ => {
            redirect(&mut stream, "/");
            return;
        }
    };

    match result {
        Ok(message) if message.starts_with("PAIR:") => send_html(
            &mut stream,
            200,
            &render_page(config, false, message.strip_prefix("PAIR:"), None),
            &[],
        ),
        Ok(message) => send_html(
            &mut stream,
            200,
            &render_page(config, false, None, Some(&message)),
            &[],
        ),
        Err(error) => send_html(
            &mut stream,
            400,
            &render_page(config, false, None, Some(&error)),
            &[],
        ),
    }
}

fn handle_login(stream: &mut TcpStream, request: &Request, config: &Config) {
    let address = request.peer.ip().to_string();
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
                    &render_page(
                        config,
                        true,
                        None,
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

    let form = parse_form(&request.body).unwrap_or_default();
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
            &render_page(config, true, None, Some("That PIN is not correct.")),
            &[],
        );
        return;
    }

    config.failed_logins.lock().unwrap().remove(&address);
    redirect_with_headers(
        stream,
        "/",
        &[(
            "Set-Cookie",
            format!(
                "t3_admin_session={}; HttpOnly; SameSite=Strict; Path=/",
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
        ApprovalScope::Once => format!("Approved the next push from {}.", current.relative_path),
        ApprovalScope::Persistent => {
            format!("Approved persistent pushes from {}.", current.relative_path)
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
        ApprovalScope::Once => format!("Approved one fetch from {}.", candidate.source.display()),
        ApprovalScope::Persistent => {
            format!(
                "Approved persistent fetches from {}.",
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

fn render_page(
    config: &Config,
    locked: bool,
    pair_url: Option<&str>,
    notice: Option<&str>,
) -> String {
    let action = if locked {
        r#"<section><h2>Unlock</h2><form method="post" action="/login"><label>Admin PIN<input name="pin" type="password" inputmode="numeric" pattern="[0-9]{4,12}" minlength="4" maxlength="12" required autofocus></label><button>Unlock</button></form></section>"#.to_string()
    } else {
        let mut sections = render_pairing_controls(&config.csrf_token, pair_url);
        if config.managed_push {
            sections.push_str(&render_push_controls(config));
        }
        if config.managed_fetch {
            sections.push_str(&render_fetch_controls(config));
        }
        sections.push_str(&render_restart_controls(&config.csrf_token));
        sections
    };
    let notice = notice
        .map(|message| format!(r#"<div class="notice">{}</div>"#, escape_html(message)))
        .unwrap_or_default();
    format!(
        r#"<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><meta name="color-scheme" content="dark"><title>T3 Code · Admin</title><style>
:root{{--ink:#f4f1e8;--muted:#a4a49b;--line:#353630;--acid:#d8ff4f;--danger:#ff8c7e;--bg:#11120f}}*{{box-sizing:border-box}}body{{margin:0;min-height:100vh;color:var(--ink);background:radial-gradient(circle at 85% 10%,#293315 0,transparent 30%),var(--bg);font:14px/1.55 "IBM Plex Mono","Courier New",monospace}}main{{width:min(920px,calc(100% - 32px));margin:auto;padding:48px 0 80px}}header{{border-top:1px solid var(--acid);padding-top:16px;margin-bottom:48px}}h1{{font:400 clamp(42px,8vw,76px)/.95 Georgia,serif;letter-spacing:-.05em;margin:18px 0}}h2{{font-size:13px;text-transform:uppercase;letter-spacing:.12em;color:var(--acid)}}section{{border:1px solid var(--line);padding:22px;margin:14px 0;background:#11120fd9}}p,small{{color:var(--muted)}}form{{display:inline-flex;gap:10px;align-items:end;margin:5px 8px 5px 0}}label{{display:grid;gap:7px}}input{{background:#090a08;color:var(--ink);border:1px solid var(--line);padding:12px}}button,.pair{{display:inline-block;border:0;background:var(--acid);color:#15170d;padding:12px 15px;font:700 11px/1 monospace;text-transform:uppercase;text-decoration:none;cursor:pointer}}button.secondary{{background:#34362e;color:var(--ink)}}button.danger{{background:#713a34;color:#fff}}.usage{{color:var(--ink);text-underline-offset:4px}}.usage:hover{{color:var(--acid)}}.pair-result{{border-top:1px solid var(--line);margin-top:17px;padding-top:17px}}.pair-url{{width:100%;margin:5px 0 12px}}.repo{{padding:16px 0;border-top:1px solid var(--line)}}.repo:first-of-type{{border-top:0}}code{{color:var(--ink);overflow-wrap:anywhere}}.meta{{display:grid;grid-template-columns:100px 1fr;gap:5px 12px}}.notice{{border:1px solid #6f752f;padding:13px;margin-bottom:14px}}.changed{{color:var(--danger)}}footer{{margin-top:42px;color:#67685f;font-size:10px;text-transform:uppercase;letter-spacing:.12em}}@media(max-width:600px){{.meta{{grid-template-columns:1fr}}form{{display:flex;flex-wrap:wrap}}}}
</style></head><body><main><header><small>Private control plane · {}</small><h1>T3 Code<br>Admin.</h1><a class="usage" href="/usage">Public usage dashboard ↗</a></header>{notice}{action}<footer>Host-owned administration surface</footer></main></body></html>"#,
        config.portal_port
    )
}

fn render_pairing_controls(csrf_token: &str, pair_url: Option<&str>) -> String {
    let result = pair_url
        .map(|url| {
            let url = escape_html(url);
            format!(
                r#"<div class="pair-result"><label>Pair another client<input class="pair-url" type="url" value="{url}" readonly></label><small>Copy this link to the client you want to pair. Creating or copying it does not pair this browser. The link must use an address that client can reach.</small><p><a class="pair" href="{url}">Pair this browser ↗</a></p></div>"#
            )
        })
        .unwrap_or_default();
    format!(
        r#"<section><h2>Client pairing</h2><p>Create a five-minute, single-use client credential.</p><form method="post" action="/pair"><input type="hidden" name="csrf" value="{}"><button>Create pairing link</button></form>{result}</section>"#,
        escape_html(csrf_token)
    )
}

fn render_restart_controls(csrf_token: &str) -> String {
    format!(
        r#"<section><h2>Sandbox lifecycle</h2><p>Stop the current T3 container. When this launcher is supervised with a restart policy, the service starts a fresh sandbox.</p><details><summary>Restart sandbox</summary><form method="post" action="/restart"><input type="hidden" name="csrf" value="{}"><button class="danger">Confirm restart</button></form></details></section>"#,
        escape_html(csrf_token)
    )
}

fn render_push_controls(config: &Config) -> String {
    let candidates = managed_push::list_candidates(&config.state_dir).unwrap_or_default();
    let approvals = managed_push::list_approvals(&config.state_dir).unwrap_or_default();
    let mut html = String::from("<section><h2>Pending push approvals</h2>");
    if candidates.is_empty() {
        html.push_str(
            "<p>No repositories are waiting. A denied push will appear here automatically.</p>",
        );
    }
    for (id, candidate) in candidates.into_iter().take(100) {
        let repository = candidate.repository;
        let changed = candidate
            .previously_approved_origin
            .as_ref()
            .map(|previous| {
                format!(
                    "<div class=\"changed\">Previously approved: <code>{}</code></div>",
                    escape_html(previous)
                )
            })
            .unwrap_or_default();
        html.push_str(&format!(
            r#"<div class="repo"><div class="meta"><span>Repository</span><strong>{}</strong><span>Origin</span><code>{}</code><span>Branch</span><code>{}</code></div>{}<form method="post" action="/approve"><input type="hidden" name="csrf" value="{}"><input type="hidden" name="id" value="{}"><button name="scope" value="once">Approve once</button><button name="scope" value="persistent">Always allow</button></form><form method="post" action="/dismiss"><input type="hidden" name="csrf" value="{}"><input type="hidden" name="id" value="{}"><button class="secondary">Dismiss</button></form></div>"#,
            escape_html(&repository.relative_path), escape_html(&repository.origin), escape_html(repository.branch.as_deref().unwrap_or("detached HEAD")), changed, config.csrf_token, id, config.csrf_token, id
        ));
    }
    html.push_str("</section><section><h2>Approved repositories</h2>");
    if approvals.is_empty() {
        html.push_str("<p>No repositories are approved.</p>");
    }
    for (id, approval) in approvals {
        let scope = match approval.scope {
            ApprovalScope::Once => "next push only",
            ApprovalScope::Persistent => "persistent",
        };
        let current_origin = managed_push::resolve_relative_repository(
            &config.workspace_root,
            &approval.repository.relative_path,
        )
        .ok()
        .map(|(_, repository)| repository.origin);
        let status = match current_origin {
            Some(current) if current == approval.repository.origin => scope.to_string(),
            Some(current) => format!("suspended — origin is now {}", escape_html(&current)),
            None => "suspended — repository unavailable".to_string(),
        };
        html.push_str(&format!(
            r#"<div class="repo"><div class="meta"><span>Repository</span><strong>{}</strong><span>Origin</span><code>{}</code><span>Status</span><span>{}</span></div><form method="post" action="/revoke"><input type="hidden" name="csrf" value="{}"><input type="hidden" name="id" value="{}"><button class="danger">Revoke</button></form></div>"#,
            escape_html(&approval.repository.relative_path), escape_html(&approval.repository.origin), status, config.csrf_token, id
        ));
    }
    html.push_str("</section>");
    html
}

fn render_fetch_controls(config: &Config) -> String {
    let candidates = managed_fetch::list_candidates(&config.fetch_state_dir).unwrap_or_default();
    let approvals = managed_fetch::list_approvals(&config.fetch_state_dir).unwrap_or_default();
    let mut html = String::from("<section><h2>Pending fetch approvals</h2>");
    html.push_str("<p>Approval grants this sandbox read access to the exact SSH repository. The requesting checkout is shown for context and is not an isolation boundary inside a shared container.</p>");
    if candidates.is_empty() {
        html.push_str(
            "<p>No repositories are waiting. A denied authenticated SSH fetch will appear here automatically.</p>",
        );
    }
    for (id, candidate) in candidates {
        html.push_str(&format!(
            r#"<div class="repo"><div class="meta"><span>Source</span><code>{}</code><span>Requested from</span><strong>{}</strong></div><form method="post" action="/approve-fetch"><input type="hidden" name="csrf" value="{}"><input type="hidden" name="id" value="{}"><button name="scope" value="once">Approve once</button><button name="scope" value="persistent">Always allow</button></form><form method="post" action="/dismiss-fetch"><input type="hidden" name="csrf" value="{}"><input type="hidden" name="id" value="{}"><button class="secondary">Dismiss</button></form></div>"#,
            escape_html(&candidate.source.display()),
            escape_html(&candidate.requested_from),
            config.csrf_token,
            id,
            config.csrf_token,
            id
        ));
    }
    html.push_str("</section><section><h2>Approved fetch sources</h2>");
    if approvals.is_empty() {
        html.push_str("<p>No private SSH repositories are approved for reading.</p>");
    }
    for (id, approval) in approvals {
        let scope = match approval.scope {
            ApprovalScope::Once => "next fetch only",
            ApprovalScope::Persistent => "persistent",
        };
        html.push_str(&format!(
            r#"<div class="repo"><div class="meta"><span>Source</span><code>{}</code><span>Status</span><span>{}</span><span>Capability</span><span>read only</span></div><form method="post" action="/revoke-fetch"><input type="hidden" name="csrf" value="{}"><input type="hidden" name="id" value="{}"><button class="danger">Revoke</button></form></div>"#,
            escape_html(&approval.source.display()),
            scope,
            config.csrf_token,
            id
        ));
    }
    html.push_str("</section>");
    html
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
    Ok(Request {
        method,
        path: path.split('?').next().unwrap_or(&path).to_string(),
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
        429 => "Too Many Requests",
        _ => "Error",
    };
    let mut headers = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\nContent-Security-Policy: default-src 'none'; style-src 'unsafe-inline'; form-action 'self'; base-uri 'none'; frame-ancestors 'none'\r\nReferrer-Policy: no-referrer\r\nX-Content-Type-Options: nosniff\r\nConnection: close\r\n",
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
    fn manages_fetch_approvals_independently_from_pushes() {
        let workspace = temporary_workspace("fetch-approval");
        std::fs::create_dir_all(&workspace).unwrap();
        let mut config = test_config(&workspace);
        config.fetch_state_dir = managed_fetch::prepare_state_dir(&config.fetch_state_dir).unwrap();
        config.managed_fetch = true;
        let source = managed_fetch::Source {
            host: "github.com".to_string(),
            repository: "org/private.git".to_string(),
        };
        let id = managed_fetch::record_candidate(&config.fetch_state_dir, &source, "services/api")
            .unwrap();

        let html = render_page(&config, false, None, None);
        assert!(html.contains("Pending fetch approvals"));
        assert!(html.contains("git@github.com:org/private.git"));
        assert!(html.contains("services/api"));
        assert!(!html.contains("Pending push approvals"));

        let form = HashMap::from([
            ("id".to_string(), id),
            ("scope".to_string(), "persistent".to_string()),
        ]);
        approve_fetch_candidate(&config, &form).unwrap();
        let html = render_page(&config, false, None, None);
        assert!(html.contains("Capability"));
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

        let locked = render_page(&config, true, None, None);
        let unlocked = render_page(&config, false, None, None);
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
            usage_state_dir: workspace.join("usage"),
            managed_push: false,
            managed_fetch: false,
            pin: "1234".to_string(),
            csrf_token: "csrf".to_string(),
            session_token: "session".to_string(),
            restart_tx,
            restart_queued: AtomicBool::new(false),
            failed_logins: Mutex::new(HashMap::new()),
        };
        (config, restart_rx)
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
}
