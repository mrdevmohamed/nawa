//! Version comparison (Arch/pacman-style) with `epoch:version-rel` support.
//!
//! Rules implemented:
//! * numeric segments compare numerically (`1.9 < 1.10`)
//! * numeric beats alphabetic when mixed
//! * when one side runs out: a digit continuation makes that side newer
//!   (`1.0 < 1.0.1`), a letter continuation makes the exhausted side newer
//!   (`1.0rc1 < 1.0`)

use std::cmp::Ordering;

/// Split `epoch:version-rel` into `(epoch, version, rel)`.
pub fn split_full(v: &str) -> (u64, &str, &str) {
    let (epoch, rest) = match v.find(':') {
        Some(i) if !v[..i].is_empty() && v[..i].bytes().all(|b| b.is_ascii_digit()) => {
            (v[..i].parse().unwrap_or(0), &v[i + 1..])
        }
        _ => (0u64, v),
    };
    let (ver, rel) = match rest.rfind('-') {
        Some(i) => (&rest[..i], &rest[i + 1..]),
        None => (rest, "0"),
    };
    (epoch, ver, rel)
}

enum Seg<'a> {
    Num(u64),
    Alpha(&'a str),
}

fn next_seg(s: &str) -> (Seg<'_>, &str) {
    match s.as_bytes().first() {
        Some(b) if b.is_ascii_digit() => {
            let end = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
            // strip leading zeros for the numeric value
            let trimmed = s[..end].trim_start_matches('0');
            let n: u64 = if trimmed.is_empty() { 0 } else { trimmed.parse().unwrap_or(u64::MAX) };
            (Seg::Num(n), &s[end..])
        }
        Some(_) => {
            // guaranteed to start with an alphabetic char (separators were
            // skipped by core_cmp), so at least one byte is consumed
            let end = s.find(|c: char| !c.is_ascii_alphabetic()).unwrap_or(s.len());
            (Seg::Alpha(&s[..end]), &s[end..])
        }
        None => (Seg::Alpha(""), ""),
    }
}

/// Skip non-alphanumeric separator characters (`.` `-` `_` `+` ...).
/// Without this, next_seg could consume zero characters and loop forever.
fn skip_non_alnum(mut s: &str) -> &str {
    while let Some(c) = s.chars().next() {
        if c.is_ascii_alphanumeric() {
            break;
        }
        s = &s[c.len_utf8()..];
    }
    s
}

fn core_cmp(a: &str, b: &str) -> Ordering {
    let (mut a, mut b) = (a, b);
    loop {
        // exhaustion check on the RAW remainder (pacman semantics: a
        // remainder starting with a letter means the exhausted side is
        // newer — pre-releases like `1.0rc1 < 1.0`; anything else, e.g.
        // `.1` or `+git5`, means the remaining side is newer)
        match (a.is_empty(), b.is_empty()) {
            (true, true) => return Ordering::Equal,
            (true, false) | (false, true) => {
                let rest = if a.is_empty() { b } else { a };
                let exhausted_is_newer = rest.starts_with(|c: char| c.is_ascii_alphabetic());
                let dir = if a.is_empty() { Ordering::Less } else { Ordering::Greater };
                return if exhausted_is_newer { dir.reverse() } else { dir };
            }
            (false, false) => {}
        }
        // skip separators, then re-check: a side may vanish entirely
        a = skip_non_alnum(a);
        b = skip_non_alnum(b);
        match (a.is_empty(), b.is_empty()) {
            (true, true) => return Ordering::Equal,
            (true, false) => {
                return if b.starts_with(|c: char| c.is_ascii_digit()) {
                    Ordering::Less
                } else {
                    Ordering::Equal
                }
            }
            (false, true) => {
                return if a.starts_with(|c: char| c.is_ascii_digit()) {
                    Ordering::Greater
                } else {
                    Ordering::Equal
                }
            }
            (false, false) => {}
        }
        let (sa, ra) = next_seg(a);
        let (sb, rb) = next_seg(b);
        let ord = match (&sa, &sb) {
            (Seg::Num(x), Seg::Num(y)) => x.cmp(y),
            (Seg::Num(_), Seg::Alpha(_)) => Ordering::Greater,
            (Seg::Alpha(_), Seg::Num(_)) => Ordering::Less,
            (Seg::Alpha(x), Seg::Alpha(y)) => x.cmp(y),
        };
        if ord != Ordering::Equal {
            return ord;
        }
        a = ra;
        b = rb;
    }
}

/// Compare full version strings including epoch and release.
pub fn vercmp(a: &str, b: &str) -> Ordering {
    let (ea, va, ra) = split_full(a);
    let (eb, vb, rb) = split_full(b);
    ea.cmp(&eb)
        .then_with(|| core_cmp(va, vb))
        .then_with(|| core_cmp(ra, rb))
}

#[cfg(test)]
mod tests {
    use super::*;
    use Ordering::*;

    #[test]
    fn basic() {
        assert_eq!(vercmp("1.0", "1.0"), Equal);
        assert_eq!(vercmp("1.0", "1.1"), Less);
        assert_eq!(vercmp("1.9", "1.10"), Less);
        assert_eq!(vercmp("2.40", "2.4"), Greater);
        assert_eq!(vercmp("1.0", "1.0.1"), Less);
    }

    #[test]
    fn prereleases() {
        assert_eq!(vercmp("1.0rc1", "1.0"), Less);
        assert_eq!(vercmp("1.0rc1", "1.0rc2"), Less);
        assert_eq!(vercmp("1.0alpha", "1.0beta"), Less);
        assert_eq!(vercmp("6.6.30", "6.6.31"), Less);
    }

    #[test]
    fn epoch_and_rel() {
        assert_eq!(vercmp("1:1.0", "0.9"), Greater);
        assert_eq!(vercmp("1.0-2", "1.0-1"), Greater);
        assert_eq!(vercmp("1.0-10", "1.0-9"), Greater);
    }

    #[test]
    fn leading_zeros() {
        assert_eq!(vercmp("1.07", "1.7"), Equal);
    }

    #[test]
    fn separators_do_not_hang_or_matter() {
        // regression: a segment starting with '.' used to loop forever
        assert_eq!(vercmp("1.0", "1.0"), Equal);
        assert_eq!(vercmp("1.0.0", "1.0"), Greater);
        assert_eq!(vercmp("1.0.1", "1.0"), Greater);
        assert_eq!(vercmp("6.6.30", "6.6.30"), Equal);
        assert_eq!(vercmp("2.43-2", "2.43-1"), Greater);
        // pacman semantics: a digit continuation wins, an alphabetic
        // continuation loses ("2.43" > "2.43-r1")
        assert_eq!(vercmp("2.43-r1", "2.43"), Less);
        assert_eq!(vercmp("1.0+git5", "1.0"), Greater);
    }
}
