//! Shared test helpers: one temp dir + db entry builder for all unit tests.

use crate::db::Entry;
use std::path::PathBuf;

/// A fresh, empty temp directory unique per tag + process.
pub(crate) fn tmpdir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("nawa-test-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// A minimal installed entry for db/history tests.
pub(crate) fn entry(name: &str, version: &str, digest: &str) -> Entry {
    Entry {
        name: name.into(),
        version: version.into(),
        rel: 1,
        arch: String::new(),
        reason: "explicit".to_string(),
        run: vec![],
        install_date: 0,
        size: 0,
        artifact_sha256: digest.into(),
    }
}
