//! Converting sample counts into byte estimates and formatting them.

pub fn estimate(k: f64, n: u64, total: u64) -> u64 {
    if n == 0 { 0 } else { (k / n as f64 * total as f64).round() as u64 }
}

/// Half-width of the 95% confidence interval, in bytes.
pub fn error95(k: f64, n: u64, total: u64) -> u64 {
    if n == 0 {
        return 0;
    }
    let p = (k / n as f64).clamp(0.0, 1.0);
    (1.96 * (p * (1.0 - p) / n as f64).sqrt() * total as f64).round() as u64
}

pub fn fmt_size(b: u64) -> String {
    const UNITS: [&str; 6] = ["KiB", "MiB", "GiB", "TiB", "PiB", "EiB"];
    if b < 1024 {
        return format!("{b} B");
    }
    let mut v = b as f64 / 1024.0;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    format!("{v:.1} {}", UNITS[i])
}

pub fn fmt_count(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimates_and_errors() {
        assert_eq!(estimate(25.0, 100, 1000), 250);
        assert_eq!(estimate(1.0, 0, 1000), 0);
        // p = 0.5, n = 100: 1.96 * 0.05 * 1000 = 98
        assert_eq!(error95(50.0, 100, 1000), 98);
    }

    #[test]
    fn formats() {
        assert_eq!(fmt_size(512), "512 B");
        assert_eq!(fmt_size(1536), "1.5 KiB");
        assert_eq!(fmt_size(3 << 30), "3.0 GiB");
        assert_eq!(fmt_count(1234567), "1,234,567");
        assert_eq!(fmt_count(12), "12");
    }
}
