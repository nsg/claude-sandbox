use serde::Serialize;
use serde_json::Value;
use std::env;
use std::fs::{self, OpenOptions};
use std::io::Read;
#[cfg(target_os = "linux")]
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

const MAX_USAGE_FILE_BYTES: u64 = 4 * 1024 * 1024;
const RESPONSE_CACHE_SECS: u64 = 5;
#[cfg(target_os = "linux")]
const O_NONBLOCK: i32 = 0o4_000;
#[cfg(target_os = "linux")]
const O_NOFOLLOW: i32 = 0o400_000;
const ANTHROPIC_FRESH_SECS: i64 = 30 * 60;
const VENDOR_FRESH_SECS: i64 = 10 * 60;
const MAX_CLOCK_SKEW_SECS: i64 = 5 * 60;
const MAX_RESET_HORIZON_SECS: i64 = 366 * 24 * 60 * 60;

static RESPONSE_CACHE: OnceLock<Mutex<Option<CachedResponse>>> = OnceLock::new();

#[derive(Clone, Debug)]
struct CachedResponse {
    workspace_root: PathBuf,
    collected_at: Instant,
    response: UsageResponse,
    available: bool,
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
    buckets: Vec<UsageBucket>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
enum Freshness {
    Fresh,
    Stale,
    Unknown,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct UsageBucket {
    period: Period,
    scope: Scope,
    used_percent: u8,
    resets_at: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "lowercase")]
enum Period {
    Session,
    Weekly,
    Monthly,
    Other,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "lowercase")]
enum Scope {
    Overall,
    Model,
}

impl ProviderUsage {
    fn unknown() -> Self {
        Self {
            freshness: Freshness::Unknown,
            buckets: Vec::new(),
        }
    }

    fn is_available(&self) -> bool {
        self.freshness != Freshness::Unknown
    }
}

pub fn collect(workspace_root: &Path) -> (UsageResponse, bool) {
    let cache = RESPONSE_CACHE.get_or_init(|| Mutex::new(None));
    {
        let cached = cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(cached) = cached.as_ref()
            && cached.workspace_root == workspace_root
            && cached.collected_at.elapsed().as_secs() < RESPONSE_CACHE_SECS
        {
            return (cached.response.clone(), cached.available);
        }
    }

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0);
    let (response, available) = collect_at(workspace_root, now);
    *cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(CachedResponse {
        workspace_root: workspace_root.to_path_buf(),
        collected_at: Instant::now(),
        response: response.clone(),
        available,
    });
    (response, available)
}

fn collect_at(workspace_root: &Path, now: i64) -> (UsageResponse, bool) {
    let anthropic = find_claude_usage()
        .as_deref()
        .and_then(read_json_file)
        .map(|value| parse_anthropic(&value, now))
        .unwrap_or_else(ProviderUsage::unknown);

    let state_dir = safe_state_dir(workspace_root);
    let openai = state_dir
        .as_ref()
        .and_then(|directory| read_json_file(&directory.join("vendor-usage.codex.json")))
        .map(|value| parse_vendor(&value, now))
        .unwrap_or_else(ProviderUsage::unknown);
    let ollama = state_dir
        .as_ref()
        .and_then(|directory| read_json_file(&directory.join("vendor-usage.ollama.json")))
        .map(|value| parse_vendor(&value, now))
        .unwrap_or_else(ProviderUsage::unknown);

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

fn find_claude_usage() -> Option<PathBuf> {
    let home = env::var_os("HOME").map(PathBuf::from)?;
    let mut candidates = Vec::new();
    if let Some(directory) = env::var_os("CLAUDE_CONFIG_DIR").filter(|value| !value.is_empty()) {
        candidates.push(PathBuf::from(directory).join(".claude.json"));
    }
    candidates.push(home.join(".claude/.claude.json"));
    candidates.push(home.join(".claude.json"));
    candidates.into_iter().find(|path| path.is_file())
}

fn safe_state_dir(workspace_root: &Path) -> Option<PathBuf> {
    let workspace = fs::canonicalize(workspace_root).ok()?;
    let state = fs::canonicalize(workspace_root.join(".claude-sandbox")).ok()?;
    state.starts_with(&workspace).then_some(state)
}

fn read_json_file(path: &Path) -> Option<Value> {
    let metadata = fs::symlink_metadata(path).ok()?;
    if !metadata.file_type().is_file() || metadata.len() > MAX_USAGE_FILE_BYTES {
        return None;
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(target_os = "linux")]
    options.custom_flags(O_NONBLOCK | O_NOFOLLOW);
    let file = options.open(path).ok()?;
    let metadata = file.metadata().ok()?;
    if !metadata.is_file() || metadata.len() > MAX_USAGE_FILE_BYTES {
        return None;
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_USAGE_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() as u64 > MAX_USAGE_FILE_BYTES {
        return None;
    }
    serde_json::from_slice(&bytes).ok()
}

fn parse_anthropic(root: &Value, now: i64) -> ProviderUsage {
    let Some(snapshot) = root.get("cachedUsageUtilization") else {
        return ProviderUsage::unknown();
    };
    let Some(fetched_at) = snapshot
        .get("fetchedAtMs")
        .and_then(Value::as_u64)
        .and_then(|milliseconds| i64::try_from(milliseconds / 1000).ok())
    else {
        return ProviderUsage::unknown();
    };
    let freshness = freshness(fetched_at, now, ANTHROPIC_FRESH_SECS);
    if freshness == Freshness::Unknown {
        return ProviderUsage::unknown();
    }

    let Some(utilization) = snapshot
        .get("utilization")
        .filter(|value| value.is_object())
    else {
        return ProviderUsage::unknown();
    };
    let mut buckets = utilization
        .get("limits")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|limit| {
            let kind = limit
                .get("kind")
                .and_then(Value::as_str)
                .unwrap_or_default();
            Some(UsageBucket {
                period: period_from_anthropic_limit(limit),
                scope: if kind == "weekly_scoped" {
                    Scope::Model
                } else {
                    Scope::Overall
                },
                used_percent: percent(limit.get("percent")?)?,
                resets_at: reset_from_text(limit.get("resets_at"), now),
            })
        })
        .collect::<Vec<_>>();

    if buckets.is_empty() {
        add_anthropic_legacy_bucket(
            &mut buckets,
            utilization.get("five_hour"),
            Period::Session,
            now,
        );
        add_anthropic_legacy_bucket(
            &mut buckets,
            utilization.get("seven_day"),
            Period::Weekly,
            now,
        );
    }

    normalize_buckets(&mut buckets);
    ProviderUsage { freshness, buckets }
}

fn period_from_anthropic_limit(limit: &Value) -> Period {
    if let Some(period) = limit
        .get("group")
        .and_then(Value::as_str)
        .and_then(period_from_name)
    {
        return period;
    }
    match limit
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or_default()
    {
        "five_hour" | "session" => Period::Session,
        "seven_day" | "weekly" | "weekly_all" | "weekly_scoped" => Period::Weekly,
        "monthly" => Period::Monthly,
        _ => Period::Other,
    }
}

fn add_anthropic_legacy_bucket(
    buckets: &mut Vec<UsageBucket>,
    source: Option<&Value>,
    period: Period,
    now: i64,
) {
    let Some(source) = source else { return };
    let Some(used_percent) = source.get("utilization").and_then(percent) else {
        return;
    };
    buckets.push(UsageBucket {
        period,
        scope: Scope::Overall,
        used_percent,
        resets_at: reset_from_text(source.get("resets_at"), now),
    });
}

fn parse_vendor(root: &Value, now: i64) -> ProviderUsage {
    let Some(fetched_at) = root.get("fetched_at").and_then(Value::as_i64) else {
        return ProviderUsage::unknown();
    };
    let freshness = freshness(fetched_at, now, VENDOR_FRESH_SECS);
    if freshness == Freshness::Unknown {
        return ProviderUsage::unknown();
    }

    let Some(windows) = root.pointer("/data/windows").and_then(Value::as_array) else {
        return ProviderUsage::unknown();
    };
    let mut buckets = windows
        .iter()
        .filter_map(|window| {
            let name = window
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let period = period_from_window(window, name);
            Some(UsageBucket {
                period,
                scope: scope_from_name(name, period),
                used_percent: percent(window.get("pct")?)?,
                resets_at: reset_from_epoch(window.get("resets_at"), now),
            })
        })
        .collect::<Vec<_>>();
    normalize_buckets(&mut buckets);
    ProviderUsage { freshness, buckets }
}

fn period_from_window(window: &Value, name: &str) -> Period {
    if let Some(minutes) = window.get("window_minutes").and_then(Value::as_u64) {
        if minutes <= 1_440 {
            Period::Session
        } else if minutes <= 20_160 {
            Period::Weekly
        } else {
            Period::Monthly
        }
    } else {
        period_from_name(name).unwrap_or(Period::Other)
    }
}

fn period_from_name(name: &str) -> Option<Period> {
    match name.to_ascii_lowercase().as_str() {
        "session" => Some(Period::Session),
        "weekly" => Some(Period::Weekly),
        "monthly" => Some(Period::Monthly),
        "other" => Some(Period::Other),
        _ => None,
    }
}

fn scope_from_name(name: &str, period: Period) -> Scope {
    let name = name.to_ascii_lowercase();
    let headline = name == "codex"
        || match period {
            Period::Session => name == "session",
            Period::Weekly => matches!(name.as_str(), "weekly" | "weekly_all"),
            Period::Monthly => name == "monthly",
            Period::Other => true,
        };
    if headline {
        Scope::Overall
    } else {
        Scope::Model
    }
}

fn percent(value: &Value) -> Option<u8> {
    let value = value.as_f64()?;
    value
        .is_finite()
        .then_some(value.clamp(0.0, 100.0).floor() as u8)
}

fn freshness(fetched_at: i64, now: i64, fresh_for: i64) -> Freshness {
    if fetched_at < 0 || fetched_at > now.saturating_add(MAX_CLOCK_SKEW_SECS) {
        Freshness::Unknown
    } else if now.saturating_sub(fetched_at) <= fresh_for {
        Freshness::Fresh
    } else {
        Freshness::Stale
    }
}

fn reset_from_epoch(value: Option<&Value>, now: i64) -> Option<String> {
    let timestamp = value?.as_i64()?;
    valid_reset(timestamp, now).then(|| format_epoch(timestamp))
}

fn reset_from_text(value: Option<&Value>, now: i64) -> Option<String> {
    let timestamp = parse_rfc3339_utc(value?.as_str()?)?;
    valid_reset(timestamp, now).then(|| format_epoch(timestamp))
}

fn valid_reset(timestamp: i64, now: i64) -> bool {
    timestamp > now && timestamp <= now.saturating_add(MAX_RESET_HORIZON_SECS)
}

fn normalize_buckets(buckets: &mut Vec<UsageBucket>) {
    buckets.sort_by_key(|bucket| (bucket.period, bucket.scope, bucket.used_percent));
    buckets.dedup();
}

fn parse_rfc3339_utc(value: &str) -> Option<i64> {
    let value = value
        .strip_suffix('Z')
        .or_else(|| value.strip_suffix("+00:00"))?;
    let timestamp = value.split_once('.').map_or(value, |(whole, fraction)| {
        if fraction.is_empty() || !fraction.bytes().all(|byte| byte.is_ascii_digit()) {
            ""
        } else {
            whole
        }
    });
    if timestamp.len() != 19 || !timestamp.is_ascii() {
        return None;
    }
    let bytes = timestamp.as_bytes();
    if bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[10] != b'T'
        || bytes[13] != b':'
        || bytes[16] != b':'
    {
        return None;
    }
    let year = parse_digits(&timestamp[0..4])? as i64;
    let month = parse_digits(&timestamp[5..7])? as i64;
    let day = parse_digits(&timestamp[8..10])? as i64;
    let hour = parse_digits(&timestamp[11..13])? as i64;
    let minute = parse_digits(&timestamp[14..16])? as i64;
    let second = parse_digits(&timestamp[17..19])? as i64;
    if !(1970..=3000).contains(&year)
        || !(1..=12).contains(&month)
        || day < 1
        || day > days_in_month(year, month)
        || hour > 23
        || minute > 59
        || second > 59
    {
        return None;
    }
    Some(days_from_civil(year, month, day) * 86_400 + hour * 3_600 + minute * 60 + second)
}

fn parse_digits(value: &str) -> Option<u32> {
    value
        .bytes()
        .all(|byte| byte.is_ascii_digit())
        .then(|| value.parse().ok())?
}

fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = year - i64::from(month <= 2);
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let adjusted_month = month + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * adjusted_month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::os::unix::fs::symlink;

    const NOW: i64 = 1_787_500_000;

    #[test]
    fn parses_vendor_cache_without_exposing_names_or_plan() {
        let usage = parse_vendor(
            &json!({
                "fetched_at": NOW - 20,
                "data": {
                    "plan": "secret-plan",
                    "windows": [
                        {"name":"codex", "pct":42, "window_minutes":10080,
                         "resets_at":1787802733},
                        {"name":"codex", "pct":8, "window_minutes":300,
                         "resets_at":1787510000},
                        {"name":"GPT Secret Model", "pct":7, "window_minutes":300,
                         "resets_at":1787510000}
                    ]
                }
            }),
            NOW,
        );

        assert_eq!(usage.freshness, Freshness::Fresh);
        assert_eq!(usage.buckets.len(), 3);
        assert_eq!(usage.buckets[0].period, Period::Session);
        assert_eq!(usage.buckets[0].scope, Scope::Overall);
        assert_eq!(usage.buckets[0].used_percent, 8);
        assert_eq!(usage.buckets[1].period, Period::Session);
        assert_eq!(usage.buckets[1].scope, Scope::Model);
        assert_eq!(usage.buckets[1].used_percent, 7);
        assert_eq!(usage.buckets[2].period, Period::Weekly);
        assert_eq!(usage.buckets[2].scope, Scope::Overall);
        assert_eq!(
            usage.buckets[2].resets_at.as_deref(),
            Some("2026-08-27T03:52:13Z")
        );
        let public = serde_json::to_string(&usage).unwrap();
        assert!(!public.contains("secret-plan"));
        assert!(!public.contains("GPT Secret Model"));
    }

    #[test]
    fn parses_anthropic_limits_and_normalizes_utc_reset() {
        let usage = parse_anthropic(
            &json!({
                "cachedUsageUtilization": {
                    "fetchedAtMs": (NOW - 60) * 1000,
                    "utilization": {
                        "limits": [
                            {"kind":"five_hour", "percent":12.9,
                             "resets_at":"2026-08-23T12:34:56.987+00:00"},
                            {"kind":"weekly_scoped", "percent":33,
                             "resets_at":"2026-08-27T03:52:13Z",
                             "scope":{"model":{"display_name":"Private model"}}}
                        ],
                        "extra_usage": {"is_enabled":true, "utilization":88}
                    }
                }
            }),
            NOW,
        );

        assert_eq!(usage.freshness, Freshness::Fresh);
        assert_eq!(usage.buckets[0].used_percent, 12);
        assert_eq!(usage.buckets[0].scope, Scope::Overall);
        assert_eq!(usage.buckets[1].scope, Scope::Model);
        assert_eq!(usage.buckets.len(), 2);
        let public = serde_json::to_string(&usage).unwrap();
        assert!(!public.contains("Private model"));
        assert!(public.contains("2026-08-27T03:52:13Z"));
    }

    #[test]
    fn clamps_percentages_and_discards_expired_resets() {
        let usage = parse_vendor(
            &json!({
                "fetched_at": NOW - 1,
                "data": {"windows": [
                    {"name":"session", "pct":101, "resets_at":NOW + 100},
                    {"name":"weekly", "pct":50, "resets_at":NOW - 1}
                ]}
            }),
            NOW,
        );

        assert_eq!(usage.buckets.len(), 2);
        assert_eq!(usage.buckets[0].used_percent, 100);
        assert!(usage.buckets[0].resets_at.is_some());
        assert_eq!(usage.buckets[1].used_percent, 50);
        assert_eq!(usage.buckets[1].resets_at, None);
    }

    #[test]
    fn treats_unknown_vendor_windows_as_overall_other_buckets() {
        let usage = parse_vendor(
            &json!({
                "fetched_at": NOW,
                "data": {"windows": [
                    {"name":"new-ollama-window", "pct":3, "window_minutes":null,
                     "resets_at":null}
                ]}
            }),
            NOW,
        );

        assert_eq!(usage.buckets.len(), 1);
        assert_eq!(usage.buckets[0].period, Period::Other);
        assert_eq!(usage.buckets[0].scope, Scope::Overall);
        assert_eq!(usage.buckets[0].used_percent, 3);
        assert_eq!(usage.buckets[0].resets_at, None);
    }

    #[test]
    fn marks_old_or_future_snapshots_appropriately() {
        assert_eq!(
            freshness(NOW - VENDOR_FRESH_SECS - 1, NOW, VENDOR_FRESH_SECS),
            Freshness::Stale
        );
        assert_eq!(
            freshness(NOW + MAX_CLOCK_SKEW_SECS + 1, NOW, VENDOR_FRESH_SECS),
            Freshness::Unknown
        );
    }

    #[test]
    fn rejects_malformed_provider_shapes() {
        let vendor = parse_vendor(&json!({"fetched_at": NOW}), NOW);
        let anthropic = parse_anthropic(
            &json!({"cachedUsageUtilization":{"fetchedAtMs":NOW * 1000}}),
            NOW,
        );

        assert_eq!(vendor.freshness, Freshness::Unknown);
        assert_eq!(anthropic.freshness, Freshness::Unknown);
    }

    #[test]
    fn rejects_symlinks_and_oversized_cache_files() {
        let directory = std::env::temp_dir().join(format!(
            "claude-sandbox-usage-reader-{}-{}",
            std::process::id(),
            format_epoch(NOW).replace([':', '-'], "")
        ));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir(&directory).unwrap();
        let regular = directory.join("regular.json");
        let link = directory.join("link.json");
        let oversized = directory.join("oversized.json");
        std::fs::write(&regular, b"{}").unwrap();
        symlink(&regular, &link).unwrap();
        std::fs::write(&oversized, vec![b' '; MAX_USAGE_FILE_BYTES as usize + 1]).unwrap();

        assert_eq!(read_json_file(&regular), Some(json!({})));
        assert_eq!(read_json_file(&link), None);
        assert_eq!(read_json_file(&oversized), None);

        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn converts_epoch_and_rfc3339_around_leap_day() {
        assert_eq!(format_epoch(0), "1970-01-01T00:00:00Z");
        assert_eq!(format_epoch(951_827_696), "2000-02-29T12:34:56Z");
        assert_eq!(
            parse_rfc3339_utc("2000-02-29T12:34:56.123+00:00"),
            Some(951_827_696)
        );
        assert_eq!(parse_rfc3339_utc("2001-02-29T12:34:56Z"), None);
        assert_eq!(parse_rfc3339_utc("2000-02-29T12:34:56-04:00"), None);
    }
}
