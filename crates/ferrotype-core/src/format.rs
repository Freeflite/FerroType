//! Short human-readable numbers for the status line and the history list.

use std::time::Duration;

/// "40 s", "12 min", "2.5 h".
pub fn duration(time: Duration) -> String {
    let seconds = time.as_secs();
    if seconds < 90 {
        format!("{seconds} s")
    } else if seconds < 5400 {
        format!("{} min", (seconds + 30) / 60)
    } else {
        format!("{:.1} h", seconds as f64 / 3600.0)
    }
}

/// "512 B", "14 KB", "1.5 MB".
pub fn bytes(count: usize) -> String {
    const KB: usize = 1024;
    const MB: usize = 1024 * 1024;
    if count < KB {
        format!("{count} B")
    } else if count < MB {
        format!("{} KB", count.div_ceil(KB))
    } else {
        let mb = count as f64 / MB as f64;
        if mb < 10.0 { format!("{mb:.1} MB") } else { format!("{mb:.0} MB") }
    }
}

/// How long ago something happened: "now", "5m", "3h", "2d".
pub fn age(seconds: u64) -> String {
    match seconds {
        0..60 => "now".to_owned(),
        60..3600 => format!("{}m", seconds / 60),
        3600..86400 => format!("{}h", seconds / 3600),
        _ => format!("{}d", seconds / 86400),
    }
}

/// Thousands separators: 160803 -> "160,803".
pub fn count(value: usize) -> String {
    let digits = value.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats() {
        assert_eq!(duration(Duration::from_secs(30)), "30 s");
        assert_eq!(duration(Duration::from_secs(600)), "10 min");
        assert_eq!(duration(Duration::from_secs(9000)), "2.5 h");
        assert_eq!(bytes(512), "512 B");
        assert_eq!(bytes(14 * 1024), "14 KB");
        assert_eq!(bytes(1536 * 1024), "1.5 MB");
        assert_eq!(bytes(100 * 1024 * 1024), "100 MB");
        assert_eq!(age(5), "now");
        assert_eq!(age(300), "5m");
        assert_eq!(age(7200), "2h");
        assert_eq!(age(200_000), "2d");
        assert_eq!(count(0), "0");
        assert_eq!(count(999), "999");
        assert_eq!(count(160_803), "160,803");
        assert_eq!(count(1_000_000), "1,000,000");
    }
}
