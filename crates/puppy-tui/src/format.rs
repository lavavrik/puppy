//! Text formatting for the dashboard: sizes, speeds, ETAs, percentages, display-width
//! truncation, and parsing of user-entered rates.

use std::borrow::Cow;

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// Shown for an unknown or meaningless value (no speed, no ETA).
pub const NONE: &str = "—";
const ELLIPSIS: char = '…';
const UNITS: [&str; 6] = ["KiB", "MiB", "GiB", "TiB", "PiB", "EiB"];

/// Byte count in binary units: `0 B`, `1023 B`, `1.0 KiB`, `1.5 GiB` (one decimal from
/// KiB up).
pub fn bytes(n: u64) -> String {
    if n < 1024 {
        return format!("{n} B");
    }
    let mut v = n as f64 / 1024.0;
    let mut unit = 0;
    // Roll over when one decimal would print as 1024.0.
    while v >= 1023.95 && unit + 1 < UNITS.len() {
        v /= 1024.0;
        unit += 1;
    }
    format!("{v:.1} {}", UNITS[unit])
}

/// Transfer rate: [`bytes`] + `/s`; `—` when nothing moves.
pub fn speed(bps: u64) -> String {
    if bps == 0 {
        NONE.to_string()
    } else {
        format!("{}/s", bytes(bps))
    }
}

/// Time left: `0:41`, `12:05`, `1:02:03`; `99h+` beyond 99 hours; `—` when unknown.
pub fn eta(secs: Option<u64>) -> String {
    let Some(secs) = secs else {
        return NONE.to_string();
    };
    let (h, m, s) = (secs / 3600, secs / 60 % 60, secs % 60);
    match h {
        0 => format!("{m}:{s:02}"),
        1..=99 => format!("{h}:{m:02}:{s:02}"),
        _ => "99h+".to_string(),
    }
}

/// Fraction done in `0.0..=1.0`; `None` when the total is unknown (or zero).
pub fn percent(done: u64, total: Option<u64>) -> Option<f64> {
    let total = total.filter(|&t| t > 0)?;
    Some((done as f64 / total as f64).min(1.0))
}

/// Cuts `s` to at most `max_width` terminal columns, never splitting a grapheme
/// cluster, ending with `…` when anything was cut.
pub fn truncate(s: &str, max_width: usize) -> String {
    if s.width() <= max_width {
        return s.to_string();
    }
    if max_width == 0 {
        return String::new();
    }
    let budget = max_width - 1; // room for the ellipsis
    let mut out = String::new();
    let mut used = 0;
    for g in s.graphemes(true) {
        let w = g.width();
        if used + w > budget {
            break;
        }
        used += w;
        out.push_str(g);
    }
    out.push(ELLIPSIS);
    out
}

/// Like [`truncate`] but cuts out the middle (`https://exa…/file.iso`), keeping the
/// tail with the file name: about a third of the room for the head, the rest for the
/// tail.
pub fn truncate_middle(s: &str, max_width: usize) -> String {
    if s.width() <= max_width {
        return s.to_string();
    }
    if max_width < 2 {
        return truncate(s, max_width);
    }
    let budget = max_width - 1;
    let head_w = budget / 3;
    let gs: Vec<&str> = s.graphemes(true).collect();
    let mut head = String::new();
    let mut used = 0;
    for g in &gs {
        if used + g.width() > head_w {
            break;
        }
        used += g.width();
        head.push_str(g);
    }
    let mut tail: Vec<&str> = Vec::new();
    let mut tail_w = 0;
    for g in gs.iter().rev() {
        if used + tail_w + g.width() > budget {
            break;
        }
        tail_w += g.width();
        tail.push(g);
    }
    head.push(ELLIPSIS);
    head.extend(tail.into_iter().rev());
    head
}

/// Makes untrusted text (names, sources, paths from the daemon) safe to draw: control
/// characters (`\t`, `\n`, `\r`, ESC, DEL, C1) and bidi overrides/isolates become a
/// space, so they can neither break the layout nor reach the terminal. Borrows when the
/// text is already clean. Apply before [`truncate`].
pub fn display_safe(s: &str) -> Cow<'_, str> {
    fn unsafe_char(c: char) -> bool {
        c.is_control() || matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
    }
    if s.contains(unsafe_char) {
        Cow::Owned(
            s.chars()
                .map(|c| if unsafe_char(c) { ' ' } else { c })
                .collect(),
        )
    } else {
        Cow::Borrowed(s)
    }
}

/// A rate as the limit inputs take it back ([`parse_rate`] round-trips it exactly):
/// `1.0M`, `500.0k`, `1.5G`; plain bytes when no unit with one decimal is exact.
pub fn rate_input(bps: u64) -> String {
    for (unit, suffix) in [(1u64 << 30, 'G'), (1 << 20, 'M'), (1 << 10, 'k')] {
        if bps >= unit && (u128::from(bps) * 10) % u128::from(unit) == 0 {
            let tenths = u128::from(bps) * 10 / u128::from(unit);
            return format!("{}.{}{suffix}", tenths / 10, tenths % 10);
        }
    }
    bps.to_string()
}

/// Parses a user-entered rate in bytes/s with binary units: `500k`, `5M`, `5MiB`,
/// `1.5g`, `5 MiB/s` (case-insensitive). `0`, `none`, `off` and the empty string mean
/// "no limit" (`None`). Errors are user-facing text.
pub fn parse_rate(s: &str) -> Result<Option<u64>, String> {
    let input = s.trim();
    let lower = input.to_ascii_lowercase();
    if matches!(lower.as_str(), "" | "none" | "off") {
        return Ok(None);
    }
    let invalid = || {
        format!(
            "invalid rate \"{input}\": use a number with an optional unit, e.g. 500k, 5M or 1.5G (\"none\" for no limit)"
        )
    };
    if lower.starts_with('-') {
        return Err(format!("invalid rate \"{input}\": it must not be negative"));
    }
    let too_large = || format!("rate \"{input}\" is too large");
    let rest = lower.strip_suffix("/s").unwrap_or(&lower);
    let split = rest
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(rest.len());
    let (number, unit) = (&rest[..split], rest[split..].trim_start());
    if !number.bytes().any(|b| b.is_ascii_digit()) {
        return Err(invalid());
    }
    let mult: u64 = match unit {
        "" | "b" => 1,
        "k" | "ki" | "kb" | "kib" => 1 << 10,
        "m" | "mi" | "mb" | "mib" => 1 << 20,
        "g" | "gi" | "gb" | "gib" => 1 << 30,
        _ => return Err(invalid()),
    };
    let value = if number.contains('.') {
        let v: f64 = number.parse().map_err(|_| invalid())?;
        // A tiny positive rate is still a limit, never "unlimited".
        let v = match v * mult as f64 {
            v if v > 0.0 => v.round().max(1.0),
            v => v,
        };
        // 2^64: the first value that does not fit.
        if v >= 18_446_744_073_709_551_616.0 {
            return Err(too_large());
        }
        v as u64
    } else {
        let v: u64 = number
            .parse()
            .map_err(|e: std::num::ParseIntError| match e.kind() {
                std::num::IntErrorKind::PosOverflow => too_large(),
                _ => invalid(),
            })?;
        v.checked_mul(mult).ok_or_else(too_large)?
    };
    Ok((value > 0).then_some(value))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::borrow::Cow;
    use unicode_width::UnicodeWidthStr;

    const KIB: u64 = 1024;

    #[test]
    fn rate_input_round_trips() {
        assert_eq!(rate_input(1 << 20), "1.0M");
        assert_eq!(rate_input(500 * KIB), "500.0k");
        assert_eq!(rate_input(3 << 29), "1.5G");
        assert_eq!(rate_input(1000), "1000");
        assert_eq!(rate_input(1_234_567), "1234567");
        for v in [1, 1000, 500 * KIB, 1 << 20, 3 << 29, 1_234_567, u64::MAX] {
            assert_eq!(parse_rate(&rate_input(v)), Ok(Some(v)), "{v}");
        }
    }
    const MIB: u64 = 1024 * KIB;
    const GIB: u64 = 1024 * MIB;

    #[test]
    fn bytes_units() {
        assert_eq!(bytes(0), "0 B");
        assert_eq!(bytes(1023), "1023 B");
        assert_eq!(bytes(1024), "1.0 KiB");
        assert_eq!(bytes(1_572_864), "1.5 MiB");
        assert_eq!(bytes(2 * GIB + 6 * GIB / 10), "2.6 GiB");
        assert_eq!(bytes(812 * KIB), "812.0 KiB");
        // Rounding up to 1024.0 rolls over to the next unit.
        assert_eq!(bytes(MIB - 1), "1.0 MiB");
        assert_eq!(bytes(1024 * GIB), "1.0 TiB");
        assert_eq!(bytes(u64::MAX), "16.0 EiB");
    }

    #[test]
    fn speed_zero_is_dash() {
        assert_eq!(speed(0), "—");
        assert_eq!(speed(512), "512 B/s");
        assert_eq!(speed(11 * MIB + 4 * MIB / 10), "11.4 MiB/s");
    }

    #[test]
    fn eta_formats() {
        assert_eq!(eta(None), "—");
        assert_eq!(eta(Some(0)), "0:00");
        assert_eq!(eta(Some(41)), "0:41");
        assert_eq!(eta(Some(725)), "12:05");
        assert_eq!(eta(Some(3723)), "1:02:03");
        assert_eq!(eta(Some(99 * 3600 + 59 * 60 + 59)), "99:59:59");
        assert_eq!(eta(Some(100 * 3600)), "99h+");
        assert_eq!(eta(Some(400_000)), "99h+");
        assert_eq!(eta(Some(u64::MAX)), "99h+");
    }

    #[test]
    fn percent_unknown_total_none() {
        assert_eq!(percent(5, None), None);
        assert_eq!(percent(0, Some(0)), None);
        assert_eq!(percent(0, Some(10)), Some(0.0));
        assert_eq!(percent(5, Some(10)), Some(0.5));
        assert_eq!(percent(10, Some(10)), Some(1.0));
        // Never beyond 100 % (e.g. a total that shrank).
        assert_eq!(percent(20, Some(10)), Some(1.0));
    }

    #[test]
    fn truncate_by_display_width() {
        let t = truncate("日本語ファイル名.iso", 8);
        assert!(t.width() <= 8, "{t} is {} wide", t.width());
        assert!(t.ends_with('…'), "{t}");
        assert_eq!(t, "日本語…");

        let family = "👨‍👩‍👧 family.zip";
        for w in 0..=family.width() + 1 {
            let t = truncate(family, w);
            assert!(t.width() <= w, "{t:?} wider than {w}");
            // The ZWJ cluster is kept whole or dropped whole.
            let kept_whole = t.starts_with("👨‍👩‍👧");
            assert!(
                kept_whole || !t.contains('👨'),
                "split the ZWJ cluster: {t:?}"
            );
        }

        assert_eq!(truncate("short.iso", 20), "short.iso");
        assert_eq!(truncate("exact", 5), "exact");
        assert_eq!(truncate("toolong", 5), "tool…");
        assert_eq!(truncate("abc", 1), "…");
        assert_eq!(truncate("abc", 0), "");
        assert_eq!(truncate("", 0), "");
    }

    #[test]
    fn truncate_middle_keeps_tail() {
        let url = "https://example.com/some/long/path/ubuntu-24.04.iso";
        let t = truncate_middle(url, 30);
        assert_eq!(t.width(), 30);
        assert!(t.starts_with("https://e"), "{t}");
        assert!(t.ends_with("ubuntu-24.04.iso"), "{t}");
        assert!(t.contains('…'));
        assert_eq!(truncate_middle("short", 10), "short");
        assert_eq!(truncate_middle("abcdef", 1), "…");
        assert_eq!(truncate_middle("abcdef", 0), "");
        for w in 0..20 {
            assert!(truncate_middle("日本語のファイル名.iso", w).width() <= w);
        }
    }

    #[test]
    fn display_safe_replaces_controls() {
        assert!(matches!(
            display_safe("plain.iso"),
            Cow::Borrowed("plain.iso")
        ));
        assert_eq!(display_safe("a\tb\nc\rd\x1be"), "a b c d e");
        assert_eq!(display_safe("x\u{7f}y\u{9b}z"), "x y z");
        // Bidi overrides would reorder the rest of the row.
        assert_eq!(display_safe("evil\u{202e}gpj.exe"), "evil gpj.exe");
        assert_eq!(display_safe("日本\n語"), "日本 語");
        // Sanitize first, then truncate: widths stay right.
        assert_eq!(truncate(&display_safe("ab\ncd"), 4), "ab …");
    }

    #[test]
    fn parse_rate_variants() {
        assert_eq!(parse_rate("5M"), Ok(Some(5 * MIB)));
        assert_eq!(parse_rate("5MiB"), Ok(Some(5 * MIB)));
        assert_eq!(parse_rate("5m"), Ok(Some(5 * MIB)));
        assert_eq!(parse_rate("5mb"), Ok(Some(5 * MIB)));
        assert_eq!(parse_rate("5 MiB/s"), Ok(Some(5 * MIB)));
        assert_eq!(parse_rate("500k"), Ok(Some(512_000)));
        assert_eq!(parse_rate("500K/s"), Ok(Some(512_000)));
        assert_eq!(parse_rate("1.5g"), Ok(Some(GIB + GIB / 2)));
        assert_eq!(parse_rate("1.5GiB"), Ok(Some(GIB + GIB / 2)));
        assert_eq!(parse_rate("0.5k"), Ok(Some(512)));
        assert_eq!(parse_rate("0.0001k"), Ok(Some(1)));
        assert_eq!(parse_rate("2048"), Ok(Some(2048)));
        assert_eq!(parse_rate("2048b"), Ok(Some(2048)));
        assert_eq!(parse_rate("  7k  "), Ok(Some(7 * KIB)));
        assert_eq!(parse_rate(&u64::MAX.to_string()), Ok(Some(u64::MAX)));
        for none in ["", "  ", "0", "0k", "0.0M", "none", "None", "OFF", "off"] {
            assert_eq!(parse_rate(none), Ok(None), "{none:?}");
        }
        for bad in [
            "abc",
            "-5M",
            "5x",
            "5 M B",
            "1.2.3k",
            ".",
            "k",
            "5MiBs",
            "18446744073709551616",
            "20000000000g",
            "nan",
            "inf",
        ] {
            let e = parse_rate(bad).expect_err(bad);
            assert!(!e.is_empty());
        }
        assert!(parse_rate("-5M").unwrap_err().contains("negative"));
        assert!(parse_rate("20000000000g").unwrap_err().contains("large"));
    }
}
