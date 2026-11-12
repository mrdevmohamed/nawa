//! Crash-recovery journal (legacy undo) and Tx.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

use crate::install::payload::ensure_inside_root;

pub(crate) enum Step {
    /// `path` did not exist and is about to be created.
    Created(PathBuf),
    /// `dest` is about to be moved to `bak` (and replaced, or deleted).
    Replaced { dest: PathBuf, bak: PathBuf },
    /// The package's db directory. `backup` holds a complete copy of its
    /// previous contents (`None`: it did not exist before).
    Db { dir: PathBuf, backup: Option<PathBuf> },
}

/// The logical transaction/step that owns a physical file journal.
///
/// The physical journal is the only record that can restore files after a
/// crash, but it used to be anonymous: recovery could not tell which history
/// step it belonged to. New journals carry this identity in an additive `T`
/// line; old nawa binaries ignore that line and keep recovering the journal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FileJournalId {
    pub(crate) tx: u64,
    pub(crate) step: u32,
}

impl FileJournalId {
    /// Build an identity only when history recording is enabled (`step` is 0
    /// for a no-history operation).
    pub(crate) fn for_step(tx: Option<u64>, step: u32) -> Option<Self> {
        match tx {
            Some(tx) if step > 0 => Some(FileJournalId { tx, step }),
            _ => None,
        }
    }
}

/// Percent-encode a path for one journal field: bytes outside printable
/// ASCII, plus `%`, tab and newline, become `%XX` (file names are arbitrary
/// bytes on Linux).
pub(crate) fn enc(p: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    let mut out = String::new();
    for &b in p.as_os_str().as_bytes() {
        if (0x21..=0x7e).contains(&b) && b != b'%' {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02x}"));
        }
    }
    if out.is_empty() {
        out.push('%');
    }
    out
}

pub(crate) fn dec(s: &str) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStringExt;
    if s == "%" {
        return Some(PathBuf::new());
    }
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let hex = s.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    Some(PathBuf::from(std::ffi::OsString::from_vec(out)))
}

/// Append-only on-disk record of what a transaction has done so far. Each
/// line is one `write(2)`, so it survives the process being killed; it is
/// fsynced every few entries, before the db is touched and at commit, which
/// bounds what a power cut can lose.
pub(crate) struct Journal {
    path: PathBuf,
    file: std::fs::File,
    unsynced: u32,
}

impl Journal {
    pub(crate) fn create(path: PathBuf, bak_root: &Path, id: Option<FileJournalId>) -> Result<Self> {
        use std::io::Write;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .with_context(|| format!("create journal {}", path.display()))?;
        file.write_all(format!("H\t{}\n", enc(bak_root)).as_bytes())?;
        // Additive identity line. Old parsers treat unknown records as no-ops,
        // so a new journal is still recoverable by an older nawa binary.
        if let Some(id) = id {
            file.write_all(format!("T\t{}\t{}\n", id.tx, id.step).as_bytes())?;
        }
        file.sync_data()?;
        if let Some(dir) = path.parent() {
            if let Ok(d) = std::fs::File::open(dir) {
                let _ = d.sync_all();
            }
        }
        Ok(Journal { path, file, unsynced: 0 })
    }

    pub(crate) fn record(&mut self, step: &Step) -> Result<()> {
        use std::io::Write;
        let line = match step {
            Step::Created(p) => format!("C\t{}\n", enc(p)),
            Step::Replaced { dest, bak } => format!("R\t{}\t{}\n", enc(dest), enc(bak)),
            Step::Db { dir, backup } => format!(
                "D\t{}\t{}\n",
                enc(dir),
                backup.as_deref().map(enc).unwrap_or_else(|| "-".to_string())
            ),
        };
        self.file
            .write_all(line.as_bytes())
            .with_context(|| format!("write journal {}", self.path.display()))?;
        self.unsynced += 1;
        // the db is about to change: everything before it must be durable
        if self.unsynced >= 32 || matches!(step, Step::Db { .. }) {
            self.sync()?;
        }
        Ok(())
    }

    pub(crate) fn sync(&mut self) -> Result<()> {
        self.file.sync_data()?;
        self.unsynced = 0;
        Ok(())
    }

    pub(crate) fn mark_committed(&mut self) -> Result<()> {
        use std::io::Write;
        self.file.write_all(b"COMMIT\n")?;
        self.sync()
    }
}

/// Parsed journal: the bak dir, the steps, whether it was committed, and the
/// optional logical transaction/step identity.
pub(crate) struct Parsed {
    pub(crate) bak_root: PathBuf,
    pub(crate) steps: Vec<Step>,
    pub(crate) id: Option<FileJournalId>,
    committed: bool,
}

pub(crate) fn parse_journal(text: &str, fallback_bak: PathBuf) -> Parsed {
    let mut bak_root = fallback_bak;
    let mut steps = Vec::new();
    let mut id = None;
    let mut committed = false;
    for line in text.lines() {
        let f: Vec<&str> = line.split('\t').collect();
        match f.as_slice() {
            ["H", b] => {
                if let Some(p) = dec(b) {
                    bak_root = p;
                }
            }
            ["T", tx, step] => {
                // A torn or future-format identity line is ignored: the step
                // records around it are still a complete description of what
                // was acted on.
                if let (Ok(tx), Ok(step)) = (tx.parse::<u64>(), step.parse::<u32>()) {
                    id = Some(FileJournalId { tx, step });
                }
            }
            ["C", p] => {
                if let Some(p) = dec(p) {
                    steps.push(Step::Created(p));
                }
            }
            ["R", d, b] => {
                if let (Some(dest), Some(bak)) = (dec(d), dec(b)) {
                    steps.push(Step::Replaced { dest, bak });
                }
            }
            ["D", d, b] => {
                if let Some(dir) = dec(d) {
                    let backup = if *b == "-" { None } else { dec(b) };
                    steps.push(Step::Db { dir, backup });
                }
            }
            ["COMMIT"] => committed = true,
            // a torn last line (power cut) or an unknown record is ignored:
            // the steps before it are all that was ever acted on
            _ => {}
        }
    }
    Parsed { bak_root, steps, id, committed }
}

/// Copy a (flat or nested) directory of small files.
pub(crate) fn copy_dir(from: &Path, to: &Path) -> Result<()> {
    std::fs::create_dir_all(to)?;
    for e in std::fs::read_dir(from)? {
        let e = e?;
        let src = e.path();
        let dst = to.join(e.file_name());
        if e.file_type()?.is_dir() {
            copy_dir(&src, &dst)?;
        } else {
            std::fs::copy(&src, &dst)?;
        }
    }
    Ok(())
}

/// Put everything a transaction changed back, newest step first. Idempotent:
/// it can be run again after being interrupted itself, and it never deletes
/// a file whose backup is missing (nothing was moved yet in that case).
pub(crate) fn undo_steps(steps: &[Step], bak_root: &Path) {
    for step in steps.iter().rev() {
        match step {
            Step::Created(p) => {
                let _ = std::fs::remove_file(p);
            }
            Step::Replaced { dest, bak } => {
                if std::fs::symlink_metadata(bak).is_ok() {
                    let _ = std::fs::remove_file(dest);
                    let _ = std::fs::rename(bak, dest);
                }
            }
            Step::Db { dir, backup } => {
                let _ = std::fs::remove_dir_all(dir);
                if let Some(b) = backup {
                    let _ = copy_dir(b, dir);
                }
            }
        }
    }
    let _ = std::fs::remove_dir_all(bak_root);
}

/// Undo journal for `move_tree`: every file that replaces an existing one is
/// first moved to a backup dir, every newly created file is remembered, so a
/// failure half-way through can restore the previous state (all-or-nothing
/// file placement).
///
/// Every step is also written to an on-disk journal *before* it happens, so
/// the same undo works after a SIGKILL or power cut: [`recover`] replays it
/// the next time nawa takes the database lock. Dropping a `Tx` that was
/// neither committed nor rolled back (an early return, a panic) rolls back.
pub(crate) struct Tx {
    pub(crate) root: PathBuf,
    pub(crate) bak_root: PathBuf,
    pub(crate) journal: Option<Journal>,
    pub(crate) steps: Vec<Step>,
    pub(crate) id: Option<FileJournalId>,
    pub(crate) done: bool,
}

pub(crate) fn journal_path(root: &Path, bak_root: &Path) -> PathBuf {
    let name = bak_root.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    root.join(".nawa").join(format!("{name}.journal"))
}

impl Tx {
    pub(crate) fn new(root: &Path, bak_root: PathBuf) -> Self {
        Tx { root: root.to_path_buf(), bak_root, journal: None, steps: Vec::new(), id: None, done: false }
    }

    /// A journal linked to the logical transaction/step that owns it. The
    /// identity is advisory recovery metadata; the physical steps remain the
    /// authority for what must be undone.
    pub(crate) fn new_linked(root: &Path, bak_root: PathBuf, id: FileJournalId) -> Self {
        Tx {
            root: root.to_path_buf(),
            bak_root,
            journal: None,
            steps: Vec::new(),
            id: Some(id),
            done: false,
        }
    }

    /// Journal `step` (creating the journal on first use), then remember it.
    pub(crate) fn log(&mut self, step: Step) -> Result<()> {
        if self.journal.is_none() {
            self.journal = Some(Journal::create(
                journal_path(&self.root, &self.bak_root),
                &self.bak_root,
                self.id,
            )?);
        }
        if let Some(j) = self.journal.as_mut() {
            j.record(&step)?;
        }
        self.steps.push(step);
        Ok(())
    }

    /// Move `from` to `to`, backing up whatever `to` currently is.
    pub(crate) fn place(&mut self, from: &Path, to: &Path) -> Result<()> {
        // the leaf may be a symlink (it is replaced, not followed); its
        // parent directories must stay inside the target root
        if let Some(parent) = to.parent() {
            ensure_inside_root(&self.root, parent)?;
        }
        if std::fs::symlink_metadata(to).is_ok() {
            let rel = to.strip_prefix(&self.root)?;
            let bak = self.bak_root.join(rel);
            self.log(Step::Replaced { dest: to.to_path_buf(), bak: bak.clone() })?;
            if let Some(parent) = bak.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::rename(to, &bak)
                .with_context(|| format!("back up {}", to.display()))?;
        } else {
            self.log(Step::Created(to.to_path_buf()))?;
        }
        std::fs::rename(from, to)
            .with_context(|| format!("move {} -> {}", from.display(), to.display()))
    }

    /// Move an existing path into the backup dir (a reversible delete).
    pub(crate) fn remove(&mut self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            ensure_inside_root(&self.root, parent)?;
        }
        let rel = path.strip_prefix(&self.root)?;
        let bak = self.bak_root.join(rel);
        self.log(Step::Replaced { dest: path.to_path_buf(), bak: bak.clone() })?;
        if let Some(parent) = bak.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::rename(path, &bak).with_context(|| format!("remove {}", path.display()))?;
        Ok(())
    }

    /// Copy the package's db directory aside and journal it. Call this right
    /// before the db is modified: a rollback (or crash recovery) then puts
    /// the db entry back together with the files.
    pub(crate) fn snapshot_db(&mut self, dir: &Path) -> Result<()> {
        let backup = if dir.exists() {
            let name = dir.file_name().map(|n| n.to_os_string()).unwrap_or_default();
            let b = self.bak_root.join(".db").join(name);
            let _ = std::fs::remove_dir_all(&b);
            // complete the copy BEFORE journaling it: a crash mid-copy then
            // leaves no record, and the db has not been touched yet
            copy_dir(dir, &b)?;
            Some(b)
        } else {
            None
        };
        self.log(Step::Db { dir: dir.to_path_buf(), backup })
    }

    /// Restore the pre-install state (best effort, idempotent).
    pub(crate) fn rollback(&mut self) {
        if self.done {
            return;
        }
        undo_steps(&self.steps, &self.bak_root);
        self.steps.clear();
        if let Some(j) = self.journal.take() {
            let _ = std::fs::remove_file(&j.path);
        }
        self.done = true;
    }

    pub(crate) fn commit(mut self) {
        if let Some(j) = self.journal.as_mut() {
            // once COMMIT is durable the transaction counts as done: a crash
            // from here on only has bak leftovers to tidy, never to restore
            let _ = j.mark_committed();
        }
        let _ = std::fs::remove_dir_all(&self.bak_root);
        if let Some(j) = self.journal.take() {
            let _ = std::fs::remove_file(&j.path);
        }
        self.done = true;
    }
}

impl Drop for Tx {
    fn drop(&mut self) {
        self.rollback();
    }
}

/// Repair what an interrupted nawa left behind. Call it while holding the
/// exclusive database lock (nothing else can be mid-transaction then):
///
/// * a journal without `COMMIT` is rolled back (files and db entry),
/// * a journal with `COMMIT` is just cleaned up (the install had finished),
/// * leftover staging directories of dead installs are removed.
///
/// Returns one line per repair for the caller to show.
pub(crate) fn recover(root: &Path) -> Result<Vec<String>> {
    let dir = root.join(".nawa");
    let mut notes = Vec::new();
    let rd = match std::fs::read_dir(&dir) {
        Ok(rd) => rd,
        Err(_) => return Ok(notes),
    };
    let mut journals = Vec::new();
    let mut stale_dirs = Vec::new();
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        let path = e.path();
        if name.ends_with(".journal") && (name.starts_with("bak-") || name.starts_with("rm-")) {
            journals.push((name, path));
        } else if name.starts_with("tmp-") && path.is_dir() {
            stale_dirs.push(path);
        }
    }
    journals.sort();
    for (name, path) in journals {
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        let fallback = dir.join(name.trim_end_matches(".journal"));
        let parsed = parse_journal(&text, fallback);
        let link = parsed
            .id
            .map(|id| format!(", tx {} step {}", id.tx, id.step))
            .unwrap_or_default();
        if parsed.committed {
            let _ = std::fs::remove_dir_all(&parsed.bak_root);
            notes.push(format!("finished cleaning up a completed transaction ({name}{link})"));
        } else if parsed.steps.is_empty() {
            let _ = std::fs::remove_dir_all(&parsed.bak_root);
        } else {
            undo_steps(&parsed.steps, &parsed.bak_root);
            notes.push(format!(
                "rolled back an interrupted transaction ({name}{link}, {} step(s)): \
                 the system is back to its state before it started",
                parsed.steps.len()
            ));
        }
        let _ = std::fs::remove_file(&path);
    }
    for d in stale_dirs {
        let _ = std::fs::remove_dir_all(&d);
    }
    Ok(notes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::tmpdir;

    #[test]
    fn linked_journal_writes_and_parses_identity() {
        let base = tmpdir("journal-identity");
        let path = base.join("bak-1.journal");
        let id = FileJournalId { tx: 42, step: 3 };
        {
            let mut journal = Journal::create(path.clone(), &base.join("bak-1"), Some(id)).unwrap();
            journal
                .record(&Step::Created(base.join("usr/bin/demo")))
                .unwrap();
        }
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("T\t42\t3\n"), "{text:?}");
        let parsed = parse_journal(&text, base.join("fallback"));
        assert_eq!(parsed.id, Some(id));
        assert_eq!(parsed.steps.len(), 1);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn old_journal_without_identity_still_parses() {
        let text = "H\t/root/.nawa/bak-1\nC\t/root/usr/bin/demo\nCOMMIT\n";
        let parsed = parse_journal(text, PathBuf::from("/fallback"));
        assert_eq!(parsed.id, None);
        assert_eq!(parsed.steps.len(), 1);
        assert!(parsed.committed);
    }

    #[test]
    fn torn_or_unknown_identity_line_is_ignored() {
        for line in ["T\tnot-a-number\t1", "T\t42", "T\t42\tx", "T\t"] {
            let text = format!("H\t/root/.nawa/bak-1\n{line}\nC\t/root/usr/bin/demo\n");
            let parsed = parse_journal(&text, PathBuf::from("/fallback"));
            assert_eq!(parsed.id, None, "{line:?}");
            assert_eq!(parsed.steps.len(), 1, "{line:?}");
        }
    }
}
