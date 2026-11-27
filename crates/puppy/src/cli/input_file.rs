//! `puppy add -i FILE`: the input-file format.
//!
//! One download per line that does not start with whitespace: its URI, or several
//! TAB-separated URIs of the same file (mirrors). Lines under it that start with a space
//! or a TAB are `key=value` options for that download (`dir`, `out`, `checksum`,
//! ...). Lines starting with `#` (also indented ones) are comments;
//! blank lines are skipped. CRLF line endings and a leading BOM are accepted.
//!
//! [`parse`] only splits the text into [`Entry`]s; [`interpret`] turns one entry's
//! option lines into [`EntryOptions`], warning about options puppy does not support.

use super::files::parse_spec;
use super::seed::{parse_ratio, parse_time};
use puppy_core::checksum::Checksum;
use puppy_core::types::Selection;
use puppy_tui::app::forms::expand_tilde;
use puppy_tui::format::parse_rate;
use std::path::PathBuf;

/// One `key=value` line under a URI line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OptionLine {
    /// 1-based.
    pub line: usize,
    pub key: String,
    pub value: String,
}

/// One download of the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// 1-based number of the URI line.
    pub line: usize,
    /// The URIs as written, in order (never empty); more than one are mirrors.
    pub uris: Vec<String>,
    pub options: Vec<OptionLine>,
}

/// A problem on one line.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("line {line}: {message}")]
pub struct LineError {
    /// 1-based.
    pub line: usize,
    pub message: String,
}

fn line_error(line: usize, message: impl Into<String>) -> LineError {
    LineError {
        line,
        message: message.into(),
    }
}

/// Splits `text` into entries, and lists every malformed line (an option line before
/// the first URI line, one without `=`, or with an empty key), which is left out.
pub fn parse(text: &str) -> (Vec<Entry>, Vec<LineError>) {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut entries: Vec<Entry> = Vec::new();
    let mut errors = Vec::new();
    for (i, raw) in text.split('\n').enumerate() {
        let n = i + 1;
        let raw = raw.strip_suffix('\r').unwrap_or(raw);
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if raw.starts_with([' ', '\t']) {
            let Some(entry) = entries.last_mut() else {
                errors.push(line_error(
                    n,
                    format!("option {line:?} comes before any URI (option lines are indented under the URI they belong to)"),
                ));
                continue;
            };
            match line.split_once('=') {
                Some((key, value)) if !key.trim().is_empty() => entry.options.push(OptionLine {
                    line: n,
                    key: key.trim().to_string(),
                    value: value.trim().to_string(),
                }),
                Some(_) => errors.push(line_error(n, format!("option {line:?} has no name"))),
                None => errors.push(line_error(
                    n,
                    format!("expected an option as key=value, got {line:?}"),
                )),
            }
            continue;
        }
        let uris: Vec<String> = line
            .split('\t')
            .map(str::trim)
            .filter(|u| !u.is_empty())
            .map(str::to_string)
            .collect();
        entries.push(Entry {
            line: n,
            uris,
            options: Vec::new(),
        });
    }
    (entries, errors)
}

/// The options of one entry, as puppy understands them. `None` leaves the value to the
/// command-line flags (or the daemon's default).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct EntryOptions {
    /// `dir`: absolute (relative ones are resolved against the current directory).
    pub dir: Option<PathBuf>,
    /// `out`: a file name.
    pub out: Option<String>,
    /// `checksum=ALGO=HEX`.
    pub checksum: Option<Checksum>,
    /// `header` (repeatable).
    pub headers: Vec<(String, String)>,
    pub referer: Option<String>,
    pub user_agent: Option<String>,
    /// `max-download-limit`: `Some(None)` is "no limit" (`0`).
    pub limit: Option<Option<u64>>,
    /// `select-file`.
    pub select: Option<Selection>,
    /// `seed-ratio`: `Some(None)` is `default`.
    pub seed_ratio: Option<Option<f32>>,
    /// `seed-time` in seconds: `Some(None)` is `default`.
    pub seed_time: Option<Option<u64>>,
    /// `pause`.
    pub pause: Option<bool>,
}

impl EntryOptions {
    /// Whether an option only a torrent can use was given; its name if so.
    pub fn torrent_only(&self) -> Option<&'static str> {
        if self.select.is_some() {
            Some("select-file")
        } else if self.seed_ratio.is_some() {
            Some("seed-ratio")
        } else if self.seed_time.is_some() {
            Some("seed-time")
        } else {
            None
        }
    }
}

/// The options puppy understands (the rest are warned about and ignored).
pub const SUPPORTED: [&str; 11] = [
    "dir",
    "out",
    "checksum",
    "header",
    "referer",
    "user-agent",
    "max-download-limit",
    "select-file",
    "seed-ratio",
    "seed-time",
    "pause",
];

/// Splits `Name: value` at the first `:`; `None` without a name.
pub fn split_header(h: &str) -> Option<(String, String)> {
    match h.split_once(':') {
        Some((k, v)) if !k.trim().is_empty() => Some((k.trim().to_string(), v.trim().to_string())),
        _ => None,
    }
}

/// `seed-time`: a bare number is minutes (`90`, `1.5`); anything else is
/// a puppy duration (`2h`) or `default`. `0` (in this format: do not seed at all) has no puppy
/// equivalent and is refused rather than turned into its opposite (puppy's `0`: never
/// stop).
fn parse_seed_time(v: &str) -> Result<Option<u64>, String> {
    if let Ok(minutes) = v.parse::<f64>() {
        if !minutes.is_finite() || minutes < 0.0 {
            return Err(format!("bad seed-time {v:?}: expected minutes, e.g. 90"));
        }
        if minutes == 0.0 {
            return Err("seed-time=0 (do not seed) is not supported: puppy seeds a \
                 finished torrent until a limit is reached; give a time, e.g. seed-time=1"
                .into());
        }
        return Ok(Some(((minutes * 60.0).round() as u64).max(1)));
    }
    parse_time(v).map_err(|e| e.to_string())
}

/// One entry's options as [`interpret`] read them.
#[derive(Debug, Default)]
pub struct Interpreted {
    pub options: EntryOptions,
    /// Unsupported options, one per line: ignored.
    pub warnings: Vec<LineError>,
    /// Bad values: the entry is invalid.
    pub errors: Vec<LineError>,
}

/// Reads one entry's options. Later values of a single-valued option replace earlier
/// ones; `header` adds up. Unsupported options become warnings (one per
/// line); bad values are errors.
pub fn interpret(entry: &Entry) -> Interpreted {
    let mut o = EntryOptions::default();
    let mut warnings = Vec::new();
    let mut errors = Vec::new();
    for OptionLine { line, key, value } in &entry.options {
        let (line, v) = (*line, value.as_str());
        if !SUPPORTED.contains(&key.as_str()) {
            warnings.push(line_error(
                line,
                format!("ignoring unsupported option `{key}`"),
            ));
            continue;
        }
        if v.is_empty() {
            errors.push(line_error(line, format!("option `{key}` needs a value")));
            continue;
        }
        let bad = |what: String| line_error(line, format!("bad {key} {v:?}: {what}"));
        let r: Result<(), LineError> = match key.as_str() {
            "dir" => {
                let p = expand_tilde(v);
                o.dir = Some(std::path::absolute(&p).unwrap_or(p));
                Ok(())
            }
            "out" => {
                if v.contains(['/', '\\']) || v == "." || v == ".." {
                    Err(bad(
                        "a file name, without folders (put those in dir=)".into()
                    ))
                } else {
                    o.out = Some(v.to_string());
                    Ok(())
                }
            }
            "checksum" => v
                .parse::<Checksum>()
                .map(|c| o.checksum = Some(c))
                .map_err(|e| bad(e.to_string())),
            "header" => match split_header(v) {
                Some(h) => {
                    o.headers.push(h);
                    Ok(())
                }
                None => Err(bad("expected \"Name: value\"".into())),
            },
            "referer" => {
                o.referer = Some(v.to_string());
                Ok(())
            }
            "user-agent" => {
                o.user_agent = Some(v.to_string());
                Ok(())
            }
            "max-download-limit" => parse_rate(v).map(|r| o.limit = Some(r)).map_err(bad),
            "select-file" => parse_spec(&[v.to_string()])
                .map(|s| o.select = Some(s))
                .map_err(|e| bad(e.to_string())),
            "seed-ratio" => parse_ratio(v).map(|r| o.seed_ratio = Some(r)).map_err(|_| {
                bad("expected a number like 1.5, 0 (no ratio limit) or `default`".into())
            }),
            "seed-time" => parse_seed_time(v)
                .map(|t| o.seed_time = Some(t))
                .map_err(|e| line_error(line, e)),
            "pause" => match v {
                "true" => {
                    o.pause = Some(true);
                    Ok(())
                }
                "false" => {
                    o.pause = Some(false);
                    Ok(())
                }
                _ => Err(bad("expected true or false".into())),
            },
            _ => unreachable!("every SUPPORTED option is handled"),
        };
        if let Err(e) = r {
            errors.push(e);
        }
    }
    Interpreted {
        options: o,
        warnings,
        errors,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opt(line: usize, key: &str, value: &str) -> OptionLine {
        OptionLine {
            line,
            key: key.into(),
            value: value.into(),
        }
    }

    fn entry(line: usize, uris: &[&str], options: Vec<OptionLine>) -> Entry {
        Entry {
            line,
            uris: uris.iter().map(|u| u.to_string()).collect(),
            options,
        }
    }

    /// `parse` of text without malformed lines.
    fn ok(text: &str) -> Vec<Entry> {
        let (entries, errors) = parse(text);
        assert_eq!(errors, [], "{text:?}");
        entries
    }

    /// The options of an entry without bad values.
    fn clean(e: &Entry) -> EntryOptions {
        let got = interpret(e);
        assert!(got.errors.is_empty(), "{got:?}");
        got.options
    }

    fn one(options: &[(&str, &str)]) -> Entry {
        entry(
            1,
            &["http://h/a"],
            options
                .iter()
                .enumerate()
                .map(|(i, (k, v))| opt(i + 2, k, v))
                .collect(),
        )
    }

    #[test]
    fn uris_options_comments_and_blank_lines() {
        let text = "# a list\n\
                    http://h/a\n  dir=/tmp/x\n\tout=a.bin\n\
                    \n   \n# between\n\
                    http://h/b\n  # an indented comment\n  pause=true\n\
                    magnet:?xt=urn:btih:abc\n";
        let got = ok(text);
        assert_eq!(
            got,
            [
                entry(
                    2,
                    &["http://h/a"],
                    vec![opt(3, "dir", "/tmp/x"), opt(4, "out", "a.bin")]
                ),
                entry(8, &["http://h/b"], vec![opt(10, "pause", "true")]),
                entry(11, &["magnet:?xt=urn:btih:abc"], vec![]),
            ]
        );
    }

    #[test]
    fn crlf_and_bom() {
        let text = "\u{feff}http://h/a\r\n  out=a.bin\r\n\r\nhttp://h/b\r\n";
        let got = ok(text);
        assert_eq!(
            got,
            [
                entry(1, &["http://h/a"], vec![opt(2, "out", "a.bin")]),
                entry(4, &["http://h/b"], vec![]),
            ]
        );
    }

    #[test]
    fn tabs_separate_mirrors_and_indent_with_either() {
        let text = "http://h/a\thttp://m/a \t\thttp://n/a\n\tout=x\n \t out=y\n";
        let got = ok(text);
        assert_eq!(
            got,
            [entry(
                1,
                &["http://h/a", "http://m/a", "http://n/a"],
                vec![opt(2, "out", "x"), opt(3, "out", "y")]
            )]
        );
    }

    #[test]
    fn values_keep_inner_equals_and_lose_outer_whitespace() {
        let text = "http://h/a \n  checksum = sha-256=abcd  \n  header=X-A: b=c\t\n  out=\n";
        let got = ok(text);
        assert_eq!(got[0].uris, ["http://h/a"]);
        assert_eq!(
            got[0].options,
            [
                opt(2, "checksum", "sha-256=abcd"),
                opt(3, "header", "X-A: b=c"),
                opt(4, "out", ""),
            ]
        );
    }

    #[test]
    fn malformed_lines_are_all_reported() {
        let text = "  dir=/x\nhttp://h/a\n  no-equals\n  =v\n  out=ok\n";
        let (entries, errs) = parse(text);
        assert_eq!(
            entries,
            [entry(2, &["http://h/a"], vec![opt(5, "out", "ok")])]
        );
        assert_eq!(
            errs.iter().map(|e| e.line).collect::<Vec<_>>(),
            [1, 3, 4],
            "{errs:?}"
        );
        assert!(errs[0].message.contains("before any URI"), "{errs:?}");
        assert!(errs[1].message.contains("key=value"), "{errs:?}");
        assert!(errs[2].to_string().starts_with("line 4: "), "{errs:?}");
    }

    #[test]
    fn empty_text_has_no_entries() {
        assert_eq!(ok(""), []);
        assert_eq!(ok("\u{feff}\r\n# x\n \n"), []);
    }

    #[test]
    fn interprets_every_supported_option() {
        let sum = format!("sha-256={}", "ab".repeat(32));
        let e = one(&[
            ("dir", "rel/dir"),
            ("out", "a.bin"),
            ("checksum", &sum),
            ("header", "X-A: 1"),
            ("header", "X-B: two: 2"),
            ("referer", "http://r/"),
            ("user-agent", "UA/1 (x=y)"),
            ("max-download-limit", "500K"),
            ("select-file", "1,3-4"),
            ("seed-ratio", "1.5"),
            ("seed-time", "90"),
            ("pause", "true"),
        ]);
        let Interpreted {
            options: o,
            warnings,
            errors,
        } = interpret(&e);
        assert!(errors.is_empty(), "{errors:?}");
        assert!(warnings.is_empty(), "{warnings:?}");
        let dir = o.dir.unwrap();
        assert!(dir.is_absolute(), "{dir:?}");
        assert!(dir.ends_with("rel/dir"), "{dir:?}");
        assert_eq!(o.out.as_deref(), Some("a.bin"));
        assert_eq!(o.checksum.unwrap().to_string(), sum);
        assert_eq!(
            o.headers,
            [("X-A".into(), "1".into()), ("X-B".into(), "two: 2".into())]
        );
        assert_eq!(o.referer.as_deref(), Some("http://r/"));
        assert_eq!(o.user_agent.as_deref(), Some("UA/1 (x=y)"));
        assert_eq!(o.limit, Some(Some(500 * 1024)));
        assert_eq!(o.select.unwrap().indices, [0, 2, 3]);
        assert_eq!(o.seed_ratio, Some(Some(1.5)));
        assert_eq!(o.seed_time, Some(Some(90 * 60)));
        assert_eq!(o.pause, Some(true));
    }

    #[test]
    fn later_values_win_and_zero_or_default_clear() {
        let o = clean(&one(&[
            ("out", "a"),
            ("out", "b"),
            ("max-download-limit", "1M"),
            ("max-download-limit", "0"),
            ("seed-ratio", "default"),
            ("seed-time", "2h"),
            ("pause", "false"),
        ]));
        assert_eq!(o.out.as_deref(), Some("b"));
        assert_eq!(o.limit, Some(None));
        assert_eq!(o.seed_ratio, Some(None));
        assert_eq!(o.seed_time, Some(Some(7200)));
        assert_eq!(o.pause, Some(false));
        assert_eq!(o.torrent_only(), Some("seed-ratio"));
        assert_eq!(EntryOptions::default().torrent_only(), None);
    }

    #[test]
    fn seed_time_minutes() {
        assert_eq!(parse_seed_time("1.5"), Ok(Some(90)));
        assert_eq!(parse_seed_time("0.001"), Ok(Some(1)));
        assert_eq!(parse_seed_time("default"), Ok(None));
        assert!(parse_seed_time("0").unwrap_err().contains("not supported"));
        assert!(parse_seed_time("-1").is_err());
        assert!(parse_seed_time("soon").is_err());
    }

    #[test]
    fn unsupported_options_warn_per_line() {
        let Interpreted {
            options: o,
            warnings,
            errors,
        } = interpret(&one(&[
            ("split", "5"),
            ("out", "a"),
            ("max-connection-per-server", "4"),
        ]));
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(o.out.as_deref(), Some("a"));
        assert_eq!(
            warnings.iter().map(ToString::to_string).collect::<Vec<_>>(),
            [
                "line 2: ignoring unsupported option `split`",
                "line 4: ignoring unsupported option `max-connection-per-server`",
            ]
        );
    }

    #[test]
    fn bad_values_are_errors_with_their_lines() {
        let got = interpret(&one(&[
            ("out", ""),
            ("out", "sub/a.bin"),
            ("checksum", "sha-256=xyz"),
            ("header", "no colon"),
            ("max-download-limit", "fast"),
            ("select-file", "0"),
            ("seed-ratio", "lots"),
            ("seed-time", "0"),
            ("pause", "yes"),
            ("dir", "/fine"),
            ("split", "2"),
        ]));
        // Unsupported options are still reported next to the errors.
        assert_eq!(got.warnings.len(), 1, "{got:?}");
        assert_eq!(got.warnings[0].line, 12, "{got:?}");
        let errs = got.errors;
        assert_eq!(
            errs.iter().map(|e| e.line).collect::<Vec<_>>(),
            (2..=10).collect::<Vec<_>>(),
            "{errs:?}"
        );
        assert!(errs[0].message.contains("needs a value"), "{errs:?}");
        assert!(errs[1].message.contains("without folders"), "{errs:?}");
        assert!(errs[8].message.contains("true or false"), "{errs:?}");
    }

    #[test]
    fn split_header_needs_a_name() {
        assert_eq!(split_header(" A : b:c "), Some(("A".into(), "b:c".into())));
        assert_eq!(split_header(": b"), None);
        assert_eq!(split_header("b"), None);
    }
}
