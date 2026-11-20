use std::fs::{DirBuilder, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::Path;

/// Seed state with trusted permissions under any ordinary test-runner umask.
pub fn create_state_dirs(path: &Path) {
    DirBuilder::new()
        .recursive(true)
        .mode(0o755)
        .create(path)
        .unwrap();
}

pub fn write_record(path: &Path, bytes: impl AsRef<[u8]>) {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .unwrap()
        .write_all(bytes.as_ref())
        .unwrap();
}
