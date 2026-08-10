use base64::Engine;
use serde::Deserialize;
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use std::{process, thread};

use crate::managed_push::{self, ApprovalScope};

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
    managed_push: bool,
    pin: String,
    csrf_token: String,
    session_token: String,
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
    pub managed_push: bool,
}

pub fn run(options: RunOptions<'_>) {
    let pin = std::env::var("T3CODE_PAIR_ADMIN_PIN").unwrap_or_default();
    if !valid_pin(&pin) {
        eprintln!("t3-admin: T3CODE_PAIR_ADMIN_PIN must contain 4 to 12 digits");
        process::exit(2);
    }
    let config = Arc::new(Config {
        portal_port: options.portal_port,
        t3_port: options.t3_port,
        container_name: options.container_name.to_string(),
        t3_base_dir: options.t3_base_dir.to_string(),
        workspace_root: options.workspace_root.to_path_buf(),
        state_dir: options.state_dir.to_path_buf(),
        managed_push: options.managed_push,
        pin,
        csrf_token: random_token(24),
        session_token: random_token(32),
        failed_logins: Mutex::new(HashMap::new()),
    });

    let listener = TcpListener::bind(("0.0.0.0", options.portal_port)).unwrap_or_else(|error| {
        eprintln!(
            "t3-admin: failed to bind port {}: {}",
            options.portal_port, error
        );
        process::exit(1);
    });

    let parent_pid = std::os::unix::process::parent_id();
    thread::spawn(move || {
        loop {
            thread::sleep(Duration::from_secs(2));
            if std::os::unix::process::parent_id() != parent_pid {
                process::exit(0);
            }
        }
    });

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let config = Arc::clone(&config);
                thread::spawn(move || handle_connection(stream, &config));
            }
            Err(error) => eprintln!("t3-admin: connection error: {error}"),
        }
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

    let result = match (request.method.as_str(), request.path.as_str()) {
        ("POST", "/pair") => create_pairing_link(config, request.headers.get("host"))
            .map(|url| format!("PAIR:{url}")),
        ("POST", "/approve") if config.managed_push => approve_candidate(config, &form),
        ("POST", "/dismiss") if config.managed_push => dismiss_candidate(config, &form),
        ("POST", "/revoke") if config.managed_push => revoke_approval(config, &form),
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
        let mut sections = format!(
            r#"<section><h2>Browser pairing</h2><p>Create a five-minute, single-use browser credential.</p><form method="post" action="/pair"><input type="hidden" name="csrf" value="{}"><button>Create pairing link</button></form>{}</section>"#,
            config.csrf_token,
            pair_url
                .map(|url| format!(
                    r#"<a class="pair" href="{}">Pair this browser ↗</a>"#,
                    escape_html(url)
                ))
                .unwrap_or_default()
        );
        if config.managed_push {
            sections.push_str(&render_push_controls(config));
        }
        sections
    };
    let notice = notice
        .map(|message| format!(r#"<div class="notice">{}</div>"#, escape_html(message)))
        .unwrap_or_default();
    format!(
        r#"<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><meta name="color-scheme" content="dark"><title>T3 Code · Admin</title><style>
:root{{--ink:#f4f1e8;--muted:#a4a49b;--line:#353630;--acid:#d8ff4f;--danger:#ff8c7e;--bg:#11120f}}*{{box-sizing:border-box}}body{{margin:0;min-height:100vh;color:var(--ink);background:radial-gradient(circle at 85% 10%,#293315 0,transparent 30%),var(--bg);font:14px/1.55 "IBM Plex Mono","Courier New",monospace}}main{{width:min(920px,calc(100% - 32px));margin:auto;padding:48px 0 80px}}header{{border-top:1px solid var(--acid);padding-top:16px;margin-bottom:48px}}h1{{font:400 clamp(42px,8vw,76px)/.95 Georgia,serif;letter-spacing:-.05em;margin:18px 0}}h2{{font-size:13px;text-transform:uppercase;letter-spacing:.12em;color:var(--acid)}}section{{border:1px solid var(--line);padding:22px;margin:14px 0;background:#11120fd9}}p,small{{color:var(--muted)}}form{{display:inline-flex;gap:10px;align-items:end;margin:5px 8px 5px 0}}label{{display:grid;gap:7px}}input{{background:#090a08;color:var(--ink);border:1px solid var(--line);padding:12px}}button,.pair{{display:inline-block;border:0;background:var(--acid);color:#15170d;padding:12px 15px;font:700 11px/1 monospace;text-transform:uppercase;text-decoration:none;cursor:pointer}}button.secondary{{background:#34362e;color:var(--ink)}}button.danger{{background:#713a34;color:#fff}}.repo{{padding:16px 0;border-top:1px solid var(--line)}}.repo:first-of-type{{border-top:0}}code{{color:var(--ink);overflow-wrap:anywhere}}.meta{{display:grid;grid-template-columns:100px 1fr;gap:5px 12px}}.notice{{border:1px solid #6f752f;padding:13px;margin-bottom:14px}}.changed{{color:var(--danger)}}footer{{margin-top:42px;color:#67685f;font-size:10px;text-transform:uppercase;letter-spacing:.12em}}@media(max-width:600px){{.meta{{grid-template-columns:1fr}}form{{display:flex;flex-wrap:wrap}}}}
</style></head><body><main><header><small>Private control plane · {}</small><h1>T3 Code<br>Admin.</h1></header>{notice}{action}<footer>Host-owned administration surface</footer></main></body></html>"#,
        config.portal_port
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
}
