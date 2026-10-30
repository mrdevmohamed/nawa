//! Version comparison (Arch/pacman-style) with `epoch:version-rel` support.
//!
//! Rules implemented:
//! * numeric segments compare numerically (`1.9 < 1.10`)
//! * numeric beats alphabetic when mixed
//! * when one side runs out: a digit continuation makes that side newer
//!   (`1.0 < 1.0.1`), a letter continuation makes the exhausted side newer
//!   (`1.0rc1 < 1.0`)

use std::cmp::Ordering;

/// Split `epoch:version-rel` into `(epoch, version, rel)`. The epoch is kept
/// as its digit string (`"0"` when absent) so arbitrarily long epochs compare
/// without integer overflow; `rel` is `"0"` when absent.
pub fn split_full(v: &str) -> (&str, &str, &str) {
    let (epoch, rest) = match v.find(':') {
        Some(i) if !v[..i].is_empty() && v[..i].bytes().all(|b| b.is_ascii_digit()) => {
            (&v[..i], &v[i + 1..])
        }
        _ => ("0", v),
    };
    let (ver, rel) = match rest.rfind('-') {
        Some(i) => (&rest[..i], &rest[i + 1..]),
        None => (rest, "0"),
    };
    (epoch, ver, rel)
}

/// Compare two digit strings numerically without converting to an integer
/// (libalpm semantics): drop leading zeros, the longer string is larger,
/// equal lengths compare bytewise. Never overflows, never saturates.
fn num_cmp(a: &[u8], b: &[u8]) -> Ordering {
    let strip = |s: &[u8]| -> usize { s.iter().take_while(|&&c| c == b'0').count() };
    let (a, b) = (&a[strip(a)..], &b[strip(b)..]);
    a.len().cmp(&b.len()).then_with(|| a.cmp(b))
}

/// Port of libalpm's `rpmvercmp` (lib/libalpm/version.c, itself adapted from
/// rpm): walk both strings segment by segment (maximal digit runs or maximal
/// letter runs, preceded by separators).
///
/// * a difference in the *number of separator characters* in front of the
///   next segment decides immediately (more separators = newer), as libalpm
///   does, so `1.0 < 1..0`;
/// * digit segment vs letter segment: the digit side is newer;
/// * once one side runs out, the leftover decides: it is older if it starts
///   with a letter (`1.0rc1 < 1.0`), otherwise it is newer (`1.0 < 1.0.1`,
///   `1.0 < 1.0+git5`).
fn core_cmp(a: &str, b: &str) -> Ordering {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let (mut i, mut j) = (0usize, 0usize);
    while i < a.len() && j < b.len() {
        let (si, sj) = (i, j);
        while i < a.len() && !a[i].is_ascii_alphanumeric() {
            i += 1;
        }
        while j < b.len() && !b[j].is_ascii_alphanumeric() {
            j += 1;
        }
        // ran off the end of either side while skipping separators
        if i >= a.len() || j >= b.len() {
            break;
        }
        if i - si != j - sj {
            return (i - si).cmp(&(j - sj));
        }
        let (ps, qs) = (i, j);
        let numeric = a[i].is_ascii_digit();
        if numeric {
            while i < a.len() && a[i].is_ascii_digit() {
                i += 1;
            }
            while j < b.len() && b[j].is_ascii_digit() {
                j += 1;
            }
        } else {
            while i < a.len() && a[i].is_ascii_alphabetic() {
                i += 1;
            }
            while j < b.len() && b[j].is_ascii_alphabetic() {
                j += 1;
            }
        }
        // segments of different types: numeric is newer
        if j == qs {
            return if numeric { Ordering::Greater } else { Ordering::Less };
        }
        let ord = if numeric {
            num_cmp(&a[ps..i], &b[qs..j])
        } else {
            a[ps..i].cmp(&b[qs..j])
        };
        if ord != Ordering::Equal {
            return ord;
        }
    }
    let (a_done, b_done) = (i >= a.len(), j >= b.len());
    if a_done && b_done {
        return Ordering::Equal;
    }
    let a_alpha = !a_done && a[i].is_ascii_alphabetic();
    let b_alpha = !b_done && b[j].is_ascii_alphabetic();
    if (a_done && !b_alpha) || a_alpha {
        Ordering::Less
    } else {
        Ordering::Greater
    }
}

/// Compare full version strings including epoch and release.
///
/// Deliberate difference from libalpm: a missing release counts as `0`
/// (libalpm skips the release comparison unless both sides have one), so
/// `1.0-1 > 1.0`. Nawa packages always carry a release.
pub fn vercmp(a: &str, b: &str) -> Ordering {
    let (ea, va, ra) = split_full(a);
    let (eb, vb, rb) = split_full(b);
    num_cmp(ea.as_bytes(), eb.as_bytes())
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

    /// Expected orderings; every row is also checked mirrored.
    fn check(rows: &[(&str, &str, Ordering)]) {
        for &(a, b, want) in rows {
            assert_eq!(vercmp(a, b), want, "vercmp({a:?}, {b:?})");
            assert_eq!(vercmp(b, a), want.reverse(), "vercmp({b:?}, {a:?})");
        }
    }

    #[test]
    fn huge_numeric_components_do_not_saturate() {
        // libalpm never converts to an integer: longer digit string wins.
        check(&[
            ("1.99999999999999999999", "1.100000000000000000000", Less),
            ("1.18446744073709551616", "1.18446744073709551615", Greater), // 2^64 vs 2^64-1
            ("1.00000000000000000000000001", "1.1", Equal),
            ("99999999999999999999:1.0", "100000000000000000000:0.1", Less), // epochs
        ]);
    }

    #[test]
    fn separator_count_matches_libalpm() {
        check(&[
            ("1.0", "1..0", Less),
            ("1.0", "1_0", Equal),
            ("1.0", "1-0", Greater), // '-' splits off the release: "1.0" vs "1" + rel 0
            ("1.a", "1.", Less),
            ("1a", "1", Less),
            ("1", "1.", Less),
            ("1.", "1..", Equal),
            ("1.", "1.0", Less),
        ]);
    }

    #[test]
    fn pacman_vercmptest_table() {
        // from pacman's own test/util/vercmptest.sh (versions without rel,
        // or with equal rel)
        check(&[
            ("1.5.0", "1.5.0", Equal),
            ("1.5.1", "1.5.0", Greater),
            ("1.5.1", "1.5", Greater),
            ("1.5.0-1", "1.5.0-1", Equal),
            ("1.5.0-1", "1.5.0-2", Less),
            ("1.5.0-1", "1.5.1-1", Less),
            ("1.5.0-2", "1.5.1-1", Less),
            ("1.5-1", "1.5.1-1", Less),
            ("1.5-2", "1.5.1-1", Less),
            ("1.5-2", "1.5.1-2", Less),
            ("1.0alpha", "1.0b", Less),
            ("1.0b", "1.0beta", Less),
            ("1.0beta", "1.0rc", Less),
            ("1.0rc", "1.0", Less),
            ("1.5.a", "1.5", Greater),
            ("1.5.b", "1.5.a", Greater),
            ("1.5.1", "1.5.b", Greater),
            ("1:1.0", "1.0", Greater),
            ("1:1.1", "1.1", Greater),
        ]);
    }

    #[test]
    fn missing_release_is_zero_not_ignored() {
        // intentional difference from libalpm (see `vercmp` docs)
        assert_eq!(vercmp("1.0-1", "1.0"), Greater);
        assert_eq!(vercmp("1.0", "1.0-0"), Equal);
    }
}
