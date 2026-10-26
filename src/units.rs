//! Sizes and durations as people type them on the command line.

/// Parse a memory size: bytes, or a number with K, M, G or T (powers of 1024).
pub fn parse_size(text: &str) -> Option<u64> {
    let text = text.trim();
    let (number, unit) = match text.find(|c: char| c.is_ascii_alphabetic()) {
        Some(i) => text.split_at(i),
        None => (text, ""),
    };
    let scale: u64 = match unit.to_ascii_uppercase().trim_end_matches(['B', 'I']) {
        "" => 1,
        "K" => 1 << 10,
        "M" => 1 << 20,
        "G" => 1 << 30,
        "T" => 1 << 40,
        _ => return None,
    };
    let value: f64 = number.trim().parse().ok()?;
    let bytes = value * scale as f64;
    (value.is_finite() && bytes >= 1.0 && bytes < u64::MAX as f64).then_some(bytes as u64)
}

/// A duration like `90`, `90s`, `1.5m`, `2h` or `500ms` (plain numbers are seconds).
pub fn parse_duration(text: &str) -> Option<std::time::Duration> {
    let text = text.trim();
    let (number, unit) = match text.find(|c: char| c.is_ascii_alphabetic()) {
        Some(i) => text.split_at(i),
        None => (text, ""),
    };
    let scale = match unit.to_ascii_lowercase().as_str() {
        "ms" => 0.001,
        "" | "s" => 1.0,
        "m" => 60.0,
        "h" => 3600.0,
        _ => return None,
    };
    let seconds = number.trim().parse::<f64>().ok()? * scale;
    // At least a millisecond; at most a year.
    (seconds.is_finite() && (0.001..=31_536_000.0).contains(&seconds)).then(|| std::time::Duration::from_secs_f64(seconds))
}

#[cfg(test)]
mod tests {
    use super::{parse_duration, parse_size};
    use std::time::Duration;

    #[test]
    fn durations() {
        assert_eq!(parse_duration("90"), Some(Duration::from_secs(90)));
        assert_eq!(parse_duration("30s"), Some(Duration::from_secs(30)));
        assert_eq!(parse_duration("1.5m"), Some(Duration::from_secs(90)));
        assert_eq!(parse_duration("2h"), Some(Duration::from_secs(7200)));
        assert_eq!(parse_duration("250ms"), Some(Duration::from_millis(250)));
        assert_eq!(parse_duration("0"), None);
        assert_eq!(parse_duration("-5s"), None);
        assert_eq!(parse_duration("5d"), None);
        assert_eq!(parse_duration("soon"), None);
    }

    #[test]
    fn sizes() {
        assert_eq!(parse_size("512"), Some(512));
        assert_eq!(parse_size("64K"), Some(64 << 10));
        assert_eq!(parse_size("1.5G"), Some(3 << 29));
        assert_eq!(parse_size("2gib"), Some(2 << 30));
        assert_eq!(parse_size("100MB"), Some(100 << 20));
        assert_eq!(parse_size("0"), None);
        assert_eq!(parse_size("-1G"), None);
        assert_eq!(parse_size("lots"), None);
        assert_eq!(parse_size("5X"), None);
    }
}
