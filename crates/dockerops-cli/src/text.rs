//! Shared rendering helpers.
//!
//! Every number a caller reads is written by something in here, which is why the tests
//! assert exact strings: the output format is the product, and `1.2G` versus
//! `1234567890 bytes` is most of the saving this crate claims.

/// Bytes as the shortest honest string: `812B`, `9.4K`, `1.2G`.
pub fn human(bytes: u64) -> String {
    const UNITS: [(u64, &str); 4] = [
        (1024 * 1024 * 1024 * 1024, "T"),
        (1024 * 1024 * 1024, "G"),
        (1024 * 1024, "M"),
        (1024, "K"),
    ];
    for (scale, suffix) in UNITS {
        if bytes >= scale {
            let whole = bytes / scale;
            let tenths = (bytes % scale) * 10 / scale;
            return if whole < 10 && tenths > 0 {
                format!("{whole}.{tenths}{suffix}")
            } else {
                format!("{whole}{suffix}")
            };
        }
    }
    format!("{bytes}B")
}

/// A count, abbreviated once it stops being worth reading in full: `941`, `8.6k`, `12k`.
pub fn compact_count(count: usize) -> String {
    if count < 1000 {
        return count.to_string();
    }
    let thousands = count / 1000;
    let hundreds = (count % 1000) / 100;
    if thousands < 10 && hundreds > 0 {
        format!("{thousands}.{hundreds}k")
    } else {
        format!("{thousands}k")
    }
}

/// `1 file` / `2 files`, so a footer never reads `1 files`.
pub fn plural(count: usize, word: &str) -> String {
    if count == 1 {
        format!("{count} {word}")
    } else {
        format!("{count} {word}s")
    }
}

/// Seconds as an age: `4s`, `12m`, `3h`, `6d`. Docker's own `Status` column says
/// "Up About an hour", which is nine characters of English for two of fact.
pub fn short_duration(seconds: u64) -> String {
    const MINUTE: u64 = 60;
    const HOUR: u64 = 60 * MINUTE;
    const DAY: u64 = 24 * HOUR;
    match seconds {
        s if s < MINUTE => format!("{s}s"),
        s if s < HOUR => format!("{}m", s / MINUTE),
        s if s < DAY => format!("{}h", s / HOUR),
        s => format!("{}d", s / DAY),
    }
}

/// Cut a value to `max` characters, marking that it was cut. Used wherever a field is
/// attacker-or-accident-controlled — an image digest, a log line, a label value.
pub fn elide(value: &str, max: usize) -> String {
    let flattened = value.replace(['\n', '\r', '\t'], " ");
    if flattened.chars().count() <= max {
        return flattened;
    }
    let kept: String = flattened.chars().take(max.saturating_sub(1)).collect();
    format!("{kept}…")
}

/// Pad `value` to `width` with spaces, for the one place alignment earns its bytes: a
/// column of names read down the page. Nothing is padded past its own content width.
pub fn pad(value: &str, width: usize) -> String {
    let len = value.chars().count();
    if len >= width {
        return value.to_owned();
    }
    format!("{value}{}", " ".repeat(width - len))
}

/// The widest entry in a column, which is what [`pad`] should be given.
pub fn width_of<'a>(values: impl Iterator<Item = &'a str>) -> usize {
    values.map(|v| v.chars().count()).max().unwrap_or(0)
}

/// Docker reports sizes as `1.23GB` / `41.2MB` / `0B`. Parsed back to bytes so output can
/// be sorted by size rather than by the alphabet, which puts `9MB` above `100GB`.
pub fn parse_size(value: &str) -> Option<u64> {
    let trimmed = value.trim();
    let digits: String = trimmed
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    if digits.is_empty() {
        return None;
    }
    let number: f64 = digits.parse().ok()?;
    let unit: String = trimmed[digits.len()..]
        .trim()
        .chars()
        .filter(|c| c.is_ascii_alphabetic())
        .collect();
    let scale = match unit.to_ascii_uppercase().as_str() {
        "B" | "" => 1.0,
        "KB" | "K" | "KIB" => 1024.0,
        "MB" | "M" | "MIB" => 1024.0 * 1024.0,
        "GB" | "G" | "GIB" => 1024.0 * 1024.0 * 1024.0,
        "TB" | "T" | "TIB" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        _ => return None,
    };
    Some((number * scale) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_read_as_short_as_they_honestly_can() {
        assert_eq!(human(0), "0B");
        assert_eq!(human(812), "812B");
        assert_eq!(human(9_600), "9.3K");
        assert_eq!(human(1024 * 1024), "1M");
        assert_eq!(human(1_288_490_188), "1.1G");
        assert_eq!(human(42 * 1024 * 1024 * 1024), "42G");
    }

    #[test]
    fn counts_abbreviate_only_once_they_stop_being_read_in_full() {
        assert_eq!(compact_count(0), "0");
        assert_eq!(compact_count(999), "999");
        assert_eq!(compact_count(8_600), "8.6k");
        assert_eq!(compact_count(12_000), "12k");
    }

    #[test]
    fn a_footer_never_reads_one_files() {
        assert_eq!(plural(1, "container"), "1 container");
        assert_eq!(plural(0, "container"), "0 containers");
        assert_eq!(plural(3, "container"), "3 containers");
    }

    #[test]
    fn an_age_is_two_characters_not_a_sentence() {
        assert_eq!(short_duration(9), "9s");
        assert_eq!(short_duration(3 * 60 + 2), "3m");
        assert_eq!(short_duration(5 * 3600), "5h");
        assert_eq!(short_duration(9 * 86_400), "9d");
    }

    #[test]
    fn eliding_flattens_newlines_so_one_value_stays_one_line() {
        assert_eq!(elide("a\nb", 10), "a b");
        assert_eq!(elide("abcdefghij", 5), "abcd…");
        assert_eq!(elide("abcd", 4), "abcd");
    }

    #[test]
    fn docker_sizes_parse_back_into_something_sortable() {
        assert_eq!(parse_size("0B"), Some(0));
        assert_eq!(parse_size("41.2MB"), Some(43_201_331));
        assert_eq!(parse_size("1.23GB"), Some(1_320_702_443));
        assert!(parse_size("1.23GB").unwrap() > parse_size("900MB").unwrap());
        assert_eq!(parse_size("N/A"), None);
        assert_eq!(parse_size(""), None);
    }

    #[test]
    fn padding_never_truncates_what_it_cannot_fit() {
        assert_eq!(pad("ab", 4), "ab  ");
        assert_eq!(pad("abcdef", 4), "abcdef");
        assert_eq!(width_of(["a", "abc", "ab"].into_iter()), 3);
        assert_eq!(width_of([].into_iter()), 0);
    }
}
