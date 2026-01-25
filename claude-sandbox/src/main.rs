use clap::{Parser, Subcommand};
use dialoguer::Confirm;
use flate2::read::GzDecoder;
use reqwest::blocking::Client;
use std::env;
use std::fs::{self, File, Permissions};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;
use tar::Archive;

const SCRIPT_URL: &str =
    "https://github.com/nsg/claude-sandbox/releases/latest/download/claude-sandbox";
const SKILLS_URL: &str =
    "https://github.com/nsg/claude-sandbox/releases/latest/download/skills.tar.gz";
const IMAGE: &str = "ghcr.io/nsg/claude-sandbox:latest";

#[derive(Parser)]
#[command(name = "claude-sandbox")]
#[command(about = "Run Claude in a sandboxed container")]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

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

fn update_check(client: &Client) {
    let cache_file = cache_dir().join("claude-sandbox-lastmod");

    let remote_lastmod = match get_last_modified(client, SCRIPT_URL) {
        Some(lm) => lm,
        None => return,
    };

    let local_lastmod = read_cache_file(&cache_file);

    if local_lastmod.is_none() {
        write_cache_file(&cache_file, &remote_lastmod);
        return;
    }

    let local_lastmod = local_lastmod.unwrap();
    if remote_lastmod != local_lastmod {
        let confirm = Confirm::new()
            .with_prompt("Update available. Update now?")
            .default(false)
            .interact()
            .unwrap_or(false);

        if confirm {
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

            // Write to a temp file first, then rename (can't write to running binary)
            let temp_path = exe_path.with_extension("new");
            if let Err(e) = fs::write(&temp_path, &bytes) {
                eprintln!("Failed to write update: {}", e);
                return;
            }

            // Set executable permissions
            if let Err(e) = fs::set_permissions(&temp_path, Permissions::from_mode(0o755)) {
                eprintln!("Failed to set permissions: {}", e);
                let _ = fs::remove_file(&temp_path);
                return;
            }

            // Remove old binary (allowed on Linux even while running)
            if let Err(e) = fs::remove_file(&exe_path) {
                eprintln!("Failed to remove old binary: {}", e);
                let _ = fs::remove_file(&temp_path);
                return;
            }

            // Rename new binary into place
            if let Err(e) = fs::rename(&temp_path, &exe_path) {
                eprintln!("Failed to rename new binary: {}", e);
                return;
            }

            write_cache_file(&cache_file, &remote_lastmod);

            let args: Vec<String> = env::args().skip(1).collect();
            let err = Command::new(&exe_path).args(&args).exec();
            eprintln!("Failed to exec: {}", err);
            std::process::exit(1);
        }
    }
}

fn install_skills(client: &Client) {
    let target_dir = home_dir().join(".claude/skills");
    let cache_file = cache_dir().join("claude-sandbox-skills-lastmod");

    println!("Installing skills to {}...", target_dir.display());

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

    println!("Skills installed successfully.");
}

fn skills_update_check(client: &Client) {
    let cache_file = cache_dir().join("claude-sandbox-skills-lastmod");

    let local_lastmod = match read_cache_file(&cache_file) {
        Some(lm) => lm,
        None => return,
    };

    let remote_lastmod = match get_last_modified(client, SKILLS_URL) {
        Some(lm) => lm,
        None => return,
    };

    if remote_lastmod != local_lastmod {
        let confirm = Confirm::new()
            .with_prompt("Skills update available. Update now?")
            .default(false)
            .interact()
            .unwrap_or(false);

        if confirm {
            install_skills(client);
        }
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

fn run_container(extra_args: &[&str]) {
    let cwd = env::current_dir().expect("Could not get current directory");
    let home = home_dir();
    let claude_dir = home.join(".claude");

    let git_user_name = git_config("user.name");
    let git_user_email = git_config("user.email");

    let mut cmd = Command::new("podman");
    cmd.args(["run", "--rm", "-it", "--pull=newer"])
        .arg("-v")
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
        .args(["-v", "/etc/localtime:/etc/localtime:ro"])
        .args(["-v", "/etc/timezone:/etc/timezone:ro"])
        .args(["-w", "/workspace"])
        .arg(IMAGE)
        .args(extra_args);

    let err = cmd.exec();
    eprintln!("Failed to exec podman: {}", err);
    std::process::exit(1);
}

fn main() {
    let cli = Cli::parse();
    let client = Client::new();

    update_check(&client);
    skills_update_check(&client);

    match cli.command {
        Some(Commands::Shell) => {
            run_container(&["bash", "-l"]);
        }
        Some(Commands::Install { target }) => {
            if target == "skills" {
                install_skills(&client);
            } else {
                eprintln!("Unknown install target: {}", target);
                eprintln!("Usage: claude-sandbox install skills");
                std::process::exit(1);
            }
        }
        None => {
            if cli.args.is_empty() {
                run_container(&["bash", "-lc", "claude"]);
            } else {
                let claude_cmd = format!("claude {}", cli.args.join(" "));
                run_container(&["bash", "-lc", &claude_cmd]);
            }
        }
    }
}
