//! "Did you mean?" suggestions for mistyped package names.
//!
//! The edit distance is `strsim::levenshtein` (same insert/delete/substitute
//! cost-1 definition as the hand-written version this replaces, case
//! sensitive like it was — callers compare lowercased names). A small cutoff
//! is enough for the package-name vocabulary of a source distribution.

/// Levenshtein distance (insert / delete / substitute, all cost 1).
pub use strsim::levenshtein;

/// Rank `target` against candidate names; best matches first.
///
/// * a candidate *containing* the target as a substring ranks first
///   (typing `zst` for `zstd` is a truncation, not a typo);
/// * otherwise candidates within the distance cutoff win, closest first;
/// * ties break alphabetically for deterministic output.
pub fn did_you_mean(target: &str, candidates: &[String]) -> Vec<String> {
    let t = target.to_ascii_lowercase();
    let cutoff = if t.len() >= 8 { 3 } else { 2 };
    let mut scored: Vec<(usize, String)> = Vec::new();
    for c in candidates {
        if *c == target {
            continue; // exact hits never reach the suggestion path
        }
        let cl = c.to_ascii_lowercase();
        let dist = if cl.contains(&t) && !t.is_empty() {
            1
        } else {
            levenshtein(&t, &cl)
        };
        if dist <= cutoff {
            scored.push((dist, c.clone()));
        }
    }
    scored.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    scored.truncate(3);
    scored.into_iter().map(|(_, c)| c).collect()
}

/// Ready-to-append error text: `" — did you mean `x`?"` / with an OR list,
/// or `""` when nothing plausible exists.
pub fn hint(target: &str, candidates: &[String]) -> String {
    let picks = did_you_mean(target, candidates);
    match picks.len() {
        0 => String::new(),
        1 => format!(" — did you mean `{}`?", picks[0]),
        _ => {
            let list: Vec<String> = picks.iter().map(|p| format!("`{p}`")).collect();
            format!(" — did you mean {}?", list.join(" or "))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn levenshtein_basics() {
        assert_eq!(levenshtein("", ""), 0);
        assert_eq!(levenshtein("abc", ""), 3);
        assert_eq!(levenshtein("kitten", "sitting"), 3);
        assert_eq!(levenshtein("gzip", "gzip"), 0);
        assert_eq!(levenshtein("gzp", "gzip"), 1);
        assert_eq!(levenshtein("", "abc"), 3);
    }

    /// Reference implementation (full-matrix DP) the strsim call is checked
    /// against — guards the dependency swap against subtle differences, incl.
    /// the empty-string edges strsim's rolling-row aliases must satisfy.
    fn reference_levenshtein(a: &str, b: &str) -> usize {
        let a: Vec<char> = a.chars().collect();
        let b: Vec<char> = b.chars().collect();
        let mut m = vec![vec![0usize; b.len() + 1]; a.len() + 1];
        for (i, row) in m.iter_mut().enumerate() {
            row[0] = i;
        }
        for (j, c) in m[0].iter_mut().enumerate() {
            *c = j;
        }
        for i in 1..=a.len() {
            for j in 1..=b.len() {
                m[i][j] = std::cmp::min(
                    m[i - 1][j] + 1,
                    std::cmp::min(
                        m[i][j - 1] + 1,
                        m[i - 1][j - 1] + usize::from(a[i - 1] != b[j - 1]),
                    ),
                );
            }
        }
        m[a.len()][b.len()]
    }

    #[test]
    fn levenshtein_matches_reference_implementation() {
        // one namespace's alnum soup including many edit neighbours, and a
        // non-ASCII name: every pair must agree with the full DP reference
        let names = [
            "gzip", "gzp", "zstd", "zst", "grep", "gawk", "gmp", "mpfr", "glibc",
            "libz", "zlib", "curl", "openssl", "openssl1", "", "换行包",
        ];
        for a in names {
            for b in names {
                assert_eq!(
                    levenshtein(a, b),
                    reference_levenshtein(a, b),
                    "strsim distance disagrees for {a:?} vs {b:?}"
                );
            }
        }
        // a random-ish sweep of generated one-char families
        let mut base;
        for seed in 0..40 {
            base = format!("pkg-{}-suffix{}", seed, seed % 7);
            for cut in [0, 1, 2] {
                let shorter = &base[..base.len().saturating_sub(cut * 2)];
                assert_eq!(levenshtein(&base, shorter), reference_levenshtein(&base, shorter));
                assert_eq!(levenshtein(shorter, &base), reference_levenshtein(shorter, &base));
            }
        }
    }

    #[test]
    fn suggests_close_names() {
        let cands = v(&["gzip", "zstd", "gawk", "grep"]);
        // gzp: gzip at distance 1, grep at distance 2 — closest first
        assert_eq!(did_you_mean("gzp", &cands), v(&["gzip", "grep"]));
        // "zst" is a substring of zstd -> ranks first, nothing else is close
        assert_eq!(did_you_mean("zst", &cands), v(&["zstd"]));
        // gawk is distance 3 from grep — out of the cutoff for a 4-char name
        assert_eq!(did_you_mean("grep", &v(&["gzip", "gawk"])), v(&["gzip"]));
    }

    #[test]
    fn no_plausible_match_is_empty() {
        let cands = v(&["gzip", "zstd"]);
        assert!(did_you_mean("totally-unrelated-package-name", &cands).is_empty());
        // exact match is not a "suggestion"
        assert!(did_you_mean("gzip", &cands).is_empty());
    }

    #[test]
    fn deterministic_and_capped_at_three() {
        // pkga0..pkga8 sit at distance 2 from the target; pkgb9 at distance 1
        let mut cands: Vec<String> = (0..9).map(|i| format!("pkga{i}")).collect();
        cands.push("pkgb9".into());
        let picks = did_you_mean("pkgz9", &cands);
        assert!(picks.len() <= 3);
        // closest first, ties alphabetically
        assert_eq!(picks, v(&["pkgb9", "pkga0", "pkga1"]));
    }

    #[test]
    fn hint_text_formats() {
        assert_eq!(hint("gzp", &v(&["gzip"])), " — did you mean `gzip`?");
        // two plausible candidates become an OR list
        assert_eq!(
            hint("gzp", &v(&["gzip", "grep"])),
            " — did you mean `gzip` or `grep`?"
        );
        assert_eq!(hint("nothing-like-this", &v(&["gzip"])), "");
    }
}
