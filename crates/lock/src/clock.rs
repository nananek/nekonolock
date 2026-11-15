//! 時計の書式。`[ui] clock_format` は strftime 風の指定を使う。
//!
//! 不正な書式を実行時に踏むと `chrono` の整形が panic するため、
//! 起動時に [`validate_format`] で弾く。

use chrono::format::{Item, StrftimeItems};
use chrono::{DateTime, TimeZone};

/// `format` が空でなく、`chrono` が解釈できることを確認する。
pub fn validate_format(format: &str) -> bool {
    !format.is_empty() && !StrftimeItems::new(format).any(|item| matches!(item, Item::Error))
}

/// 現在時刻を設定の書式で整形する。
pub fn format_now(format: &str) -> String {
    format_at(format, chrono::Local::now())
}

/// 任意の時刻を整形する（テスト用）。
pub fn format_at<Tz: TimeZone>(format: &str, now: DateTime<Tz>) -> String
where
    Tz::Offset: std::fmt::Display,
{
    now.format(format).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::FixedOffset;

    fn datetime() -> DateTime<FixedOffset> {
        FixedOffset::east_opt(9 * 3600)
            .unwrap()
            .with_ymd_and_hms(2026, 11, 15, 10, 7, 3)
            .unwrap()
    }

    #[test]
    fn accepts_default_formats() {
        assert!(validate_format("%H:%M"));
        assert!(validate_format("%A, %-d %B %Y"));
        assert!(validate_format("%Y-%m-%d (%a) %H:%M"));
    }

    #[test]
    fn rejects_empty_and_invalid_formats() {
        assert!(!validate_format(""));
        assert!(!validate_format("%"));
        assert!(!validate_format("%Q"));
    }

    #[test]
    fn formats_without_panicking() {
        assert_eq!(
            format_at("%Y-%m-%d (%a) %H:%M", datetime()),
            "2026-11-15 (Sun) 10:07"
        );
        assert_eq!(format_at("%H:%M:%S", datetime()), "10:07:03");
    }
}
