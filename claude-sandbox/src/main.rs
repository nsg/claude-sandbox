mod extension;
mod gh_proxy;
mod git_proxy;
mod incus_console;
mod logging;
mod managed_fetch;
mod managed_gh;
mod managed_push;
mod novnc_assets;
mod proxy_log;
mod proxy_socket;
mod service;
mod ssh_proxy;
mod t3_admin;
mod usage_api;
mod usage_collector;
mod usage_dashboard;
mod vm_proxy;
mod websocket;

use clap::{Parser, Subcommand};
use dialoguer::Confirm;
use flate2::read::GzDecoder;
use reqwest::blocking::Client;
use std::env;
use std::fs::{self, DirBuilder, File, Permissions};
use std::hash::{Hash, Hasher};
use std::io::{IsTerminal, Write};
use std::net::TcpListener;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tar::Archive;

const SCRIPT_URL: &str =
    "https://github.com/nsg/claude-sandbox/releases/latest/download/claude-sandbox";
const SKILLS_URL: &str =
    "https://github.com/nsg/claude-sandbox/releases/latest/download/skills.tar.gz";
const IMAGE: &str = "ghcr.io/nsg/claude-sandbox:latest";
const IMAGE_SOURCE_LABEL: &str =
    "org.opencontainers.image.source=https://github.com/nsg/claude-sandbox";
const GH_PROXY_SOCKET_NAME: &str = "gh-proxy.sock";
const GIT_PROXY_SOCKET_NAME: &str = "git-proxy.sock";
const VM_PROXY_SOCKET_NAME: &str = "vm-proxy.sock";
const SSH_PROXY_SOCKET_NAME: &str = "ssh-proxy.sock";
const CONTAINER_PROXY_RUNTIME_DIR: &str = "/run/claude-sandbox";
// Must match the default session name in config/wrap.sh.
const WRAP_TMUX_SESSION: &str = "claude-sandbox";

#[derive(Parser)]
#[command(name = "claude-sandbox")]
#[command(about = "Run AI coding agents in a sandboxed container")]
#[command(after_help = "Run a one-off command with: claude-sandbox run <command>")]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    /// Expose port(s) from container (can be repeated, e.g., -p 8080 -p 3000)
    #[arg(short = 'p', long = "port", action = clap::ArgAction::Append)]
    ports: Vec<u16>,

    /// Automatically update without prompting
    #[arg(long)]
    auto_update: bool,

    /// Install this invocation as a systemd user service instead of running it
    #[arg(
        long,
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "",
        value_name = "NAME"
    )]
    install_service: Option<String>,

    /// Suppress informational output, only show errors
    #[arg(short, long)]
    quiet: bool,

    /// Allow `git push` / `git push --tags` for the launch repository (run from its root), executed on the host
    #[arg(long = "allow-push")]
    allow_push: bool,

    /// Allow the agent to merge same-repository pull requests with `gh pr merge`
    #[arg(long = "allow-merge")]
    allow_merge: bool,

    /// Do not append the agent disclosure to issues, pull requests and comments written through `gh`
    #[arg(long = "no-agent-disclosure")]
    no_agent_disclosure: bool,

    /// Replace the agent disclosure appended to issues, pull requests and comments written through `gh`
    #[arg(
        long = "agent-disclosure-text",
        value_name = "TEXT",
        conflicts_with = "no_agent_disclosure"
    )]
    agent_disclosure_text: Option<String>,

    /// Allow the agent to create and drive virtual machines through Incus on the host (see "Virtual Machine Bridge")
    #[arg(long)]
    vm: bool,

    /// Start the admin portal (t3code only; needs CLAUDE_SANDBOX_PORTAL_PIN) and let it approve pushes, fetches and cross-repository GitHub actions
    #[arg(long)]
    portal: bool,

    /// Port for the t3code web GUI (t3code only; default: first free port from 3773)
    #[arg(long, value_name = "PORT", value_parser = clap::value_parser!(u16).range(1..))]
    t3_port: Option<u16>,

    /// Port for the admin portal (needs --portal; default: first free port from 3774)
    #[arg(long, value_name = "PORT", value_parser = clap::value_parser!(u16).range(1..))]
    portal_port: Option<u16>,

    /// Run the command in a named tmux session so keys can be injected
    #[arg(long, global = true)]
    wrap: bool,
}

#[derive(Subcommand)]
enum Commands {
    /// Install components
    Install {
        /// Component to install: "skills" or "extension" (GNOME usage indicator)
        target: String,
        /// Host the extension reads usage from, as HOST or HOST:PORT (default: this machine)
        #[arg(long, value_name = "HOST[:PORT]")]
        host: Option<String>,
    },
    /// Start the gh CLI proxy (internal, spawned automatically)
    GhProxy {
        /// Socket path (absolute)
        #[arg(long)]
        socket: String,
        /// Persistent log path
        #[arg(long)]
        log: PathBuf,
        /// Host workspace root used to resolve container working directories
        #[arg(long)]
        workspace_root: PathBuf,
        /// Permit `gh pr merge` in launch-approved repositories
        #[arg(long)]
        allow_merge: bool,
        /// Text appended to issues, pull requests and comments (omit to disable)
        #[arg(long)]
        agent_disclosure: Option<String>,
        /// Host-only portal GitHub action state directory
        #[arg(long)]
        portal_state_dir: Option<PathBuf>,
    },
    /// Start the git push proxy (internal, spawned automatically)
    GitProxy {
        /// Socket path (absolute)
        #[arg(long)]
        socket: String,
        /// Persistent log path
        #[arg(long)]
        log: PathBuf,
        /// Host workspace root
        #[arg(long)]
        workspace_root: PathBuf,
        /// Launch repository origin URL snapshotted at launch (--allow-push)
        #[arg(long)]
        pre_approved_origin: Option<String>,
        /// Host-only portal push approval state directory
        #[arg(long)]
        state_dir: Option<PathBuf>,
    },
    /// Start the host-side admin portal (internal)
    T3Admin {
        #[arg(long)]
        port: u16,
        #[arg(long)]
        t3_port: u16,
        #[arg(long)]
        container_name: String,
        #[arg(long)]
        t3_base_dir: String,
        #[arg(long)]
        workspace_root: PathBuf,
        #[arg(long)]
        state_dir: PathBuf,
        #[arg(long)]
        fetch_state_dir: PathBuf,
        #[arg(long)]
        gh_state_dir: PathBuf,
        #[arg(long)]
        usage_state_dir: PathBuf,
    },
    /// Start the virtual machine proxy (internal, spawned automatically)
    VmProxy {
        /// Socket path (absolute)
        #[arg(long)]
        socket: String,
        /// Persistent log path
        #[arg(long)]
        log: PathBuf,
        /// Host workspace root used to resolve container paths
        #[arg(long)]
        workspace_root: PathBuf,
        /// Host proxy runtime directory used for SPICE relay sockets
        #[arg(long)]
        runtime_dir: PathBuf,
        /// Launcher process to watch for shutdown
        #[arg(long)]
        parent_pid: u32,
    },
    /// Start the SSH proxy (internal, spawned automatically)
    SshProxy {
        /// Socket path (absolute)
        #[arg(long)]
        socket: String,
        /// Persistent log path
        #[arg(long)]
        log: PathBuf,
        /// Launch repository origin URL snapshotted at launch
        #[arg(long)]
        launch_origin: Option<String>,
        /// Host workspace root used to validate portal fetch request context
        #[arg(long)]
        workspace_root: Option<PathBuf>,
        /// Host-only portal fetch approval state
        #[arg(long)]
        state_dir: Option<PathBuf>,
    },
    /// Run a command inside the container
    Run {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        command: Vec<String>,
    },
    /// Run the t3code web GUI in the container (port from --t3-port or the first free one)
    T3code {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Type text into a wrapped terminal
    WrapType {
        /// Target session name (needed when several sessions are running)
        #[arg(long)]
        session: Option<String>,
        /// Press Enter after typing the text
        #[arg(long)]
        enter: bool,
        /// Minimum delay between typed characters in milliseconds
        #[arg(long, default_value_t = 25)]
        delay_min_ms: u64,
        /// Maximum delay between typed characters in milliseconds
        #[arg(long, default_value_t = 120)]
        delay_max_ms: u64,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        text: Vec<String>,
    },
    /// Press a key in a wrapped terminal
    WrapKey {
        /// Target session name (needed when several sessions are running)
        #[arg(long)]
        session: Option<String>,
        /// tmux key name, for example Enter, Escape, BSpace, C-c
        key: String,
    },
    /// Print the screen contents of a wrapped terminal
    WrapRead {
        /// Target session name (needed when several sessions are running)
        #[arg(long)]
        session: Option<String>,
        /// Number of scrollback lines to include above the visible screen
        #[arg(long)]
        lines: Option<u32>,
    },
    /// List running wrapped terminal sessions
    WrapList,
}

const T3CODE_PORT: u16 = 3773;
const PORTAL_PORT: u16 = 3774;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PortDecision {
    Auto,
    Exact(u16),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct T3PortDecisions {
    t3: PortDecision,
    admin: Option<PortDecision>,
}

fn decide_t3_ports(
    t3_port: Option<u16>,
    portal_port: Option<u16>,
    portal: bool,
    published_ports: &[u16],
) -> Result<T3PortDecisions, String> {
    let exact = |port: Option<u16>| port.map_or(PortDecision::Auto, PortDecision::Exact);
    let t3 = exact(t3_port);
    let admin = portal.then(|| exact(portal_port));

    if let Some(PortDecision::Exact(admin_port)) = admin {
        if t3 == PortDecision::Exact(admin_port) {
            return Err(format!(
                "--portal-port {admin_port} conflicts with --t3-port"
            ));
        }
        if published_ports.contains(&admin_port) {
            return Err(format!(
                "--portal-port {admin_port} conflicts with published port {admin_port}"
            ));
        }
    }
    Ok(T3PortDecisions { t3, admin })
}

// Podman publishes on every address, so a pinned port has to be free on all.
fn fixed_port_is_free(name: &str, port: u16) -> Result<u16, String> {
    TcpListener::bind(("0.0.0.0", port))
        .map(|_| port)
        .map_err(|error| match error.kind() {
            std::io::ErrorKind::AddrInUse => format!("{name} {port} is already in use"),
            _ => format!("{name} {port} cannot be used: {error}"),
        })
}

fn resolve_t3_ports(
    decisions: T3PortDecisions,
    published_ports: &[u16],
) -> Result<(u16, Option<u16>), String> {
    let explicit_admin = match decisions.admin {
        Some(PortDecision::Exact(port)) => Some(port),
        _ => None,
    };
    let t3 = match decisions.t3 {
        PortDecision::Auto => {
            find_free_port_avoiding(T3CODE_PORT, &explicit_admin.into_iter().collect::<Vec<_>>())
        }
        PortDecision::Exact(port) => fixed_port_is_free("--t3-port", port)?,
    };
    let admin = match decisions.admin {
        None => None,
        Some(PortDecision::Auto) => {
            let mut excluded = published_ports.to_vec();
            excluded.push(t3);
            Some(find_free_port_avoiding(PORTAL_PORT, &excluded))
        }
        Some(PortDecision::Exact(port)) => Some(fixed_port_is_free("--portal-port", port)?),
    };
    Ok((t3, admin))
}

/// Derive a stable, filesystem-safe identifier from a project path.
/// Returns `"name-abcd1234"` where `name` is the directory basename
/// (sanitised) and the suffix is a short hash of the full path to
/// disambiguate projects with the same name in different locations.
fn sanitised_project_basename(path: &Path) -> String {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("default");

    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn project_instance_name(path: &Path) -> String {
    let sanitised = sanitised_project_basename(path);

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    path.hash(&mut hasher);
    let hash = hasher.finish();

    format!("{}-{:08x}", sanitised, hash as u32)
}

fn wrap_container_name(path: &Path) -> String {
    format!("claude-sandbox-{}", project_instance_name(path))
}

fn shell_quote(value: &str) -> String {
    if !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./:=@%+".contains(c))
    {
        return value.to_string();
    }
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn shell_command(args: &[&str]) -> String {
    args.iter()
        .map(|arg| shell_quote(arg))
        .collect::<Vec<_>>()
        .join(" ")
}

fn run_command_string(command: &[String]) -> String {
    match command {
        [command] => command.clone(),
        _ => shell_command(&command.iter().map(String::as_str).collect::<Vec<_>>()),
    }
}

/// Forward a wrap command to its in-container implementation
/// (/usr/local/bin/wrap and friends, from config/wrap.sh).
fn run_in_container(container_name: &str, args: &[&str]) {
    let status = Command::new("podman")
        .args(["exec", container_name])
        .args(args)
        .status()
        .unwrap_or_else(|e| {
            eprintln!("Error: failed to run podman exec: {}", e);
            std::process::exit(1);
        });

    if !status.success() {
        // podman exec exits 125 when it cannot reach the container; other
        // codes come from the wrap script, which prints its own error.
        if status.code() == Some(125) {
            eprintln!(
                "Error: could not reach the wrapped session. Start one first, for example: claude-sandbox --wrap"
            );
        }
        std::process::exit(status.code().unwrap_or(1));
    }
}

fn write_wrap_type(
    text: &[String],
    session: Option<&str>,
    enter: bool,
    delay_min_ms: u64,
    delay_max_ms: u64,
) {
    let cwd = env::current_dir().expect("Could not get current directory");
    let container_name = wrap_container_name(&cwd);
    let delay_min = delay_min_ms.to_string();
    let delay_max = delay_max_ms.to_string();
    let mut args = vec![
        "wrap-type",
        "--delay-min-ms",
        &delay_min,
        "--delay-max-ms",
        &delay_max,
    ];
    if let Some(name) = session {
        args.extend(["--session", name]);
    }
    if enter {
        args.push("--enter");
    }
    args.push("--");
    args.extend(text.iter().map(String::as_str));
    run_in_container(&container_name, &args);
}

fn write_wrap_key(key: &str, session: Option<&str>) {
    let cwd = env::current_dir().expect("Could not get current directory");
    let container_name = wrap_container_name(&cwd);
    let mut args = vec!["wrap-key"];
    if let Some(name) = session {
        args.extend(["--session", name]);
    }
    args.push(key);
    run_in_container(&container_name, &args);
}

fn print_wrap_screen(lines: Option<u32>, session: Option<&str>) {
    let cwd = env::current_dir().expect("Could not get current directory");
    let container_name = wrap_container_name(&cwd);
    let mut args = vec!["wrap-read"];
    if let Some(name) = session {
        args.extend(["--session", name]);
    }
    let lines_arg;
    if let Some(n) = lines {
        lines_arg = n.to_string();
        args.extend(["--lines", lines_arg.as_str()]);
    }
    run_in_container(&container_name, &args);
}

fn print_wrap_sessions() {
    let cwd = env::current_dir().expect("Could not get current directory");
    let container_name = wrap_container_name(&cwd);
    run_in_container(&container_name, &["wrap", "--list"]);
}

fn find_free_port_avoiding(preferred: u16, excluded: &[u16]) -> u16 {
    for port in preferred..=preferred.saturating_add(100) {
        if !excluded.contains(&port) && TcpListener::bind(("127.0.0.1", port)).is_ok() {
            return port;
        }
    }
    loop {
        let port = TcpListener::bind(("127.0.0.1", 0))
            .expect("Failed to find a free port")
            .local_addr()
            .expect("Failed to get local address")
            .port();
        if !excluded.contains(&port) {
            return port;
        }
    }
}

struct T3AdminConfig<'a> {
    portal_port: u16,
    t3_port: u16,
    container_name: &'a str,
    t3_base_dir: &'a str,
    workspace_root: &'a Path,
    state_dir: &'a Path,
    fetch_state_dir: &'a Path,
    gh_state_dir: &'a Path,
    usage_state_dir: &'a Path,
}

fn ensure_t3_admin(config: &T3AdminConfig<'_>) {
    let exe = env::current_exe().expect("Could not get executable path");
    let mut command = Command::new(exe);
    command
        .arg("t3-admin")
        .arg("--port")
        .arg(config.portal_port.to_string())
        .arg("--t3-port")
        .arg(config.t3_port.to_string())
        .arg("--container-name")
        .arg(config.container_name)
        .arg("--t3-base-dir")
        .arg(config.t3_base_dir)
        .arg("--workspace-root")
        .arg(config.workspace_root)
        .arg("--state-dir")
        .arg(config.state_dir)
        .arg("--fetch-state-dir")
        .arg(config.fetch_state_dir)
        .arg("--gh-state-dir")
        .arg(config.gh_state_dir)
        .arg("--usage-state-dir")
        .arg(config.usage_state_dir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    if let Err(error) = command.spawn() {
        eprintln!("Warning: failed to start the admin portal: {error}");
        return;
    }
    for _ in 0..30 {
        thread::sleep(Duration::from_millis(100));
        if std::net::TcpStream::connect(("127.0.0.1", config.portal_port)).is_ok() {
            return;
        }
    }
    eprintln!("Warning: the admin portal did not start in time");
}

const PORTAL_PIN_VARIABLE: &str = "CLAUDE_SANDBOX_PORTAL_PIN";

fn is_valid_portal_pin(pin: &str) -> bool {
    (4..=12).contains(&pin.len()) && pin.chars().all(|character| character.is_ascii_digit())
}

/// `--portal` is a t3code switch that needs the PIN; a PIN without the
/// switch is refused so a forgotten flag never runs without approvals.
fn validate_portal(portal: bool, t3code: bool, pin: Option<&str>) -> Result<(), String> {
    if portal && !t3code {
        return Err("--portal can only be used with the t3code command".to_string());
    }
    if !t3code {
        return Ok(());
    }
    match (portal, pin) {
        (true, None) => Err(format!(
            "--portal requires {PORTAL_PIN_VARIABLE} (4 to 12 digits) to protect the admin portal"
        )),
        (true, Some(pin)) if !is_valid_portal_pin(pin) => {
            Err(format!("{PORTAL_PIN_VARIABLE} must contain 4 to 12 digits"))
        }
        (false, Some(_)) => Err(format!(
            "{PORTAL_PIN_VARIABLE} is set but --portal was not given"
        )),
        _ => Ok(()),
    }
}

fn validate_port_flags(cli: &Cli, t3code: bool) -> Result<(), String> {
    if cli.t3_port.is_some() && !t3code {
        return Err("--t3-port can only be used with the t3code command".to_string());
    }
    if cli.portal_port.is_some() && !cli.portal {
        return Err("--portal-port needs --portal".to_string());
    }
    Ok(())
}

/// The portal PIN matters only to t3code; other commands ignore it entirely.
/// A present but empty value still counts as set.
fn portal_pin(value: Option<std::ffi::OsString>, t3code: bool) -> Result<Option<String>, String> {
    if !t3code {
        return Ok(None);
    }
    value
        .map(|value| {
            value
                .into_string()
                .map_err(|_| format!("{PORTAL_PIN_VARIABLE} is not valid UTF-8"))
        })
        .transpose()
}

fn t3_port_decisions(cli: &Cli) -> Result<T3PortDecisions, String> {
    decide_t3_ports(cli.t3_port, cli.portal_port, cli.portal, &cli.ports)
}

fn invoked_program() -> std::ffi::OsString {
    env::args_os()
        .next()
        .unwrap_or_else(|| env::current_exe().unwrap_or_default().into_os_string())
}

fn home_dir() -> PathBuf {
    PathBuf::from(env::var("HOME").expect("HOME environment variable not set"))
}

fn usage_state_dir() -> PathBuf {
    env::var_os("XDG_STATE_HOME")
        .filter(|value| !value.is_empty() && Path::new(value).is_absolute())
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".local/state"))
        .join("claude-sandbox/usage")
}

fn prepare_usage_state_dir(path: &Path, workspace: &Path) -> Result<PathBuf, String> {
    if path.starts_with(workspace) {
        return Err("usage state must be outside the agent-mounted workspace".to_string());
    }
    let canonical_workspace = fs::canonicalize(workspace)
        .map_err(|error| format!("could not resolve T3 workspace: {error}"))?;
    let mut existing = path;
    while !existing.exists() {
        existing = existing
            .parent()
            .ok_or_else(|| "usage state has no existing ancestor".to_string())?;
    }
    let unresolved = path
        .strip_prefix(existing)
        .map_err(|_| "could not resolve usage state path".to_string())?;
    let resolved_candidate = fs::canonicalize(existing)
        .map_err(|error| format!("could not resolve usage state parent: {error}"))?
        .join(unresolved);
    if resolved_candidate.starts_with(&canonical_workspace) {
        return Err("usage state must be outside the agent-mounted workspace".to_string());
    }
    let canonical_state = usage_api::prepare_usage_dir(path)?;
    if canonical_state.starts_with(&canonical_workspace) {
        return Err("usage state must be outside the agent-mounted workspace".to_string());
    }
    Ok(canonical_state)
}

/// Host-only approval and usage state for the admin portal; never inside
/// the agent-mounted workspace.
struct PortalState {
    push: PathBuf,
    fetch: PathBuf,
    gh: PathBuf,
    usage: PathBuf,
}

fn prepare_portal_state(home: &Path, instance: &str, cwd: &Path) -> Result<PortalState, String> {
    let workspace = fs::canonicalize(cwd)
        .map_err(|error| format!("could not resolve the workspace: {error}"))?;
    let outside = |name: &str, prepared: Result<PathBuf, String>| {
        let path = prepared.map_err(|error| format!("could not prepare {name} state: {error}"))?;
        if path.starts_with(&workspace) {
            return Err(format!(
                "{name} state must be outside the agent-mounted workspace"
            ));
        }
        Ok(path)
    };
    Ok(PortalState {
        push: outside(
            "push approval",
            managed_push::prepare_state_dir(&managed_push::state_dir(home, instance)),
        )?,
        fetch: outside(
            "fetch approval",
            managed_fetch::prepare_state_dir(&managed_fetch::state_dir(home, instance)),
        )?,
        gh: outside(
            "GitHub action approval",
            managed_gh::prepare_state_dir(&managed_gh::state_dir(home, instance)),
        )?,
        usage: outside("usage", prepare_usage_state_dir(&usage_state_dir(), cwd))?,
    })
}

fn cache_dir() -> PathBuf {
    env::var("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home_dir().join(".cache"))
}

fn get_last_modified(client: &Client, url: &str) -> Option<String> {
    let response = client.head(url).send().ok()?;
    response
        .headers()
        .get("last-modified")
        .and_then(|h| h.to_str().ok())
        .map(|s| s.to_string())
}

fn read_cache_file(path: &PathBuf) -> Option<String> {
    fs::read_to_string(path).ok()
}

fn write_cache_file(path: &PathBuf, content: &str) {
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(mut file) = File::create(path) {
        let _ = file.write_all(content.as_bytes());
    }
}

struct UpdateStatus {
    binary_available: Option<String>,
    skills_available: Option<String>,
}

fn check_available_updates(client: &Client) -> UpdateStatus {
    let binary_cache = cache_dir().join("claude-sandbox-lastmod");
    let skills_cache = cache_dir().join("claude-sandbox-skills-lastmod");

    let binary_available = get_last_modified(client, SCRIPT_URL).and_then(|remote| {
        let local = read_cache_file(&binary_cache);
        if local.is_none() {
            write_cache_file(&binary_cache, &remote);
            return None;
        }
        if local.as_ref() != Some(&remote) {
            Some(remote)
        } else {
            None
        }
    });

    let skills_available = read_cache_file(&skills_cache)
        .and_then(|local| get_last_modified(client, SKILLS_URL).filter(|remote| local != *remote));

    UpdateStatus {
        binary_available,
        skills_available,
    }
}

fn perform_updates(client: &Client, status: &UpdateStatus, auto: bool, quiet: bool) -> bool {
    let has_binary = status.binary_available.is_some();
    let has_skills = status.skills_available.is_some();

    if !has_binary && !has_skills {
        return true;
    }

    if !auto {
        if quiet {
            return false;
        }

        let prompt = match (has_binary, has_skills) {
            (true, true) => "Updates available: binary, skills, container image. Update now?",
            (true, false) => "Updates available: binary, container image. Update now?",
            (false, true) => "Updates available: skills, container image. Update now?",
            (false, false) => unreachable!(),
        };

        let confirm = Confirm::new()
            .with_prompt(prompt)
            .default(false)
            .interact()
            .unwrap_or(false);

        if !confirm {
            return false;
        }
    }

    if has_skills {
        install_skills(client, quiet);
    }

    if let Some(ref remote_lastmod) = status.binary_available {
        do_binary_update(client, remote_lastmod);
    }

    true
}

fn do_binary_update(client: &Client, remote_lastmod: &str) {
    let cache_file = cache_dir().join("claude-sandbox-lastmod");
    let exe_path = env::current_exe().expect("Could not get executable path");
    let invoked_program = invoked_program();

    let response = match client.get(SCRIPT_URL).send() {
        Ok(r) => r,
        Err(_) => {
            eprintln!("Failed to download update");
            return;
        }
    };

    let bytes = match response.bytes() {
        Ok(b) => b,
        Err(_) => {
            eprintln!("Failed to read update");
            return;
        }
    };

    let temp_path = exe_path.with_extension("new");
    if let Err(e) = fs::write(&temp_path, &bytes) {
        eprintln!("Failed to write update: {}", e);
        return;
    }

    if let Err(e) = fs::set_permissions(&temp_path, Permissions::from_mode(0o755)) {
        eprintln!("Failed to set permissions: {}", e);
        let _ = fs::remove_file(&temp_path);
        return;
    }

    if let Err(e) = fs::remove_file(&exe_path) {
        eprintln!("Failed to remove old binary: {}", e);
        let _ = fs::remove_file(&temp_path);
        return;
    }

    if let Err(e) = fs::rename(&temp_path, &exe_path) {
        eprintln!("Failed to rename new binary: {}", e);
        return;
    }

    write_cache_file(&cache_file, remote_lastmod);

    let args: Vec<_> = env::args_os().skip(1).collect();
    let err = Command::new(&invoked_program).args(&args).exec();
    eprintln!("Failed to exec: {}", err);
    std::process::exit(1);
}

fn install_skills(client: &Client, quiet: bool) {
    let target_dirs = [
        home_dir().join(".claude/skills"),
        home_dir().join(".agents/skills"),
    ];
    let cache_file = cache_dir().join("claude-sandbox-skills-lastmod");

    if !quiet {
        for target_dir in &target_dirs {
            println!("Installing skills to {}...", target_dir.display());
        }
    }

    let response = match client.get(SKILLS_URL).send() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Failed to download skills: {}", e);
            return;
        }
    };

    let bytes = match response.bytes() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("Failed to read skills tarball: {}", e);
            return;
        }
    };

    for target_dir in &target_dirs {
        if let Err(e) = fs::create_dir_all(target_dir) {
            eprintln!(
                "Failed to create skills directory {}: {}",
                target_dir.display(),
                e
            );
            return;
        }
        let decoder = GzDecoder::new(&bytes[..]);
        let mut archive = Archive::new(decoder);

        if let Err(e) = archive.unpack(target_dir) {
            eprintln!(
                "Failed to extract skills to {}: {}",
                target_dir.display(),
                e
            );
            return;
        }
    }

    if let Some(remote_lastmod) = get_last_modified(client, SKILLS_URL) {
        write_cache_file(&cache_file, &remote_lastmod);
    }

    if !quiet {
        println!("Skills installed successfully.");
    }
}

fn git_config(key: &str) -> String {
    Command::new("git")
        .args(["config", key])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

fn proxy_log_path(filename: &str) -> Result<PathBuf, String> {
    let cwd = env::current_dir().expect("Could not get current directory");
    let directory = home_dir()
        .join(".claude-sandbox/projects")
        .join(project_instance_name(&cwd))
        .join("logs");
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&directory)
        .map_err(|error| format!("could not create proxy log directory: {error}"))?;
    fs::set_permissions(&directory, Permissions::from_mode(0o700))
        .map_err(|error| format!("could not secure proxy log directory: {error}"))?;
    Ok(directory.join(filename))
}

fn create_proxy_runtime_dir() -> Result<PathBuf, String> {
    let base = home_dir().join(".claude-sandbox/runtime");
    create_proxy_runtime_dir_at(&base)
}

fn create_proxy_runtime_dir_at(base: &Path) -> Result<PathBuf, String> {
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(base)
        .map_err(|error| format!("could not create proxy runtime directory: {error}"))?;
    fs::set_permissions(base, Permissions::from_mode(0o700))
        .map_err(|error| format!("could not secure proxy runtime directory: {error}"))?;

    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    for counter in 0..10 {
        let path = base.join(format!("{}-{nonce}-{counter}", std::process::id()));
        match DirBuilder::new().mode(0o700).create(&path) {
            Ok(()) => return Ok(path),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(format!("could not create proxy runtime: {error}")),
        }
    }
    Err("could not allocate a unique proxy runtime directory".to_string())
}

fn wait_for_proxy_ready<F>(
    name: &str,
    socket_path: &Path,
    attempts: usize,
    delay: Duration,
    mut child_status: F,
) -> Result<(), String>
where
    F: FnMut() -> Result<Option<ExitStatus>, String>,
{
    for attempt in 0..attempts {
        if UnixStream::connect(socket_path).is_ok() {
            return Ok(());
        }
        if let Some(status) = child_status()? {
            return Err(format!("{name} exited before becoming ready ({status})"));
        }
        if attempt + 1 < attempts {
            thread::sleep(delay);
        }
    }
    Err(format!("{name} did not become ready in time"))
}

fn start_proxy(name: &str, socket_path: &Path, mut command: Command) -> Result<(), String> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|error| format!("failed to start {name}: {error}"))?;

    wait_for_proxy_ready(name, socket_path, 50, Duration::from_millis(100), || {
        child
            .try_wait()
            .map_err(|error| format!("failed to inspect {name}: {error}"))
    })
}

fn ensure_gh_proxy(
    runtime_dir: &Path,
    workspace_root: &Path,
    allow_merge: bool,
    agent_disclosure: Option<&str>,
    portal_state_dir: Option<&Path>,
) -> Result<(), String> {
    let socket_path = runtime_dir.join(GH_PROXY_SOCKET_NAME);
    let mut command = Command::new(env::current_exe().expect("Could not get executable path"));
    command
        .arg("gh-proxy")
        .arg("--socket")
        .arg(&socket_path)
        .arg("--log")
        .arg(proxy_log_path("gh-proxy.log")?)
        .arg("--workspace-root")
        .arg(workspace_root);
    if allow_merge {
        command.arg("--allow-merge");
    }
    if let Some(text) = agent_disclosure {
        command.arg(format!("--agent-disclosure={text}"));
    }
    if let Some(state) = portal_state_dir {
        command.arg("--portal-state-dir").arg(state);
    }
    start_proxy("gh-proxy", &socket_path, command)
}

fn ensure_vm_proxy(runtime_dir: &Path, workspace_root: &Path) -> Result<(), String> {
    let socket_path = runtime_dir.join(VM_PROXY_SOCKET_NAME);
    let mut command = Command::new(env::current_exe().expect("Could not get executable path"));
    command
        .arg("vm-proxy")
        .arg("--socket")
        .arg(&socket_path)
        .arg("--log")
        .arg(proxy_log_path("vm-proxy.log")?)
        .arg("--workspace-root")
        .arg(workspace_root)
        .arg("--runtime-dir")
        .arg(runtime_dir)
        .arg("--parent-pid")
        .arg(std::process::id().to_string())
        .process_group(0);
    start_proxy("vm-proxy", &socket_path, command)
}

fn ensure_git_proxy(
    runtime_dir: &Path,
    workspace_root: &Path,
    pre_approved_origin: Option<&str>,
    portal_state_dir: Option<&Path>,
) -> Result<(), String> {
    let socket_path = runtime_dir.join(GIT_PROXY_SOCKET_NAME);
    let mut command = Command::new(env::current_exe().expect("Could not get executable path"));
    command
        .arg("git-proxy")
        .arg("--socket")
        .arg(&socket_path)
        .arg("--log")
        .arg(proxy_log_path("git-proxy.log")?)
        .arg("--workspace-root")
        .arg(workspace_root);
    if let Some(origin) = pre_approved_origin {
        command.arg("--pre-approved-origin").arg(origin);
    }
    if let Some(state_dir) = portal_state_dir {
        command.arg("--state-dir").arg(state_dir);
    }
    start_proxy("git-proxy", &socket_path, command)
}

fn ensure_ssh_proxy(
    runtime_dir: &Path,
    launch_origin: Option<&str>,
    portal: Option<(&Path, &Path)>,
) -> Result<(), String> {
    let socket_path = runtime_dir.join(SSH_PROXY_SOCKET_NAME);
    let mut command = Command::new(env::current_exe().expect("Could not get executable path"));
    command
        .arg("ssh-proxy")
        .arg("--socket")
        .arg(&socket_path)
        .arg("--log")
        .arg(proxy_log_path("ssh-proxy.log")?);
    if let Some(origin) = launch_origin {
        command.arg("--launch-origin").arg(origin);
    }
    if let Some((workspace_root, state_dir)) = portal {
        command
            .arg("--workspace-root")
            .arg(workspace_root)
            .arg("--state-dir")
            .arg(state_dir);
    }
    start_proxy("ssh-proxy", &socket_path, command)
}

/// The launch repository's origin, when it is an SSH remote the fetch bridge
/// can serve. Read once here on the host and held by the SSH proxy.
fn launch_fetch_origin(cwd: &Path, quiet: bool) -> Option<String> {
    let url = git_proxy::launch_origin(cwd).ok()?;
    match ssh_proxy::origin_source(&url) {
        Ok(source) => source.map(|_| url),
        Err(reason) => {
            if !quiet {
                eprintln!("Note: origin is not fetchable through the host's SSH keys: {reason}");
            }
            None
        }
    }
}

fn podman_image_ids(args: &[&str]) -> Vec<String> {
    Command::new("podman")
        .args(["images", "--format", "{{.Id}}"])
        .args(args)
        .output()
        .map(|out| {
            String::from_utf8_lossy(&out.stdout)
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(String::from)
                .collect()
        })
        .unwrap_or_default()
}

fn stale_image_ids(
    current: &[String],
    candidates: impl IntoIterator<Item = String>,
) -> Vec<String> {
    if current.is_empty() {
        return Vec::new();
    }
    let mut stale: Vec<String> = candidates
        .into_iter()
        .filter(|id| !current.contains(id))
        .collect();
    stale.sort();
    stale.dedup();
    stale
}

fn admin_pasta_network(container_env: &[String]) -> Option<String> {
    container_env.iter().find_map(|entry| {
        entry
            .strip_prefix("CLAUDE_SANDBOX_PORTAL_PORT=")
            .and_then(|port| port.parse::<u16>().ok())
            .map(|port| format!("pasta:-T,{port}"))
    })
}

// The wrapped tmux session needs a pty even when nothing is attached to it.
fn podman_terminal_flags(stdin_is_terminal: bool, wrap: bool) -> &'static [&'static str] {
    match (stdin_is_terminal, wrap) {
        (true, _) => &["-i", "-t"],
        (false, true) => &["-t"],
        (false, false) => &["-i"],
    }
}

fn update_container_image(quiet: bool) {
    let previous = podman_image_ids(&[IMAGE]);

    let mut cmd = Command::new("podman");
    cmd.args(["pull", IMAGE]);
    if quiet {
        cmd.arg("--quiet").stdout(Stdio::null());
    }
    if !cmd.status().map(|s| s.success()).unwrap_or(false) {
        eprintln!("Warning: failed to pull {}, using the local image", IMAGE);
        return;
    }

    let current = podman_image_ids(&[IMAGE]);
    let label_filter = format!("label={}", IMAGE_SOURCE_LABEL);
    let labeled = podman_image_ids(&["--filter", &label_filter]);
    let stale = stale_image_ids(&current, previous.into_iter().chain(labeled));
    if stale.is_empty() {
        return;
    }
    if !quiet {
        eprintln!("Removing {} old container image(s)", stale.len());
    }
    for id in &stale {
        let _ = Command::new("podman")
            .args(["rmi", id])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

#[allow(clippy::too_many_arguments)]
fn run_container(
    extra_args: &[&str],
    pull_image: bool,
    ports: &[u16],
    container_env: &[String],
    quiet: bool,
    wrap: bool,
    pre_approved_origin: Option<&str>,
    allow_merge: bool,
    agent_disclosure: Option<&str>,
    portal: Option<&PortalState>,
    explicit_container_name: Option<&str>,
    vm_enabled: bool,
) {
    let cwd = env::current_dir().expect("Could not get current directory");
    let proxy_runtime_dir = create_proxy_runtime_dir().unwrap_or_else(|error| {
        eprintln!("Error: {error}");
        std::process::exit(1);
    });
    let require_proxy = |result: Result<(), String>| {
        result.unwrap_or_else(|error| {
            eprintln!("Error: {error}");
            std::process::exit(1);
        });
    };

    require_proxy(ensure_gh_proxy(
        &proxy_runtime_dir,
        &cwd,
        allow_merge,
        agent_disclosure,
        portal.map(|state| state.gh.as_path()),
    ));

    if vm_enabled {
        let report = vm_proxy::preflight(&home_dir(), &cwd, &proxy_runtime_dir);
        if !vm_proxy::preflight_is_healthy(&report) {
            eprintln!("Warning: Virtual Machine Bridge setup is incomplete:");
            eprintln!("{report}");
            eprintln!(
                "The sandbox will continue; see https://nsg.github.io/claude-sandbox/vm.html#one-time-host-setup"
            );
        }
        if let Err(error) = ensure_vm_proxy(&proxy_runtime_dir, &cwd) {
            eprintln!("Warning: failed to start vm-proxy: {error}");
        }
    }

    if pre_approved_origin.is_some() || portal.is_some() {
        require_proxy(ensure_git_proxy(
            &proxy_runtime_dir,
            &cwd,
            pre_approved_origin,
            portal.map(|state| state.push.as_path()),
        ));
    }

    let fetch_origin = launch_fetch_origin(&cwd, quiet);
    if fetch_origin.is_some() || portal.is_some() {
        require_proxy(ensure_ssh_proxy(
            &proxy_runtime_dir,
            fetch_origin.as_deref(),
            portal.map(|state| (cwd.as_path(), state.fetch.as_path())),
        ));
    }

    let home = home_dir();
    let claude_dir = home.join(".claude");
    let codex_dir = home.join(".codex");
    let agents_dir = home.join(".agents");
    let t3_dir = home.join(".t3");
    let opencode_config_dir = home.join(".config/opencode");
    let opencode_data_dir = home.join(".local/share/opencode");
    let _ = fs::create_dir_all(&codex_dir);
    let _ = fs::create_dir_all(&agents_dir);
    let _ = fs::create_dir_all(&t3_dir);
    let _ = fs::create_dir_all(&opencode_config_dir);
    let _ = fs::create_dir_all(&opencode_data_dir);

    let git_user_name = git_config("user.name");
    let git_user_email = git_config("user.email");

    if pull_image {
        update_container_image(quiet);
    }

    let mut cmd = Command::new("podman");
    cmd.args(["run", "--rm"])
        .args(podman_terminal_flags(std::io::stdin().is_terminal(), wrap))
        .arg("--init");
    if wrap && !std::io::stdout().is_terminal() {
        // Keep tmux screen redraws out of the journal or log file.
        cmd.stdout(Stdio::null());
    }
    let default_container_name = wrap_container_name(&cwd);
    let container_name = explicit_container_name.unwrap_or(&default_container_name);
    if wrap || explicit_container_name.is_some() {
        cmd.arg("--name").arg(container_name);
    }
    if quiet {
        cmd.arg("--quiet");
    }
    if let Some(network) = admin_pasta_network(container_env) {
        cmd.arg("--network").arg(network);
    }
    cmd.arg("-v").arg(format!("{}:/workspace", cwd.display()));
    cmd.arg("-v")
        .arg(format!(
            "{}:{}:ro",
            proxy_runtime_dir.display(),
            CONTAINER_PROXY_RUNTIME_DIR
        ))
        .arg("-v")
        .arg(format!("{}:/root/.claude", claude_dir.display()))
        .arg("-v")
        .arg(format!("{}:/root/.codex", codex_dir.display()))
        .arg("-v")
        .arg(format!("{}:/root/.agents", agents_dir.display()))
        .arg("-v")
        .arg(format!("{}:/root/.t3", t3_dir.display()))
        .arg("-v")
        .arg(format!(
            "{}:/root/.config/opencode",
            opencode_config_dir.display()
        ))
        .arg("-v")
        .arg(format!(
            "{}:/root/.local/share/opencode",
            opencode_data_dir.display()
        ))
        .args(["-e", "CLAUDE_CONFIG_DIR=/root/.claude"])
        .args(["-e", "CODEX_HOME=/root/.codex"])
        .args(["-e", "TERM=xterm-256color"])
        .args(["-e", "COLORTERM=truecolor"])
        .arg("-e")
        .arg(format!("GIT_USER_NAME={}", git_user_name))
        .arg("-e")
        .arg(format!("GIT_USER_EMAIL={}", git_user_email))
        .args(["-e", "IS_SANDBOX=1"])
        .args(["-v", "/etc/localtime:/etc/localtime:ro"]);

    // /etc/timezone was removed in newer distros (e.g. Ubuntu 26.04); only
    // bind-mount it when present, otherwise Docker fails to statfs the source.
    if Path::new("/etc/timezone").exists() {
        cmd.args(["-v", "/etc/timezone:/etc/timezone:ro"]);
    }

    for entry in container_env {
        cmd.arg("-e").arg(entry);
    }

    for port in ports {
        cmd.args(["-p", &format!("{}:{}", port, port)]);
    }

    let mut wrapped_args: Option<Vec<String>> = None;
    if wrap {
        wrapped_args = Some(vec![
            "tmux".to_string(),
            "new-session".to_string(),
            "-A".to_string(),
            "-s".to_string(),
            WRAP_TMUX_SESSION.to_string(),
            shell_command(extra_args),
        ]);
    }

    cmd.args(["-w", "/workspace"]).arg(IMAGE);
    if let Some(ref wa) = wrapped_args {
        cmd.args(wa);
    } else {
        cmd.args(extra_args);
    }

    if wrap && !quiet {
        eprintln!("Wrapped tmux session: {}", container_name);
        eprintln!("Type into it with: claude-sandbox wrap-type --enter \"hello\"");
    }

    let err = cmd.exec();
    eprintln!("Failed to exec podman: {}", err);
    std::process::exit(1);
}

fn run_internal_command(command: Option<&Commands>) -> bool {
    match command {
        Some(Commands::GhProxy {
            socket,
            log,
            workspace_root,
            allow_merge,
            agent_disclosure,
            portal_state_dir,
        }) => {
            gh_proxy::run(
                socket,
                log,
                workspace_root,
                *allow_merge,
                agent_disclosure.as_deref(),
                portal_state_dir.as_deref(),
            );
        }
        Some(Commands::GitProxy {
            socket,
            log,
            workspace_root,
            pre_approved_origin,
            state_dir,
        }) => {
            if pre_approved_origin.is_none() && state_dir.is_none() {
                eprintln!("git-proxy requires --pre-approved-origin or --state-dir");
                std::process::exit(2);
            }
            git_proxy::run(
                socket,
                log,
                git_proxy::Policy {
                    workspace_root: workspace_root.clone(),
                    pre_approved_origin: pre_approved_origin.clone(),
                    portal_state_dir: state_dir.clone(),
                },
            );
        }
        Some(Commands::T3Admin {
            port,
            t3_port,
            container_name,
            t3_base_dir,
            workspace_root,
            state_dir,
            fetch_state_dir,
            gh_state_dir,
            usage_state_dir,
        }) => {
            t3_admin::run(t3_admin::RunOptions {
                portal_port: *port,
                t3_port: *t3_port,
                container_name,
                t3_base_dir,
                workspace_root,
                state_dir,
                fetch_state_dir,
                gh_state_dir,
                usage_state_dir,
            });
        }
        Some(Commands::VmProxy {
            socket,
            log,
            workspace_root,
            runtime_dir,
            parent_pid,
        }) => {
            vm_proxy::run(socket, log, workspace_root, runtime_dir, *parent_pid);
        }
        Some(Commands::SshProxy {
            socket,
            log,
            launch_origin,
            workspace_root,
            state_dir,
        }) => {
            let launch_origin = launch_origin
                .as_deref()
                .map(|url| match ssh_proxy::origin_source(url) {
                    Ok(Some(source)) => source,
                    Ok(None) => {
                        eprintln!("ssh-proxy: launch origin {url} is not an SSH remote");
                        std::process::exit(2);
                    }
                    Err(error) => {
                        eprintln!("ssh-proxy: {error}");
                        std::process::exit(2);
                    }
                });
            let portal = match (workspace_root.as_ref(), state_dir.as_ref()) {
                (Some(workspace_root), Some(state_dir)) => Some(ssh_proxy::PortalFetch {
                    workspace_root: workspace_root.clone(),
                    state_dir: state_dir.clone(),
                }),
                (None, None) => None,
                _ => {
                    eprintln!("ssh-proxy requires both --workspace-root and --state-dir");
                    std::process::exit(2);
                }
            };
            ssh_proxy::run(
                socket,
                log,
                ssh_proxy::Policy {
                    launch_origin,
                    portal,
                },
            );
        }
        _ => return false,
    }
    true
}

fn starts_sandbox(command: Option<&Commands>) -> bool {
    matches!(
        command,
        None | Some(Commands::Run { .. } | Commands::T3code { .. })
    )
}

/// `--allow-push` pre-approves the launch repository with the origin it has
/// now; launching anywhere but a repository root with an origin is an error.
fn push_pre_approval(cli: &Cli) -> Result<Option<String>, String> {
    if !cli.allow_push || !starts_sandbox(cli.command.as_ref()) {
        return Ok(None);
    }
    let cwd =
        env::current_dir().map_err(|error| format!("could not get current directory: {error}"))?;
    allow_push_origin(&cwd).map(Some)
}

fn allow_push_origin(cwd: &Path) -> Result<String, String> {
    git_proxy::launch_origin(cwd).map_err(|reason| {
        format!(
            "--allow-push needs a git repository root with an 'origin' remote: {reason}. Launch claude-sandbox from the repository root."
        )
    })
}

fn service_command(cli: &Cli) -> Result<&'static str, (i32, String)> {
    let command = match cli.command.as_ref() {
        None => "shell",
        Some(Commands::Run { .. }) => "run",
        Some(Commands::T3code { .. }) => "t3code",
        _ => {
            return Err((
                2,
                "--install-service only applies to commands that start a sandbox".to_string(),
            ));
        }
    };

    if command == "shell" && !cli.wrap {
        return Err((
            2,
            "a shell service needs --wrap so it runs in a tmux session: claude-sandbox --wrap --install-service".to_string(),
        ));
    }
    Ok(command)
}

fn install_service(cli: &Cli) -> Result<(), (i32, String)> {
    let command = service_command(cli)?;

    if command == "t3code" {
        let ports = t3_port_decisions(cli).map_err(|error| (2, error))?;
        let mut unpinned = Vec::new();
        if ports.t3 == PortDecision::Auto {
            unpinned.push("--t3-port");
        }
        if ports.admin == Some(PortDecision::Auto) {
            unpinned.push("--portal-port");
        }
        if !unpinned.is_empty() {
            let unpinned = unpinned.join(" and ");
            eprintln!(
                "Note: {unpinned} not given; the service picks the first free port on every start. Pass {unpinned} for a stable address."
            );
        }
    }

    let cwd = env::current_dir()
        .map_err(|error| (1, format!("could not get current directory: {error}")))?;
    let raw_arguments = env::args_os().skip(1).collect::<Vec<_>>();
    let (arguments, explicit_name) =
        service::filter_install_argument(raw_arguments).map_err(|error| (1, error))?;
    service::derive_unit_name(explicit_name.as_deref(), command, &cwd)
        .map_err(|error| (2, error))?;

    let invoked = invoked_program();
    let current_exe = env::current_exe()
        .map_err(|error| (1, format!("could not resolve current executable: {error}")))?;
    let program =
        service::resolve_program_path(&invoked, &cwd, env::var_os("PATH").as_deref(), &current_exe);
    let home = env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| (1, "HOME environment variable not set".to_string()))?;
    let config_dir = service::service_config_dir(env::var_os("XDG_CONFIG_HOME").as_deref(), &home);
    let environment =
        service::recorded_environment(|name| env::var_os(name)).map_err(|error| (1, error))?;

    let outcome = service::install(service::InstallOptions {
        explicit_name,
        command: command.to_string(),
        cwd,
        program,
        arguments,
        environment,
        config_dir,
    })
    .map_err(|error| (1, error))?;
    service::print_summary(&outcome);
    Ok(())
}

fn extension_data_dir() -> PathBuf {
    extension::data_dir(env::var_os("XDG_DATA_HOME").as_deref(), &home_dir())
}

fn install_extension(host: Option<&str>) -> Result<(), String> {
    let (host, port) = match host {
        Some(host) => extension::parse_host(host)?,
        None => ("127.0.0.1".to_string(), None),
    };
    let port = port.unwrap_or(PORTAL_PORT);
    let outcome = extension::install(&extension_data_dir(), &host, port)?;
    extension::print_summary(&outcome);
    Ok(())
}

fn refresh_extension(quiet: bool) {
    match extension::refresh(&extension_data_dir()) {
        Ok(true) if !quiet => {
            println!("Updated the GNOME usage indicator; log out and back in to load it.");
        }
        Ok(_) => {}
        Err(error) => eprintln!("Could not update the GNOME usage indicator: {error}"),
    }
}

fn main() {
    let cli = Cli::parse();
    if cli.wrap && matches!(&cli.command, Some(Commands::T3code { .. })) {
        eprintln!("Error: --wrap applies to the shell and run, not t3code");
        std::process::exit(2);
    }
    if cli.install_service.is_none() && run_internal_command(cli.command.as_ref()) {
        return;
    }
    let is_t3code = matches!(&cli.command, Some(Commands::T3code { .. }));
    if let Err(error) = portal_pin(env::var_os(PORTAL_PIN_VARIABLE), is_t3code)
        .and_then(|pin| validate_portal(cli.portal, is_t3code, pin.as_deref()))
        .and_then(|()| validate_port_flags(&cli, is_t3code))
    {
        eprintln!("Error: {error}");
        std::process::exit(2);
    }
    let pre_approved_origin = push_pre_approval(&cli).unwrap_or_else(|error| {
        eprintln!("Error: {error}");
        std::process::exit(2);
    });
    let agent_disclosure = if cli.no_agent_disclosure {
        None
    } else {
        let text = cli
            .agent_disclosure_text
            .as_deref()
            .unwrap_or(gh_proxy::DEFAULT_DISCLOSURE_TEXT);
        if let Err(error) = gh_proxy::validate_disclosure_text(text) {
            eprintln!("Error: {error}");
            std::process::exit(2);
        }
        Some(text.trim().to_string())
    };
    if cli.install_service.is_some() {
        if let Err((code, error)) = install_service(&cli) {
            eprintln!("Error: {error}");
            std::process::exit(code);
        }
        return;
    }
    if let Some(Commands::Install { target, host }) = &cli.command {
        if target == "extension" {
            if let Err(error) = install_extension(host.as_deref()) {
                eprintln!("Error: {error}");
                std::process::exit(1);
            }
            return;
        }
        if host.is_some() {
            eprintln!("Error: --host only applies to: claude-sandbox install extension");
            std::process::exit(2);
        }
    }
    let client = Client::new();

    let update_status = check_available_updates(&client);
    let should_pull = perform_updates(&client, &update_status, cli.auto_update, cli.quiet);
    refresh_extension(cli.quiet);

    match cli.command {
        Some(Commands::Install { target, .. }) => {
            if target == "skills" {
                install_skills(&client, cli.quiet);
            } else {
                eprintln!("Unknown install target: {}", target);
                eprintln!("Usage: claude-sandbox install <skills|extension>");
                std::process::exit(1);
            }
        }
        Some(
            Commands::GhProxy { .. }
            | Commands::GitProxy { .. }
            | Commands::T3Admin { .. }
            | Commands::VmProxy { .. }
            | Commands::SshProxy { .. },
        ) => unreachable!("internal commands are dispatched before update checks"),
        Some(Commands::Run { command }) => {
            let cmd_str = run_command_string(&command);
            run_container(
                &["bash", "-lc", &cmd_str],
                should_pull,
                &cli.ports,
                &[],
                cli.quiet,
                cli.wrap,
                pre_approved_origin.as_deref(),
                cli.allow_merge,
                agent_disclosure.as_deref(),
                None,
                None,
                cli.vm,
            );
        }
        Some(Commands::T3code { args }) => {
            let port_decisions =
                decide_t3_ports(cli.t3_port, cli.portal_port, cli.portal, &cli.ports)
                    .unwrap_or_else(|error| {
                        eprintln!("Error: {error}");
                        std::process::exit(2);
                    });
            let t3_port_was_auto = port_decisions.t3 == PortDecision::Auto;
            let (port, portal_port) =
                resolve_t3_ports(port_decisions, &cli.ports).unwrap_or_else(|error| {
                    eprintln!("Error: {error}");
                    std::process::exit(1);
                });
            let cwd = env::current_dir().expect("Could not get current directory");
            let instance_name = project_instance_name(&cwd);
            let instance_dir = format!("/root/.t3/instances/{}", instance_name);
            let portal = portal_port.map(|_| {
                prepare_portal_state(&home_dir(), &instance_name, &cwd).unwrap_or_else(|error| {
                    eprintln!("Error: {error}");
                    std::process::exit(1);
                })
            });
            let container_name = format!("{}-t3-{}", wrap_container_name(&cwd), std::process::id());

            let t3_cmd = format!("t3code-register {}", args.join(" "));

            let mut ports = cli.ports.clone();
            if !ports.contains(&port) {
                ports.push(port);
            }
            if t3_port_was_auto && port != T3CODE_PORT {
                eprintln!(
                    "Port {} is in use, using port {} instead",
                    T3CODE_PORT, port
                );
            }
            eprintln!("t3code available at http://localhost:{}", port);
            if let (Some(portal_port), Some(state)) = (portal_port, portal.as_ref()) {
                eprintln!("admin portal available at http://localhost:{portal_port}");
                ensure_t3_admin(&T3AdminConfig {
                    portal_port,
                    t3_port: port,
                    container_name: &container_name,
                    t3_base_dir: &instance_dir,
                    workspace_root: &cwd,
                    state_dir: &state.push,
                    fetch_state_dir: &state.fetch,
                    gh_state_dir: &state.gh,
                    usage_state_dir: &state.usage,
                });
            }

            let mut container_env = vec![
                format!("T3CODE_PORT={}", port),
                format!("T3CODE_BASE_DIR={}", instance_dir),
            ];
            if let Some(portal_port) = portal_port {
                container_env.push(format!("CLAUDE_SANDBOX_PORTAL_PORT={portal_port}"));
            }
            let named_container = portal.as_ref().map(|_| container_name.as_str());

            run_container(
                &["bash", "-lc", &t3_cmd],
                should_pull,
                &ports,
                &container_env,
                cli.quiet,
                false,
                pre_approved_origin.as_deref(),
                cli.allow_merge,
                agent_disclosure.as_deref(),
                portal.as_ref(),
                named_container,
                cli.vm,
            );
        }
        Some(Commands::WrapType {
            session,
            enter,
            delay_min_ms,
            delay_max_ms,
            text,
        }) => {
            write_wrap_type(&text, session.as_deref(), enter, delay_min_ms, delay_max_ms);
        }
        Some(Commands::WrapKey { session, key }) => {
            write_wrap_key(&key, session.as_deref());
        }
        Some(Commands::WrapRead { session, lines }) => {
            print_wrap_screen(lines, session.as_deref());
        }
        Some(Commands::WrapList) => {
            print_wrap_sessions();
        }
        None => {
            run_container(
                &["bash", "-l"],
                should_pull,
                &cli.ports,
                &[],
                cli.quiet,
                cli.wrap,
                pre_approved_origin.as_deref(),
                cli.allow_merge,
                agent_disclosure.as_deref(),
                None,
                None,
                cli.vm,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::os::unix::net::UnixListener;
    use std::os::unix::process::ExitStatusExt;

    fn test_root(name: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "claude-sandbox-main-{name}-{}-{nonce}",
            std::process::id()
        ))
    }

    #[test]
    fn run_command_preserves_a_single_shell_expression() {
        let command = ["cargo build && cargo test".to_string()];
        assert_eq!(run_command_string(&command), "cargo build && cargo test");
    }

    #[test]
    fn run_command_quotes_separate_arguments() {
        let command = ["printf", r"%s\n", "a  b", "x;y", "it's"].map(String::from);
        assert_eq!(
            run_command_string(&command),
            r#"printf '%s\n' 'a  b' 'x;y' 'it'\''s'"#
        );
    }

    #[test]
    fn bare_invocation_has_no_subcommand() {
        let cli = Cli::try_parse_from(["claude-sandbox"]).unwrap();
        assert!(cli.command.is_none());
    }

    #[test]
    fn stray_arguments_and_removed_commands_are_rejected() {
        for argument in ["foo", "shell", "codex", "opencode"] {
            assert!(Cli::try_parse_from(["claude-sandbox", argument]).is_err());
        }
    }

    #[test]
    fn run_parsing_preserves_argument_spacing() {
        let cli = Cli::try_parse_from(["claude-sandbox", "run", "echo", "a  b"]).unwrap();
        let Some(Commands::Run { command }) = cli.command else {
            panic!("expected run command");
        };
        assert_eq!(command, ["echo", "a  b"]);
    }

    #[test]
    fn shell_service_requires_wrap() {
        let cli = Cli::try_parse_from(["claude-sandbox", "--install-service"]).unwrap();
        assert_eq!(
            service_command(&cli),
            Err((
                2,
                "a shell service needs --wrap so it runs in a tmux session: claude-sandbox --wrap --install-service".to_string(),
            ))
        );
        let cli = Cli::try_parse_from(["claude-sandbox", "--wrap", "--install-service"]).unwrap();
        assert_eq!(service_command(&cli), Ok("shell"));
    }

    #[test]
    fn service_commands_allow_run_and_t3code_without_wrap() {
        for (arguments, command) in [
            (
                vec!["claude-sandbox", "--install-service", "run", "true"],
                "run",
            ),
            (
                vec!["claude-sandbox", "--install-service", "t3code"],
                "t3code",
            ),
        ] {
            let cli = Cli::try_parse_from(arguments).unwrap();
            assert_eq!(service_command(&cli), Ok(command));
        }
        let cli =
            Cli::try_parse_from(["claude-sandbox", "--install-service", "wrap-list"]).unwrap();
        assert_eq!(
            service_command(&cli),
            Err((
                2,
                "--install-service only applies to commands that start a sandbox".to_string(),
            ))
        );
    }

    #[test]
    fn allow_push_outside_a_repository_root_is_an_error() {
        let root = test_root("allow-push");
        fs::create_dir_all(&root).unwrap();
        let error = allow_push_origin(&root).unwrap_err();
        assert!(error.contains("--allow-push"), "{error}");
        assert!(error.contains("from the repository root"), "{error}");

        let git = |args: &[&str]| {
            assert!(
                Command::new("git")
                    .arg("-C")
                    .arg(&root)
                    .args(args)
                    .output()
                    .unwrap()
                    .status
                    .success()
            );
        };
        git(&["init", "-q"]);
        assert!(allow_push_origin(&root).unwrap_err().contains("'origin'"));
        fs::create_dir_all(root.join("sub")).unwrap();
        git(&["remote", "add", "origin", "git@example.com:org/repo.git"]);
        assert!(allow_push_origin(&root.join("sub")).is_err());
        assert_eq!(
            allow_push_origin(&root).unwrap(),
            "git@example.com:org/repo.git"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn stale_images_exclude_current_and_dedupe() {
        let current = vec!["new".to_string()];
        let candidates = ["old", "new", "older", "old"].map(String::from);
        assert_eq!(
            stale_image_ids(&current, candidates),
            vec!["old".to_string(), "older".to_string()]
        );
    }

    #[test]
    fn stale_images_are_empty_without_a_current_image() {
        let candidates = ["old"].map(String::from);
        assert!(stale_image_ids(&[], candidates).is_empty());
    }

    #[test]
    fn admin_port_enables_pasta_reverse_forward() {
        let env = vec!["CLAUDE_SANDBOX_PORTAL_PORT=3774".to_string()];
        assert_eq!(admin_pasta_network(&env).as_deref(), Some("pasta:-T,3774"));
        assert_eq!(admin_pasta_network(&[]), None);
    }

    #[test]
    fn validates_portal_pins() {
        assert!(is_valid_portal_pin("0000"));
        assert!(is_valid_portal_pin("123456789012"));
        assert!(!is_valid_portal_pin(""));
        assert!(!is_valid_portal_pin("123"));
        assert!(!is_valid_portal_pin("1234567890123"));
        assert!(!is_valid_portal_pin("12a4"));
    }

    #[test]
    fn portal_requires_t3code_and_a_pin_and_a_pin_requires_portal() {
        assert!(validate_portal(true, true, Some("1234")).is_ok());
        assert!(validate_portal(false, true, None).is_ok());
        assert!(validate_portal(false, false, Some("1234")).is_ok());
        assert!(
            validate_portal(true, false, Some("1234"))
                .unwrap_err()
                .contains("only be used with the t3code command")
        );
        assert!(
            validate_portal(true, true, None)
                .unwrap_err()
                .contains("requires CLAUDE_SANDBOX_PORTAL_PIN")
        );
        assert!(
            validate_portal(true, true, Some("12ab"))
                .unwrap_err()
                .contains("4 to 12 digits")
        );
        assert_eq!(
            validate_portal(false, true, Some("1234")).unwrap_err(),
            "CLAUDE_SANDBOX_PORTAL_PIN is set but --portal was not given"
        );
    }

    #[test]
    fn portal_pin_is_read_only_for_t3code_and_empty_counts_as_set() {
        use std::os::unix::ffi::OsStringExt;
        let invalid = || Some(std::ffi::OsString::from_vec(vec![0xff, b'1']));
        assert_eq!(portal_pin(invalid(), false), Ok(None));
        assert!(portal_pin(invalid(), true).is_err());

        let empty = portal_pin(Some("".into()), true).unwrap();
        assert_eq!(empty.as_deref(), Some(""));
        assert!(
            validate_portal(false, true, empty.as_deref())
                .unwrap_err()
                .contains("set but --portal was not given")
        );
        assert!(
            validate_portal(true, true, empty.as_deref())
                .unwrap_err()
                .contains("4 to 12 digits")
        );
        assert_eq!(portal_pin(None, true), Ok(None));
    }

    #[test]
    fn t3_port_decisions_cover_auto_and_exact_values() {
        assert_eq!(
            decide_t3_ports(None, None, true, &[]).unwrap(),
            T3PortDecisions {
                t3: PortDecision::Auto,
                admin: Some(PortDecision::Auto),
            }
        );
        assert_eq!(
            decide_t3_ports(Some(3800), Some(3801), true, &[]).unwrap(),
            T3PortDecisions {
                t3: PortDecision::Exact(3800),
                admin: Some(PortDecision::Exact(3801)),
            }
        );
        assert_eq!(
            decide_t3_ports(Some(3800), None, false, &[]).unwrap().admin,
            None
        );
        let parse = |args: &[&str]| Cli::try_parse_from([&["claude-sandbox"], args].concat());
        assert!(parse(&["--t3-port", "0", "t3code"]).is_err());
        assert!(parse(&["--portal-port", "nope", "t3code"]).is_err());
    }

    #[test]
    fn explicit_portal_port_must_not_collide() {
        assert!(
            decide_t3_ports(Some(3800), Some(3800), true, &[])
                .unwrap_err()
                .contains("conflicts with --t3-port")
        );
        assert!(
            decide_t3_ports(None, Some(3801), true, &[3801])
                .unwrap_err()
                .contains("conflicts with published port 3801")
        );
    }

    #[test]
    fn port_flags_need_t3code_and_portal() {
        let check = |args: &[&str]| {
            let cli = Cli::try_parse_from([&["claude-sandbox"], args].concat()).unwrap();
            let t3code = matches!(cli.command, Some(Commands::T3code { .. }));
            validate_port_flags(&cli, t3code)
        };
        assert!(
            check(&[
                "--t3-port",
                "3800",
                "--portal",
                "--portal-port",
                "3801",
                "t3code"
            ])
            .is_ok()
        );
        assert!(
            check(&["--t3-port", "3800"])
                .unwrap_err()
                .contains("only be used with the t3code command")
        );
        assert!(check(&["--t3-port", "3800", "run", "true"]).is_err());
        assert_eq!(
            check(&["--portal-port", "3801", "t3code"]).unwrap_err(),
            "--portal-port needs --portal"
        );
    }

    #[test]
    fn auto_t3_port_avoids_an_explicit_portal_port() {
        let first_available = find_free_port_avoiding(T3CODE_PORT, &[]);
        let decisions = decide_t3_ports(None, Some(first_available), true, &[]).unwrap();
        let (t3, resolved_admin) = resolve_t3_ports(decisions, &[]).unwrap();
        assert_ne!(t3, first_available);
        assert_eq!(resolved_admin, Some(first_available));
    }

    #[test]
    fn podman_tty_flags_depend_on_terminal_or_wrap() {
        assert_eq!(podman_terminal_flags(false, false), &["-i"]);
        assert_eq!(podman_terminal_flags(true, false), &["-i", "-t"]);
        assert_eq!(podman_terminal_flags(false, true), &["-t"]);
    }

    #[test]
    fn free_port_selection_honors_exclusions() {
        let available = find_free_port_avoiding(45_000, &[45_000, 45_001]);
        assert_ne!(available, 45_000);
        assert_ne!(available, 45_001);
    }

    #[test]
    fn proxy_readiness_waits_for_a_connectable_listener() {
        let root = test_root("delayed-proxy");
        fs::create_dir_all(&root).unwrap();
        let socket = root.join("proxy.sock");
        let listener_socket = socket.clone();
        let listener = thread::spawn(move || {
            thread::sleep(Duration::from_millis(20));
            let listener = UnixListener::bind(listener_socket).unwrap();
            listener.accept().unwrap();
        });

        wait_for_proxy_ready("test-proxy", &socket, 20, Duration::from_millis(10), || {
            Ok(None)
        })
        .unwrap();
        listener.join().unwrap();
        fs::remove_file(&socket).unwrap();
        fs::remove_dir(&root).unwrap();
    }

    #[test]
    fn stale_socket_path_is_not_ready() {
        let root = test_root("stale-proxy");
        fs::create_dir_all(&root).unwrap();
        let socket = root.join("proxy.sock");
        drop(UnixListener::bind(&socket).unwrap());

        let error = wait_for_proxy_ready("test-proxy", &socket, 1, Duration::ZERO, || Ok(None))
            .unwrap_err();
        assert!(error.contains("did not become ready"));
        fs::remove_file(&socket).unwrap();
        fs::remove_dir(&root).unwrap();
    }

    #[test]
    fn proxy_readiness_reports_an_early_child_exit() {
        let socket = test_root("exited-proxy").join("proxy.sock");
        let error = wait_for_proxy_ready("test-proxy", &socket, 1, Duration::ZERO, || {
            Ok(Some(ExitStatus::from_raw(7 << 8)))
        })
        .unwrap_err();
        assert!(error.contains("exit status: 7"));
    }

    #[test]
    fn proxy_runtime_directories_are_private_and_unique() {
        let root = test_root("runtime");
        let first = create_proxy_runtime_dir_at(&root).unwrap();
        let second = create_proxy_runtime_dir_at(&root).unwrap();

        assert_ne!(first, second);
        assert_eq!(
            fs::metadata(&root).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(&first).unwrap().permissions().mode() & 0o777,
            0o700
        );
        fs::remove_dir(first).unwrap();
        fs::remove_dir(second).unwrap();
        fs::remove_dir(root).unwrap();
    }

    #[test]
    fn usage_state_must_stay_outside_the_workspace() {
        let root = test_root("usage-state");
        let workspace = root.join("workspace");
        let outside = root.join("outside");
        fs::create_dir_all(&workspace).unwrap();
        fs::create_dir_all(&outside).unwrap();

        let lexical = workspace.join(".state/usage");
        assert!(prepare_usage_state_dir(&lexical, &workspace).is_err());
        assert!(!lexical.exists());

        let linked_parent = outside.join("linked");
        symlink(&workspace, &linked_parent).unwrap();
        let canonical = linked_parent.join("nested/usage");
        assert!(prepare_usage_state_dir(&canonical, &workspace).is_err());
        assert!(!workspace.join("nested").exists());

        let valid = outside.join("usage");
        let prepared = prepare_usage_state_dir(&valid, &workspace).unwrap();
        assert_eq!(prepared, fs::canonicalize(&valid).unwrap());
        assert_eq!(
            fs::metadata(&prepared).unwrap().permissions().mode() & 0o777,
            0o700
        );
        fs::remove_dir_all(root).unwrap();
    }
}
