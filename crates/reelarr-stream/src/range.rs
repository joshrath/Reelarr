//! Parsing a single HTTP `Range: bytes=...` header (RFC 9110 section 14).

/// What to send for a request against a resource of known length.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ByteRange {
    /// No usable range: send the whole file with 200.
    Full,
    /// Send `start..=end` with 206.
    Partial { start: u64, end: u64 },
    /// No requested byte exists: send 416 with `Content-Range: bytes */len`.
    Unsatisfiable,
}

/// Interpret a `Range` header value for a resource of `len` bytes.
///
/// Malformed headers, other units and multi-range requests are ignored (the
/// server may always answer with the full representation). Browsers' media
/// elements only ever send a single range.
pub fn parse(header: Option<&str>, len: u64) -> ByteRange {
    let Some(value) = header.map(str::trim) else {
        return ByteRange::Full;
    };
    let Some((unit, spec)) = value.split_once('=') else {
        return ByteRange::Full;
    };
    if !unit.trim().eq_ignore_ascii_case("bytes") || spec.contains(',') {
        return ByteRange::Full;
    }
    let Some((first, last)) = spec.trim().split_once('-') else {
        return ByteRange::Full;
    };
    let (first, last) = (first.trim(), last.trim());
    match (number(first), number(last)) {
        // bytes=-N : the last N bytes.
        (None, Some(n)) if first.is_empty() => {
            if n == 0 || len == 0 {
                ByteRange::Unsatisfiable
            } else {
                ByteRange::Partial {
                    start: len.saturating_sub(n),
                    end: len - 1,
                }
            }
        }
        // bytes=S- : from S to the end.
        (Some(start), None) if last.is_empty() => from(start, u64::MAX, len),
        // bytes=S-E
        (Some(start), Some(end)) if start <= end => from(start, end, len),
        _ => ByteRange::Full,
    }
}

fn from(start: u64, end: u64, len: u64) -> ByteRange {
    if start >= len {
        ByteRange::Unsatisfiable
    } else {
        ByteRange::Partial {
            start,
            end: end.min(len - 1),
        }
    }
}

/// Digits only; values too large for u64 saturate.
fn number(s: &str) -> Option<u64> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(s.parse().unwrap_or(u64::MAX))
}

#[cfg(test)]
mod tests {
    use super::ByteRange::*;
    use super::*;

    #[test]
    fn ranges() {
        let p = |h: &str| parse(Some(h), 1000);
        assert_eq!(parse(None, 1000), Full);
        assert_eq!(p("bytes=0-"), Partial { start: 0, end: 999 });
        assert_eq!(p("bytes=0-0"), Partial { start: 0, end: 0 });
        assert_eq!(
            p("bytes=100-199"),
            Partial {
                start: 100,
                end: 199
            }
        );
        assert_eq!(
            p("bytes=900-5000"),
            Partial {
                start: 900,
                end: 999
            }
        );
        assert_eq!(
            p("bytes=-100"),
            Partial {
                start: 900,
                end: 999
            }
        );
        assert_eq!(p("bytes=-5000"), Partial { start: 0, end: 999 });
        assert_eq!(p("Bytes = 5 - 9"), Partial { start: 5, end: 9 });
        assert_eq!(
            p("bytes=10-99999999999999999999999"),
            Partial {
                start: 10,
                end: 999
            }
        );
    }

    #[test]
    fn unsatisfiable() {
        let p = |h: &str| parse(Some(h), 1000);
        assert_eq!(p("bytes=1000-"), Unsatisfiable);
        assert_eq!(p("bytes=1000-2000"), Unsatisfiable);
        assert_eq!(p("bytes=-0"), Unsatisfiable);
        assert_eq!(p("bytes=99999999999999999999999-"), Unsatisfiable);
        assert_eq!(parse(Some("bytes=0-"), 0), Unsatisfiable);
        assert_eq!(parse(Some("bytes=-1"), 0), Unsatisfiable);
    }

    #[test]
    fn ignored() {
        let p = |h: &str| parse(Some(h), 1000);
        for h in [
            "",
            "bytes",
            "bytes=",
            "bytes=-",
            "bytes=abc-",
            "bytes=5-1",
            "bytes=+1-2",
            "items=0-1",
            "bytes=0-1,5-6",
            "bytes=0x10-",
        ] {
            assert_eq!(p(h), Full, "{h:?}");
        }
    }
}
