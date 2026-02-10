use std::fs::File;
use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

pub fn timestamp() -> String {
    let dur = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = dur.as_secs();
    let days = secs / 86400;
    let time_secs = secs % 86400;
    let h = time_secs / 3600;
    let m = (time_secs % 3600) / 60;
    let s = time_secs % 60;
    let mut y: u64 = 1970;
    let mut remaining = days;
    loop {
        let leap = y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
        let ydays: u64 = if leap { 366 } else { 365 };
        if remaining < ydays {
            break;
        }
        remaining -= ydays;
        y += 1;
    }
    let leap = y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
    let mdays: &[u64] = if leap {
        &[31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    } else {
        &[31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    };
    let mut mo: u64 = 0;
    for md in mdays {
        if remaining < *md {
            break;
        }
        remaining -= md;
        mo += 1;
    }
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        y,
        mo + 1,
        remaining + 1,
        h,
        m,
        s
    )
}

pub fn log_line(log: &Arc<Mutex<File>>, message: &str) {
    if let Ok(mut f) = log.lock() {
        let _ = writeln!(f, "{} {}", timestamp(), message);
    }
}
