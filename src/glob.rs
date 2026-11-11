//! Tiny case-insensitive glob matcher shared by `search --glob` and `revdep`.

/// Case-insensitive glob with `*` (any run) and `?` (any single char),
/// anchored at both ends — `nawa search --glob`.
pub(crate) fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.to_lowercase().chars().collect();
    let t: Vec<char> = text.to_lowercase().chars().collect();
    fn rec(p: &[char], t: &[char]) -> bool {
        match (p.first(), t.first()) {
            (None, None) => true,
            (None, Some(_)) => false,
            (Some('*'), _) => rec(&p[1..], t) || (!t.is_empty() && rec(p, &t[1..])),
            (Some('?'), Some(_)) => rec(&p[1..], &t[1..]),
            (Some(&c), Some(&x)) if c == x => rec(&p[1..], &t[1..]),
            _ => false,
        }
    }
    rec(&p, &t)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_matching() {
        assert!(glob_match("*", "anything"));
        assert!(glob_match("gz*", "gzip"));
        assert!(!glob_match("gz*", "zstd"));
        assert!(!glob_match("z?d", "zstd")); // 4 chars vs 3
        assert!(glob_match("z?d", "zad"));
        assert!(glob_match("*std*", "zstd"));
        assert!(glob_match("GZ*", "gzip"), "case-insensitive");
        assert!(glob_match("zstd", "zstd"));
        assert!(!glob_match("", "zstd"));
        assert!(glob_match("", ""));
    }
}
