mod clipboard_proxy;
mod gh_proxy;
mod logging;

use clap::{Parser, Subcommand};
use dialoguer::Confirm;
use flate2::read::GzDecoder;
use reqwest::blocking::Client;
use std::env;
use std::fs::{self, File, Permissions};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
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
const CLIPBOARD_PROXY_SOCKET_NAME: &str = "clipboard-proxy.sock";

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
    /// Start the clipboard image proxy (internal, spawned automatically)
    ClipboardProxy {
        /// Socket path (absolute)
        #[arg(long)]
        socket: String,
    },
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

    let args: Vec<String> = env::args().skip(1).collect();
    let err = Command::new(&exe_path).args(&args).exec();
    eprintln!("Failed to exec: {}", err);
    std::process::exit(1);
}

fn install_skills(client: &Client, quiet: bool) {
    let target_dir = home_dir().join(".claude/skills");
    let cache_file = cache_dir().join("claude-sandbox-skills-lastmod");

    if !quiet {
        println!("Installing skills to {}...", target_dir.display());
    }

    if let Err(e) = fs::create_dir_all(&target_dir) {
        eprintln!("Failed to create directory: {}", e);
        return;
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

    let decoder = GzDecoder::new(&bytes[..]);
    let mut archive = Archive::new(decoder);

    if let Err(e) = archive.unpack(&target_dir) {
        eprintln!("Failed to extract skills: {}", e);
        return;
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
    if socket_path.exists() {
        if std::os::unix::net::UnixStream::connect(&socket_path).is_ok() {
            return;
        }
        // Stale socket, will be cleaned up by the proxy on start
    }

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

    if socket_path.exists() {
        if std::os::unix::net::UnixStream::connect(&socket_path).is_ok() {
            return;
        }
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

fn run_container(
    extra_args: &[&str],
    pull_image: bool,
    ports: &[u16],
    host_env: &[String],
    quiet: bool,
) {
    ensure_gh_proxy();
    ensure_clipboard_proxy();

    let cwd = env::current_dir().expect("Could not get current directory");
    let home = home_dir();
    let claude_dir = home.join(".claude");

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
    cmd.args(["run", "--rm", "-it"]);
    if quiet {
        cmd.arg("--quiet");
    }
    if pull_image {
        cmd.arg("--pull=newer");
    }
    cmd.arg("-v")
        .arg(format!("{}:/workspace", cwd.display()))
        .arg("-v")
        .arg(format!("{}:/root/.claude", claude_dir.display()))
        .args(["-e", "CLAUDE_CONFIG_DIR=/root/.claude"])
        .args(["-e", "TERM=xterm-256color"])
        .args(["-e", "COLORTERM=truecolor"])
        .arg("-e")
        .arg(format!("GIT_USER_NAME={}", git_user_name))
        .arg("-e")
        .arg(format!("GIT_USER_EMAIL={}", git_user_email))
        .args(["-e", "IS_SANDBOX=1"])
        .args(["-v", "/etc/localtime:/etc/localtime:ro"])
        .args(["-v", "/etc/timezone:/etc/timezone:ro"]);

    for port in ports {
        cmd.args(["-p", &format!("{}:{}", port, port)]);
    }

    cmd.args(["-w", "/workspace"]).arg(IMAGE).args(extra_args);

    let mut child = cmd
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("Failed to spawn podman");

    let stderr = child.stderr.take().unwrap();
    thread::spawn(move || {
        let reader = BufReader::new(stderr);
        for line in reader.lines() {
            if let Ok(line) = line {
                if !line.contains("input device is not a TTY") {
                    eprintln!("{}", line);
                }
            }
        }
    });

    let status = child.wait().expect("Failed to wait for podman");
    std::process::exit(status.code().unwrap_or(1));
}

fn main() {
    let cli = Cli::parse();
    let client = Client::new();

    let update_status = check_available_updates(&client);
    let should_pull = perform_updates(&client, &update_status, cli.auto_update, cli.quiet);

    match cli.command {
        Some(Commands::Shell) => {
            run_container(
                &["bash", "-l"],
                should_pull,
                &cli.ports,
                &cli.host_env,
                cli.quiet,
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
        Some(Commands::ClipboardProxy { socket }) => {
            clipboard_proxy::run(&socket);
        }
        None => {
            if cli.args.is_empty() {
                run_container(
                    &["bash", "-lc", "claude"],
                    should_pull,
                    &cli.ports,
                    &cli.host_env,
                    cli.quiet,
                );
            } else {
                let claude_cmd = format!("claude {}", cli.args.join(" "));
                run_container(
                    &["bash", "-lc", &claude_cmd],
                    should_pull,
                    &cli.ports,
                    &cli.host_env,
                    cli.quiet,
                );
            }
        }
    }
}
