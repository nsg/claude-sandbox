use crate::usage_api::{self, Provider, ProviderSnapshot};
use std::fs::{File, OpenOptions, Permissions, TryLockError};
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const LEASE_FILE: &str = "collector.lock";
const HELPER_PATH: &str = "/usr/local/bin/claude-sandbox-usage-probe";
const REFRESH_INTERVAL: Duration = Duration::from_secs(30 * 60);
const FOLLOWER_RETRY: Duration = Duration::from_secs(10);
const OUTER_TIMEOUT: Duration = Duration::from_secs(35);
const POLL_INTERVAL: Duration = Duration::from_millis(100);
const MAX_HELPER_OUTPUT: u64 = 64 * 1024;
const STARTUP_RETRY: Duration = Duration::from_secs(5);
const FAILED_ROUNDS_BEFORE_YIELD: usize = 3;
const FAILURE_DELAYS: [Duration; 5] = [
    Duration::from_secs(60),
    Duration::from_secs(2 * 60),
    Duration::from_secs(5 * 60),
    Duration::from_secs(10 * 60),
    Duration::from_secs(30 * 60),
];
#[cfg(target_os = "linux")]
const O_NOFOLLOW: i32 = 0o400_000;
const SIGKILL: i32 = 9;

unsafe extern "C" {
    fn kill(pid: i32, signal: i32) -> i32;
}

struct Schedule {
    provider: Provider,
    next_due: Instant,
    failures: usize,
    startup_retry: bool,
}

impl Schedule {
    fn new(provider: Provider, usage_dir: &Path, now: i64) -> Self {
        let observed_at = usage_api::observed_at(usage_dir, provider, now);
        let delay = observed_at
            .and_then(|observed| {
                u64::try_from(observed.saturating_add(REFRESH_INTERVAL.as_secs() as i64) - now).ok()
            })
            .map(Duration::from_secs)
            .unwrap_or(Duration::ZERO);
        Self {
            provider,
            next_due: Instant::now() + delay,
            failures: 0,
            startup_retry: observed_at.is_none(),
        }
    }

    fn succeeded(&mut self) {
        self.failures = 0;
        self.startup_retry = false;
        self.next_due = Instant::now() + REFRESH_INTERVAL;
    }

    fn failed(&mut self) {
        if self.startup_retry {
            self.startup_retry = false;
            self.next_due = Instant::now() + STARTUP_RETRY;
        } else {
            let index = self.failures.min(FAILURE_DELAYS.len() - 1);
            self.next_due = Instant::now() + FAILURE_DELAYS[index];
            self.failures = self.failures.saturating_add(1);
        }
    }
}

#[derive(Default)]
struct FailedRounds {
    consecutive: usize,
}

impl FailedRounds {
    fn observe(&mut self, attempted: usize, succeeded: usize) -> bool {
        if attempted == Provider::ALL.len() && succeeded == 0 {
            self.consecutive = self.consecutive.saturating_add(1);
        } else {
            self.consecutive = 0;
        }
        self.consecutive >= FAILED_ROUNDS_BEFORE_YIELD
    }
}

fn record_store_success(
    schedule: &mut Schedule,
    provider: Provider,
    outcome: usage_api::StoreOutcome,
) {
    if let usage_api::StoreOutcome::DirectorySyncFailed(warning) = outcome {
        eprintln!(
            "t3-admin: {} usage cache warning: {warning}",
            provider.as_str()
        );
    }
    schedule.succeeded();
}

pub(crate) fn start(container_name: String, usage_dir: PathBuf) {
    thread::spawn(move || collector_loop(&container_name, &usage_dir));
}

fn collector_loop(container_name: &str, usage_dir: &Path) {
    loop {
        if let Err(error) = usage_api::prepare_usage_dir(usage_dir) {
            eprintln!("t3-admin: usage collector unavailable: {error}");
            thread::sleep(FAILURE_DELAYS[0]);
            continue;
        }
        match acquire_lease(usage_dir) {
            Ok(Some(lease)) => {
                run_leader(container_name, usage_dir, lease);
                thread::sleep(FOLLOWER_RETRY);
            }
            Ok(None) => thread::sleep(FOLLOWER_RETRY),
            Err(error) => {
                eprintln!("t3-admin: could not acquire usage collector lease: {error}");
                thread::sleep(FAILURE_DELAYS[0]);
            }
        }
    }
}

fn acquire_lease(usage_dir: &Path) -> Result<Option<File>, String> {
    let path = usage_dir.join(LEASE_FILE);
    let mut options = OpenOptions::new();
    options.create(true).read(true).write(true).mode(0o600);
    #[cfg(target_os = "linux")]
    options.custom_flags(O_NOFOLLOW);
    let file = options
        .open(&path)
        .map_err(|error| format!("could not open {}: {error}", path.display()))?;
    file.set_permissions(Permissions::from_mode(0o600))
        .map_err(|error| format!("could not protect {}: {error}", path.display()))?;
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(TryLockError::WouldBlock) => Ok(None),
        Err(TryLockError::Error(error)) => Err(error.to_string()),
    }
}

fn run_leader(container_name: &str, usage_dir: &Path, _lease: File) {
    let now = unix_time();
    let mut schedules = Provider::ALL.map(|provider| Schedule::new(provider, usage_dir, now));
    let mut failed_rounds = FailedRounds::default();
    loop {
        let due = schedules
            .iter()
            .filter(|schedule| schedule.next_due <= Instant::now())
            .map(|schedule| schedule.provider)
            .collect::<Vec<_>>();
        if due.is_empty() {
            let wait = schedules
                .iter()
                .map(|schedule| schedule.next_due.saturating_duration_since(Instant::now()))
                .min()
                .unwrap_or(REFRESH_INTERVAL);
            thread::sleep(wait);
            continue;
        }

        let (sender, receiver) = mpsc::channel();
        for provider in &due {
            let sender = sender.clone();
            let container_name = container_name.to_string();
            let usage_dir = usage_dir.to_path_buf();
            let provider = *provider;
            thread::spawn(move || {
                let _ = sender.send((provider, run_probe(&container_name, &usage_dir, provider)));
            });
        }
        drop(sender);

        let mut completed = Vec::new();
        let mut succeeded = 0;
        for (provider, result) in receiver {
            completed.push(provider);
            let schedule = schedules
                .iter_mut()
                .find(|schedule| schedule.provider == provider)
                .expect("every provider has a schedule");
            let now = unix_time();
            match result {
                Ok(snapshot) => match usage_api::store_provider(usage_dir, provider, snapshot, now)
                {
                    Ok(outcome) => {
                        record_store_success(schedule, provider, outcome);
                        succeeded += 1;
                    }
                    Err(error) => {
                        eprintln!(
                            "t3-admin: {} usage refresh failed: {error}",
                            provider.as_str()
                        );
                        schedule.failed();
                    }
                },
                Err(error) => {
                    eprintln!(
                        "t3-admin: {} usage refresh failed: {error}",
                        provider.as_str()
                    );
                    schedule.failed();
                }
            }
        }
        for provider in due.iter().filter(|provider| !completed.contains(provider)) {
            schedules
                .iter_mut()
                .find(|schedule| schedule.provider == *provider)
                .expect("every provider has a schedule")
                .failed();
        }
        if failed_rounds.observe(due.len(), succeeded) {
            eprintln!("t3-admin: yielding usage collector lease after repeated provider failures");
            return;
        }
    }
}

fn run_probe(
    container_name: &str,
    usage_dir: &Path,
    provider: Provider,
) -> Result<ProviderSnapshot, String> {
    let output = collect_command_output(
        probe_command(container_name, provider),
        usage_dir,
        OUTER_TIMEOUT,
    )?;
    if !output.status.success() {
        return Err(format!("helper exited with {}", output.status));
    }
    usage_api::parse_probe(&output.stdout, provider, unix_time())
}

fn probe_command(container_name: &str, provider: Provider) -> Command {
    let mut command = Command::new("podman");
    command.args([
        "exec",
        container_name,
        "timeout",
        "--signal=TERM",
        "--kill-after=2s",
        "25s",
        HELPER_PATH,
        provider.as_str(),
    ]);
    command
}

#[derive(Debug)]
struct CommandOutput {
    status: std::process::ExitStatus,
    stdout: Vec<u8>,
}

struct OutputCapture {
    path: PathBuf,
    file: File,
}

impl OutputCapture {
    fn new(directory: &Path) -> Result<Self, String> {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        for counter in 0..10 {
            let path = directory.join(format!(
                "claude-sandbox-usage-output-{}-{nonce}-{counter}",
                std::process::id()
            ));
            match OpenOptions::new()
                .create_new(true)
                .read(true)
                .write(true)
                .mode(0o600)
                .open(&path)
            {
                Ok(file) => return Ok(Self { path, file }),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(format!("could not create helper output: {error}")),
            }
        }
        Err("could not allocate helper output".to_string())
    }

    fn exceeds_limit(&self) -> bool {
        self.file
            .metadata()
            .is_ok_and(|metadata| metadata.len() > MAX_HELPER_OUTPUT)
    }

    fn read(mut self) -> Result<Vec<u8>, String> {
        if self.exceeds_limit() {
            return Err("helper output exceeds its size limit".to_string());
        }
        self.file
            .seek(SeekFrom::Start(0))
            .and_then(|_| {
                let mut bytes = Vec::new();
                self.file
                    .by_ref()
                    .take(MAX_HELPER_OUTPUT + 1)
                    .read_to_end(&mut bytes)
                    .map(|_| bytes)
            })
            .map_err(|error| format!("could not read helper output: {error}"))
    }
}

impl Drop for OutputCapture {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn collect_command_output(
    mut command: Command,
    capture_dir: &Path,
    timeout: Duration,
) -> Result<CommandOutput, String> {
    command.process_group(0);
    let capture = OutputCapture::new(capture_dir)?;
    let stdout = capture
        .file
        .try_clone()
        .map_err(|error| format!("could not capture helper output: {error}"))?;
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("could not start helper: {error}"))?;

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if capture.exceeds_limit() => {
                terminate_process_group(child.id());
                let _ = child.kill();
                let _ = child.wait();
                return Err("helper output exceeds its size limit".to_string());
            }
            Ok(None) if Instant::now() < deadline => thread::sleep(POLL_INTERVAL),
            Ok(None) => {
                terminate_process_group(child.id());
                let _ = child.kill();
                let _ = child.wait();
                return Err("helper timed out".to_string());
            }
            Err(error) => {
                terminate_process_group(child.id());
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("could not wait for helper: {error}"));
            }
        }
    };
    let stdout = capture.read()?;
    if stdout.len() as u64 > MAX_HELPER_OUTPUT {
        return Err("helper output exceeds its size limit".to_string());
    }
    Ok(CommandOutput { status, stdout })
}

fn terminate_process_group(pid: u32) {
    let Ok(pid) = i32::try_from(pid) else { return };
    // SAFETY: the child was placed in a process group whose ID is its PID.
    unsafe {
        kill(-pid, SIGKILL);
    }
}

fn unix_time() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;
    use std::fs;

    #[test]
    fn lease_is_process_wide_and_released_when_dropped() {
        let directory = temporary_directory("lease");
        usage_api::prepare_usage_dir(&directory).unwrap();
        let first = acquire_lease(&directory).unwrap().unwrap();
        assert!(acquire_lease(&directory).unwrap().is_none());
        assert_eq!(
            fs::metadata(directory.join(LEASE_FILE))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        drop(first);
        assert!(acquire_lease(&directory).unwrap().is_some());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn failures_follow_the_requested_backoff_and_success_resets_it() {
        let directory = temporary_directory("schedule");
        let mut schedule = Schedule::new(Provider::Openai, &directory, unix_time());
        assert!(schedule.next_due <= Instant::now());
        let before = Instant::now();
        schedule.failed();
        assert_eq!(schedule.failures, 0);
        assert!(schedule.next_due >= before + STARTUP_RETRY);
        assert!(schedule.next_due <= Instant::now() + STARTUP_RETRY);
        for (index, delay) in FAILURE_DELAYS.iter().enumerate() {
            let before = Instant::now();
            schedule.failed();
            assert_eq!(schedule.failures, index + 1);
            assert!(schedule.next_due >= before + *delay);
            assert!(schedule.next_due <= Instant::now() + *delay);
        }
        schedule.failed();
        assert_eq!(schedule.failures, FAILURE_DELAYS.len() + 1);
        schedule.succeeded();
        assert_eq!(schedule.failures, 0);
        assert!(schedule.next_due > Instant::now() + REFRESH_INTERVAL - Duration::from_secs(1));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn repeated_full_failures_yield_but_partial_rounds_do_not() {
        let mut rounds = FailedRounds::default();
        assert!(!rounds.observe(3, 0));
        assert!(!rounds.observe(3, 0));
        assert!(rounds.observe(3, 0));

        let mut rounds = FailedRounds::default();
        assert!(!rounds.observe(3, 0));
        assert!(!rounds.observe(2, 0));
        assert!(!rounds.observe(3, 0));
        assert!(!rounds.observe(3, 1));
        assert_eq!(rounds.consecutive, 0);
    }

    #[test]
    fn directory_sync_warning_does_not_apply_provider_backoff() {
        let directory = temporary_directory("sync-warning");
        let mut schedule = Schedule::new(Provider::Openai, &directory, unix_time());
        schedule.failed();
        record_store_success(
            &mut schedule,
            Provider::Openai,
            usage_api::StoreOutcome::DirectorySyncFailed("simulated".to_string()),
        );

        assert_eq!(schedule.failures, 0);
        assert!(!schedule.startup_retry);
        assert!(schedule.next_due > Instant::now() + REFRESH_INTERVAL - Duration::from_secs(1));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn existing_snapshot_is_not_refreshed_before_thirty_minutes() {
        let directory = temporary_directory("existing");
        let now = unix_time();
        let response = format!(
            r#"{{"schema_version":1,"provider":"ollama","observed_at":{now},"buckets":[{{"period":"weekly","scope":"overall","used_percent":1,"resets_at":null}}]}}"#
        );
        let snapshot = usage_api::parse_probe(response.as_bytes(), Provider::Ollama, now).unwrap();
        usage_api::store_provider(&directory, Provider::Ollama, snapshot, now).unwrap();

        let before = Instant::now();
        let schedule = Schedule::new(Provider::Ollama, &directory, now);
        assert!(schedule.next_due >= before + REFRESH_INTERVAL);
        assert!(schedule.next_due <= Instant::now() + REFRESH_INTERVAL);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn probe_command_has_only_fixed_arguments() {
        let command = probe_command("managed-container", Provider::Anthropic);
        assert_eq!(command.get_program(), OsStr::new("podman"));
        let arguments = command
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            arguments,
            [
                "exec",
                "managed-container",
                "timeout",
                "--signal=TERM",
                "--kill-after=2s",
                "25s",
                HELPER_PATH,
                "anthropic"
            ]
        );
    }

    #[test]
    fn command_output_is_bounded_and_times_out() {
        let directory = temporary_directory("command-output");
        usage_api::prepare_usage_dir(&directory).unwrap();
        let mut success = Command::new("sh");
        success.args(["-c", "printf small"]);
        let output = collect_command_output(success, &directory, Duration::from_secs(2)).unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"small");

        let mut oversized = Command::new("sh");
        oversized.args(["-c", "head -c 70000 /dev/zero"]);
        assert!(collect_command_output(oversized, &directory, Duration::from_secs(2)).is_err());

        let mut slow = Command::new("sh");
        slow.args(["-c", "sleep 2"]);
        let started = Instant::now();
        assert_eq!(
            collect_command_output(slow, &directory, Duration::from_millis(100)).unwrap_err(),
            "helper timed out"
        );
        assert!(started.elapsed() < Duration::from_secs(1));

        let mut inherited_pipe = Command::new("sh");
        inherited_pipe.args(["-c", "sleep 5 & exit 0"]);
        let started = Instant::now();
        assert!(collect_command_output(inherited_pipe, &directory, Duration::from_secs(2)).is_ok());
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(fs::read_dir(&directory).unwrap().count(), 0);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn output_capture_is_private_and_created_under_usage_state() {
        let directory = temporary_directory("capture-location");
        usage_api::prepare_usage_dir(&directory).unwrap();
        let capture = OutputCapture::new(&directory).unwrap();

        assert_eq!(capture.path.parent(), Some(directory.as_path()));
        assert_eq!(
            capture.file.metadata().unwrap().permissions().mode() & 0o777,
            0o600
        );
        let path = capture.path.clone();
        drop(capture);
        assert!(!path.exists());
        fs::remove_dir_all(directory).unwrap();
    }

    fn temporary_directory(label: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "claude-sandbox-collector-{label}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&path).unwrap();
        path
    }
}
