mod clipboard_proxy;
mod gh_proxy;
mod git_proxy;
mod logging;
mod ssh_proxy;

use clap::{Parser, Subcommand};
use dialoguer::Confirm;
use flate2::read::GzDecoder;
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::env;
use std::fs::{self, File, Permissions};
use std::hash::{Hash, Hasher};
use std::io::Write;
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::{thread, time::Duration};
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
    },
    /// Start the git push proxy (internal, spawned automatically)
    GitProxy {
        /// Socket path (absolute)
        #[arg(long)]
        socket: String,
        /// Origin remote URL snapshotted at launch
        #[arg(long)]
        origin_url: String,
    },
    /// Start the clipboard image proxy (internal, spawned automatically)
    ClipboardProxy {
        /// Socket path (absolute)
        #[arg(long)]
        socket: String,
    },
    /// Start the SSH proxy (internal, spawned automatically)
    SshProxy {
        /// Socket path (absolute)
        #[arg(long)]
        socket: String,
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

    let skills_available = read_cache_file(&skills_cache).and_then(|local| {
        get_last_modified(client, SKILLS_URL)
            .and_then(|remote| if local != remote { Some(remote) } else { None })
    });

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

fn gh_proxy_socket_path() -> PathBuf {
    env::current_dir()
        .expect("Could not get current directory")
        .join(GH_PROXY_SUBDIR)
        .join(GH_PROXY_SOCKET_NAME)
}

fn clipboard_proxy_socket_path() -> PathBuf {
    env::current_dir()
        .expect("Could not get current directory")
        .join(GH_PROXY_SUBDIR)
        .join(CLIPBOARD_PROXY_SOCKET_NAME)
}

fn ensure_gh_proxy() {
    let socket_path = gh_proxy_socket_path();

    // If socket already exists and is connectable, proxy is running
    if socket_path.exists() && std::os::unix::net::UnixStream::connect(&socket_path).is_ok() {
        return;
    }
    // Stale socket, will be cleaned up by the proxy on start

    // Spawn proxy as a background process
    let exe = env::current_exe().expect("Could not get executable path");
    let socket_str = socket_path.to_str().expect("Invalid socket path");
    match Command::new(&exe)
        .args(["gh-proxy", "--socket", socket_str])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(_) => {}
        Err(e) => {
            eprintln!("Warning: failed to start gh-proxy: {}", e);
            return;
        }
    }

    // Poll for socket to appear (100ms intervals, 3s timeout)
    for _ in 0..30 {
        thread::sleep(Duration::from_millis(100));
        if socket_path.exists() {
            return;
        }
    }

    eprintln!("Warning: gh-proxy did not start in time");
}

fn ensure_clipboard_proxy() {
    let socket_path = clipboard_proxy_socket_path();

    if socket_path.exists() && std::os::unix::net::UnixStream::connect(&socket_path).is_ok() {
        return;
    }

    let exe = env::current_exe().expect("Could not get executable path");
    let socket_str = socket_path.to_str().expect("Invalid socket path");
    match Command::new(&exe)
        .args(["clipboard-proxy", "--socket", socket_str])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(_) => {}
        Err(e) => {
            eprintln!("Warning: failed to start clipboard-proxy: {}", e);
            return;
        }
    }

    for _ in 0..30 {
        thread::sleep(Duration::from_millis(100));
        if socket_path.exists() {
            return;
        }
    }

    eprintln!("Warning: clipboard-proxy did not start in time");
}

fn git_proxy_socket_path() -> PathBuf {
    env::current_dir()
        .expect("Could not get current directory")
        .join(GH_PROXY_SUBDIR)
        .join(GIT_PROXY_SOCKET_NAME)
}

fn ensure_git_proxy(origin_url: &str) {
    let socket_path = git_proxy_socket_path();

    if socket_path.exists() && std::os::unix::net::UnixStream::connect(&socket_path).is_ok() {
        return;
    }

    let exe = env::current_exe().expect("Could not get executable path");
    let socket_str = socket_path.to_str().expect("Invalid socket path");
    match Command::new(&exe)
        .args([
            "git-proxy",
            "--socket",
            socket_str,
            "--origin-url",
            origin_url,
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(_) => {}
        Err(e) => {
            eprintln!("Warning: failed to start git-proxy: {}", e);
            return;
        }
    }

    for _ in 0..30 {
        thread::sleep(Duration::from_millis(100));
        if socket_path.exists() {
            return;
        }
    }

    eprintln!("Warning: git-proxy did not start in time");
}

/// Without --allow-push a leftover dead socket would make the container's
/// git shim try (and confusingly fail) to bridge pushes — remove it. A live
/// socket from a concurrent --allow-push session is left alone.
fn remove_stale_git_proxy_socket() {
    let socket_path = git_proxy_socket_path();
    if socket_path.exists() && std::os::unix::net::UnixStream::connect(&socket_path).is_err() {
        let _ = fs::remove_file(&socket_path);
    }
}

fn ssh_proxy_socket_path() -> PathBuf {
    env::current_dir()
        .expect("Could not get current directory")
        .join(GH_PROXY_SUBDIR)
        .join(SSH_PROXY_SOCKET_NAME)
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

fn ensure_ssh_proxy(config: &ssh_proxy::Config) {
    let socket_path = ssh_proxy_socket_path();

    if socket_path.exists() && std::os::unix::net::UnixStream::connect(&socket_path).is_ok() {
        return;
    }

    let config_json = serde_json::to_string(config).expect("Failed to serialize ssh-proxy config");
    let exe = env::current_exe().expect("Could not get executable path");
    let socket_str = socket_path.to_str().expect("Invalid socket path");
    match Command::new(&exe)
        .args([
            "ssh-proxy",
            "--socket",
            socket_str,
            "--config-json",
            &config_json,
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(_) => {}
        Err(e) => {
            eprintln!("Warning: failed to start ssh-proxy: {}", e);
            return;
        }
    }

    for _ in 0..30 {
        thread::sleep(Duration::from_millis(100));
        if socket_path.exists() {
            return;
        }
    }

    eprintln!("Warning: ssh-proxy did not start in time");
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
) {
    ensure_gh_proxy();
    ensure_clipboard_proxy();

    match (allow_push, git_proxy::origin_url()) {
        (true, Some(url)) => ensure_git_proxy(&url),
        (true, None) => {
            eprintln!(
                "Warning: --allow-push ignored, requires a git repository with an 'origin' remote"
            );
            remove_stale_git_proxy_socket();
        }
        (false, _) => remove_stale_git_proxy_socket(),
    }

    let ssh_proxy_config = load_ssh_proxy_config();
    if ssh_proxy::is_empty(&ssh_proxy_config) {
        save_ssh_proxy_config(&ssh_proxy_config);
    } else {
        ensure_ssh_proxy(&ssh_proxy_config);
    }
    ensure_ssh_proxy_symlink();

    let cwd = env::current_dir().expect("Could not get current directory");
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
    let container_name = wrap_container_name(&cwd);
    if wrap {
        cmd.arg("--name").arg(&container_name);
    }
    if quiet {
        cmd.arg("--quiet");
    }
    if pull_image {
        cmd.arg("--pull=newer");
    }
    if mount_workspace {
        cmd.arg("-v").arg(format!("{}:/workspace", cwd.display()));
    }
    cmd.arg("-v")
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

fn main() {
    let cli = Cli::parse();
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
        Some(Commands::GhProxy { socket }) => {
            gh_proxy::run(&socket);
        }
        Some(Commands::GitProxy { socket, origin_url }) => {
            git_proxy::run(&socket, &origin_url);
        }
        Some(Commands::ClipboardProxy { socket }) => {
            clipboard_proxy::run(&socket);
        }
        Some(Commands::SshProxy {
            socket,
            config_json,
        }) => {
            let config: ssh_proxy::Config =
                serde_json::from_str(&config_json).unwrap_or_else(|e| {
                    eprintln!("ssh-proxy: invalid config JSON: {}", e);
                    std::process::exit(1);
                });
            ssh_proxy::run(&socket, &config);
        }
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
            let pair_admin_port = pair_admin_pin.as_ref().map(|_| {
                let mut excluded_ports = cli.ports.clone();
                excluded_ports.push(port);
                find_free_port_avoiding(T3CODE_PAIR_ADMIN_PORT, &excluded_ports)
            });
            let cwd = env::current_dir().expect("Could not get current directory");
            let instance_name = project_instance_name(&cwd);
            let instance_dir = format!("/root/.t3/instances/{}", instance_name);

            let t3_cmd = format!("t3code-register {}", args.join(" "));

            let mut ports = cli.ports.clone();
            if !ports.contains(&port) {
                ports.push(port);
            }
            if let Some(pair_admin_port) = pair_admin_port
                && !ports.contains(&pair_admin_port)
            {
                ports.push(pair_admin_port);
            }
            if port != T3CODE_PORT {
                eprintln!(
                    "Port {} is in use, using port {} instead",
                    T3CODE_PORT, port
                );
            }
            eprintln!("t3code available at http://localhost:{}", port);
            if let Some(pair_admin_port) = pair_admin_port {
                eprintln!(
                    "t3code pairing portal available at http://localhost:{}",
                    pair_admin_port
                );
            }

            let mut container_env = vec![
                format!("T3CODE_PORT={}", port),
                format!("T3CODE_BASE_DIR={}", instance_dir),
            ];
            if let (Some(pair_admin_port), Some(pair_admin_pin)) = (pair_admin_port, pair_admin_pin)
            {
                container_env.push(format!("T3CODE_PAIR_ADMIN_PORT={}", pair_admin_port));
                container_env.push(format!("T3CODE_PAIR_ADMIN_PIN={}", pair_admin_pin));
            }

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
                cli.allow_push,
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
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
