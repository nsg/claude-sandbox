use flate2::Compression;
use flate2::write::GzEncoder;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::net::Shutdown;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Component, Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use std::{env, process, thread};
use tar::Builder;

use crate::gh_proxy::{open_untrusted_path, proc_fd_path};
use crate::logging::log_line;
use crate::{proxy_log, proxy_socket};

const REMOTE: &str = "claude-sandbox";
const PROJECT: &str = "claude-sandbox";
const COMMAND_OUTPUT_LIMIT: usize = 4 * 1024 * 1024;
const EXEC_OUTPUT_LIMIT: usize = 1024 * 1024;
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);
const LONG_LAUNCH_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const IMPORT_TIMEOUT: Duration = Duration::from_secs(60 * 60);
const SCREEN_START_TIMEOUT: Duration = Duration::from_secs(15);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_MAX_IMPORT_SIZE: u64 = 64 * 1024 * 1024 * 1024;
const DEFAULT_MAX_ISO_SIZE: u64 = 16 * 1024 * 1024 * 1024;
const DEFAULT_MAX_MEDIA: usize = 16;
const DEFAULT_MAX_REQUESTS: usize = 16;
const DEFAULT_MAX_RELAY_CONNECTIONS: usize = 32;
const DEFAULT_RELAY_IDLE_TIMEOUT: Duration = Duration::from_secs(60);

const HELP: &str = "vm - restricted Incus virtual-machine bridge\n\
\n\
Usage:\n\
  vm help\n\
  vm status\n\
  vm list [--json]\n\
  vm info NAME [--json]\n\
  vm launch NAME (--image images:ALIAS | --image LOCAL | --iso MEDIA) [--cpus N] [--memory SIZE] [--disk SIZE] [--no-secureboot]\n\
  vm stop NAME [--force]\n\
  vm restart NAME [--force]\n\
  vm delete NAME\n\
  vm screen NAME          print the VM's spice+unix:// display address\n\
  vm view NAME            open that display full-screen on the sandbox X display\n\
  vm exec NAME [--timeout SECS] -- CMD [ARG...]\n\
  vm console-log NAME\n\
  vm import PATH NAME\n\
  vm media [--json]\n\
  vm media-delete NAME\n\
\n\
VMs are always ephemeral: stopping a VM deletes it and its disk.\n\
exec needs the Incus agent, which images: VMs have and ISO installs do not.\n\
console-log requires an Incus server newer than 6.0.0 for VMs.\n";

#[derive(Deserialize)]
struct Request {
    args: Vec<String>,
    #[serde(default)]
    cwd: Option<String>,
}

#[derive(Serialize)]
struct Response {
    exit_code: i32,
    stdout: String,
    stderr: String,
}

impl Response {
    fn ok(stdout: impl Into<String>) -> Self {
        Self {
            exit_code: 0,
            stdout: stdout.into(),
            stderr: String::new(),
        }
    }

    fn error(message: impl Into<String>) -> Self {
        Self {
            exit_code: 1,
            stdout: String::new(),
            stderr: format!("vm: {}\n", message.into()),
        }
    }
}

struct CommandOutput {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    timed_out: bool,
}

enum CommandInput {
    Null,
    Bytes(Vec<u8>),
    File(File),
}

#[derive(Clone)]
struct Relay {
    child: Arc<Mutex<Child>>,
    socket_path: PathBuf,
    upstream_path: PathBuf,
    connections: Arc<Mutex<HashMap<u64, RelayConnection>>>,
    stopping: Arc<AtomicBool>,
}

struct RelayConnection {
    client: UnixStream,
    upstream: Option<UnixStream>,
}

struct Snapshot {
    path: PathBuf,
}

impl Drop for Snapshot {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

struct RequestSlot(Arc<AtomicUsize>);

impl Drop for RequestSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

#[derive(Clone)]
pub(crate) struct IncusBridge {
    incus_bin: Arc<Mutex<Option<PathBuf>>>,
    conf_dir: PathBuf,
    remote: &'static str,
    project: &'static str,
    workspace_root: PathBuf,
    runtime_dir: PathBuf,
    snapshot_dir: PathBuf,
    max_import_size: u64,
    max_iso_size: u64,
    max_media: usize,
    max_relay_connections: usize,
    relay_idle_timeout: Duration,
    relays: Arc<Mutex<HashMap<String, Relay>>>,
    relay_start: Arc<Mutex<()>>,
    import_lock: Arc<Mutex<()>>,
    healthy: Arc<AtomicBool>,
    cleaned: Arc<AtomicBool>,
    shutting_down: Arc<AtomicBool>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum LaunchSource {
    PublicImage(String),
    LocalImage(String),
    Iso(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct LaunchOptions {
    name: String,
    source: LaunchSource,
    cpus: u8,
    memory: String,
    disk: String,
    no_secureboot: bool,
}

#[derive(Debug, PartialEq, Eq)]
enum VmCommand {
    Help,
    Status,
    List {
        json: bool,
    },
    Info {
        name: String,
        json: bool,
    },
    Launch(LaunchOptions),
    Stop {
        name: String,
        force: bool,
    },
    Restart {
        name: String,
        force: bool,
    },
    Delete {
        name: String,
    },
    Screen {
        name: String,
    },
    Exec {
        name: String,
        timeout: Duration,
        command: Vec<String>,
    },
    ConsoleLog {
        name: String,
    },
    Import {
        path: String,
        name: String,
    },
    Media {
        json: bool,
    },
    MediaDelete {
        name: String,
    },
}

impl IncusBridge {
    fn new(home: &Path, workspace_root: &Path, runtime_dir: &Path) -> Self {
        let workspace_root =
            fs::canonicalize(workspace_root).unwrap_or_else(|_| workspace_root.to_path_buf());
        Self {
            incus_bin: Arc::new(Mutex::new(find_executable("incus"))),
            conf_dir: home.join(".claude-sandbox/incus"),
            remote: REMOTE,
            project: PROJECT,
            workspace_root,
            runtime_dir: runtime_dir.to_path_buf(),
            snapshot_dir: home.join(".claude-sandbox/vm-import"),
            max_import_size: DEFAULT_MAX_IMPORT_SIZE,
            max_iso_size: DEFAULT_MAX_ISO_SIZE,
            max_media: DEFAULT_MAX_MEDIA,
            max_relay_connections: DEFAULT_MAX_RELAY_CONNECTIONS,
            relay_idle_timeout: DEFAULT_RELAY_IDLE_TIMEOUT,
            relays: Arc::new(Mutex::new(HashMap::new())),
            relay_start: Arc::new(Mutex::new(())),
            import_lock: Arc::new(Mutex::new(())),
            healthy: Arc::new(AtomicBool::new(false)),
            cleaned: Arc::new(AtomicBool::new(false)),
            shutting_down: Arc::new(AtomicBool::new(false)),
        }
    }

    fn refresh_incus_bin(&self) -> Option<PathBuf> {
        if let Some(path) = self
            .incus_bin
            .lock()
            .expect("incus path lock poisoned")
            .clone()
        {
            return Some(path);
        }
        let found = find_executable("incus");
        *self.incus_bin.lock().expect("incus path lock poisoned") = found.clone();
        found
    }

    fn command(&self) -> Result<Command, String> {
        let incus_bin = self
            .incus_bin
            .lock()
            .expect("incus path lock poisoned")
            .clone()
            .ok_or_else(|| "Incus client was not found in PATH".to_string())?;
        let home = self
            .conf_dir
            .parent()
            .and_then(Path::parent)
            .unwrap_or(Path::new("/"));
        let mut command = Command::new(incus_bin);
        command
            .env_clear()
            .env("INCUS_CONF", &self.conf_dir)
            .env("LC_ALL", "C")
            .env("HOME", home)
            .env("PATH", "/nonexistent")
            .current_dir("/");
        Ok(command)
    }

    fn run_command(
        &self,
        args: &[String],
        input: CommandInput,
        timeout: Duration,
        limit: usize,
    ) -> Result<CommandOutput, String> {
        let mut command = self.command()?;
        command
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        match &input {
            CommandInput::Null => {
                command.stdin(Stdio::null());
            }
            CommandInput::Bytes(_) => {
                command.stdin(Stdio::piped());
            }
            CommandInput::File(file) => {
                command
                    .stdin(Stdio::from(file.try_clone().map_err(|error| {
                        format!("could not clone import file: {error}")
                    })?));
            }
        }
        let mut child = command
            .spawn()
            .map_err(|error| format!("failed to execute Incus client: {error}"))?;

        let stdin_thread = match input {
            CommandInput::Bytes(bytes) => child.stdin.take().map(|mut stdin| {
                thread::spawn(move || {
                    let _ = stdin.write_all(&bytes);
                })
            }),
            _ => None,
        };
        let stdout = child.stdout.take().expect("piped stdout missing");
        let stderr = child.stderr.take().expect("piped stderr missing");
        let stdout_thread = thread::spawn(move || read_capped(stdout, limit, "stdout"));
        let stderr_thread = thread::spawn(move || read_capped(stderr, limit, "stderr"));

        let deadline = Instant::now() + timeout;
        let (status, timed_out) = loop {
            match child.try_wait() {
                Ok(Some(status)) => break (status, false),
                Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(20)),
                Ok(None) => {
                    let _ = child.kill();
                    let status = child.wait().map_err(|error| {
                        format!("failed waiting for timed-out Incus client: {error}")
                    })?;
                    break (status, true);
                }
                Err(error) => return Err(format!("failed waiting for Incus client: {error}")),
            }
        };
        if let Some(handle) = stdin_thread {
            let _ = handle.join();
        }
        let stdout = stdout_thread.join().unwrap_or_default();
        let stderr = stderr_thread.join().unwrap_or_default();
        Ok(CommandOutput {
            status,
            stdout,
            stderr,
            timed_out,
        })
    }

    fn query(&self, path: &str, timeout: Duration) -> Result<Value, String> {
        let args = vec![
            "query".to_string(),
            "--project".to_string(),
            self.project.to_string(),
            format!("{}:{}", self.remote, path),
        ];
        let output = self.run_command(&args, CommandInput::Null, timeout, COMMAND_OUTPUT_LIMIT)?;
        if output.timed_out {
            return Err(format!(
                "Incus query timed out after {}s",
                timeout.as_secs()
            ));
        }
        if !output.status.success() {
            return Err(output_error(&output));
        }
        let value: Value = serde_json::from_slice(&output.stdout)
            .map_err(|error| format!("Incus returned invalid JSON: {error}"))?;
        Ok(unwrap_metadata(value))
    }

    fn preflight_report(&self) -> String {
        self.preflight_report_inner(true)
    }

    fn preflight_report_inner(&self, refresh_binary: bool) -> String {
        let mut lines = Vec::new();
        let incus = if refresh_binary {
            self.refresh_incus_bin()
        } else {
            self.incus_bin
                .lock()
                .expect("incus path lock poisoned")
                .clone()
        };
        if incus.is_some() {
            lines.push("OK   Incus client found in PATH".to_string());
        } else {
            lines.push("FAIL Incus client not found in PATH; install it with `sudo apt install incus-client` (see README: Virtual Machine Bridge)".to_string());
        }

        let workspace = canonicalize_for_comparison(&self.workspace_root);
        let conf_dir = canonicalize_for_comparison(&self.conf_dir);
        let snapshot_dir = canonicalize_for_comparison(&self.snapshot_dir);
        if paths_overlap(&workspace, &conf_dir) || paths_overlap(&workspace, &snapshot_dir) {
            lines.push("FAIL the workspace overlaps ~/.claude-sandbox private VM bridge state; this exposes the Incus client key to the agent, so launch claude-sandbox from a project directory instead of your home directory".to_string());
        } else {
            lines.push("OK   Workspace does not overlap private VM bridge state".to_string());
        }

        if !self.conf_dir.is_dir() {
            lines.push(format!(
                "FAIL {} is missing; run `INCUS_CONF=~/.claude-sandbox/incus incus remote add claude-sandbox <token>` (see README: Virtual Machine Bridge)",
                self.conf_dir.display()
            ));
        } else if incus.is_none() {
            lines.push("FAIL cannot check the claude-sandbox remote until the Incus client is installed (see README: Virtual Machine Bridge)".to_string());
        } else if client_config_has_remote(&self.conf_dir, self.remote) {
            lines.push(format!(
                "OK   {} contains the claude-sandbox remote",
                self.conf_dir.display()
            ));
        } else {
            lines.push(format!(
                "FAIL {} does not contain the claude-sandbox remote; run `INCUS_CONF=~/.claude-sandbox/incus incus remote add claude-sandbox <token>` (see README: Virtual Machine Bridge)",
                self.conf_dir.display()
            ));
        }

        if incus.is_some() && self.conf_dir.is_dir() {
            match self.query("/1.0", Duration::from_secs(10)) {
                Ok(server) if server.get("auth").and_then(Value::as_str) == Some("trusted") => {
                    lines.push("OK   Incus server is reachable and the client certificate is trusted".to_string());
                }
                Ok(_) => lines.push("FAIL Incus server does not trust this client certificate; recreate the remote with `INCUS_CONF=~/.claude-sandbox/incus incus remote add claude-sandbox <token>` (see README: Virtual Machine Bridge)".to_string()),
                Err(error) if connection_refused(&error) => lines.push("FAIL Incus HTTPS listener is unreachable; run `incus config set core.https_address 127.0.0.1:8443` as an Incus admin (see README: Virtual Machine Bridge)".to_string()),
                Err(error) if certificate_not_trusted(&error) => lines.push("FAIL Incus server does not trust this client certificate; recreate the remote with `INCUS_CONF=~/.claude-sandbox/incus incus remote add claude-sandbox <token>` (see README: Virtual Machine Bridge)".to_string()),
                Err(error) => lines.push(format!("FAIL Incus server/certificate check failed: {}; recreate the restricted trust token and remote (see README: Virtual Machine Bridge)", one_line(&error))),
            }

            // Incus filters the project list to what the certificate may view, so a
            // confined certificate sees exactly its own project and nothing else.
            match self.query("/1.0/projects?recursion=1", Duration::from_secs(10)) {
                Ok(projects) => match visible_projects(&projects) {
                    Some(names) if names == [self.project] => lines.push("OK   Client certificate is confined to the claude-sandbox project".to_string()),
                    Some(names) if names.iter().any(|name| *name != self.project) => lines.push(format!("FAIL the client certificate can also see other projects ({}); remove it with `incus config trust remove <fingerprint>` and add a replacement using `incus config trust add claude-sandbox --restricted --projects claude-sandbox` (see README: Virtual Machine Bridge)", names.join(", "))),
                    Some(_) => lines.push("FAIL the client certificate cannot see the claude-sandbox project; create the project, then add a certificate using `incus config trust add claude-sandbox --restricted --projects claude-sandbox` (see README: Virtual Machine Bridge)".to_string()),
                    None => lines.push("FAIL could not verify certificate confinement: unexpected project list from Incus (see README: Virtual Machine Bridge)".to_string()),
                },
                Err(error) => lines.push(format!("FAIL could not verify certificate confinement: {} (see README: Virtual Machine Bridge)", one_line(&error))),
            }

            match self.query(
                "/1.0/projects/claude-sandbox?project=claude-sandbox",
                Duration::from_secs(10),
            ) {
                Ok(project) => {
                    let config = project.get("config").and_then(Value::as_object);
                    if config.and_then(|value| value.get("restricted")).and_then(Value::as_str)
                        == Some("true")
                    {
                        lines.push("OK   Project claude-sandbox is readable and restricted".to_string());
                    } else {
                        lines.push("FAIL project claude-sandbox is not restricted; run `incus project set claude-sandbox restricted=true limits.virtual-machines=<n> limits.cpu=<n> limits.memory=<size> limits.disk=<size>` (see README: Virtual Machine Bridge)".to_string());
                    }

                    let disabled_features: Vec<&str> = [
                        "features.images",
                        "features.storage.volumes",
                        "features.profiles",
                    ]
                    .into_iter()
                    .filter(|key| {
                        config.and_then(|value| value.get(*key)).and_then(Value::as_str)
                            == Some("false")
                    })
                    .collect();
                    if disabled_features.is_empty() {
                        lines.push("OK   Project media and profile features are enabled".to_string());
                    } else {
                        let settings = disabled_features
                            .iter()
                            .map(|key| format!("{key}=true"))
                            .collect::<Vec<_>>()
                            .join(" ");
                        lines.push(format!("FAIL project features must not be disabled ({}); run `incus project set claude-sandbox {settings}`", disabled_features.join(", ")));
                    }

                    let missing_limits: Vec<&str> = [
                        "limits.virtual-machines",
                        "limits.cpu",
                        "limits.memory",
                        "limits.disk",
                    ]
                    .into_iter()
                    .filter(|key| {
                        config
                            .and_then(|value| value.get(*key))
                            .and_then(Value::as_str)
                            .is_none_or(str::is_empty)
                    })
                    .collect();
                    if missing_limits.is_empty() {
                        lines.push("OK   Project VM, CPU, memory, and disk limits are set".to_string());
                    } else {
                        let settings = missing_limits
                            .iter()
                            .map(|key| match *key {
                                "limits.virtual-machines" | "limits.cpu" => format!("{key}=<n>"),
                                _ => format!("{key}=<size>"),
                            })
                            .collect::<Vec<_>>()
                            .join(" ");
                        lines.push(format!("FAIL project resource limits are missing or empty ({}); run `incus project set claude-sandbox {settings}`", missing_limits.join(", ")));
                    }
                }
                Err(error) => lines.push(format!("FAIL project claude-sandbox is not readable: {}; create it with `incus project create claude-sandbox -c restricted=true -c limits.containers=0 -c limits.virtual-machines=<n> -c limits.cpu=<n> -c limits.memory=<size> -c limits.disk=<size>` (see README: Virtual Machine Bridge)", one_line(&error))),
            }

            match self.default_profile() {
                Ok((_, _, has_nic)) => {
                    lines.push("OK   Project default profile has a root disk with a storage pool".to_string());
                    if has_nic {
                        lines.push("OK   Project default profile has a NIC".to_string());
                    } else {
                        lines.push("WARN Project default profile has no NIC; add one with `incus profile device add default eth0 nic network=incusbr0 name=eth0 --project claude-sandbox` (see README: Virtual Machine Bridge)".to_string());
                    }
                }
                Err(error) => lines.push(format!("FAIL project default profile has no usable root disk: {}; run `incus profile device add default root disk path=/ pool=default --project claude-sandbox` (see README: Virtual Machine Bridge)", one_line(&error))),
            }
        } else {
            lines.push("FAIL cannot check server trust until the client and remote are configured (see README: Virtual Machine Bridge)".to_string());
            lines.push("FAIL cannot check certificate confinement until the client and remote are configured (see README: Virtual Machine Bridge)".to_string());
            lines.push("FAIL cannot check project restrictions until the client and remote are configured (see README: Virtual Machine Bridge)".to_string());
            lines.push("FAIL cannot check project media and profile features until the client and remote are configured (see README: Virtual Machine Bridge)".to_string());
            lines.push("FAIL cannot check project resource limits until the client and remote are configured (see README: Virtual Machine Bridge)".to_string());
            lines.push("FAIL cannot check the project default profile until the client and remote are configured (see README: Virtual Machine Bridge)".to_string());
        }
        let report = format!("Virtual Machine Bridge preflight:\n{}\n", lines.join("\n"));
        self.healthy
            .store(preflight_is_healthy(&report), Ordering::Release);
        report
    }

    fn default_profile(&self) -> Result<(String, String, bool), String> {
        let profile = self.query(
            "/1.0/profiles/default?project=claude-sandbox",
            Duration::from_secs(10),
        )?;
        let devices = profile
            .get("devices")
            .and_then(Value::as_object)
            .ok_or_else(|| "profile response has no devices".to_string())?;
        let (root_name, root_device) = devices
            .iter()
            .find(|(_, device)| {
                device.get("type").and_then(Value::as_str) == Some("disk")
                    && device.get("path").and_then(Value::as_str) == Some("/")
            })
            .ok_or_else(|| "profile has no root disk device".to_string())?;
        let pool = root_device
            .get("pool")
            .and_then(Value::as_str)
            .filter(|pool| !pool.is_empty())
            .ok_or_else(|| "root disk device has no pool".to_string())?;
        let has_nic = devices
            .values()
            .any(|device| device.get("type").and_then(Value::as_str) == Some("nic"));
        Ok((pool.to_string(), root_name.to_string(), has_nic))
    }

    fn ensure_healthy(&self) -> Result<(), Response> {
        if self.healthy.load(Ordering::Acquire) {
            return Ok(());
        }
        let report = self.checked_report();
        if preflight_is_healthy(&report) {
            Ok(())
        } else {
            Err(Response::error(report.trim_end()))
        }
    }

    // Leftover VMs from an earlier session are removed once, the first time the
    // setup is seen healthy; a later outage must not wipe this session's VMs.
    fn checked_report(&self) -> String {
        let report = self.preflight_report();
        if preflight_is_healthy(&report) && !self.cleaned.swap(true, Ordering::AcqRel) {
            self.cleanup_instances();
        }
        report
    }

    fn handle(&self, request: Request) -> Response {
        if self.shutting_down.load(Ordering::Acquire) {
            return Response::error("bridge is shutting down");
        }
        let parsed = match parse_args(&request.args) {
            Ok(command) => command,
            Err(error) => return Response::error(error),
        };
        if parsed == VmCommand::Help {
            return Response::ok(HELP);
        }
        if parsed == VmCommand::Status {
            let report = self.checked_report();
            return Response {
                exit_code: if preflight_is_healthy(&report) { 0 } else { 1 },
                stdout: report,
                stderr: String::new(),
            };
        }
        if let Err(response) = self.ensure_healthy() {
            return response;
        }

        match parsed {
            VmCommand::List { json } => self.list(json),
            VmCommand::Info { name, json } => self.info(&name, json),
            VmCommand::Launch(options) => self.launch(&options),
            VmCommand::Stop { name, force } => self.instance_action("stop", &name, force),
            VmCommand::Restart { name, force } => self.instance_action("restart", &name, force),
            VmCommand::Delete { name } => self.instance_action("delete", &name, true),
            VmCommand::Screen { name } => self.screen(&name),
            VmCommand::Exec {
                name,
                timeout,
                command,
            } => self.exec(&name, timeout, &command),
            VmCommand::ConsoleLog { name } => self.console_log(&name),
            VmCommand::Import { path, name } => self.import(&path, &name, request.cwd.as_deref()),
            VmCommand::Media { json } => self.media(json),
            VmCommand::MediaDelete { name } => self.media_delete(&name),
            VmCommand::Help | VmCommand::Status => unreachable!(),
        }
    }

    fn list(&self, as_json: bool) -> Response {
        let instances = match self.query(
            "/1.0/instances?recursion=2&project=claude-sandbox",
            DEFAULT_TIMEOUT,
        ) {
            Ok(value) => summarize_instances(&value),
            Err(error) => return Response::error(error),
        };
        if as_json {
            return json_response(&instances);
        }
        let mut output = String::new();
        for item in instances.as_array().into_iter().flatten() {
            output.push_str(&format!(
                "{}\t{}\t{}\t{}\t{}\t{}\n",
                string_field(item, "name"),
                string_field(item, "status"),
                string_array(item, "ipv4").join(","),
                string_array(item, "ipv6").join(","),
                string_field(item, "cpus"),
                string_field(item, "memory")
            ));
        }
        Response::ok(output)
    }

    fn info(&self, name: &str, as_json: bool) -> Response {
        let instances = match self.query(
            "/1.0/instances?recursion=2&project=claude-sandbox",
            DEFAULT_TIMEOUT,
        ) {
            Ok(value) => value,
            Err(error) => return Response::error(error),
        };
        let Some(instance) = instances
            .as_array()
            .into_iter()
            .flatten()
            .find(|instance| instance.get("name").and_then(Value::as_str) == Some(name))
            .map(summarize_instance)
        else {
            return Response::error(format!("instance '{name}' does not exist"));
        };
        if as_json {
            return json_response(&instance);
        }
        Response::ok(format!(
            "name: {}\nstatus: {}\nipv4: {}\nipv6: {}\ncpus: {}\nmemory: {}\n",
            string_field(&instance, "name"),
            string_field(&instance, "status"),
            string_array(&instance, "ipv4").join(", "),
            string_array(&instance, "ipv6").join(", "),
            string_field(&instance, "cpus"),
            string_field(&instance, "memory")
        ))
    }

    fn launch(&self, options: &LaunchOptions) -> Response {
        self.stop_relay(&options.name);
        let wanted = match &options.source {
            LaunchSource::PublicImage(_) => None,
            LaunchSource::LocalImage(name) => Some((name, "qcow2", "--image")),
            LaunchSource::Iso(name) => Some((name, "iso", "--iso")),
        };
        if let Some((name, kind, flag)) = wanted {
            let media = match self.media_value() {
                Ok(media) => media,
                Err(error) => return Response::error(error),
            };
            let found = media.as_array().into_iter().flatten().any(|entry| {
                string_field(entry, "name") == name && string_field(entry, "type") == kind
            });
            if !found {
                return Response::error(format!(
                    "{flag} '{name}' is not an imported {kind} (see `vm media`; import with `vm import PATH NAME`)"
                ));
            }
        }
        let (pool, root_device, _) = match self.default_profile() {
            Ok(profile) => profile,
            Err(error) => return Response::error(error),
        };
        let (args, body) = build_launch(options, &pool, &root_device, self.remote, self.project);
        let output = match self.run_command(
            &args,
            CommandInput::Bytes(body.into_bytes()),
            LONG_LAUNCH_TIMEOUT,
            COMMAND_OUTPUT_LIMIT,
        ) {
            Ok(output) => output,
            Err(error) => {
                self.force_delete(&options.name);
                return Response::error(format!("{error}; deleted any half-created VM"));
            }
        };
        if output.timed_out {
            self.force_delete(&options.name);
            return Response::error(
                "Incus create timed out after 1800s; deleted any half-created VM",
            );
        }
        if !output.status.success() {
            self.force_delete(&options.name);
            let mut response = command_response(output);
            response
                .stderr
                .push_str("vm: deleted any half-created VM\n");
            return response;
        }
        let start_args = vec![
            "start".to_string(),
            "--project".to_string(),
            self.project.to_string(),
            format!("{}:{}", self.remote, options.name),
        ];
        match self.run_command(
            &start_args,
            CommandInput::Null,
            LONG_LAUNCH_TIMEOUT,
            COMMAND_OUTPUT_LIMIT,
        ) {
            Ok(output) if output.status.success() && !output.timed_out => {
                Response::ok(format!("started {}\n", options.name))
            }
            Ok(output) => {
                self.force_delete(&options.name);
                if output.timed_out {
                    Response::error(
                        "Incus start timed out after 1800s; deleted the half-created VM",
                    )
                } else {
                    let mut response = command_response(output);
                    response
                        .stderr
                        .push_str("vm: deleted the half-created VM\n");
                    response
                }
            }
            Err(error) => {
                self.force_delete(&options.name);
                Response::error(format!("{error}; deleted the half-created VM"))
            }
        }
    }

    fn instance_action(&self, verb: &str, name: &str, force: bool) -> Response {
        if matches!(verb, "stop" | "delete") {
            self.stop_relay(name);
        }
        let mut args = vec![
            verb.to_string(),
            "--project".to_string(),
            self.project.to_string(),
        ];
        if force {
            args.push("--force".to_string());
        }
        args.push(format!("{}:{name}", self.remote));
        match self.run_command(
            &args,
            CommandInput::Null,
            DEFAULT_TIMEOUT,
            COMMAND_OUTPUT_LIMIT,
        ) {
            Ok(output) if output.timed_out => {
                if verb == "stop" && !force {
                    Response::error(format!(
                        "guest did not respond to the shutdown request within {}s; retry with `vm stop {name} --force`",
                        DEFAULT_TIMEOUT.as_secs()
                    ))
                } else {
                    Response::error(format!(
                        "Incus {verb} timed out after {}s",
                        DEFAULT_TIMEOUT.as_secs()
                    ))
                }
            }
            Ok(output) => command_response(output),
            Err(error) => Response::error(error),
        }
    }

    fn exec(&self, name: &str, timeout: Duration, guest_command: &[String]) -> Response {
        let mut args = vec![
            "exec".to_string(),
            "-T".to_string(),
            "-n".to_string(),
            "--project".to_string(),
            self.project.to_string(),
            format!("{}:{name}", self.remote),
            "--".to_string(),
        ];
        args.extend_from_slice(guest_command);
        match self.run_command(&args, CommandInput::Null, timeout, EXEC_OUTPUT_LIMIT) {
            Ok(mut output) if output.timed_out => {
                append_notice_capped(
                    &mut output.stderr,
                    EXEC_OUTPUT_LIMIT,
                    &format!("vm: exec timed out after {}s\n", timeout.as_secs()),
                );
                Response {
                    exit_code: 124,
                    stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                    stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
                }
            }
            Ok(output) => command_response(output),
            Err(error) => Response::error(error),
        }
    }

    fn console_log(&self, name: &str) -> Response {
        let args = vec![
            "console".to_string(),
            "--show-log".to_string(),
            "--project".to_string(),
            self.project.to_string(),
            format!("{}:{name}", self.remote),
        ];
        match self.run_command(
            &args,
            CommandInput::Null,
            DEFAULT_TIMEOUT,
            COMMAND_OUTPUT_LIMIT,
        ) {
            Ok(output) => {
                let message = b"Instance is not container type";
                let too_old = output
                    .stderr
                    .windows(message.len())
                    .any(|window| window == message)
                    || output
                        .stdout
                        .windows(message.len())
                        .any(|window| window == message);
                let mut response = command_response(output);
                if too_old {
                    response.stderr.push_str(
                        "vm: the host's Incus server is too old for VM console logs; use a version newer than 6.0.0\n",
                    );
                }
                response
            }
            Err(error) => Response::error(error),
        }
    }

    fn import(&self, path: &str, name: &str, cwd: Option<&str>) -> Response {
        let source = match self.open_workspace_file(path, cwd) {
            Ok(file) => file,
            Err(error) => return Response::error(error),
        };
        self.import_opened(source, name)
    }

    fn import_opened(&self, mut source: File, name: &str) -> Response {
        let _import = self.import_lock.lock().expect("import lock poisoned");
        let source_size = match source.metadata() {
            Ok(metadata) => metadata.len(),
            Err(error) => {
                return Response::error(format!("could not inspect import size: {error}"));
            }
        };
        if source_size > self.max_import_size {
            return Response::error(format!(
                "import exceeds the {} byte maximum",
                self.max_import_size
            ));
        }
        match self.media_names() {
            Ok(names) if names.iter().any(|existing| existing == name) => {
                return Response::error(format!("media name '{name}' already exists"));
            }
            Ok(names) if names.len() >= self.max_media => {
                return Response::error(format!(
                    "retained media limit ({}) reached; run `vm media-delete NAME` before importing another image",
                    self.max_media
                ));
            }
            Err(error) => return Response::error(error),
            _ => {}
        }
        let (mut file, _snapshot, snapshot_size) = match self.snapshot_file(&mut source) {
            Ok(snapshot) => snapshot,
            Err(error) => return Response::error(error),
        };
        let kind = match inspect_media(&mut file) {
            Ok(kind) => kind,
            Err(error) => return Response::error(error),
        };
        let type_cap = match kind {
            MediaKind::Iso => self.max_iso_size,
            MediaKind::Qcow2 => self.max_import_size,
        };
        if snapshot_size > type_cap {
            return Response::error(format!(
                "{} import exceeds the {} byte maximum",
                match kind {
                    MediaKind::Iso => "ISO",
                    MediaKind::Qcow2 => "qcow2",
                },
                type_cap
            ));
        }
        if let Err(error) = file.seek(SeekFrom::Start(0)) {
            return Response::error(format!("could not rewind import file: {error}"));
        }

        match kind {
            MediaKind::Iso => {
                let (pool, _, _) = match self.default_profile() {
                    Ok(profile) => profile,
                    Err(error) => return Response::error(error),
                };
                let args = vec![
                    "storage".to_string(),
                    "volume".to_string(),
                    "import".to_string(),
                    "--type=iso".to_string(),
                    "--project".to_string(),
                    self.project.to_string(),
                    format!("{}:{pool}", self.remote),
                    "/dev/stdin".to_string(),
                    name.to_string(),
                ];
                match self.run_command(
                    &args,
                    CommandInput::File(file),
                    IMPORT_TIMEOUT,
                    COMMAND_OUTPUT_LIMIT,
                ) {
                    Ok(output) => command_response(output),
                    Err(error) => Response::error(error),
                }
            }
            MediaKind::Qcow2 => self.import_qcow2(file, name),
        }
    }

    fn prepare_snapshot_dir(&self) -> Result<(), String> {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&self.snapshot_dir)
            .map_err(|error| format!("could not create import snapshot directory: {error}"))?;
        fs::set_permissions(&self.snapshot_dir, fs::Permissions::from_mode(0o700))
            .map_err(|error| format!("could not secure import snapshot directory: {error}"))
    }

    fn cleanup_snapshots(&self) -> Result<(), String> {
        if paths_overlap(
            &canonicalize_for_comparison(&self.workspace_root),
            &canonicalize_for_comparison(&self.snapshot_dir),
        ) {
            return Err(
                "refusing to clean an import snapshot directory that overlaps the workspace"
                    .to_string(),
            );
        }
        self.prepare_snapshot_dir()?;
        let entries = fs::read_dir(&self.snapshot_dir)
            .map_err(|error| format!("could not scan import snapshot directory: {error}"))?;
        for entry in entries {
            let entry =
                entry.map_err(|error| format!("could not read import snapshot entry: {error}"))?;
            let file_type = entry
                .file_type()
                .map_err(|error| format!("could not inspect import snapshot entry: {error}"))?;
            if file_type.is_file() || file_type.is_symlink() {
                fs::remove_file(entry.path())
                    .map_err(|error| format!("could not remove stale import snapshot: {error}"))?;
            }
        }
        Ok(())
    }

    fn snapshot_file(&self, source: &mut File) -> Result<(File, Snapshot, u64), String> {
        self.prepare_snapshot_dir()?;
        source
            .seek(SeekFrom::Start(0))
            .map_err(|error| format!("could not rewind import file: {error}"))?;
        let mut random = [0_u8; 16];
        File::open("/dev/urandom")
            .and_then(|mut file| file.read_exact(&mut random))
            .map_err(|error| format!("could not generate import snapshot name: {error}"))?;
        let random = random
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let path = self
            .snapshot_dir
            .join(format!("import-{}-{random}", process::id()));
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .map_err(|error| format!("could not create import snapshot: {error}"))?;
        let snapshot = Snapshot { path: path.clone() };
        let copied = std::io::copy(
            &mut source.take(self.max_import_size.saturating_add(1)),
            &mut output,
        )
        .map_err(|error| format!("could not copy import snapshot: {error}"))?;
        if copied > self.max_import_size {
            return Err(format!(
                "import exceeds the {} byte maximum",
                self.max_import_size
            ));
        }
        output
            .flush()
            .map_err(|error| format!("could not finish import snapshot: {error}"))?;
        drop(output);
        let file = File::open(&path)
            .map_err(|error| format!("could not reopen import snapshot: {error}"))?;
        Ok((file, snapshot, copied))
    }

    fn import_qcow2(&self, file: File, name: &str) -> Response {
        let metadata_path = match make_metadata_tarball(&self.runtime_dir) {
            Ok(path) => path,
            Err(error) => return Response::error(error),
        };
        let args = vec![
            "image".to_string(),
            "import".to_string(),
            "--alias".to_string(),
            name.to_string(),
            "--project".to_string(),
            self.project.to_string(),
            metadata_path.to_string_lossy().into_owned(),
            "/dev/stdin".to_string(),
            format!("{}:", self.remote),
        ];
        let result = self.run_command(
            &args,
            CommandInput::File(file),
            IMPORT_TIMEOUT,
            COMMAND_OUTPUT_LIMIT,
        );
        let _ = fs::remove_file(&metadata_path);
        match result {
            Ok(output) => command_response(output),
            Err(error) => Response::error(error),
        }
    }

    fn open_workspace_file(&self, requested: &str, cwd: Option<&str>) -> Result<File, String> {
        let requested = Path::new(requested);
        let host_path = if requested.is_absolute() {
            let relative = clean_container_relative(requested)?;
            self.workspace_root.join(relative)
        } else {
            let cwd =
                cwd.ok_or_else(|| "request did not include a working directory".to_string())?;
            let cwd_relative = clean_container_relative(Path::new(cwd))?;
            let relative = clean_relative(requested)?;
            self.workspace_root.join(cwd_relative).join(relative)
        };
        let error = || {
            format!(
                "cannot import {}: not a regular file inside the workspace",
                requested.display()
            )
        };
        let canonical = fs::canonicalize(&host_path).map_err(|_| error())?;
        if !canonical.starts_with(&self.workspace_root) {
            return Err(error());
        }
        let Some(file) = open_untrusted_path(&host_path) else {
            return Err(error());
        };
        let metadata = file.metadata().map_err(|_| error())?;
        if !metadata.is_file() {
            return Err(error());
        }
        let resolved = fs::canonicalize(proc_fd_path(&file)).map_err(|_| error())?;
        if !resolved.starts_with(&self.workspace_root) {
            return Err(error());
        }
        Ok(file)
    }

    fn media_value(&self) -> Result<Value, String> {
        let (pool, _, _) = self.default_profile()?;
        let volumes = self.query(
            &format!("/1.0/storage-pools/{pool}/volumes/custom?recursion=1&project=claude-sandbox"),
            DEFAULT_TIMEOUT,
        )?;
        let images = self.query(
            "/1.0/images?recursion=1&project=claude-sandbox",
            DEFAULT_TIMEOUT,
        )?;
        let mut media = Vec::new();
        for volume in volumes.as_array().into_iter().flatten() {
            if volume.get("content_type").and_then(Value::as_str) == Some("iso")
                && let Some(name) = volume.get("name").and_then(Value::as_str)
            {
                media.push(json!({"name": name, "type": "iso"}));
            }
        }
        for image in images.as_array().into_iter().flatten() {
            for alias in image
                .get("aliases")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if let Some(name) = alias.get("name").and_then(Value::as_str) {
                    media.push(json!({"name": name, "type": "qcow2"}));
                }
            }
        }
        media.sort_by(|left, right| string_field(left, "name").cmp(string_field(right, "name")));
        Ok(Value::Array(media))
    }

    fn media_names(&self) -> Result<Vec<String>, String> {
        Ok(self
            .media_value()?
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|entry| {
                entry
                    .get("name")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .collect())
    }

    fn media(&self, as_json: bool) -> Response {
        let media = match self.media_value() {
            Ok(media) => media,
            Err(error) => return Response::error(error),
        };
        if as_json {
            return json_response(&media);
        }
        let mut output = String::new();
        for entry in media.as_array().into_iter().flatten() {
            output.push_str(&format!(
                "{}\t{}\n",
                string_field(entry, "name"),
                string_field(entry, "type")
            ));
        }
        Response::ok(output)
    }

    fn media_delete(&self, name: &str) -> Response {
        let media = match self.media_value() {
            Ok(media) => media,
            Err(error) => return Response::error(error),
        };
        let Some(kind) = media.as_array().into_iter().flatten().find_map(|entry| {
            (entry.get("name").and_then(Value::as_str) == Some(name))
                .then(|| string_field(entry, "type").to_string())
        }) else {
            return Response::error(format!("media '{name}' does not exist"));
        };
        let args = if kind == "iso" {
            let (pool, _, _) = match self.default_profile() {
                Ok(profile) => profile,
                Err(error) => return Response::error(error),
            };
            vec![
                "storage".to_string(),
                "volume".to_string(),
                "delete".to_string(),
                "--project".to_string(),
                self.project.to_string(),
                format!("{}:{pool}", self.remote),
                name.to_string(),
            ]
        } else {
            vec![
                "image".to_string(),
                "delete".to_string(),
                "--project".to_string(),
                self.project.to_string(),
                format!("{}:{name}", self.remote),
            ]
        };
        match self.run_command(
            &args,
            CommandInput::Null,
            DEFAULT_TIMEOUT,
            COMMAND_OUTPUT_LIMIT,
        ) {
            Ok(output) => command_response(output),
            Err(error) => Response::error(error),
        }
    }

    fn screen(&self, name: &str) -> Response {
        let _starting = self.relay_start.lock().expect("relay start lock poisoned");
        if let Some(relay) = self.relays.lock().expect("relay lock poisoned").get(name)
            && relay
                .child
                .lock()
                .expect("relay child lock poisoned")
                .try_wait()
                .ok()
                == Some(None)
            && relay.socket_path.exists()
        {
            return Response::ok(format!(
                "spice+unix:///run/claude-sandbox/vm-{name}.spice\n"
            ));
        }
        self.stop_relay(name);
        match self.start_relay(name) {
            Ok(()) => Response::ok(format!(
                "spice+unix:///run/claude-sandbox/vm-{name}.spice\n"
            )),
            Err(error) => Response::error(error),
        }
    }

    fn start_relay(&self, name: &str) -> Result<(), String> {
        let public_path = self.runtime_dir.join(format!("vm-{name}.spice"));
        let _ = fs::remove_file(&public_path);
        let mut command = self.command()?;
        let mut child = command
            .args([
                "console",
                "--type=vga",
                "--project",
                self.project,
                &format!("{}:{name}", self.remote),
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| format!("failed to start Incus console: {error}"))?;
        let stdout = child.stdout.take().expect("piped stdout missing");
        let stderr = child.stderr.take().expect("piped stderr missing");
        let (line_tx, line_rx) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                if line_tx.send(line).is_err() {
                    break;
                }
            }
        });
        let stderr_bytes = Arc::new(Mutex::new(Vec::new()));
        let stderr_target = Arc::clone(&stderr_bytes);
        thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = stderr
                .take(COMMAND_OUTPUT_LIMIT as u64 + 1)
                .read_to_end(&mut bytes);
            *stderr_target.lock().expect("relay stderr lock poisoned") = bytes;
        });

        let deadline = Instant::now() + SCREEN_START_TIMEOUT;
        let mut upstream = None;
        while Instant::now() < deadline {
            match line_rx.recv_timeout(Duration::from_millis(100)) {
                Ok(Ok(line)) => {
                    if let Some(path) = spice_socket_path(&line) {
                        upstream = Some(path);
                        break;
                    }
                }
                Ok(Err(error)) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!("failed reading Incus console output: {error}"));
                }
                Err(mpsc::RecvTimeoutError::Timeout) => match child.try_wait() {
                    Ok(Some(status)) => {
                        let stderr = String::from_utf8_lossy(
                            &stderr_bytes.lock().expect("relay stderr lock poisoned"),
                        )
                        .into_owned();
                        let _ = child.wait();
                        return Err(format!(
                            "Incus console exited with {status}: {}",
                            stderr.trim()
                        ));
                    }
                    Ok(None) => {}
                    Err(error) => {
                        let _ = child.kill();
                        let _ = child.wait();
                        return Err(format!("failed checking Incus console: {error}"));
                    }
                },
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        let Some(upstream) = upstream else {
            let _ = child.kill();
            let _ = child.wait();
            let stderr =
                String::from_utf8_lossy(&stderr_bytes.lock().expect("relay stderr lock poisoned"))
                    .into_owned();
            return Err(format!(
                "Incus console did not provide a SPICE socket within 15s: {}",
                stderr.trim()
            ));
        };
        let listener = match UnixListener::bind(&public_path) {
            Ok(listener) => listener,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("could not bind SPICE relay: {error}"));
            }
        };
        if let Err(error) = fs::set_permissions(&public_path, fs::Permissions::from_mode(0o600)) {
            let _ = child.kill();
            let _ = child.wait();
            let _ = fs::remove_file(&public_path);
            return Err(format!("could not secure SPICE relay: {error}"));
        }
        if let Err(error) = listener.set_nonblocking(true) {
            let _ = child.kill();
            let _ = child.wait();
            let _ = fs::remove_file(&public_path);
            return Err(format!("could not configure SPICE relay: {error}"));
        }
        let child = Arc::new(Mutex::new(child));
        let connections = Arc::new(Mutex::new(HashMap::new()));
        let connection_count = Arc::new(AtomicUsize::new(0));
        let stopping = Arc::new(AtomicBool::new(false));
        let relay = Relay {
            child: Arc::clone(&child),
            socket_path: public_path.clone(),
            upstream_path: upstream.clone(),
            connections: Arc::clone(&connections),
            stopping: Arc::clone(&stopping),
        };
        self.relays
            .lock()
            .expect("relay lock poisoned")
            .insert(name.to_string(), relay);
        let relays = Arc::clone(&self.relays);
        let relay_name = name.to_string();
        let max_connections = self.max_relay_connections;
        let idle_timeout = self.relay_idle_timeout;
        thread::spawn(move || {
            let started = Instant::now();
            let connection_ids = AtomicU64::new(0);
            let mut had_connection = false;
            loop {
                match listener.accept() {
                    Ok((client, _)) => {
                        if !reserve_connection(&connection_count, max_connections) {
                            let _ = client.shutdown(Shutdown::Both);
                            continue;
                        }
                        let Ok(client_clone) = client.try_clone() else {
                            let _ = client.shutdown(Shutdown::Both);
                            connection_count.fetch_sub(1, Ordering::AcqRel);
                            continue;
                        };
                        let id = connection_ids.fetch_add(1, Ordering::Relaxed);
                        {
                            let mut active =
                                connections.lock().expect("relay connection lock poisoned");
                            if stopping.load(Ordering::Acquire) {
                                let _ = client.shutdown(Shutdown::Both);
                                connection_count.fetch_sub(1, Ordering::AcqRel);
                                continue;
                            }
                            active.insert(
                                id,
                                RelayConnection {
                                    client: client_clone,
                                    upstream: None,
                                },
                            );
                        }
                        had_connection = true;
                        let upstream = upstream.clone();
                        let connections = Arc::clone(&connections);
                        let connection_count = Arc::clone(&connection_count);
                        let stopping = Arc::clone(&stopping);
                        thread::spawn(move || {
                            if stopping.load(Ordering::Acquire) {
                                connections
                                    .lock()
                                    .expect("relay connection lock poisoned")
                                    .remove(&id);
                                connection_count.fetch_sub(1, Ordering::AcqRel);
                                return;
                            }
                            let Ok(server) = UnixStream::connect(upstream) else {
                                connections
                                    .lock()
                                    .expect("relay connection lock poisoned")
                                    .remove(&id);
                                connection_count.fetch_sub(1, Ordering::AcqRel);
                                return;
                            };
                            let Ok(server_clone) = server.try_clone() else {
                                let _ = client.shutdown(Shutdown::Both);
                                let _ = server.shutdown(Shutdown::Both);
                                connections
                                    .lock()
                                    .expect("relay connection lock poisoned")
                                    .remove(&id);
                                connection_count.fetch_sub(1, Ordering::AcqRel);
                                return;
                            };
                            {
                                let mut active =
                                    connections.lock().expect("relay connection lock poisoned");
                                if stopping.load(Ordering::Acquire) {
                                    let _ = client.shutdown(Shutdown::Both);
                                    let _ = server.shutdown(Shutdown::Both);
                                    active.remove(&id);
                                    connection_count.fetch_sub(1, Ordering::AcqRel);
                                    return;
                                }
                                if let Some(connection) = active.get_mut(&id) {
                                    connection.upstream = Some(server_clone);
                                }
                            }
                            copy_bidirectional(client, server);
                            connections
                                .lock()
                                .expect("relay connection lock poisoned")
                                .remove(&id);
                            connection_count.fetch_sub(1, Ordering::AcqRel);
                        });
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(_) => break,
                }
                let child_done = child
                    .lock()
                    .expect("relay child lock poisoned")
                    .try_wait()
                    .map_or(true, |status| status.is_some());
                if child_done || (!had_connection && started.elapsed() >= idle_timeout) {
                    break;
                }
                thread::sleep(Duration::from_millis(25));
            }
            stopping.store(true, Ordering::Release);
            shutdown_connections(&connections);
            {
                let mut child = child.lock().expect("relay child lock poisoned");
                let _ = child.kill();
                let _ = child.wait();
            }
            let mut map = relays.lock().expect("relay lock poisoned");
            if map
                .get(&relay_name)
                .is_some_and(|relay| Arc::ptr_eq(&relay.child, &child))
            {
                map.remove(&relay_name);
                let _ = fs::remove_file(&public_path);
                let _ = fs::remove_file(&upstream);
            }
        });
        Ok(())
    }

    fn stop_relay(&self, name: &str) {
        if let Some(relay) = self
            .relays
            .lock()
            .expect("relay lock poisoned")
            .remove(name)
        {
            relay.stopping.store(true, Ordering::Release);
            shutdown_connections(&relay.connections);
            let mut child = relay.child.lock().expect("relay child lock poisoned");
            let _ = child.kill();
            let _ = child.wait();
            let _ = fs::remove_file(relay.socket_path);
            // A killed client cannot remove its own socket.
            let _ = fs::remove_file(relay.upstream_path);
        }
    }

    fn stop_all_relays(&self) {
        let names: Vec<String> = self
            .relays
            .lock()
            .expect("relay lock poisoned")
            .keys()
            .cloned()
            .collect();
        for name in names {
            self.stop_relay(&name);
        }
    }

    fn force_delete(&self, name: &str) {
        self.stop_relay(name);
        let args = vec![
            "delete".to_string(),
            "--force".to_string(),
            "--project".to_string(),
            self.project.to_string(),
            format!("{}:{name}", self.remote),
        ];
        let _ = self.run_command(
            &args,
            CommandInput::Null,
            DEFAULT_TIMEOUT,
            COMMAND_OUTPUT_LIMIT,
        );
    }

    fn cleanup_instances(&self) {
        self.stop_all_relays();
        if let Ok(instances) = self.query(
            "/1.0/instances?recursion=1&project=claude-sandbox",
            Duration::from_secs(30),
        ) {
            for instance in instances.as_array().into_iter().flatten() {
                if let Some(name) = instance.get("name").and_then(Value::as_str) {
                    self.force_delete(name);
                }
            }
        }
    }
}

pub fn preflight(home: &Path, workspace_root: &Path, runtime_dir: &Path) -> String {
    IncusBridge::new(home, workspace_root, runtime_dir).preflight_report()
}

pub fn preflight_is_healthy(report: &str) -> bool {
    !report.lines().any(|line| line.starts_with("FAIL "))
}

pub fn run(
    socket_path: &str,
    log_path: &Path,
    workspace_root: &Path,
    runtime_dir: &Path,
    parent_pid: u32,
) {
    let log_file = proxy_log::open(log_path).unwrap_or_else(|error| {
        eprintln!(
            "vm-proxy: failed to open log {}: {error}",
            log_path.display()
        );
        process::exit(1);
    });
    let log = Arc::new(Mutex::new(log_file));
    let home = env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/root"));
    let bridge = Arc::new(IncusBridge::new(&home, workspace_root, runtime_dir));
    let bound = proxy_socket::bind(Path::new(socket_path)).unwrap_or_else(|error| {
        eprintln!("vm-proxy: failed to bind {socket_path}: {error}");
        process::exit(1);
    });
    let listener = bound.listener;
    let socket_identity = bound.identity;
    log_line(&log, &format!("listening on {socket_path}"));

    let watchdog_bridge = Arc::clone(&bridge);
    let watchdog_log = Arc::clone(&log);
    thread::spawn(move || {
        loop {
            let current_ppid = std::os::unix::process::parent_id();
            if current_ppid != parent_pid {
                watchdog_bridge.shutting_down.store(true, Ordering::Release);
                log_line(
                    &watchdog_log,
                    &format!("parent {parent_pid} exited (ppid now {current_ppid}), cleaning up"),
                );
                watchdog_bridge.cleanup_instances();
                thread::sleep(Duration::from_secs(2));
                watchdog_bridge.cleanup_instances();
                let _ = socket_identity.remove_if_owned();
                process::exit(0);
            }
            thread::sleep(Duration::from_secs(2));
        }
    });

    if let Err(error) = bridge.cleanup_snapshots() {
        log_line(&log, &format!("ERROR   startup snapshot cleanup: {error}"));
    }
    bridge.checked_report();

    let requests = Arc::new(AtomicUsize::new(0));
    let mut last_accept_log = None;
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                if let Err(error) = stream.set_read_timeout(Some(REQUEST_TIMEOUT)) {
                    log_line(&log, &format!("ERROR   request read timeout: {error}"));
                    continue;
                }
                if let Err(error) = stream.set_write_timeout(Some(REQUEST_TIMEOUT)) {
                    log_line(&log, &format!("ERROR   request write timeout: {error}"));
                    continue;
                }
                if !reserve_connection(&requests, DEFAULT_MAX_REQUESTS) {
                    let mut writer = &stream;
                    let response = Response::error("bridge is busy; retry shortly");
                    let _ = serde_json::to_writer(&mut writer, &response);
                    let _ = writer.write_all(b"\n");
                    let _ = stream.shutdown(Shutdown::Both);
                    continue;
                }
                let bridge = Arc::clone(&bridge);
                let log = Arc::clone(&log);
                let requests = Arc::clone(&requests);
                thread::spawn(move || {
                    let _slot = RequestSlot(requests);
                    let reader = BufReader::new(&stream);
                    let mut writer = &stream;
                    let mut line = String::new();
                    if let Ok(n) = reader.take(1_048_576).read_line(&mut line) {
                        if n == 0 {
                            return;
                        }
                        let response = match serde_json::from_str::<Request>(&line) {
                            Ok(request) => {
                                let display = truncate_log(escape_log(&request.args.join(" ")));
                                let denied = parse_args(&request.args).is_err();
                                let response = bridge.handle(request);
                                let outcome = if denied {
                                    "DENIED "
                                } else if response.exit_code == 0 {
                                    "ALLOWED"
                                } else {
                                    "ERROR  "
                                };
                                log_line(
                                    &log,
                                    &format!("{outcome} vm {display} -> {}", response.exit_code),
                                );
                                response
                            }
                            Err(error) => {
                                log_line(&log, &format!("ERROR   invalid request: {error}"));
                                Response::error(format!("invalid request: {error}"))
                            }
                        };
                        let _ = serde_json::to_writer(&mut writer, &response);
                        let _ = writer.write_all(b"\n");
                    }
                });
            }
            Err(error) => {
                if last_accept_log
                    .is_none_or(|last: Instant| last.elapsed() >= Duration::from_secs(1))
                {
                    log_line(&log, &format!("ERROR   connection: {error}"));
                    last_accept_log = Some(Instant::now());
                }
                thread::sleep(Duration::from_millis(100));
            }
        }
    }
}

fn parse_args(args: &[String]) -> Result<VmCommand, String> {
    let Some(verb) = args.first().map(String::as_str) else {
        return Ok(VmCommand::Help);
    };
    match verb {
        "help" if args.len() == 1 => Ok(VmCommand::Help),
        "status" if args.len() == 1 => Ok(VmCommand::Status),
        "list" => Ok(VmCommand::List {
            json: parse_optional_json(&args[1..])?,
        }),
        "info" => {
            let (name, json) = parse_name_and_json(&args[1..])?;
            Ok(VmCommand::Info { name, json })
        }
        "launch" => parse_launch(&args[1..]).map(VmCommand::Launch),
        "stop" => {
            parse_action(&args[1..], true).map(|(name, force)| VmCommand::Stop { name, force })
        }
        "restart" => {
            parse_action(&args[1..], true).map(|(name, force)| VmCommand::Restart { name, force })
        }
        "delete" => parse_single_name(&args[1..]).map(|name| VmCommand::Delete { name }),
        "screen" => parse_single_name(&args[1..]).map(|name| VmCommand::Screen { name }),
        "console-log" => parse_single_name(&args[1..]).map(|name| VmCommand::ConsoleLog { name }),
        "exec" => parse_exec(&args[1..]),
        "import" if args.len() == 3 => {
            validate_name(&args[2])?;
            Ok(VmCommand::Import {
                path: args[1].clone(),
                name: args[2].clone(),
            })
        }
        "media" => Ok(VmCommand::Media {
            json: parse_optional_json(&args[1..])?,
        }),
        "media-delete" => parse_single_name(&args[1..]).map(|name| VmCommand::MediaDelete { name }),
        _ => Err(format!("unknown or invalid command\n\n{HELP}")),
    }
}

fn parse_optional_json(args: &[String]) -> Result<bool, String> {
    match args {
        [] => Ok(false),
        [flag] if flag == "--json" => Ok(true),
        _ => Err("expected no arguments or --json".to_string()),
    }
}

fn parse_name_and_json(args: &[String]) -> Result<(String, bool), String> {
    match args {
        [name] => {
            validate_name(name)?;
            Ok((name.clone(), false))
        }
        [name, flag] if flag == "--json" => {
            validate_name(name)?;
            Ok((name.clone(), true))
        }
        _ => Err("usage: vm info NAME [--json]".to_string()),
    }
}

fn parse_single_name(args: &[String]) -> Result<String, String> {
    let [name] = args else {
        return Err("expected exactly one instance or media name".to_string());
    };
    validate_name(name)?;
    Ok(name.clone())
}

fn parse_action(args: &[String], allow_force: bool) -> Result<(String, bool), String> {
    match args {
        [name] => {
            validate_name(name)?;
            Ok((name.clone(), false))
        }
        [name, flag] if allow_force && flag == "--force" => {
            validate_name(name)?;
            Ok((name.clone(), true))
        }
        _ => Err("expected NAME [--force]".to_string()),
    }
}

fn parse_launch(args: &[String]) -> Result<LaunchOptions, String> {
    let Some(name) = args.first() else {
        return Err("usage: vm launch NAME (--image IMAGE | --iso MEDIA) [options]".to_string());
    };
    validate_name(name)?;
    let mut source = None;
    let mut cpus = 2;
    let mut memory = "4GiB".to_string();
    let mut disk = "20GiB".to_string();
    let mut no_secureboot = false;
    let mut index = 1;
    while index < args.len() {
        match args[index].as_str() {
            "--image" => {
                let value = required_value(args, &mut index, "--image")?;
                if source.is_some() {
                    return Err("exactly one of --image or --iso is required".to_string());
                }
                source = Some(if value.starts_with("images:") {
                    validate_public_alias(value)?;
                    LaunchSource::PublicImage(value.to_string())
                } else {
                    validate_name(value)?;
                    LaunchSource::LocalImage(value.to_string())
                });
            }
            "--iso" => {
                let value = required_value(args, &mut index, "--iso")?;
                validate_name(value)?;
                if source.is_some() {
                    return Err("exactly one of --image or --iso is required".to_string());
                }
                source = Some(LaunchSource::Iso(value.to_string()));
            }
            "--cpus" => {
                let value = required_value(args, &mut index, "--cpus")?;
                cpus = value
                    .parse::<u8>()
                    .ok()
                    .filter(|value| (1..=64).contains(value))
                    .ok_or_else(|| "CPU count must be an integer from 1 through 64".to_string())?;
            }
            "--memory" => {
                let value = required_value(args, &mut index, "--memory")?;
                validate_size(value, SizeKind::Memory)?;
                memory = value.to_string();
            }
            "--disk" => {
                let value = required_value(args, &mut index, "--disk")?;
                validate_size(value, SizeKind::Disk)?;
                disk = value.to_string();
            }
            "--no-secureboot" => no_secureboot = true,
            flag => return Err(format!("unknown launch option: {flag}")),
        }
        index += 1;
    }
    let source = source.ok_or_else(|| "exactly one of --image or --iso is required".to_string())?;
    Ok(LaunchOptions {
        name: name.clone(),
        source,
        cpus,
        memory,
        disk,
        no_secureboot,
    })
}

fn parse_exec(args: &[String]) -> Result<VmCommand, String> {
    let Some(name) = args.first() else {
        return Err("usage: vm exec NAME [--timeout SECS] -- CMD [ARG...]".to_string());
    };
    validate_name(name)?;
    let mut timeout = DEFAULT_TIMEOUT;
    let mut index = 1;
    if args.get(index).map(String::as_str) == Some("--timeout") {
        let value = required_value(args, &mut index, "--timeout")?;
        let seconds = value
            .parse::<u64>()
            .ok()
            .filter(|value| (1..=3600).contains(value))
            .ok_or_else(|| "exec timeout must be an integer from 1 through 3600".to_string())?;
        timeout = Duration::from_secs(seconds);
        index += 1;
    }
    if args.get(index).map(String::as_str) != Some("--") {
        return Err("vm exec requires -- before the guest command".to_string());
    }
    let command = args[index + 1..].to_vec();
    if command.is_empty() {
        return Err("vm exec requires a guest command after --".to_string());
    }
    Ok(VmCommand::Exec {
        name: name.clone(),
        timeout,
        command,
    })
}

fn required_value<'a>(
    args: &'a [String],
    index: &mut usize,
    flag: &str,
) -> Result<&'a str, String> {
    *index += 1;
    args.get(*index)
        .map(String::as_str)
        .filter(|value| !value.starts_with('-'))
        .ok_or_else(|| format!("{flag} requires a value"))
}

fn valid_name(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.is_empty() || bytes.len() > 32 || !bytes[0].is_ascii_lowercase() {
        return false;
    }
    if bytes.len() > 1 && !bytes[bytes.len() - 1].is_ascii_alphanumeric() {
        return false;
    }
    bytes[1..]
        .iter()
        .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-')
}

fn validate_name(value: &str) -> Result<(), String> {
    valid_name(value).then_some(()).ok_or_else(|| {
        format!(
            "invalid name '{value}': use a lowercase letter followed by lowercase letters, digits, or hyphens (maximum 32 characters)"
        )
    })
}

fn validate_public_alias(value: &str) -> Result<(), String> {
    let Some(alias) = value.strip_prefix("images:") else {
        return Err("public images must use images:ALIAS".to_string());
    };
    let bytes = alias.as_bytes();
    let valid = !bytes.is_empty()
        && bytes.len() <= 64
        && (bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit());
    let valid = valid
        && bytes.iter().all(|byte| {
            byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || matches!(*byte, b'.' | b'_' | b'/' | b'-')
        });
    valid
        .then_some(())
        .ok_or_else(|| format!("invalid public image alias '{value}'"))
}

enum SizeKind {
    Memory,
    Disk,
}

fn validate_size(value: &str, kind: SizeKind) -> Result<(), String> {
    let (number, unit) = value
        .strip_suffix("MiB")
        .map(|number| (number, "MiB"))
        .or_else(|| value.strip_suffix("GiB").map(|number| (number, "GiB")))
        .ok_or_else(|| format!("invalid size '{value}': use MiB or GiB"))?;
    if number.is_empty()
        || number.len() > 6
        || number.starts_with('0')
        || !number.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(format!("invalid size '{value}'"));
    }
    let amount = number
        .parse::<u64>()
        .map_err(|_| "invalid size".to_string())?;
    let mib = if unit == "GiB" {
        amount.checked_mul(1024)
    } else {
        Some(amount)
    }
    .ok_or_else(|| "size is too large".to_string())?;
    let allowed = match kind {
        SizeKind::Memory => (256..=256 * 1024).contains(&mib),
        SizeKind::Disk => (1024..=2000 * 1024).contains(&mib),
    };
    allowed.then_some(()).ok_or_else(|| match kind {
        SizeKind::Memory => "memory must be between 256MiB and 256GiB".to_string(),
        SizeKind::Disk => "disk must be between 1GiB and 2000GiB".to_string(),
    })
}

fn build_launch(
    options: &LaunchOptions,
    pool: &str,
    root_device: &str,
    remote: &str,
    project: &str,
) -> (Vec<String>, String) {
    let mut config = serde_json::Map::new();
    config.insert("limits.cpu".to_string(), json!(options.cpus.to_string()));
    config.insert("limits.memory".to_string(), json!(options.memory));
    if options.no_secureboot {
        config.insert("security.secureboot".to_string(), json!("false"));
    }
    let mut devices = serde_json::Map::new();
    devices.insert(
        root_device.to_string(),
        json!({"type": "disk", "path": "/", "pool": pool, "size": options.disk}),
    );
    if let LaunchSource::Iso(name) = &options.source {
        devices.insert(
            "iso".to_string(),
            json!({"type": "disk", "pool": pool, "source": name, "boot.priority": "10"}),
        );
    }
    let body = serde_json::to_string(&json!({
        "ephemeral": true,
        "config": config,
        "devices": devices,
    }))
    .expect("launch JSON serialization failed");
    let mut args = vec![
        "create".to_string(),
        "--vm".to_string(),
        "--ephemeral".to_string(),
    ];
    if matches!(options.source, LaunchSource::Iso(_)) {
        args.push("--empty".to_string());
    }
    args.extend(["--project".to_string(), project.to_string()]);
    match &options.source {
        LaunchSource::PublicImage(alias) => args.push(alias.clone()),
        LaunchSource::LocalImage(alias) => args.push(format!("{remote}:{alias}")),
        LaunchSource::Iso(_) => {}
    }
    args.push(format!("{remote}:{}", options.name));
    (args, body)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MediaKind {
    Iso,
    Qcow2,
}

fn inspect_media(file: &mut File) -> Result<MediaKind, String> {
    let mut qcow = [0_u8; 104];
    file.seek(SeekFrom::Start(0))
        .map_err(|error| format!("could not inspect import file: {error}"))?;
    let qcow_len = file
        .read(&mut qcow)
        .map_err(|error| format!("could not inspect import file: {error}"))?;
    if qcow_len >= 24 && &qcow[..4] == b"QFI\xfb" {
        let version = u32::from_be_bytes(qcow[4..8].try_into().expect("slice length"));
        if !matches!(version, 2 | 3) {
            return Err(format!(
                "unsupported qcow2 version {version}; expected version 2 or 3"
            ));
        }
        let backing_offset = u64::from_be_bytes(qcow[8..16].try_into().expect("slice length"));
        let backing_size = u32::from_be_bytes(qcow[16..20].try_into().expect("slice length"));
        if backing_offset != 0 || backing_size != 0 {
            return Err("qcow2 images with a backing file are not allowed".to_string());
        }
        if version == 2 && qcow_len < 72 {
            return Err("truncated qcow2 version 2 header".to_string());
        }
        if version == 3 {
            if qcow_len < 80 {
                return Err("truncated qcow2 version 3 header".to_string());
            }
            let incompatible = u64::from_be_bytes(qcow[72..80].try_into().expect("slice length"));
            if incompatible & (1 << 2) != 0 {
                return Err("qcow2 external data files are not allowed".to_string());
            }
        }
        return Ok(MediaKind::Qcow2);
    }
    let mut iso = [0_u8; 5];
    file.seek(SeekFrom::Start(0x8001))
        .map_err(|error| format!("could not inspect import file: {error}"))?;
    if file.read_exact(&mut iso).is_ok() && &iso == b"CD001" {
        return Ok(MediaKind::Iso);
    }
    Err("unsupported disk image; ISO files need a CD001 primary volume descriptor, and raw disk images must be converted with `qemu-img convert -O qcow2 INPUT OUTPUT.qcow2`".to_string())
}

fn clean_container_relative(path: &Path) -> Result<PathBuf, String> {
    let relative = path
        .strip_prefix("/workspace")
        .map_err(|_| "absolute import paths must be inside /workspace".to_string())?;
    clean_relative(relative)
}

fn clean_relative(path: &Path) -> Result<PathBuf, String> {
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(value) => result.push(value),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(
                    "import path must not contain '..'; use an absolute /workspace/... path"
                        .to_string(),
                );
            }
        }
    }
    Ok(result)
}

fn make_metadata_tarball(runtime_dir: &Path) -> Result<PathBuf, String> {
    let temp_root = env::temp_dir();
    let runtime_canonical =
        fs::canonicalize(runtime_dir).unwrap_or_else(|_| runtime_dir.to_path_buf());
    let temp_canonical = fs::canonicalize(&temp_root).unwrap_or_else(|_| temp_root.clone());
    if temp_canonical.starts_with(&runtime_canonical) {
        return Err(
            "refusing to write image metadata inside the container-visible proxy runtime directory"
                .to_string(),
        );
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| format!("system clock is before Unix epoch: {error}"))?;
    let architecture = match env::consts::ARCH {
        "x86_64" => "x86_64",
        "aarch64" => "aarch64",
        "powerpc64" if cfg!(target_endian = "little") => "ppc64le",
        "s390x" => "s390x",
        other => {
            return Err(format!(
                "unsupported host architecture for Incus VMs: {other}"
            ));
        }
    };
    let metadata = format!(
        "architecture: {architecture}\ncreation_date: {}\nproperties:\n  description: Imported by claude-sandbox VM bridge\n",
        now.as_secs()
    );
    for attempt in 0..100_u32 {
        let path = temp_root.join(format!(
            "claude-sandbox-vm-metadata-{}-{}-{attempt}.tar.gz",
            process::id(),
            now.as_nanos()
        ));
        let file = match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(format!("could not create image metadata: {error}")),
        };
        let encoder = GzEncoder::new(file, Compression::default());
        let mut archive = Builder::new(encoder);
        let mut header = tar::Header::new_gnu();
        header.set_mode(0o600);
        header.set_size(metadata.len() as u64);
        header.set_mtime(now.as_secs());
        header.set_cksum();
        archive
            .append_data(&mut header, "metadata.yaml", metadata.as_bytes())
            .map_err(|error| format!("could not write image metadata: {error}"))?;
        let encoder = archive
            .into_inner()
            .map_err(|error| format!("could not finish image metadata: {error}"))?;
        encoder
            .finish()
            .map_err(|error| format!("could not finish compressed image metadata: {error}"))?;
        return Ok(path);
    }
    Err("could not allocate a private image metadata file".to_string())
}

fn find_executable(name: &str) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    env::split_paths(&path).find_map(|directory| {
        let candidate = directory.join(name);
        let metadata = fs::metadata(&candidate).ok()?;
        if metadata.is_file() && metadata.permissions().mode() & 0o111 != 0 {
            fs::canonicalize(candidate).ok()
        } else {
            None
        }
    })
}

fn client_config_has_remote(conf_dir: &Path, remote: &str) -> bool {
    let Ok(config) = fs::read_to_string(conf_dir.join("config.yml")) else {
        return false;
    };
    let mut in_remotes = false;
    for line in config.lines() {
        if line == "remotes:" {
            in_remotes = true;
            continue;
        }
        if in_remotes && !line.is_empty() && !line.starts_with(char::is_whitespace) {
            break;
        }
        if in_remotes && line.trim() == format!("{remote}:") {
            return true;
        }
    }
    false
}

fn read_capped(mut reader: impl Read, limit: usize, stream: &str) -> Vec<u8> {
    let mut output = Vec::new();
    let mut buffer = [0_u8; 16 * 1024];
    let mut truncated = false;
    loop {
        match reader.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(count) => {
                let remaining = limit.saturating_sub(output.len());
                output.extend_from_slice(&buffer[..count.min(remaining)]);
                truncated |= count > remaining;
            }
        }
    }
    if truncated {
        let notice = format!("\n[vm: {stream} truncated after {limit} bytes]\n");
        append_notice_capped(&mut output, limit, &notice);
    }
    output
}

fn append_notice_capped(output: &mut Vec<u8>, limit: usize, notice: &str) {
    output.truncate(limit.saturating_sub(notice.len()));
    output.extend_from_slice(notice.as_bytes());
}

fn output_error(output: &CommandOutput) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let detail = if stderr.trim().is_empty() {
        stdout.trim()
    } else {
        stderr.trim()
    };
    if detail.is_empty() {
        format!("Incus client exited with {}", output.status)
    } else {
        detail.to_string()
    }
}

fn command_response(output: CommandOutput) -> Response {
    Response {
        exit_code: output.status.code().unwrap_or(1),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

fn json_response(value: &Value) -> Response {
    match serde_json::to_string_pretty(value) {
        Ok(output) => Response::ok(format!("{output}\n")),
        Err(error) => Response::error(format!("could not serialize response: {error}")),
    }
}

fn unwrap_metadata(value: Value) -> Value {
    value
        .as_object()
        .and_then(|object| object.get("metadata"))
        .cloned()
        .unwrap_or(value)
}

fn summarize_instances(value: &Value) -> Value {
    Value::Array(
        value
            .as_array()
            .into_iter()
            .flatten()
            .map(summarize_instance)
            .collect(),
    )
}

fn summarize_instance(instance: &Value) -> Value {
    let mut ipv4 = Vec::new();
    let mut ipv6 = Vec::new();
    if let Some(network) = instance
        .get("state")
        .and_then(|state| state.get("network"))
        .and_then(Value::as_object)
    {
        for interface in network.values() {
            for address in interface
                .get("addresses")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let value = address.get("address").and_then(Value::as_str);
                match address.get("family").and_then(Value::as_str) {
                    Some("inet") => ipv4.extend(value.map(str::to_string)),
                    Some("inet6") => ipv6.extend(value.map(str::to_string)),
                    _ => {}
                }
            }
        }
    }
    let config = instance.get("config").and_then(Value::as_object);
    json!({
        "name": instance.get("name").and_then(Value::as_str).unwrap_or(""),
        "status": instance.get("status").and_then(Value::as_str).unwrap_or(""),
        "ipv4": ipv4,
        "ipv6": ipv6,
        "cpus": config.and_then(|c| c.get("limits.cpu")).and_then(Value::as_str).unwrap_or(""),
        "memory": config.and_then(|c| c.get("limits.memory")).and_then(Value::as_str).unwrap_or(""),
    })
}

fn string_field<'a>(value: &'a Value, field: &str) -> &'a str {
    value.get(field).and_then(Value::as_str).unwrap_or("")
}

fn string_array<'a>(value: &'a Value, field: &str) -> Vec<&'a str> {
    value
        .get(field)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect()
}

fn connection_refused(error: &str) -> bool {
    let lower = error.to_ascii_lowercase();
    lower.contains("connection refused")
        || lower.contains("connect: connection")
        || lower.contains("no route to host")
}

fn certificate_not_trusted(error: &str) -> bool {
    let lower = error.to_ascii_lowercase();
    lower.contains("not trusted") || lower.contains("certificate is not trusted")
}

fn visible_projects(projects: &Value) -> Option<Vec<&str>> {
    projects
        .as_array()?
        .iter()
        .map(|project| project.get("name").and_then(Value::as_str))
        .collect()
}

fn one_line(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn escape_log(value: &str) -> String {
    value.chars().flat_map(char::escape_default).collect()
}

fn truncate_log(value: String) -> String {
    const LIMIT: usize = 512;
    const MARKER: &str = "…[truncated]";
    if value.chars().count() <= LIMIT {
        return value;
    }
    value
        .chars()
        .take(LIMIT - MARKER.chars().count())
        .chain(MARKER.chars())
        .collect()
}

fn spice_socket_path(line: &str) -> Option<PathBuf> {
    let start = line.find("spice+unix://")? + "spice+unix://".len();
    let path = line[start..].trim();
    if path.is_empty() {
        return None;
    }
    Path::new(path).is_absolute().then(|| PathBuf::from(path))
}

fn reserve_connection(count: &AtomicUsize, maximum: usize) -> bool {
    let mut current = count.load(Ordering::Acquire);
    loop {
        if current >= maximum {
            return false;
        }
        match count.compare_exchange_weak(current, current + 1, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) => return true,
            Err(updated) => current = updated,
        }
    }
}

fn shutdown_connections(connections: &Mutex<HashMap<u64, RelayConnection>>) {
    for connection in connections
        .lock()
        .expect("relay connection lock poisoned")
        .values()
    {
        let _ = connection.client.shutdown(Shutdown::Both);
        if let Some(upstream) = &connection.upstream {
            let _ = upstream.shutdown(Shutdown::Both);
        }
    }
}

fn canonicalize_for_comparison(path: &Path) -> PathBuf {
    if let Ok(path) = fs::canonicalize(path) {
        return path;
    }
    let mut current = path;
    let mut missing = Vec::new();
    while let Some(name) = current.file_name() {
        missing.push(name.to_os_string());
        let Some(parent) = current.parent() else {
            break;
        };
        current = parent;
        if let Ok(mut base) = fs::canonicalize(current) {
            for component in missing.iter().rev() {
                base.push(component);
            }
            return base;
        }
    }
    path.to_path_buf()
}

fn paths_overlap(left: &Path, right: &Path) -> bool {
    left.starts_with(right) || right.starts_with(left)
}

fn copy_bidirectional(left: UnixStream, right: UnixStream) {
    let Ok(mut left_reader) = left.try_clone() else {
        return;
    };
    let Ok(mut right_writer) = right.try_clone() else {
        return;
    };
    let left_shutdown = left.try_clone().ok();
    let right_shutdown = right.try_clone().ok();
    let forward = thread::spawn(move || {
        let result = std::io::copy(&mut left_reader, &mut right_writer);
        if let Some(socket) = left_shutdown {
            let _ = socket.shutdown(Shutdown::Both);
        }
        if let Some(socket) = right_shutdown {
            let _ = socket.shutdown(Shutdown::Both);
        }
        result
    });
    let mut right_reader = right;
    let mut left_writer = left;
    let _ = std::io::copy(&mut right_reader, &mut left_writer);
    let _ = right_reader.shutdown(Shutdown::Both);
    let _ = left_writer.shutdown(Shutdown::Both);
    let _ = forward.join();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    fn temp_dir(label: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let value = COUNTER.fetch_add(1, AtomicOrdering::Relaxed);
        let path = env::temp_dir().join(format!(
            "claude-sandbox-vm-{label}-{}-{value}",
            process::id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn executable(path: &Path, contents: &str) {
        fs::write(path, contents).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }

    fn test_bridge(root: &Path, incus_bin: &Path) -> IncusBridge {
        let home = root.join("home");
        let workspace = root.join("workspace");
        let runtime = root.join("runtime");
        fs::create_dir_all(home.join(".claude-sandbox/incus")).unwrap();
        fs::write(
            home.join(".claude-sandbox/incus/config.yml"),
            "remotes:\n  claude-sandbox:\n    addr: https://127.0.0.1:8443\n",
        )
        .unwrap();
        fs::create_dir_all(&workspace).unwrap();
        fs::create_dir_all(&runtime).unwrap();
        let bridge = IncusBridge::new(&home, &workspace, &runtime);
        *bridge.incus_bin.lock().unwrap() = Some(incus_bin.to_path_buf());
        bridge
    }

    #[test]
    fn names_and_aliases_reject_flag_smuggling() {
        for valid in ["a", "vm-1", "a123", &format!("a{}z", "1".repeat(30))] {
            assert!(validate_name(valid).is_ok(), "{valid}");
        }
        for invalid in [
            "--project",
            "A",
            "a_1",
            "a-",
            "1a",
            "",
            &format!("a{}", "1".repeat(32)),
        ] {
            assert!(validate_name(invalid).is_err(), "{invalid}");
        }
        assert!(validate_public_alias("images:ubuntu/24.04").is_ok());
        assert!(validate_public_alias("other:ubuntu").is_err());
        assert!(validate_public_alias("images:Ubuntu").is_err());
        assert!(validate_public_alias("images:foo:bar").is_err());
    }

    #[test]
    fn sizes_enforce_units_and_ranges() {
        assert!(validate_size("256MiB", SizeKind::Memory).is_ok());
        assert!(validate_size("256GiB", SizeKind::Memory).is_ok());
        assert!(validate_size("255MiB", SizeKind::Memory).is_err());
        assert!(validate_size("257GiB", SizeKind::Memory).is_err());
        assert!(validate_size("1GiB", SizeKind::Disk).is_ok());
        assert!(validate_size("1024MiB", SizeKind::Disk).is_ok());
        assert!(validate_size("2000GiB", SizeKind::Disk).is_ok());
        assert!(validate_size("1023MiB", SizeKind::Disk).is_err());
        assert!(validate_size("020GiB", SizeKind::Disk).is_err());
    }

    #[test]
    fn parser_requires_controlled_shapes() {
        assert!(matches!(
            parse_args(&strings(&["stop", "test", "--force"])),
            Ok(VmCommand::Stop { force: true, .. })
        ));
        assert!(parse_args(&strings(&["stop", "--project"])).is_err());
        assert!(parse_args(&strings(&["exec", "test", "id"])).is_err());
        assert!(parse_args(&strings(&["exec", "test", "--", "--project"])).is_ok());
        assert!(parse_args(&strings(&["launch", "test", "--image", "--project"])).is_err());
    }

    #[test]
    fn launch_builds_each_source_without_agent_configuration() {
        let base = LaunchOptions {
            name: "test".to_string(),
            source: LaunchSource::PublicImage("images:ubuntu/24.04".to_string()),
            cpus: 3,
            memory: "8GiB".to_string(),
            disk: "40GiB".to_string(),
            no_secureboot: true,
        };
        let (public_args, public_body) = build_launch(&base, "pool", "system", REMOTE, PROJECT);
        assert_eq!(
            public_args,
            strings(&[
                "create",
                "--vm",
                "--ephemeral",
                "--project",
                "claude-sandbox",
                "images:ubuntu/24.04",
                "claude-sandbox:test",
            ])
        );
        let body: Value = serde_json::from_str(&public_body).unwrap();
        assert_eq!(body["ephemeral"], true);
        assert_eq!(body["config"]["limits.cpu"], "3");
        assert_eq!(body["config"]["security.secureboot"], "false");
        assert_eq!(body["devices"]["system"]["pool"], "pool");
        assert!(body["devices"].get("root").is_none());

        let mut local = base.clone();
        local.source = LaunchSource::LocalImage("installer".to_string());
        local.no_secureboot = false;
        let (local_args, local_body) = build_launch(&local, "pool", "system", REMOTE, PROJECT);
        assert_eq!(
            local_args,
            strings(&[
                "create",
                "--vm",
                "--ephemeral",
                "--project",
                "claude-sandbox",
                "claude-sandbox:installer",
                "claude-sandbox:test",
            ])
        );
        let body: Value = serde_json::from_str(&local_body).unwrap();
        assert!(body["config"].get("security.secureboot").is_none());

        let mut iso = base;
        iso.source = LaunchSource::Iso("ubuntu-iso".to_string());
        let (iso_args, iso_body) = build_launch(&iso, "pool", "system", REMOTE, PROJECT);
        assert_eq!(
            iso_args,
            strings(&[
                "create",
                "--vm",
                "--ephemeral",
                "--empty",
                "--project",
                "claude-sandbox",
                "claude-sandbox:test",
            ])
        );
        let body: Value = serde_json::from_str(&iso_body).unwrap();
        assert_eq!(body["devices"]["iso"]["source"], "ubuntu-iso");
        assert_eq!(body["devices"]["iso"]["boot.priority"], "10");
    }

    #[test]
    fn media_headers_are_strict() {
        let root = temp_dir("headers");
        let qcow_path = root.join("disk.qcow2");
        let mut qcow = vec![0_u8; 104];
        qcow[..4].copy_from_slice(b"QFI\xfb");
        qcow[4..8].copy_from_slice(&3_u32.to_be_bytes());
        fs::write(&qcow_path, &qcow).unwrap();
        assert_eq!(
            inspect_media(&mut File::open(&qcow_path).unwrap()),
            Ok(MediaKind::Qcow2)
        );
        qcow[15] = 1;
        fs::write(&qcow_path, &qcow).unwrap();
        assert!(
            inspect_media(&mut File::open(&qcow_path).unwrap())
                .unwrap_err()
                .contains("backing file")
        );
        qcow[15] = 0;
        qcow[79] = 4;
        fs::write(&qcow_path, &qcow).unwrap();
        assert!(
            inspect_media(&mut File::open(&qcow_path).unwrap())
                .unwrap_err()
                .contains("external")
        );

        let iso_path = root.join("install.iso");
        let mut iso = vec![0_u8; 0x8006];
        iso[0x8001..0x8006].copy_from_slice(b"CD001");
        fs::write(&iso_path, iso).unwrap();
        assert_eq!(
            inspect_media(&mut File::open(&iso_path).unwrap()),
            Ok(MediaKind::Iso)
        );
        fs::write(&iso_path, b"raw disk").unwrap();
        assert!(
            inspect_media(&mut File::open(&iso_path).unwrap())
                .unwrap_err()
                .contains("qemu-img")
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn workspace_file_is_pinned_and_confined() {
        use std::os::unix::fs::symlink;
        let root = temp_dir("workspace");
        let runtime = temp_dir("runtime");
        let home = temp_dir("home");
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::write(root.join("sub/good.iso"), b"data").unwrap();
        symlink("/etc/passwd", root.join("sub/bad.iso")).unwrap();
        let bridge = IncusBridge::new(&home, &root, &runtime);
        let result = bridge.open_workspace_file("good.iso", Some("/workspace/sub"));
        assert!(result.is_ok(), "{result:?}");
        assert!(
            bridge
                .open_workspace_file("bad.iso", Some("/workspace/sub"))
                .is_err()
        );
        assert!(
            bridge
                .open_workspace_file("../good.iso", Some("/workspace/sub"))
                .is_err()
        );
        assert!(
            bridge
                .open_workspace_file("/etc/passwd", Some("/workspace"))
                .is_err()
        );
        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_dir_all(runtime);
        let _ = fs::remove_dir_all(home);
    }

    #[test]
    fn spice_uri_is_found_without_localized_prose() {
        assert_eq!(
            spice_socket_path("beliebiger Text spice+unix:///tmp/incus.sock"),
            Some(PathBuf::from("/tmp/incus.sock"))
        );
        assert_eq!(spice_socket_path("spice+unix://relative"), None);
        assert_eq!(
            spice_socket_path("spice+unix:///home/user name/incus.sock  "),
            Some(PathBuf::from("/home/user name/incus.sock"))
        );
    }

    #[test]
    fn preflight_accepts_restricted_setup_and_fails_open_default_project() {
        if !Path::new("/usr/bin/python3").exists() {
            return;
        }
        let root = temp_dir("preflight");
        let fake = root.join("incus");
        executable(
            &fake,
            r#"#!/usr/bin/python3
import json, pathlib, sys
a = sys.argv[1:]
if not a or a[0] != "query":
    raise SystemExit(2)
url = a[-1]
if "/projects?recursion=1" in url:
    names = ["claude-sandbox"]
    if pathlib.Path(__file__).with_name("allow-default").exists():
        names.insert(0, "default")
    print(json.dumps([{"name": name} for name in names]))
elif "/projects/claude-sandbox" in url:
    config = {"restricted": "true", "limits.virtual-machines": "2", "limits.cpu": "4", "limits.memory": "8GiB", "limits.disk": "100GiB"}
    if pathlib.Path(__file__).with_name("missing-limits").exists():
        del config["limits.disk"]
    if pathlib.Path(__file__).with_name("disabled-storage").exists():
        config["features.storage.volumes"] = "false"
    print(json.dumps({"config": config}))
elif "/profiles/default" in url:
    print(json.dumps({"devices": {"root": {"type": "disk", "path": "/", "pool": "default"}, "eth0": {"type": "nic"}}}))
else:
    print(json.dumps({"auth": "trusted"}))
"#,
        );
        let bridge = test_bridge(&root, &fake);
        let report = bridge.preflight_report_inner(false);
        assert!(preflight_is_healthy(&report), "{report}");

        fs::write(root.join("allow-default"), b"").unwrap();
        let report = bridge.preflight_report_inner(false);
        assert!(!preflight_is_healthy(&report));
        assert!(report.contains("can also see other projects (default"));
        fs::remove_file(root.join("allow-default")).unwrap();

        fs::write(root.join("missing-limits"), b"").unwrap();
        let report = bridge.preflight_report_inner(false);
        assert!(!preflight_is_healthy(&report));
        assert!(report.contains("limits.disk"));
        fs::remove_file(root.join("missing-limits")).unwrap();

        fs::write(root.join("disabled-storage"), b"").unwrap();
        let report = bridge.preflight_report_inner(false);
        assert!(!preflight_is_healthy(&report));
        assert!(report.contains("features.storage.volumes"));
        fs::remove_file(root.join("disabled-storage")).unwrap();

        let mut overlap_bridge = bridge.clone();
        overlap_bridge.conf_dir = overlap_bridge.workspace_root.join(".claude-sandbox/incus");
        fs::create_dir_all(&overlap_bridge.conf_dir).unwrap();
        fs::write(
            overlap_bridge.conf_dir.join("config.yml"),
            "remotes:\n  claude-sandbox:\n    addr: https://127.0.0.1:8443\n",
        )
        .unwrap();
        let report = overlap_bridge.preflight_report_inner(false);
        assert!(!preflight_is_healthy(&report));
        assert!(report.contains("exposes the Incus client key"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn import_uses_private_snapshot_and_cleans_it_up() {
        if !Path::new("/usr/bin/python3").exists() {
            return;
        }
        let root = temp_dir("import-snapshot");
        let fake = root.join("incus");
        executable(
            &fake,
            r#"#!/usr/bin/python3
import hashlib, json, pathlib, sys
a = sys.argv[1:]
root = pathlib.Path(__file__).parent
if a and a[0] == "query":
    url = a[-1]
    if "/profiles/default" in url:
        print(json.dumps({"devices": {"system": {"type": "disk", "path": "/", "pool": "default"}}}))
    else:
        print("[]")
    raise SystemExit(0)
if len(a) >= 2 and a[0:2] == ["image", "import"]:
    source = root / "workspace" / "disk.qcow2"
    original = source.read_bytes()
    changed = bytearray(original)
    changed[15] = 1
    source.write_bytes(changed)
    uploaded = sys.stdin.buffer.read()
    (root / "hashes").write_text(hashlib.sha256(original).hexdigest() + "\n" + hashlib.sha256(uploaded).hexdigest() + "\n")
    raise SystemExit(0)
raise SystemExit(2)
"#,
        );
        let bridge = test_bridge(&root, &fake);
        let mut qcow = vec![0_u8; 104];
        qcow[..4].copy_from_slice(b"QFI\xfb");
        qcow[4..8].copy_from_slice(&3_u32.to_be_bytes());
        fs::write(root.join("workspace/disk.qcow2"), &qcow).unwrap();

        let response = bridge.import_opened(
            File::open(root.join("workspace/disk.qcow2")).unwrap(),
            "disk",
        );
        assert_eq!(response.exit_code, 0, "{}", response.stderr);
        let hashes = fs::read_to_string(root.join("hashes")).unwrap();
        let hashes: Vec<&str> = hashes.lines().collect();
        assert_eq!(hashes[0], hashes[1]);
        assert_eq!(fs::read_dir(&bridge.snapshot_dir).unwrap().count(), 0);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn import_rejects_file_over_bridge_cap() {
        let root = temp_dir("import-cap");
        let fake = root.join("incus");
        executable(&fake, "#!/bin/sh\nexit 2\n");
        let mut bridge = test_bridge(&root, &fake);
        bridge.max_import_size = 8;
        fs::write(root.join("workspace/large.img"), b"123456789").unwrap();
        let response = bridge.import_opened(
            File::open(root.join("workspace/large.img")).unwrap(),
            "large",
        );
        assert_ne!(response.exit_code, 0);
        assert!(response.stderr.contains("8 byte maximum"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn launch_deletes_instance_when_start_fails() {
        if !Path::new("/usr/bin/python3").exists() {
            return;
        }
        let root = temp_dir("launch-cleanup");
        let fake = root.join("incus");
        executable(
            &fake,
            r#"#!/usr/bin/python3
import json, os, pathlib, sys
p = pathlib.Path(__file__).with_name("calls")
with p.open("a") as f:
    f.write(" ".join(sys.argv[1:]) + "\n")
pathlib.Path(__file__).with_name("environment").write_text(json.dumps(dict(os.environ)))
a = sys.argv[1:]
if a and a[0] == "query" and "/profiles/default" in a[-1]:
    print(json.dumps({"devices": {"root": {"type": "disk", "path": "/", "pool": "default"}}}))
    raise SystemExit(0)
if a and a[0] == "create":
    pathlib.Path(__file__).with_name("stdin").write_bytes(sys.stdin.buffer.read())
    raise SystemExit(0)
if a and a[0] == "start":
    print("start failed", file=sys.stderr)
    raise SystemExit(1)
raise SystemExit(0)
"#,
        );
        let bridge = test_bridge(&root, &fake);
        let response = bridge.launch(&LaunchOptions {
            name: "test".to_string(),
            source: LaunchSource::PublicImage("images:ubuntu/24.04".to_string()),
            cpus: 2,
            memory: "4GiB".to_string(),
            disk: "20GiB".to_string(),
            no_secureboot: false,
        });
        assert_ne!(response.exit_code, 0);
        let calls = fs::read_to_string(root.join("calls")).unwrap();
        assert!(
            calls
                .lines()
                .any(|line| line.starts_with("delete --force "))
        );
        let environment: Value =
            serde_json::from_slice(&fs::read(root.join("environment")).unwrap()).unwrap();
        assert_eq!(
            environment,
            json!({
                "HOME": root.join("home"),
                "INCUS_CONF": root.join("home/.claude-sandbox/incus"),
                "LC_ALL": "C",
                "PATH": "/nonexistent",
            })
        );
        let stdin: Value = serde_json::from_slice(&fs::read(root.join("stdin")).unwrap()).unwrap();
        assert_eq!(stdin["ephemeral"], true);
        assert_eq!(stdin["config"]["limits.cpu"], "2");
        assert_eq!(stdin["devices"]["root"]["size"], "20GiB");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn spice_relay_echoes_through_cli_socket() {
        if !Path::new("/usr/bin/python3").exists() {
            return;
        }
        let root = temp_dir("spice-relay");
        let fake = root.join("incus");
        executable(
            &fake,
            r#"#!/usr/bin/python3
import os, pathlib, socket, sys
upstream = pathlib.Path(__file__).with_name("upstream.spice")
try:
    upstream.unlink()
except FileNotFoundError:
    pass
s = socket.socket(socket.AF_UNIX)
s.bind(str(upstream))
s.listen(1)
print("localized console text")
print("connect using")
print("spice+unix://" + str(upstream), flush=True)
c, _ = s.accept()
while True:
    data = c.recv(65536)
    if not data:
        break
    c.sendall(data)
c.close()
s.close()
"#,
        );
        let bridge = test_bridge(&root, &fake);
        bridge.start_relay("test").unwrap();
        let mut stream = UnixStream::connect(root.join("runtime/vm-test.spice")).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        stream.write_all(b"hello").unwrap();
        let mut echoed = [0_u8; 5];
        stream.read_exact(&mut echoed).unwrap();
        assert_eq!(&echoed, b"hello");
        drop(stream);
        bridge.stop_relay("test");

        let mut expiring_bridge = bridge.clone();
        expiring_bridge.relay_idle_timeout = Duration::from_millis(200);
        expiring_bridge.start_relay("idle").unwrap();
        let idle_socket = root.join("runtime/vm-idle.spice");
        let deadline = Instant::now() + Duration::from_secs(2);
        while idle_socket.exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(25));
        }
        assert!(!idle_socket.exists());
        assert!(!expiring_bridge.relays.lock().unwrap().contains_key("idle"));
        let _ = fs::remove_dir_all(root);
    }
}
