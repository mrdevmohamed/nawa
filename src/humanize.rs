//! Small human-readable formatters (durations, dates, sizes).


/// "3m05s" / "1h02m" / "12s"
pub(crate) fn format_duration(secs: u64) -> String {
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m{:02}s", secs / 60, secs % 60),
        _ => format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60),
    }
}

/// Unix seconds -> "YYYY-MM-DD" (UTC). 0 (never recorded) renders as "-".
pub(crate) fn format_date(secs: u64) -> String {
    if secs == 0 {
        return "-".to_string();
    }
    // Howard Hinnant's civil_from_days: valid for the whole u64 seconds range
    let days = (secs / 86_400) as i64;
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

/// Byte count in human units.
pub(crate) fn format_size(bytes: u64) -> String {
    const K: f64 = 1024.0;
    let b = bytes as f64;
    if bytes >= 1 << 30 {
        format!("{:.1} GiB", b / (K * K * K))
    } else if bytes >= 1 << 20 {
        format!("{:.1} MiB", b / (K * K))
    } else if bytes >= 1 << 10 {
        format!("{:.1} KiB", b / K)
    } else {
        format!("{bytes} B")
    }
}

// ---- shell completions (hand-written; no extra crates) ---------------------
// Package names complete dynamically via `nawa __complete_pkgs`.
