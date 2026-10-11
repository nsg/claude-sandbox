use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const UUID: &str = "plan-usage@claude-sandbox";
const SHELL_VERSION: &str = "46";
const FILES: [(&str, &str); 3] = [
    ("metadata.json", include_str!("../extension/metadata.json")),
    ("extension.js", include_str!("../extension/extension.js")),
    (
        "stylesheet.css",
        include_str!("../extension/stylesheet.css"),
    ),
];

pub struct InstallOutcome {
    pub path: PathBuf,
    pub url: String,
    pub updated: bool,
    pub enabled: bool,
    pub shell_version: Option<String>,
}

pub fn data_dir(xdg_data_home: Option<&OsStr>, home: &Path) -> PathBuf {
    xdg_data_home
        .filter(|value| !value.is_empty() && Path::new(value).is_absolute())
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".local/share"))
}

pub fn parse_host(value: &str) -> Result<(String, Option<u16>), String> {
    let invalid =
        || "--host must be HOST or HOST:PORT, with an IPv6 address in brackets".to_string();
    let (host, port) = match value
        .strip_prefix('[')
        .and_then(|rest| rest.split_once(']'))
    {
        Some((address, rest)) => {
            let valid = !address.is_empty()
                && address
                    .chars()
                    .all(|character| character.is_ascii_hexdigit() || ":.".contains(character));
            let port = match rest {
                "" => None,
                rest => Some(rest.strip_prefix(':').ok_or_else(invalid)?),
            };
            (valid.then(|| format!("[{address}]")), port)
        }
        None => {
            let (name, port) = match value.rsplit_once(':') {
                Some((name, port)) => (name, Some(port)),
                None => (value, None),
            };
            let valid = !name.is_empty()
                && name.chars().all(|character| {
                    character.is_ascii_alphanumeric() || "-._".contains(character)
                });
            (valid.then(|| name.to_string()), port)
        }
    };
    let port = port
        .map(|port| port.parse::<u16>().ok().filter(|port| *port != 0))
        .map(|port| port.ok_or_else(invalid))
        .transpose()?;
    Ok((host.ok_or_else(invalid)?, port))
}

fn extension_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("gnome-shell/extensions").join(UUID)
}

fn write_file(path: &Path, name: &str, contents: &str) -> Result<(), String> {
    let file = path.join(name);
    fs::write(&file, contents)
        .map_err(|error| format!("could not write {}: {error}", file.display()))
}

fn write_files(data_dir: &Path, url: &str) -> Result<(PathBuf, bool), String> {
    let path = extension_dir(data_dir);
    let updated = path.exists();
    fs::create_dir_all(&path)
        .map_err(|error| format!("could not create {}: {error}", path.display()))?;
    let config = serde_json::json!({ "url": url }).to_string();
    for (name, contents) in FILES.into_iter().chain([("config.json", config.as_str())]) {
        write_file(&path, name, contents)?;
    }
    Ok((path, updated))
}

/// Brings an installed extension in line with this binary, leaving its
/// configured endpoint alone. Returns whether anything changed.
pub fn refresh(data_dir: &Path) -> Result<bool, String> {
    let path = extension_dir(data_dir);
    if !path.is_dir() {
        return Ok(false);
    }
    let mut changed = false;
    for (name, contents) in FILES {
        if fs::read_to_string(path.join(name)).is_ok_and(|installed| installed == contents) {
            continue;
        }
        write_file(&path, name, contents)?;
        changed = true;
    }
    Ok(changed)
}

fn command_output(program: &str, arguments: &[&str]) -> Option<String> {
    let output = Command::new(program)
        .args(arguments)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn append_extension(list: &str, uuid: &str) -> Option<String> {
    let entries = list.trim().strip_suffix(']')?.trim_end();
    if entries.ends_with('[') {
        Some(format!("['{uuid}']"))
    } else {
        Some(format!("{entries}, '{uuid}']"))
    }
}

fn enable() -> bool {
    if command_output("gnome-extensions", &["enable", UUID]).is_some() {
        return true;
    }
    // The shell learns about a new extension at login and gnome-extensions
    // refuses it until then, so record it for the next session instead.
    let setting = ["org.gnome.shell", "enabled-extensions"];
    let Some(current) = command_output("gsettings", &["get", setting[0], setting[1]]) else {
        return false;
    };
    if current.contains(&format!("'{UUID}'")) {
        return true;
    }
    append_extension(&current, UUID).is_some_and(|value| {
        command_output("gsettings", &["set", setting[0], setting[1], &value]).is_some()
    })
}

fn shell_major_version(output: &str) -> Option<String> {
    let version = output.split_whitespace().last()?;
    let major = version.split('.').next()?;
    (!major.is_empty() && major.chars().all(|character| character.is_ascii_digit()))
        .then(|| major.to_string())
}

pub fn install(data_dir: &Path, host: &str, port: u16) -> Result<InstallOutcome, String> {
    let url = format!("http://{host}:{port}");
    let (path, updated) = write_files(data_dir, &url)?;
    Ok(InstallOutcome {
        path,
        url,
        updated,
        enabled: enable(),
        shell_version: command_output("gnome-shell", &["--version"])
            .and_then(|output| shell_major_version(&output)),
    })
}

pub fn print_summary(outcome: &InstallOutcome) {
    let action = if outcome.updated {
        "Updated"
    } else {
        "Installed"
    };
    println!("{action} {}", outcome.path.display());
    println!();
    println!("  Usage endpoint          {}/api/usage", outcome.url);
    println!("  Remove it               gnome-extensions uninstall {UUID}");
    println!();
    if let Some(version) = outcome
        .shell_version
        .as_deref()
        .filter(|version| *version != SHELL_VERSION)
    {
        println!(
            "The extension targets GNOME Shell {SHELL_VERSION}; GNOME Shell {version} reports it as incompatible."
        );
        println!();
    }
    if outcome.enabled {
        println!("Log out and back in to load it.");
    } else {
        println!("Could not enable the extension. Log out and back in, then run:");
        println!("  gnome-extensions enable {UUID}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_data_dir(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "claude-sandbox-extension-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn extension_directory_matches_the_metadata_uuid() {
        let metadata: serde_json::Value = serde_json::from_str(FILES[0].1).unwrap();
        assert_eq!(metadata["uuid"], UUID);
        assert_eq!(
            metadata["shell-version"],
            serde_json::json!([SHELL_VERSION])
        );
    }

    #[test]
    fn appends_to_the_enabled_extension_list() {
        assert_eq!(
            append_extension("@as []", "new@example").unwrap(),
            "['new@example']"
        );
        assert_eq!(
            append_extension("['one@example', 'two@example']\n", "new@example").unwrap(),
            "['one@example', 'two@example', 'new@example']"
        );
        assert_eq!(append_extension("unexpected", "new@example"), None);
    }

    #[test]
    fn reads_the_major_shell_version() {
        assert_eq!(
            shell_major_version("GNOME Shell 46.0").as_deref(),
            Some("46")
        );
        assert_eq!(shell_major_version("GNOME Shell"), None);
    }

    #[test]
    fn writes_the_extension_with_the_configured_endpoint() {
        let data_dir = test_data_dir("install");
        let (path, updated) = write_files(&data_dir, "http://127.0.0.1:3801").unwrap();
        assert!(!updated);
        assert_eq!(path, data_dir.join("gnome-shell/extensions").join(UUID));
        for (name, contents) in FILES {
            assert_eq!(fs::read_to_string(path.join(name)).unwrap(), contents);
        }
        let config: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(path.join("config.json")).unwrap()).unwrap();
        assert_eq!(config["url"], "http://127.0.0.1:3801");
        assert!(write_files(&data_dir, "http://127.0.0.1:3801").unwrap().1);
        fs::remove_dir_all(data_dir).unwrap();
    }

    #[test]
    fn refresh_restores_changed_files_and_keeps_the_endpoint() {
        let data_dir = test_data_dir("refresh");
        assert!(!refresh(&data_dir).unwrap());
        assert!(!extension_dir(&data_dir).exists());

        let (path, _) = write_files(&data_dir, "http://build-box:3801").unwrap();
        assert!(!refresh(&data_dir).unwrap());
        fs::write(path.join("extension.js"), "// older release").unwrap();
        fs::remove_file(path.join("stylesheet.css")).unwrap();
        assert!(refresh(&data_dir).unwrap());
        for (name, contents) in FILES {
            assert_eq!(fs::read_to_string(path.join(name)).unwrap(), contents);
        }
        assert!(
            fs::read_to_string(path.join("config.json"))
                .unwrap()
                .contains("http://build-box:3801")
        );
        fs::remove_dir_all(data_dir).unwrap();
    }

    #[test]
    fn parses_hosts_with_an_optional_port() {
        let parsed = |value| parse_host(value).unwrap();
        assert_eq!(parsed("build-box"), ("build-box".to_string(), None));
        assert_eq!(
            parsed("build-box.lan:3801"),
            ("build-box.lan".to_string(), Some(3801))
        );
        assert_eq!(
            parsed("[fd00::1]:3801"),
            ("[fd00::1]".to_string(), Some(3801))
        );
        assert_eq!(parsed("[::1]"), ("[::1]".to_string(), None));
        for invalid in [
            "",
            ":3801",
            "host:0",
            "host:port",
            "fd00::1",
            "[fd00::1]3801",
            "http://host:3801",
            "host/path",
        ] {
            assert!(parse_host(invalid).is_err(), "{invalid}");
        }
    }
}
