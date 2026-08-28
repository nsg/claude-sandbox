use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs::{self, DirBuilder, OpenOptions, Permissions};
use std::io::{Read, Write};
#[cfg(target_os = "linux")]
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;
use std::time::{SystemTime, UNIX_EPOCH};

pub(crate) const CACHE_FILE: &str = "usage-v1.json";
const MAX_CACHE_BYTES: u64 = 64 * 1024;
const FRESH_SECS: i64 = 40 * 60;
const MAX_CLOCK_SKEW_SECS: i64 = 5 * 60;
const MAX_RESET_HORIZON_SECS: i64 = 366 * 24 * 60 * 60;
const MAX_BUCKETS: usize = 32;
const MAX_LABEL_BYTES: usize = 128;
const MAX_WINDOW_BYTES: usize = 64;
const RESPONSE_CACHE_SECS: u64 = 5;
#[cfg(target_os = "linux")]
const O_NONBLOCK: i32 = 0o4_000;
#[cfg(target_os = "linux")]
const O_NOFOLLOW: i32 = 0o400_000;

static RESPONSE_CACHE: OnceLock<Mutex<Option<CachedResponse>>> = OnceLock::new();

#[derive(Clone, Debug)]
struct CachedResponse {
    usage_dir: PathBuf,
    collected_at: Instant,
    response: UsageResponse,
    available: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Provider {
    Anthropic,
    Openai,
    Ollama,
}

impl Provider {
    pub(crate) const ALL: [Self; 3] = [Self::Anthropic, Self::Openai, Self::Ollama];

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic",
            Self::Openai => "openai",
            Self::Ollama => "ollama",
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProbeResponse {
    schema_version: u8,
    provider: Provider,
    observed_at: i64,
    buckets: Vec<ProbeBucket>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct StoredUsage {
    schema_version: u8,
    providers: StoredProviders,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct StoredProviders {
    anthropic: Option<ProviderSnapshot>,
    openai: Option<ProviderSnapshot>,
    ollama: Option<ProviderSnapshot>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct ProviderSnapshot {
    observed_at: i64,
    buckets: Vec<UsageBucket>,
}

#[derive(Clone, Debug, Serialize)]
pub struct UsageResponse {
    schema_version: u8,
    providers: Providers,
}

#[derive(Clone, Debug, Serialize)]
struct Providers {
    anthropic: ProviderUsage,
    openai: ProviderUsage,
    ollama: ProviderUsage,
}

#[derive(Clone, Debug, Serialize)]
struct ProviderUsage {
    freshness: Freshness,
    updated_at: Option<String>,
    buckets: Vec<PublicBucket>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
enum Freshness {
    Fresh,
    Stale,
    Unknown,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProbeBucket {
    period: Period,
    #[serde(default)]
    scope: Option<Scope>,
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    window: Option<String>,
    used_percent: u8,
    resets_at: Option<i64>,
}

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
struct UsageBucket {
    period: Period,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    scope: Option<Scope>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    window: Option<String>,
    used_percent: u8,
    resets_at: Option<i64>,
}

#[derive(Clone, Debug, Serialize)]
struct PublicBucket {
    period: Period,
    #[serde(skip_serializing_if = "Option::is_none")]
    scope: Option<Scope>,
    #[serde(skip_serializing_if = "Option::is_none")]
    label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    window: Option<String>,
    used_percent: u8,
    resets_at: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "lowercase")]
enum Period {
    Session,
    Weekly,
    Monthly,
    Other,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "lowercase")]
enum Scope {
    Overall,
    Model,
}

impl StoredUsage {
    fn empty() -> Self {
        Self {
            schema_version: 1,
            providers: StoredProviders::default(),
        }
    }

    fn provider(&self, provider: Provider) -> Option<&ProviderSnapshot> {
        match provider {
            Provider::Anthropic => self.providers.anthropic.as_ref(),
            Provider::Openai => self.providers.openai.as_ref(),
            Provider::Ollama => self.providers.ollama.as_ref(),
        }
    }

    fn set_provider(&mut self, provider: Provider, snapshot: ProviderSnapshot) {
        *match provider {
            Provider::Anthropic => &mut self.providers.anthropic,
            Provider::Openai => &mut self.providers.openai,
            Provider::Ollama => &mut self.providers.ollama,
        } = Some(snapshot);
    }
}

enum StoredRead {
    Missing,
    Unsupported,
    Current(StoredUsage),
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum StoreOutcome {
    Durable,
    DirectorySyncFailed(String),
}

impl ProviderUsage {
    fn unknown() -> Self {
        Self {
            freshness: Freshness::Unknown,
            updated_at: None,
            buckets: Vec::new(),
        }
    }

    fn from_snapshot(snapshot: ProviderSnapshot, now: i64) -> Self {
        let freshness = freshness(snapshot.observed_at, now);
        if freshness == Freshness::Unknown {
            return Self::unknown();
        }
        let buckets = snapshot
            .buckets
            .into_iter()
            .map(|bucket| PublicBucket {
                period: bucket.period,
                scope: bucket.scope,
                label: bucket.label,
                window: bucket.window,
                used_percent: bucket.used_percent,
                resets_at: bucket
                    .resets_at
                    .filter(|timestamp| valid_public_reset(*timestamp, now))
                    .map(format_epoch),
            })
            .collect();
        Self {
            freshness,
            updated_at: Some(format_epoch(snapshot.observed_at)),
            buckets,
        }
    }

    fn is_available(&self) -> bool {
        self.freshness != Freshness::Unknown
    }
}

pub fn collect(usage_dir: &Path) -> (UsageResponse, bool) {
    let cache = RESPONSE_CACHE.get_or_init(|| Mutex::new(None));
    {
        let cached = cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(cached) = cached.as_ref()
            && cached.usage_dir == usage_dir
            && cached.collected_at.elapsed().as_secs() < RESPONSE_CACHE_SECS
        {
            return (cached.response.clone(), cached.available);
        }
    }

    let (response, available) = collect_at(usage_dir, unix_time());
    *cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(CachedResponse {
        usage_dir: usage_dir.to_path_buf(),
        collected_at: Instant::now(),
        response: response.clone(),
        available,
    });
    (response, available)
}

fn collect_at(usage_dir: &Path, now: i64) -> (UsageResponse, bool) {
    let stored = match read_stored(usage_dir, now) {
        StoredRead::Current(stored) => stored,
        StoredRead::Missing | StoredRead::Unsupported => StoredUsage::empty(),
    };
    let provider = |name| {
        stored
            .provider(name)
            .cloned()
            .map(|snapshot| ProviderUsage::from_snapshot(snapshot, now))
            .unwrap_or_else(ProviderUsage::unknown)
    };
    let anthropic = provider(Provider::Anthropic);
    let openai = provider(Provider::Openai);
    let ollama = provider(Provider::Ollama);
    let available = anthropic.is_available() || openai.is_available() || ollama.is_available();
    (
        UsageResponse {
            schema_version: 1,
            providers: Providers {
                anthropic,
                openai,
                ollama,
            },
        },
        available,
    )
}

pub(crate) fn prepare_usage_dir(path: &Path) -> Result<PathBuf, String> {
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
        .map_err(|error| format!("could not create usage state: {error}"))?;
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("could not inspect usage state: {error}"))?;
    if !metadata.file_type().is_dir() {
        return Err("usage state path is not a directory".to_string());
    }
    fs::set_permissions(path, Permissions::from_mode(0o700))
        .map_err(|error| format!("could not protect usage state: {error}"))?;
    fs::canonicalize(path).map_err(|error| format!("could not resolve usage state: {error}"))
}

pub(crate) fn parse_probe(
    bytes: &[u8],
    expected: Provider,
    now: i64,
) -> Result<ProviderSnapshot, String> {
    let response: ProbeResponse =
        serde_json::from_slice(bytes).map_err(|error| format!("invalid helper JSON: {error}"))?;
    if response.schema_version != 1 {
        return Err("unsupported helper schema".to_string());
    }
    if response.provider != expected {
        return Err("helper returned the wrong provider".to_string());
    }
    let mut snapshot = ProviderSnapshot {
        observed_at: response.observed_at,
        buckets: response
            .buckets
            .into_iter()
            .map(|bucket| UsageBucket {
                period: bucket.period,
                scope: bucket.scope,
                label: bucket.label,
                window: bucket.window,
                used_percent: bucket.used_percent,
                resets_at: bucket.resets_at,
            })
            .collect(),
    };
    sanitize_snapshot(&mut snapshot, now)?;
    normalize_buckets(&mut snapshot.buckets);
    Ok(snapshot)
}

pub(crate) fn observed_at(usage_dir: &Path, provider: Provider, now: i64) -> Option<i64> {
    match read_stored(usage_dir, now) {
        StoredRead::Current(stored) => stored
            .provider(provider)
            .map(|snapshot| snapshot.observed_at),
        StoredRead::Missing | StoredRead::Unsupported => None,
    }
}

pub(crate) fn store_provider(
    usage_dir: &Path,
    provider: Provider,
    mut snapshot: ProviderSnapshot,
    now: i64,
) -> Result<StoreOutcome, String> {
    sanitize_snapshot(&mut snapshot, now)?;
    normalize_buckets(&mut snapshot.buckets);
    prepare_usage_dir(usage_dir)?;
    let mut stored = match read_stored(usage_dir, now) {
        StoredRead::Current(stored) => stored,
        StoredRead::Missing => StoredUsage::empty(),
        StoredRead::Unsupported => {
            return Err(
                "usage cache has an unsupported schema; refusing to overwrite it".to_string(),
            );
        }
    };
    stored.set_provider(provider, snapshot);
    atomic_write_json(&usage_dir.join(CACHE_FILE), &stored)
}

fn sanitize_snapshot(snapshot: &mut ProviderSnapshot, now: i64) -> Result<(), String> {
    if snapshot.observed_at < 0 || snapshot.observed_at > now.saturating_add(MAX_CLOCK_SKEW_SECS) {
        return Err("helper timestamp is outside the allowed range".to_string());
    }
    if !(1..=MAX_BUCKETS).contains(&snapshot.buckets.len()) {
        return Err("helper must return between 1 and 32 buckets".to_string());
    }
    for bucket in &mut snapshot.buckets {
        if bucket.used_percent > 100 {
            return Err("helper percentage exceeds 100".to_string());
        }
        if bucket
            .label
            .as_deref()
            .is_some_and(|label| !valid_display_text(label, MAX_LABEL_BYTES))
        {
            bucket.label = None;
        }
        if bucket
            .window
            .as_deref()
            .is_some_and(|window| !valid_display_text(window, MAX_WINDOW_BYTES))
        {
            bucket.window = None;
        }
        if let Some(reset) = bucket.resets_at
            && (reset < snapshot.observed_at.saturating_sub(MAX_CLOCK_SKEW_SECS)
                || reset > snapshot.observed_at.saturating_add(MAX_RESET_HORIZON_SECS))
        {
            bucket.resets_at = None;
        }
    }
    Ok(())
}

fn valid_display_text(value: &str, max_bytes: usize) -> bool {
    !value.is_empty()
        && value.len() <= max_bytes
        && value.chars().any(|character| character != ' ')
        && value.chars().all(|character| {
            !character.is_control()
                && (!character.is_whitespace() || character == ' ')
                && !matches!(
                    character,
                    '\u{00ad}'
                        | '\u{061c}'
                        | '\u{200b}'..='\u{200f}'
                        | '\u{202a}'..='\u{202e}'
                        | '\u{2060}'..='\u{2064}'
                        | '\u{2066}'..='\u{2069}'
                        | '\u{feff}'
                        | '\u{fff9}'..='\u{fffb}'
                        | '\u{e0000}'..='\u{e007f}'
                )
        })
}

fn read_stored(usage_dir: &Path, now: i64) -> StoredRead {
    let Some(root) = read_json_file::<Value>(&usage_dir.join(CACHE_FILE), MAX_CACHE_BYTES) else {
        return StoredRead::Missing;
    };
    let Some(schema_version) = root.get("schema_version").and_then(Value::as_u64) else {
        return StoredRead::Missing;
    };
    if schema_version != 1 {
        return StoredRead::Unsupported;
    }
    let providers = root.get("providers").and_then(Value::as_object);
    let mut stored = StoredUsage::empty();
    for provider in Provider::ALL {
        if let Some(mut snapshot) = providers
            .and_then(|values| values.get(provider.as_str()))
            .and_then(|value| serde_json::from_value::<ProviderSnapshot>(value.clone()).ok())
            && sanitize_snapshot(&mut snapshot, now).is_ok()
        {
            normalize_buckets(&mut snapshot.buckets);
            stored.set_provider(provider, snapshot);
        }
    }
    StoredRead::Current(stored)
}

fn atomic_write_json<T: Serialize>(path: &Path, value: &T) -> Result<StoreOutcome, String> {
    let parent = path
        .parent()
        .ok_or_else(|| "usage cache has no parent directory".to_string())?;
    let bytes = serde_json::to_vec(value)
        .map_err(|error| format!("could not encode usage cache: {error}"))?;
    if bytes.len() as u64 > MAX_CACHE_BYTES {
        return Err("usage cache exceeds its size limit".to_string());
    }
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let temp = parent.join(format!(
        ".{}.{}.{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("usage"),
        std::process::id(),
        nonce
    ));
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(&temp)
        .map_err(|error| format!("could not create usage cache: {error}"))?;
    if let Err(error) = file.write_all(&bytes).and_then(|_| file.sync_all()) {
        let _ = fs::remove_file(&temp);
        return Err(format!("could not write usage cache: {error}"));
    }
    fs::rename(&temp, path).map_err(|error| {
        let _ = fs::remove_file(&temp);
        format!("could not replace usage cache: {error}")
    })?;
    fs::set_permissions(path, Permissions::from_mode(0o600))
        .map_err(|error| format!("could not protect usage cache: {error}"))?;
    match OpenOptions::new()
        .read(true)
        .open(parent)
        .and_then(|directory| directory.sync_all())
    {
        Ok(()) => Ok(StoreOutcome::Durable),
        Err(error) => Ok(StoreOutcome::DirectorySyncFailed(format!(
            "could not sync usage state: {error}"
        ))),
    }
}

fn read_json_file<T: for<'de> Deserialize<'de>>(path: &Path, limit: u64) -> Option<T> {
    let metadata = fs::symlink_metadata(path).ok()?;
    if !metadata.file_type().is_file() || metadata.len() > limit {
        return None;
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(target_os = "linux")]
    options.custom_flags(O_NONBLOCK | O_NOFOLLOW);
    let file = options.open(path).ok()?;
    let metadata = file.metadata().ok()?;
    if !metadata.is_file() || metadata.len() > limit {
        return None;
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(limit + 1).read_to_end(&mut bytes).ok()?;
    if bytes.len() as u64 > limit {
        return None;
    }
    serde_json::from_slice(&bytes).ok()
}

fn freshness(observed_at: i64, now: i64) -> Freshness {
    if observed_at < 0 || observed_at > now.saturating_add(MAX_CLOCK_SKEW_SECS) {
        Freshness::Unknown
    } else if now.saturating_sub(observed_at) <= FRESH_SECS {
        Freshness::Fresh
    } else {
        Freshness::Stale
    }
}

fn valid_public_reset(timestamp: i64, now: i64) -> bool {
    timestamp > now && timestamp <= now.saturating_add(MAX_RESET_HORIZON_SECS)
}

fn normalize_buckets(buckets: &mut Vec<UsageBucket>) {
    buckets.sort();
    buckets.dedup();
}

fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let days = days + 719_468;
    let era = if days >= 0 { days } else { days - 146_096 } / 146_097;
    let day_of_era = days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    (year + i64::from(month <= 2), month, day)
}

fn format_epoch(timestamp: i64) -> String {
    let days = timestamp.div_euclid(86_400);
    let seconds = timestamp.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let hour = seconds / 3_600;
    let minute = seconds % 3_600 / 60;
    let second = seconds % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
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
    use serde_json::json;
    use std::os::unix::fs::{PermissionsExt, symlink};

    const NOW: i64 = 1_787_500_000;

    #[test]
    fn validates_and_normalizes_strict_probe_output() {
        let response = json!({
            "schema_version": 1,
            "provider": "openai",
            "observed_at": NOW - 20,
            "buckets": [
                {"period":"weekly", "scope":"overall", "label":"Codex",
                 "window":"secondary", "used_percent":42, "resets_at":NOW + 5000},
                {"period":"session", "label":"GPT-5 Codex", "window":"primary",
                 "used_percent":7,
                 "resets_at":null},
                {"period":"session", "label":"GPT-5 Codex", "window":"primary",
                 "used_percent":7,
                 "resets_at":null}
            ]
        });
        let snapshot = parse_probe(
            serde_json::to_string(&response).unwrap().as_bytes(),
            Provider::Openai,
            NOW,
        )
        .unwrap();

        assert_eq!(snapshot.buckets.len(), 2);
        assert_eq!(snapshot.buckets[0].period, Period::Session);
        assert_eq!(snapshot.buckets[0].scope, None);
        assert_eq!(snapshot.buckets[0].label.as_deref(), Some("GPT-5 Codex"));
        assert_eq!(snapshot.buckets[1].period, Period::Weekly);
    }

    #[test]
    fn rejects_unknown_fields_provider_mismatch_and_invalid_ranges() {
        let base = json!({
            "schema_version": 1,
            "provider": "openai",
            "observed_at": NOW,
            "buckets": [{"period":"weekly", "scope":"overall",
                         "used_percent":42, "resets_at":NOW + 5000}]
        });
        let mut unknown = base.clone();
        unknown["account"] = json!("private");
        assert!(parse_probe(unknown.to_string().as_bytes(), Provider::Openai, NOW).is_err());
        let mut unknown_bucket = base.clone();
        unknown_bucket["buckets"][0]["account_id"] = json!("private");
        assert!(parse_probe(unknown_bucket.to_string().as_bytes(), Provider::Openai, NOW).is_err());
        assert!(parse_probe(base.to_string().as_bytes(), Provider::Ollama, NOW).is_err());

        let mut percentage = base.clone();
        percentage["buckets"][0]["used_percent"] = json!(101);
        assert!(parse_probe(percentage.to_string().as_bytes(), Provider::Openai, NOW).is_err());

        let mut future = base;
        future["observed_at"] = json!(NOW + MAX_CLOCK_SKEW_SECS + 1);
        assert!(parse_probe(future.to_string().as_bytes(), Provider::Openai, NOW).is_err());
    }

    #[test]
    fn preserves_safe_labels_and_drops_unsafe_optional_display_text() {
        let long_window = "x".repeat(MAX_WINDOW_BYTES + 1);
        let response = json!({
            "schema_version":1, "provider":"openai", "observed_at":NOW,
            "buckets":[
                {"period":"session", "label":"GPT-5 Codex", "window":"primary",
                 "used_percent":7, "resets_at":null},
                {"period":"weekly", "scope":"overall", "label":"bad\u{1b}[31m",
                 "window":"\u{202e}secondary", "used_percent":42, "resets_at":null},
                {"period":"monthly", "label":"Named limit", "window":long_window,
                 "used_percent":3, "resets_at":null}
            ]
        });
        let snapshot = parse_probe(response.to_string().as_bytes(), Provider::Openai, NOW).unwrap();

        assert_eq!(snapshot.buckets[0].label.as_deref(), Some("GPT-5 Codex"));
        assert_eq!(snapshot.buckets[0].window.as_deref(), Some("primary"));
        assert_eq!(snapshot.buckets[0].scope, None);
        assert_eq!(snapshot.buckets[1].label, None);
        assert_eq!(snapshot.buckets[1].window, None);
        assert_eq!(snapshot.buckets[1].scope, Some(Scope::Overall));
        assert_eq!(snapshot.buckets[2].label.as_deref(), Some("Named limit"));
        assert_eq!(snapshot.buckets[2].window, None);
    }

    #[test]
    fn rejects_invisible_and_directional_display_text() {
        assert!(valid_display_text("GPT-5 Codex", MAX_LABEL_BYTES));
        for value in [
            "",
            "   ",
            "line\u{2028}break",
            "hidden\u{feff}mark",
            "tag\u{e0001}",
            "right\u{202e}left",
        ] {
            assert!(!valid_display_text(value, MAX_LABEL_BYTES), "{value:?}");
        }
        assert!(!valid_display_text(
            &"x".repeat(MAX_LABEL_BYTES + 1),
            MAX_LABEL_BYTES
        ));
    }

    #[test]
    fn invalid_optional_reset_is_dropped_without_losing_provider() {
        let response = json!({
            "schema_version":1, "provider":"openai", "observed_at":NOW,
            "buckets":[{"period":"weekly", "scope":"overall",
                        "used_percent":42,
                        "resets_at":NOW + MAX_RESET_HORIZON_SECS + 1}]
        });
        let snapshot = parse_probe(response.to_string().as_bytes(), Provider::Openai, NOW).unwrap();

        assert_eq!(snapshot.buckets.len(), 1);
        assert_eq!(snapshot.buckets[0].resets_at, None);
    }

    #[test]
    fn rejects_empty_and_excessive_bucket_lists() {
        for count in [0, MAX_BUCKETS + 1] {
            let buckets = (0..count)
                .map(|_| {
                    json!({"period":"weekly", "scope":"overall",
                           "used_percent":1, "resets_at":null})
                })
                .collect::<Vec<_>>();
            let response = json!({
                "schema_version":1, "provider":"anthropic",
                "observed_at":NOW, "buckets":buckets
            });
            assert!(
                parse_probe(response.to_string().as_bytes(), Provider::Anthropic, NOW).is_err()
            );
        }
    }

    #[test]
    fn global_cache_is_sanitized_private_and_drives_freshness() {
        let directory = temporary_directory("global-cache");
        let snapshot = ProviderSnapshot {
            observed_at: NOW - 10,
            buckets: vec![UsageBucket {
                period: Period::Weekly,
                scope: Some(Scope::Overall),
                label: Some("Codex".to_string()),
                window: Some("secondary".to_string()),
                used_percent: 42,
                resets_at: Some(NOW + 5000),
            }],
        };
        store_provider(&directory, Provider::Openai, snapshot, NOW).unwrap();

        assert_eq!(
            fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(directory.join(CACHE_FILE))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        let (fresh, available) = collect_at(&directory, NOW);
        assert!(available);
        let fresh = serde_json::to_value(fresh).unwrap();
        assert_eq!(fresh["providers"]["openai"]["freshness"], "fresh");
        assert_eq!(
            fresh["providers"]["openai"]["updated_at"],
            format_epoch(NOW - 10)
        );
        assert_eq!(
            fresh["providers"]["openai"]["buckets"][0]["resets_at"],
            format_epoch(NOW + 5000)
        );
        assert_eq!(fresh["providers"]["openai"]["buckets"][0]["label"], "Codex");

        let (stale, _) = collect_at(&directory, NOW + FRESH_SECS + 1);
        let stale = serde_json::to_value(stale).unwrap();
        assert_eq!(stale["providers"]["openai"]["freshness"], "stale");
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn all_unknown_is_unavailable_with_fixed_provider_keys() {
        let directory = temporary_directory("unknown");
        let (response, available) = collect_at(&directory, NOW);
        let response = serde_json::to_value(response).unwrap();

        assert!(!available);
        assert_eq!(response["schema_version"], 1);
        for provider in ["anthropic", "openai", "ollama"] {
            assert_eq!(response["providers"][provider]["freshness"], "unknown");
            assert!(response["providers"][provider]["updated_at"].is_null());
            assert_eq!(response["providers"][provider]["buckets"], json!([]));
        }
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn updates_one_provider_without_discarding_another() {
        let directory = temporary_directory("merge");
        for (provider, used) in [(Provider::Openai, 12), (Provider::Ollama, 34)] {
            store_provider(
                &directory,
                provider,
                ProviderSnapshot {
                    observed_at: NOW,
                    buckets: vec![UsageBucket {
                        period: Period::Weekly,
                        scope: Some(Scope::Overall),
                        label: None,
                        window: None,
                        used_percent: used,
                        resets_at: None,
                    }],
                },
                NOW,
            )
            .unwrap();
        }
        let (response, available) = collect_at(&directory, NOW);
        let response = serde_json::to_value(response).unwrap();
        assert!(available);
        assert_eq!(
            response["providers"]["openai"]["buckets"][0]["used_percent"],
            12
        );
        assert_eq!(
            response["providers"]["ollama"]["buckets"][0]["used_percent"],
            34
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn invalid_persisted_provider_does_not_clobber_other_last_good_data() {
        let directory = temporary_directory("provider-pruning");
        let cache = json!({
            "schema_version":1,
            "future_extension":"ignored",
            "providers":{
                "anthropic":null,
                "openai":{
                    "observed_at":NOW,
                    "buckets":[{"period":"weekly", "scope":"overall",
                                "used_percent":41, "resets_at":null,
                                "future_bucket_extension":"ignored"}],
                    "future_extension":true
                },
                "ollama":{
                    "observed_at":NOW + MAX_CLOCK_SKEW_SECS + 1,
                    "buckets":[{"period":"weekly", "scope":"overall",
                                "used_percent":99, "resets_at":null}]
                }
            }
        });
        fs::write(directory.join(CACHE_FILE), cache.to_string()).unwrap();
        let anthropic = json!({
            "schema_version":1, "provider":"anthropic", "observed_at":NOW,
            "buckets":[{"period":"session", "scope":"overall",
                        "used_percent":3, "resets_at":null}]
        });
        let snapshot =
            parse_probe(anthropic.to_string().as_bytes(), Provider::Anthropic, NOW).unwrap();
        store_provider(&directory, Provider::Anthropic, snapshot, NOW).unwrap();

        let (response, available) = collect_at(&directory, NOW);
        let response = serde_json::to_value(response).unwrap();
        assert!(available);
        assert_eq!(
            response["providers"]["openai"]["buckets"][0]["used_percent"],
            41
        );
        assert_eq!(response["providers"]["ollama"]["freshness"], "unknown");
        assert_eq!(
            response["providers"]["anthropic"]["buckets"][0]["used_percent"],
            3
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn unsupported_cache_schema_is_never_overwritten() {
        let directory = temporary_directory("future-schema");
        let path = directory.join(CACHE_FILE);
        let future = br#"{"schema_version":2,"providers":{},"future":"keep-me"}"#;
        fs::write(&path, future).unwrap();
        let response = json!({
            "schema_version":1, "provider":"ollama", "observed_at":NOW,
            "buckets":[{"period":"weekly", "scope":"overall",
                        "used_percent":1, "resets_at":null}]
        });
        let snapshot = parse_probe(response.to_string().as_bytes(), Provider::Ollama, NOW).unwrap();

        assert!(store_provider(&directory, Provider::Ollama, snapshot, NOW).is_err());
        assert_eq!(fs::read(path).unwrap(), future);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn response_cache_holds_a_snapshot_for_five_seconds() {
        let directory = temporary_directory("response-cache");
        let now = unix_time();
        for used in [10, 20] {
            let response = json!({
                "schema_version":1, "provider":"openai", "observed_at":now,
                "buckets":[{"period":"weekly", "scope":"overall",
                            "used_percent":used, "resets_at":null}]
            });
            let snapshot =
                parse_probe(response.to_string().as_bytes(), Provider::Openai, now).unwrap();
            store_provider(&directory, Provider::Openai, snapshot, now).unwrap();
            let (public, _) = collect(&directory);
            let public = serde_json::to_value(public).unwrap();
            assert_eq!(
                public["providers"]["openai"]["buckets"][0]["used_percent"],
                10
            );
        }
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn rejects_symlinks_and_oversized_cache_files() {
        let directory = temporary_directory("reader");
        let regular = directory.join("regular.json");
        let link = directory.join("link.json");
        let oversized = directory.join("oversized.json");
        fs::write(&regular, b"{}").unwrap();
        symlink(&regular, &link).unwrap();
        fs::write(&oversized, vec![b' '; MAX_CACHE_BYTES as usize + 1]).unwrap();

        assert!(read_json_file::<Value>(&regular, MAX_CACHE_BYTES).is_some());
        assert!(read_json_file::<Value>(&link, MAX_CACHE_BYTES).is_none());
        assert!(read_json_file::<Value>(&oversized, MAX_CACHE_BYTES).is_none());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn refuses_a_symlink_as_the_global_state_directory() {
        let directory = temporary_directory("state-symlink");
        let target = directory.join("target");
        let link = directory.join("link");
        fs::create_dir(&target).unwrap();
        symlink(&target, &link).unwrap();

        assert!(prepare_usage_dir(&link).is_err());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn formats_epochs_around_leap_day() {
        assert_eq!(format_epoch(0), "1970-01-01T00:00:00Z");
        assert_eq!(format_epoch(951_827_696), "2000-02-29T12:34:56Z");
    }

    fn temporary_directory(label: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "claude-sandbox-usage-{label}-{}-{}",
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
