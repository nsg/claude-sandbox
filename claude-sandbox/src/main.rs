mod clipboard_proxy;
mod gh_proxy;
mod git_proxy;
mod logging;
mod managed_push;
mod proxy_log;
mod proxy_socket;
mod ssh_proxy;
mod t3_admin;
mod usage_api;
mod usage_collector;

use clap::{Parser, Subcommand};
use dialoguer::Confirm;
use flate2::read::GzDecoder;
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::env;
use std::fs::{self, DirBuilder, File, Permissions};
use std::hash::{Hash, Hasher};
use std::io::Write;
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
const GH_PROXY_SUBDIR: &str = ".claude-sandbox";
const GH_PROXY_SOCKET_NAME: &str = "gh-proxy.sock";
const GIT_PROXY_SOCKET_NAME: &str = "git-proxy.sock";
const CLIPBOARD_PROXY_SOCKET_NAME: &str = "clipboard-proxy.sock";
const SSH_PROXY_SOCKET_NAME: &str = "ssh-proxy.sock";
const SSH_PROXY_CONFIG_FILE: &str = "ssh-proxy.json";
const SSHD_CONFIG_FILE: &str = "sshd.json";
const CONTAINER_PROXY_RUNTIME_DIR: &str = "/run/claude-sandbox";
// Must match the default session name in config/wrap.sh.
const WRAP_TMUX_SESSION: &str = "claude-sandbox";

#[derive(Debug, Serialize, Deserialize, Default)]
struct SshdConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    port: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    authorized_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    host_keys: Option<HashMap<String, String>>,
}

fn sshd_config_path() -> PathBuf {
    env::current_dir()
        .expect("Could not get current directory")
        .join(GH_PROXY_SUBDIR)
        .join(SSHD_CONFIG_FILE)
}

fn load_sshd_config() -> SshdConfig {
    let path = sshd_config_path();
    match fs::read_to_string(&path) {
        Ok(contents) => serde_json::from_str(&contents).unwrap_or_default(),
        Err(_) => SshdConfig::default(),
    }
}

fn save_sshd_config(config: &SshdConfig) {
    let path = sshd_config_path();
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_string_pretty(config) {
        let _ = fs::write(&path, json);
    }
}

#[derive(Parser)]
#[command(name = "claude-sandbox")]
#[command(about = "Run Claude in a sandboxed container")]
#[command(after_help = "Use -- to pass arguments to claude, e.g.: claude-sandbox -p 8080 -- -p")]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    /// Expose port(s) from container (can be repeated, e.g., -p 8080 -p 3000)
    #[arg(short = 'p', long = "port", action = clap::ArgAction::Append)]
    ports: Vec<u16>,

    /// Automatically update without prompting
    #[arg(long)]
    auto_update: bool,

    /// Suppress informational output, only show errors
    #[arg(short, long)]
    quiet: bool,

    /// Set host environment variable for the podman process (e.g., --host-env XDG_DATA_HOME=/home/user/.local/share)
    #[arg(long = "host-env", action = clap::ArgAction::Append)]
    host_env: Vec<String>,

    /// Disable audio passthrough (PulseAudio socket mount for voice mode)
    #[arg(long)]
    no_audio: bool,

    /// Allow the agent to run `git push` / `git push --tags`, executed on the host
    #[arg(long = "allow-push")]
    allow_push: bool,

    /// Let the T3 admin portal approve repositories for host-side pushes
    #[arg(long = "t3-managed-push", conflicts_with = "allow_push")]
    t3_managed_push: bool,

    /// Enable SSH server in the container
    #[arg(long)]
    ssh: bool,

    /// Path to the public key file to authorize for SSH access
    #[arg(long = "ssh-allow-key")]
    ssh_allow_key: Option<PathBuf>,

    /// Host port to map to container's SSH port 22
    #[arg(long = "ssh-port")]
    ssh_port: Option<u16>,

    /// Run the command in a named tmux session so keys can be injected
    #[arg(long, global = true)]
    wrap: bool,

    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    args: Vec<String>,
}

#[derive(Subcommand)]
enum Commands {
    /// Open interactive bash shell in container
    Shell,
    /// Install components
    Install {
        /// Component to install (e.g., "skills")
        target: String,
    },
    /// Start the gh CLI proxy (internal, spawned automatically)
    GhProxy {
        /// Socket path (absolute)
        #[arg(long)]
        socket: String,
        /// Persistent log path
        #[arg(long)]
        log: PathBuf,
    },
    /// Start the git push proxy (internal, spawned automatically)
    GitProxy {
        /// Socket path (absolute)
        #[arg(long)]
        socket: String,
        /// Persistent log path
        #[arg(long)]
        log: PathBuf,
        /// Origin remote URL snapshotted at launch (single-repository mode)
        #[arg(long)]
        origin_url: Option<String>,
        /// Host workspace root (managed T3 mode)
        #[arg(long)]
        workspace_root: Option<PathBuf>,
        /// Host-only managed push state directory
        #[arg(long)]
        state_dir: Option<PathBuf>,
    },
    /// Start the host-side T3 administration portal (internal)
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
        usage_state_dir: PathBuf,
        #[arg(long)]
        managed_push: bool,
    },
    /// Start the clipboard image proxy (internal, spawned automatically)
    ClipboardProxy {
        /// Socket path (absolute)
        #[arg(long)]
        socket: String,
        /// Persistent log path
        #[arg(long)]
        log: PathBuf,
    },
    /// Start the SSH proxy (internal, spawned automatically)
    SshProxy {
        /// Socket path (absolute)
        #[arg(long)]
        socket: String,
        /// Persistent log path
        #[arg(long)]
        log: PathBuf,
        /// Config as JSON string
        #[arg(long)]
        config_json: String,
    },
    /// Run a command inside the container
    Run {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        command: Vec<String>,
    },
    /// Run the OpenAI Codex CLI in the container
    Codex {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Run the t3code web GUI in the container (auto-discovers a free host port)
    T3code {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Run the opencode CLI in the container
    Opencode {
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
const T3CODE_PAIR_ADMIN_PORT: u16 = 3774;

/// Derive a stable, filesystem-safe identifier from a project path.
/// Returns `"name-abcd1234"` where `name` is the directory basename
/// (sanitised) and the suffix is a short hash of the full path to
/// disambiguate projects with the same name in different locations.
fn project_instance_name(path: &Path) -> String {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("default");

    // Keep only ASCII alphanumeric, dash and underscore.
    let sanitised: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();

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
                "Error: could not reach the wrapped session. Start one first, for example: claude-sandbox --wrap shell"
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

fn find_free_port(preferred: u16) -> u16 {
    find_free_port_avoiding(preferred, &[])
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
    usage_state_dir: &'a Path,
    managed_push: bool,
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
        .arg("--usage-state-dir")
        .arg(config.usage_state_dir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    if config.managed_push {
        command.arg("--managed-push");
    }
    if let Err(error) = command.spawn() {
        eprintln!("Warning: failed to start T3 admin portal: {error}");
        return;
    }
    for _ in 0..30 {
        thread::sleep(Duration::from_millis(100));
        if std::net::TcpStream::connect(("127.0.0.1", config.portal_port)).is_ok() {
            return;
        }
    }
    eprintln!("Warning: T3 admin portal did not start in time");
}

fn is_valid_pair_admin_pin(pin: &str) -> bool {
    (4..=12).contains(&pin.len()) && pin.chars().all(|character| character.is_ascii_digit())
}

fn default_tool() -> &'static str {
    let invoked = invoked_program();
    let name = PathBuf::from(&invoked)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_string();
    if name.starts_with("codex") {
        "codex"
    } else {
        "claude"
    }
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

fn ensure_gh_proxy(runtime_dir: &Path) -> Result<(), String> {
    let socket_path = runtime_dir.join(GH_PROXY_SOCKET_NAME);
    let mut command = Command::new(env::current_exe().expect("Could not get executable path"));
    command
        .arg("gh-proxy")
        .arg("--socket")
        .arg(&socket_path)
        .arg("--log")
        .arg(proxy_log_path("gh-proxy.log")?);
    start_proxy("gh-proxy", &socket_path, command)
}

fn ensure_clipboard_proxy(runtime_dir: &Path) -> Result<(), String> {
    let socket_path = runtime_dir.join(CLIPBOARD_PROXY_SOCKET_NAME);
    let mut command = Command::new(env::current_exe().expect("Could not get executable path"));
    command
        .arg("clipboard-proxy")
        .arg("--socket")
        .arg(&socket_path)
        .arg("--log")
        .arg(proxy_log_path("clipboard-proxy.log")?);
    start_proxy("clipboard-proxy", &socket_path, command)
}

fn ensure_git_proxy_single(runtime_dir: &Path, origin_url: &str) -> Result<(), String> {
    let socket_path = runtime_dir.join(GIT_PROXY_SOCKET_NAME);
    let mut command = Command::new(env::current_exe().expect("Could not get executable path"));
    command
        .arg("git-proxy")
        .arg("--socket")
        .arg(&socket_path)
        .arg("--log")
        .arg(proxy_log_path("git-proxy.log")?)
        .arg("--origin-url")
        .arg(origin_url);
    start_proxy("git-proxy", &socket_path, command)
}

fn ensure_git_proxy_managed(
    runtime_dir: &Path,
    workspace_root: &Path,
    state_dir: &Path,
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
        .arg(workspace_root)
        .arg("--state-dir")
        .arg(state_dir);
    start_proxy("managed git-proxy", &socket_path, command)
}

fn ssh_proxy_host_config_path() -> PathBuf {
    let cwd = env::current_dir().expect("Could not get current directory");
    let instance = project_instance_name(&cwd);
    home_dir()
        .join(".claude-sandbox/projects")
        .join(instance)
        .join(SSH_PROXY_CONFIG_FILE)
}

fn ssh_proxy_workspace_symlink_path() -> PathBuf {
    env::current_dir()
        .expect("Could not get current directory")
        .join(GH_PROXY_SUBDIR)
        .join(SSH_PROXY_CONFIG_FILE)
}

fn ensure_ssh_proxy_symlink() {
    let link_path = ssh_proxy_workspace_symlink_path();
    let target = ssh_proxy_host_config_path();

    if link_path.is_symlink() {
        if let Ok(existing) = fs::read_link(&link_path)
            && existing == target
        {
            return;
        }
        let _ = fs::remove_file(&link_path);
    } else if link_path.exists() {
        // Regular file exists (old-style config) — migrate it
        let _ = fs::remove_file(&link_path);
    }

    if let Some(parent) = link_path.parent() {
        let _ = fs::create_dir_all(parent);
    }

    let _ = std::os::unix::fs::symlink(&target, &link_path);
}

fn load_ssh_proxy_config() -> ssh_proxy::Config {
    let path = ssh_proxy_host_config_path();
    match fs::read_to_string(&path) {
        Ok(contents) => {
            serde_json::from_str(&contents).unwrap_or_else(|_| ssh_proxy::default_config())
        }
        Err(_) => ssh_proxy::default_config(),
    }
}

fn save_ssh_proxy_config(config: &ssh_proxy::Config) {
    let path = ssh_proxy_host_config_path();
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_string_pretty(config) {
        let _ = fs::write(&path, json);
    }
}

fn ensure_ssh_proxy(runtime_dir: &Path, config: &ssh_proxy::Config) -> Result<(), String> {
    let socket_path = runtime_dir.join(SSH_PROXY_SOCKET_NAME);
    let config_json = serde_json::to_string(config).expect("Failed to serialize ssh-proxy config");
    let mut command = Command::new(env::current_exe().expect("Could not get executable path"));
    command
        .arg("ssh-proxy")
        .arg("--socket")
        .arg(&socket_path)
        .arg("--log")
        .arg(proxy_log_path("ssh-proxy.log")?)
        .arg("--config-json")
        .arg(config_json);
    start_proxy("ssh-proxy", &socket_path, command)
}

struct SshConfig {
    authorized_key: String,
    host_port: u16,
}

#[allow(clippy::too_many_arguments)]
fn run_container(
    extra_args: &[&str],
    pull_image: bool,
    ports: &[u16],
    host_env: &[String],
    container_env: &[String],
    quiet: bool,
    ssh: Option<&SshConfig>,
    audio: bool,
    mount_workspace: bool,
    wrap: bool,
    allow_push: bool,
    managed_push_state: Option<&Path>,
    explicit_container_name: Option<&str>,
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

    require_proxy(ensure_gh_proxy(&proxy_runtime_dir));
    require_proxy(ensure_clipboard_proxy(&proxy_runtime_dir));

    match (managed_push_state, allow_push, git_proxy::origin_url()) {
        (Some(state_dir), true, _) => {
            require_proxy(ensure_git_proxy_managed(
                &proxy_runtime_dir,
                &cwd,
                state_dir,
            ));
        }
        (None, true, Some(url)) => {
            require_proxy(ensure_git_proxy_single(&proxy_runtime_dir, &url));
        }
        (None, true, None) => {
            eprintln!(
                "Warning: --allow-push ignored, requires a git repository with an 'origin' remote"
            );
        }
        (_, false, _) => {}
    }

    let ssh_proxy_config = load_ssh_proxy_config();
    if ssh_proxy::is_empty(&ssh_proxy_config) {
        save_ssh_proxy_config(&ssh_proxy_config);
    } else {
        require_proxy(ensure_ssh_proxy(&proxy_runtime_dir, &ssh_proxy_config));
    }
    ensure_ssh_proxy_symlink();

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

    let mut cmd = Command::new("podman");
    for entry in host_env {
        if let Some((key, val)) = entry.split_once('=') {
            cmd.env(key, val);
        } else {
            cmd.env_remove(entry);
        }
    }
    cmd.args(["run", "--rm", "-it", "--init"]);
    let default_container_name = wrap_container_name(&cwd);
    let container_name = explicit_container_name.unwrap_or(&default_container_name);
    if wrap || explicit_container_name.is_some() {
        cmd.arg("--name").arg(container_name);
    }
    if quiet {
        cmd.arg("--quiet");
    }
    if pull_image {
        cmd.arg("--pull=always");
    }
    if mount_workspace {
        cmd.arg("-v").arg(format!("{}:/workspace", cwd.display()));
    }
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

    if audio
        && let Some(pulse_path) = env::var_os("XDG_RUNTIME_DIR")
            .map(|d| PathBuf::from(d).join("pulse"))
            .filter(|p| p.join("native").exists())
    {
        cmd.arg("-v")
            .arg(format!("{}:/run/user/0/pulse:ro", pulse_path.display()))
            .args(["-e", "PULSE_SERVER=unix:/run/user/0/pulse/native"]);
    }

    for port in ports {
        cmd.args(["-p", &format!("{}:{}", port, port)]);
    }

    if let Some(ssh_cfg) = ssh {
        cmd.arg("-e")
            .arg(format!("SSH_AUTHORIZED_KEY={}", ssh_cfg.authorized_key));
        cmd.args(["-p", &format!("{}:22", ssh_cfg.host_port)]);
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
        Some(Commands::GhProxy { socket, log }) => {
            gh_proxy::run(socket, log);
        }
        Some(Commands::GitProxy {
            socket,
            log,
            origin_url,
            workspace_root,
            state_dir,
        }) => {
            let mode = match (
                origin_url.as_ref(),
                workspace_root.as_ref(),
                state_dir.as_ref(),
            ) {
                (Some(origin), None, None) => git_proxy::Mode::Single {
                    repository: git_proxy::repository_root()
                        .expect("git-proxy could not resolve the repository root"),
                    origin: origin.clone(),
                },
                (None, Some(workspace_root), Some(state_dir)) => git_proxy::Mode::Managed {
                    workspace_root: workspace_root.clone(),
                    state_dir: state_dir.clone(),
                },
                _ => {
                    eprintln!(
                        "git-proxy requires either --origin-url or both --workspace-root and --state-dir"
                    );
                    std::process::exit(2);
                }
            };
            git_proxy::run(socket, log, mode);
        }
        Some(Commands::T3Admin {
            port,
            t3_port,
            container_name,
            t3_base_dir,
            workspace_root,
            state_dir,
            usage_state_dir,
            managed_push,
        }) => {
            t3_admin::run(t3_admin::RunOptions {
                portal_port: *port,
                t3_port: *t3_port,
                container_name,
                t3_base_dir,
                workspace_root,
                state_dir,
                usage_state_dir,
                managed_push: *managed_push,
            });
        }
        Some(Commands::ClipboardProxy { socket, log }) => {
            clipboard_proxy::run(socket, log);
        }
        Some(Commands::SshProxy {
            socket,
            log,
            config_json,
        }) => {
            let config: ssh_proxy::Config =
                serde_json::from_str(config_json).unwrap_or_else(|error| {
                    eprintln!("ssh-proxy: invalid config JSON: {error}");
                    std::process::exit(1);
                });
            ssh_proxy::run(socket, log, &config);
        }
        _ => return false,
    }
    true
}

fn main() {
    let cli = Cli::parse();
    if run_internal_command(cli.command.as_ref()) {
        return;
    }
    if cli.t3_managed_push && !matches!(&cli.command, Some(Commands::T3code { .. })) {
        eprintln!("Error: --t3-managed-push can only be used with the t3code command");
        std::process::exit(2);
    }
    let client = Client::new();

    let update_status = check_available_updates(&client);
    let should_pull = perform_updates(&client, &update_status, cli.auto_update, cli.quiet);

    let ssh_config = if cli.ssh {
        let mut saved = load_sshd_config();

        // Resolve authorized_key: CLI flag overrides saved value
        let authorized_key = if let Some(ref key_path) = cli.ssh_allow_key {
            let key = fs::read_to_string(key_path).unwrap_or_else(|e| {
                eprintln!(
                    "Error: could not read public key file {}: {}",
                    key_path.display(),
                    e
                );
                std::process::exit(1);
            });
            let key = key.trim().to_string();
            if key.is_empty() {
                eprintln!("Error: public key file {} is empty", key_path.display());
                std::process::exit(1);
            }
            key
        } else if let Some(ref key) = saved.authorized_key {
            key.clone()
        } else {
            eprintln!("Error: --ssh-allow-key is required (no saved config found)");
            std::process::exit(1);
        };

        // Resolve port: CLI flag overrides saved value, default 2222
        let host_port = cli.ssh_port.or(saved.port).unwrap_or(2222);

        // Save resolved config back to sshd.json
        saved.authorized_key = Some(authorized_key.clone());
        saved.port = Some(host_port);
        save_sshd_config(&saved);

        Some(SshConfig {
            authorized_key,
            host_port,
        })
    } else {
        None
    };

    match cli.command {
        Some(Commands::Shell) => {
            run_container(
                &["bash", "-l"],
                should_pull,
                &cli.ports,
                &cli.host_env,
                &[],
                cli.quiet,
                ssh_config.as_ref(),
                !cli.no_audio,
                true,
                cli.wrap,
                cli.allow_push,
                None,
                None,
            );
        }
        Some(Commands::Install { target }) => {
            if target == "skills" {
                install_skills(&client, cli.quiet);
            } else {
                eprintln!("Unknown install target: {}", target);
                eprintln!("Usage: claude-sandbox install skills");
                std::process::exit(1);
            }
        }
        Some(
            Commands::GhProxy { .. }
            | Commands::GitProxy { .. }
            | Commands::T3Admin { .. }
            | Commands::ClipboardProxy { .. }
            | Commands::SshProxy { .. },
        ) => unreachable!("internal commands are dispatched before update checks"),
        Some(Commands::Run { command }) => {
            let cmd_str = command.join(" ");
            run_container(
                &["bash", "-lc", &cmd_str],
                should_pull,
                &cli.ports,
                &cli.host_env,
                &[],
                cli.quiet,
                ssh_config.as_ref(),
                !cli.no_audio,
                true,
                cli.wrap,
                cli.allow_push,
                None,
                None,
            );
        }
        Some(Commands::Codex { args }) => {
            let codex_cmd = if args.is_empty() {
                "codex".to_string()
            } else {
                format!("codex {}", args.join(" "))
            };
            run_container(
                &["bash", "-lc", &codex_cmd],
                should_pull,
                &cli.ports,
                &cli.host_env,
                &[],
                cli.quiet,
                ssh_config.as_ref(),
                !cli.no_audio,
                true,
                cli.wrap,
                cli.allow_push,
                None,
                None,
            );
        }
        Some(Commands::Opencode { args }) => {
            let opencode_cmd = if args.is_empty() {
                "opencode".to_string()
            } else {
                format!("opencode {}", args.join(" "))
            };
            run_container(
                &["bash", "-lc", &opencode_cmd],
                should_pull,
                &cli.ports,
                &cli.host_env,
                &[],
                cli.quiet,
                ssh_config.as_ref(),
                !cli.no_audio,
                true,
                cli.wrap,
                cli.allow_push,
                None,
                None,
            );
        }
        Some(Commands::T3code { args }) => {
            let port = find_free_port(T3CODE_PORT);
            let pair_admin_pin = env::var("T3CODE_PAIR_ADMIN_PIN")
                .ok()
                .filter(|pin| !pin.is_empty());
            if let Some(pin) = pair_admin_pin.as_deref()
                && !is_valid_pair_admin_pin(pin)
            {
                eprintln!("T3CODE_PAIR_ADMIN_PIN must contain 4 to 12 digits");
                std::process::exit(2);
            }
            if cli.t3_managed_push && pair_admin_pin.is_none() {
                eprintln!(
                    "Error: --t3-managed-push requires T3CODE_PAIR_ADMIN_PIN so repositories can be approved"
                );
                std::process::exit(2);
            }
            let pair_admin_port = pair_admin_pin.as_ref().map(|_| {
                let mut excluded_ports = cli.ports.clone();
                excluded_ports.push(port);
                find_free_port_avoiding(T3CODE_PAIR_ADMIN_PORT, &excluded_ports)
            });
            let cwd = env::current_dir().expect("Could not get current directory");
            let usage_state_dir = pair_admin_port.map(|_| {
                prepare_usage_state_dir(&usage_state_dir(), &cwd).unwrap_or_else(|error| {
                    eprintln!("Error: could not prepare usage state: {error}");
                    std::process::exit(1);
                })
            });
            let instance_name = project_instance_name(&cwd);
            let instance_dir = format!("/root/.t3/instances/{}", instance_name);
            let mut push_state_dir = managed_push::state_dir(&home_dir(), &instance_name);
            if cli.t3_managed_push {
                push_state_dir =
                    managed_push::prepare_state_dir(&push_state_dir).unwrap_or_else(|error| {
                        eprintln!("Error: could not prepare managed push state: {error}");
                        std::process::exit(1);
                    });
                let canonical_workspace = fs::canonicalize(&cwd).unwrap_or_else(|error| {
                    eprintln!("Error: could not resolve T3 workspace: {error}");
                    std::process::exit(1);
                });
                if push_state_dir.starts_with(&canonical_workspace) {
                    eprintln!(
                        "Error: managed push state must be outside the agent-mounted T3 workspace"
                    );
                    std::process::exit(1);
                }
            }
            let container_name = if cli.wrap {
                wrap_container_name(&cwd)
            } else {
                format!("{}-t3-{}", wrap_container_name(&cwd), std::process::id())
            };

            let t3_cmd = format!("t3code-register {}", args.join(" "));

            let mut ports = cli.ports.clone();
            if !ports.contains(&port) {
                ports.push(port);
            }
            if port != T3CODE_PORT {
                eprintln!(
                    "Port {} is in use, using port {} instead",
                    T3CODE_PORT, port
                );
            }
            eprintln!("t3code available at http://localhost:{}", port);
            if let (Some(pair_admin_port), Some(usage_state_dir)) =
                (pair_admin_port, usage_state_dir.as_deref())
            {
                eprintln!(
                    "t3code admin portal available at http://localhost:{}",
                    pair_admin_port
                );
                ensure_t3_admin(&T3AdminConfig {
                    portal_port: pair_admin_port,
                    t3_port: port,
                    container_name: &container_name,
                    t3_base_dir: &instance_dir,
                    workspace_root: &cwd,
                    state_dir: &push_state_dir,
                    usage_state_dir,
                    managed_push: cli.t3_managed_push,
                });
            }

            let mut container_env = vec![
                format!("T3CODE_PORT={}", port),
                format!("T3CODE_BASE_DIR={}", instance_dir),
            ];
            if let Some(pair_admin_port) = pair_admin_port {
                container_env.push(format!("T3CODE_ADMIN_PORT={pair_admin_port}"));
            }
            let managed_state = cli.t3_managed_push.then_some(push_state_dir.as_path());
            let named_container = pair_admin_pin.as_ref().map(|_| container_name.as_str());

            run_container(
                &["bash", "-lc", &t3_cmd],
                should_pull,
                &ports,
                &cli.host_env,
                &container_env,
                cli.quiet,
                ssh_config.as_ref(),
                !cli.no_audio,
                true,
                cli.wrap,
                cli.allow_push || cli.t3_managed_push,
                managed_state,
                named_container,
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
            let tool = default_tool();
            let inner_cmd = if cli.args.is_empty() {
                tool.to_string()
            } else {
                format!("{} {}", tool, cli.args.join(" "))
            };
            run_container(
                &["bash", "-lc", &inner_cmd],
                should_pull,
                &cli.ports,
                &cli.host_env,
                &[],
                cli.quiet,
                ssh_config.as_ref(),
                !cli.no_audio,
                true,
                cli.wrap,
                cli.allow_push,
                None,
                None,
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
    fn validates_pair_admin_pins() {
        assert!(is_valid_pair_admin_pin("0000"));
        assert!(is_valid_pair_admin_pin("123456789012"));
        assert!(!is_valid_pair_admin_pin(""));
        assert!(!is_valid_pair_admin_pin("123"));
        assert!(!is_valid_pair_admin_pin("1234567890123"));
        assert!(!is_valid_pair_admin_pin("12a4"));
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
