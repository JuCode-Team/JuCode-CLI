//! Hand-rolled structured logging: line-JSON records appended to
//! ~/.jucode/logs/jucode.log. The global logger is set once at process start;
//! before init (or after a failed init) every log call is a no-op.

use serde_json::Value;
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
    time::{SystemTime, UNIX_EPOCH},
};

pub use serde_json::json;

const MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;
const DOCTOR_TAIL_BYTES: u64 = 256 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum LogLevel {
    Error,
    Warn,
    Info,
    Debug,
}

impl LogLevel {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warn => "warn",
            Self::Info => "info",
            Self::Debug => "debug",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value.trim() {
            "error" => Some(Self::Error),
            "warn" => Some(Self::Warn),
            "info" => Some(Self::Info),
            "debug" => Some(Self::Debug),
            _ => None,
        }
    }
}

pub struct Logger {
    level: LogLevel,
    path: PathBuf,
    file: Mutex<File>,
}

impl Logger {
    pub fn create(path: &Path, level: LogLevel) -> io::Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        rotate_if_large(path, MAX_LOG_BYTES)?;
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Self {
            level,
            path: path.to_path_buf(),
            file: Mutex::new(file),
        })
    }

    pub fn enabled(&self, level: LogLevel) -> bool {
        level <= self.level
    }

    pub fn log(&self, level: LogLevel, target: &str, msg: &str, fields: Value) {
        if !self.enabled(level) {
            return;
        }
        let mut line = record_line(unix_now_secs(), level, target, msg, &fields);
        line.push('\n');
        if let Ok(mut file) = self.file.lock() {
            let _ = file.write_all(line.as_bytes());
            if level == LogLevel::Error {
                let _ = file.flush();
            }
        }
    }
}

static GLOBAL: OnceLock<Option<Logger>> = OnceLock::new();

/// Install the global logger. Called once from the binary entry points; a
/// failed init leaves logging as a silent no-op after one stderr warning.
pub fn init_global() {
    let _ = GLOBAL.set(build_global());
}

fn build_global() -> Option<Logger> {
    let level = std::env::var("JUCODE_LOG")
        .ok()
        .and_then(|raw| LogLevel::parse(&raw))
        .unwrap_or(LogLevel::Warn);
    let dir = match crate::config::profile_dir() {
        Ok(dir) => dir,
        Err(error) => {
            eprintln!("jucode: logging disabled: {error}");
            return None;
        }
    };
    let path = dir.join("logs").join("jucode.log");
    match Logger::create(&path, level) {
        Ok(logger) => Some(logger),
        Err(error) => {
            eprintln!("jucode: logging disabled: {error}");
            None
        }
    }
}

/// Log through the global logger; a no-op when logging is uninitialized.
pub fn log(level: LogLevel, target: &str, msg: &str, fields: Value) {
    if let Some(Some(logger)) = GLOBAL.get() {
        logger.log(level, target, msg, fields);
    }
}

#[macro_export]
macro_rules! log_record {
    ($level:expr, $target:expr, $msg:expr $(,)?) => {
        $crate::logging::log($level, $target, $msg, $crate::logging::json!(null))
    };
    ($level:expr, $target:expr, $msg:expr, $($key:ident = $value:expr),+ $(,)?) => {
        $crate::logging::log(
            $level,
            $target,
            $msg,
            $crate::logging::json!({ $(stringify!($key): $value),+ }),
        )
    };
}

#[macro_export]
macro_rules! log_error {
    ($($args:tt)*) => { $crate::log_record!($crate::logging::LogLevel::Error, $($args)*) };
}

#[macro_export]
macro_rules! log_warn {
    ($($args:tt)*) => { $crate::log_record!($crate::logging::LogLevel::Warn, $($args)*) };
}

#[macro_export]
macro_rules! log_info {
    ($($args:tt)*) => { $crate::log_record!($crate::logging::LogLevel::Info, $($args)*) };
}

#[macro_export]
macro_rules! log_debug {
    ($($args:tt)*) => { $crate::log_record!($crate::logging::LogLevel::Debug, $($args)*) };
}

fn record_line(ts_secs: u64, level: LogLevel, target: &str, msg: &str, fields: &Value) -> String {
    let mut record = serde_json::Map::new();
    record.insert("ts".to_string(), Value::String(format_rfc3339(ts_secs)));
    record.insert(
        "level".to_string(),
        Value::String(level.as_str().to_string()),
    );
    record.insert("target".to_string(), Value::String(target.to_string()));
    record.insert("msg".to_string(), Value::String(msg.to_string()));
    if !fields.is_null() {
        record.insert("fields".to_string(), fields.clone());
    }
    Value::Object(record).to_string()
}

fn rotate_if_large(path: &Path, max_bytes: u64) -> io::Result<()> {
    let Ok(metadata) = fs::metadata(path) else {
        return Ok(());
    };
    if metadata.len() <= max_bytes {
        return Ok(());
    }
    let rotated = path.with_extension("log.1");
    let _ = fs::remove_file(&rotated);
    fs::rename(path, rotated)
}

fn unix_now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn format_rfc3339(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

fn parse_rfc3339_secs(ts: &str) -> Option<u64> {
    // Accepts exactly the shape this module writes: YYYY-MM-DDTHH:MM:SSZ.
    let bytes = ts.as_bytes();
    if bytes.len() != 20 || bytes[4] != b'-' || bytes[7] != b'-' || bytes[10] != b'T' {
        return None;
    }
    if bytes[13] != b':' || bytes[16] != b':' || bytes[19] != b'Z' {
        return None;
    }
    let num = |range: std::ops::Range<usize>| ts.get(range)?.parse::<u64>().ok();
    let (year, month, day) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (hour, minute, second) = (num(11..13)?, num(14..16)?, num(17..19)?);
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    if hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    let days = days_from_civil(year as i64, month as u32, day as u32);
    if days < 0 {
        return None;
    }
    Some(days as u64 * 86_400 + hour * 3600 + minute * 60 + second)
}

// Date conversions from Howard Hinnant's civil calendar algorithms.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let yoe = year - era * 400;
    let mp = if month > 2 { month - 3 } else { month + 9 } as i64;
    let doy = (153 * mp + 2) / 5 + day as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Count error-level records within the last 24h by scanning at most the last
/// 256 KB of the log file.
fn recent_error_count(path: &Path, now_secs: u64) -> usize {
    let Ok(mut file) = File::open(path) else {
        return 0;
    };
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    if len > DOCTOR_TAIL_BYTES
        && file
            .seek(SeekFrom::End(-(DOCTOR_TAIL_BYTES as i64)))
            .is_err()
    {
        return 0;
    }
    let mut bytes = Vec::new();
    if file.read_to_end(&mut bytes).is_err() {
        return 0;
    }
    let tail = String::from_utf8_lossy(&bytes);
    let cutoff = now_secs.saturating_sub(24 * 3600);
    tail.lines()
        .rev()
        .filter(|line| line.contains("\"level\":\"error\""))
        .filter(|line| {
            record_timestamp_secs(line).is_some_and(|secs| secs >= cutoff && secs <= now_secs)
        })
        .count()
}

fn record_timestamp_secs(line: &str) -> Option<u64> {
    let start = line.find("\"ts\":\"")? + "\"ts\":\"".len();
    let rest = &line[start..];
    let end = rest.find('"')?;
    parse_rfc3339_secs(&rest[..end])
}

/// One `/doctor` line describing the logging setup.
pub fn doctor_line() -> String {
    match GLOBAL.get() {
        None => "logging: not initialized".to_string(),
        Some(None) => "logging: disabled (init failed)".to_string(),
        Some(Some(logger)) => format!(
            "logging: {} (level {}, {} error(s) in last 24h)",
            logger.path.display(),
            logger.level.as_str(),
            recent_error_count(&logger.path, unix_now_secs())
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn test_dir(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("jucode-logging-test-{name}-{nanos}"))
    }

    #[test]
    fn level_filtering_skips_lower_priority_records() {
        let dir = test_dir("filter");
        let path = dir.join("jucode.log");
        let logger = Logger::create(&path, LogLevel::Warn).unwrap();
        logger.log(LogLevel::Debug, "t", "dropped debug", Value::Null);
        logger.log(LogLevel::Info, "t", "dropped info", Value::Null);
        logger.log(LogLevel::Warn, "t", "kept warn", Value::Null);
        logger.log(LogLevel::Error, "t", "kept error", Value::Null);
        let content = fs::read_to_string(&path).unwrap();
        assert_eq!(content.lines().count(), 2);
        assert!(content.contains("kept warn"));
        assert!(!content.contains("dropped info"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn record_line_is_json_with_expected_shape() {
        let line = record_line(
            1_752_364_800,
            LogLevel::Info,
            "llm",
            "retrying",
            &json!({ "attempt": 2 }),
        );
        let value = serde_json::from_str::<Value>(&line).unwrap();
        assert_eq!(value["ts"], "2025-07-13T00:00:00Z");
        assert_eq!(value["level"], "info");
        assert_eq!(value["target"], "llm");
        assert_eq!(value["msg"], "retrying");
        assert_eq!(value["fields"]["attempt"], 2);
    }

    #[test]
    fn record_line_omits_fields_when_null() {
        let line = record_line(0, LogLevel::Error, "t", "m", &Value::Null);
        let value = serde_json::from_str::<Value>(&line).unwrap();
        assert_eq!(value["ts"], "1970-01-01T00:00:00Z");
        assert!(value.get("fields").is_none());
    }

    #[test]
    fn rotation_renames_oversized_log_on_create() {
        let dir = test_dir("rotate");
        let path = dir.join("jucode.log");
        fs::create_dir_all(&dir).unwrap();
        fs::write(&path, "old".repeat(64)).unwrap();
        rotate_if_large(&path, 100).unwrap();
        assert!(!path.exists());
        let rotated = dir.join("jucode.log.1");
        assert!(rotated.exists());
        // A second rotation replaces the previous .1 file.
        fs::write(&path, "new".repeat(64)).unwrap();
        rotate_if_large(&path, 100).unwrap();
        assert!(fs::read_to_string(&rotated).unwrap().starts_with("new"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn small_log_is_not_rotated() {
        let dir = test_dir("no-rotate");
        let path = dir.join("jucode.log");
        fs::create_dir_all(&dir).unwrap();
        fs::write(&path, "small").unwrap();
        rotate_if_large(&path, 100).unwrap();
        assert!(path.exists());
        assert!(!dir.join("jucode.log.1").exists());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn uninitialized_global_logging_is_noop() {
        // No test initializes the global logger, so this must silently drop.
        log(LogLevel::Error, "test", "dropped", Value::Null);
        crate::log_error!("test", "dropped too", key = "value");
        assert!(GLOBAL.get().is_none());
    }

    #[test]
    fn rfc3339_roundtrips_through_parse() {
        for secs in [0, 951_782_400, 1_752_364_800, 4_102_444_799] {
            let ts = format_rfc3339(secs);
            assert_eq!(parse_rfc3339_secs(&ts), Some(secs), "{ts}");
        }
        assert_eq!(parse_rfc3339_secs("not-a-timestamp"), None);
        assert_eq!(parse_rfc3339_secs("2025-13-01T00:00:00Z"), None);
    }

    #[test]
    fn recent_error_count_ignores_old_and_non_error_records() {
        let dir = test_dir("count");
        let path = dir.join("jucode.log");
        fs::create_dir_all(&dir).unwrap();
        let now = 1_752_364_800;
        let lines = [
            record_line(now - 3600, LogLevel::Error, "a", "recent", &Value::Null),
            record_line(now - 100_000, LogLevel::Error, "a", "old", &Value::Null),
            record_line(now - 60, LogLevel::Warn, "a", "warn", &Value::Null),
            record_line(now, LogLevel::Error, "a", "now", &Value::Null),
        ];
        fs::write(&path, lines.join("\n")).unwrap();
        assert_eq!(recent_error_count(&path, now), 2);
        let _ = fs::remove_dir_all(dir);
    }
}
