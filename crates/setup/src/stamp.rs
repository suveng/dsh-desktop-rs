//! 时间戳格式化。
//!
//! 只需要「UTC 下可排序、人可读」的一种格式（`20261104T002735Z`），
//! 为它拉一个 chrono 不值得。算法是 Howard Hinnant 的 civil_from_days。

/// Unix 秒 → `YYYYMMDDTHHMMSSZ`。
#[must_use]
pub fn utc_stamp(unix_secs: u64) -> String {
    let days = unix_secs / 86_400;
    let secs_of_day = unix_secs % 86_400;
    let (year, month, day) = civil_from_days(days as i64);
    format!(
        "{year:04}{month:02}{day:02}T{:02}{:02}{:02}Z",
        secs_of_day / 3600,
        (secs_of_day % 3600) / 60,
        secs_of_day % 60
    )
}

/// 1970-01-01 起的天数 → (年, 月, 日)。Gregorian，支持 1970 之前（负数天）。
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// 当前时刻的 Unix 秒。系统时间早于 1970 时返回 0。
#[must_use]
pub fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_known_instants() {
        assert_eq!(utc_stamp(0), "19700101T000000Z");
        assert_eq!(utc_stamp(1_700_000_000), "20231114T221320Z");
        // 2026-11-25 03:07:35 UTC
        assert_eq!(utc_stamp(1_795_576_055), "20261125T030735Z");
    }

    #[test]
    fn handles_leap_day() {
        // 2024-02-29 12:00:00 UTC
        assert_eq!(utc_stamp(1_709_208_000), "20240229T120000Z");
    }

    #[test]
    fn stamps_sort_lexicographically() {
        assert!(utc_stamp(1_700_000_000) < utc_stamp(1_795_576_055));
    }
}
