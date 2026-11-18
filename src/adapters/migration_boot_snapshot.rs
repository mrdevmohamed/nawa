//! Durable, migration-scoped snapshots of the boot/init files that can be
//! changed while preparing an init-system transition.
//!
//! Snapshots live below the selected target root in a per-migration directory.
//! They are deliberately separate from the ordinary install journal: a
//! committed package transaction deletes its undo tree, while boot-recovery
//! data must survive the reboot boundary.
//!
//! Capture publishes the manifest last. A caller must not mutate boot state
//! unless capture returned successfully. Restore validates every blob and
//! path before changing the target, then applies entries idempotently. The
//! caller must verify the restored boot state before clearing the migration
//! gate. Raw BIOS sectors and EFI NVRAM are outside this file snapshot and
//! remain part of the external rescue contract (B5).
//!
//! This adapter is initially testable but intentionally not wired to the
//! production migration handler until the complete BIOS/UEFI rollback
//! contract and external rescue path are accepted.

#![allow(dead_code)]

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io::Read;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};

use crate::adapters::{atomic, unique};
use crate::domain::system::migration::check_id;

const SNAPSHOT_SCHEMA: u32 = 2;
const MAX_SNAPSHOT_ENTRIES: usize = 999;
const FIXED_PATHS: &[&str] = &[
    "boot/grub/grub.cfg",
    "boot/vmlinuz",
    "boot/initramfs.img",
    "boot/efi/EFI/BOOT/BOOTX64.EFI",
    "etc/fstab",
    "usr/bin/init",
    "usr/bin/runit",
    "etc/runit/2",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SnapshotInfo {
    pub(crate) migration_id: String,
    pub(crate) entries: usize,
    pub(crate) bytes: u64,
    pub(crate) directory: PathBuf,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    schema: u32,
    migration_id: String,
    entries: Vec<Entry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    path: String,
    /// absent | file | symlink | directory
    kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    mode: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    blob: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    target: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sha256: Option<String>,
}

#[derive(Debug)]
enum Restorable {
    Absent,
    File { bytes: Vec<u8>, mode: u32 },
    Symlink { target: String },
    Directory { mode: u32 },
}

#[derive(Debug)]
struct ValidatedEntry {
    path: PathBuf,
    state: Restorable,
}

/// Where this migration's retained boot snapshot lives. The id is validated
/// by the same domain rule as the durable migration record.
pub(crate) fn snapshot_dir(root: &Path, migration_id: &str) -> Result<PathBuf> {
    check_id(migration_id).map_err(anyhow::Error::msg)?;
    Ok(crate::adapters::migration_record::migrations_dir(root)
        .join(format!("{migration_id}.boot-recovery")))
}

/// Capture the current, known boot/init recovery set. The fixed paths include
/// the installed GRUB config, stable kernel/initramfs aliases, the init entry,
/// Faseela's fstab and the old runit provider artifacts. The versioned kernel
/// and initramfs targets behind the two stable aliases are captured too.
pub(crate) fn capture(root: &Path, migration_id: &str) -> Result<SnapshotInfo> {
    let root = fs::canonicalize(root)
        .with_context(|| format!("resolve migration snapshot root {}", root.display()))?;
    let directory = snapshot_dir(&root, migration_id)?;

    ensure_real_dir_chain(&root, Path::new("var/lib/nawa/migrations"), 0o700)?;
    match fs::symlink_metadata(&directory) {
        Ok(_) => bail!(
            "boot recovery snapshot {} already exists; refusing to replace retained recovery data",
            directory.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| {
                format!("inspect boot recovery snapshot {}", directory.display())
            })
        }
    }
    fs::create_dir(&directory)
        .with_context(|| format!("create boot recovery snapshot {}", directory.display()))?;
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
    atomic::sync_directory(directory.parent().expect("snapshot directory has parent"))?;
    ensure_real_dir_chain(
        &root,
        Path::new(&format!(
            "var/lib/nawa/migrations/{}.boot-recovery/objects",
            migration_id
        )),
        0o700,
    )?;

    let result = (|| -> Result<SnapshotInfo> {
        let mut paths: Vec<String> = FIXED_PATHS.iter().map(|path| (*path).to_string()).collect();
        for (alias, expected_prefix) in [
            ("boot/vmlinuz", "vmlinuz-"),
            ("boot/initramfs.img", "initramfs-"),
        ] {
            if let Some(target) = versioned_boot_target(&root, Path::new(alias), expected_prefix)? {
                paths.push(target);
            }
        }

        // Preserve the old runit service configuration as part of the boot
        // recovery set. Symlinked directories are captured as leaves, never
        // traversed, so the walk cannot escape the selected target root.
        paths.extend(collect_runit_service_paths(&root)?);
        paths.sort();
        paths.dedup();
        if paths.len() > MAX_SNAPSHOT_ENTRIES {
            bail!(
                "boot recovery snapshot has {} entries; the safe limit is {}",
                paths.len(),
                MAX_SNAPSHOT_ENTRIES
            );
        }

        let mut entries = Vec::with_capacity(paths.len());
        let mut total_bytes = 0u64;
        for (index, relative) in paths.iter().enumerate() {
            let blob_name = format!("entry-{index:03}.bin");
            let (entry, bytes) = capture_entry(&root, relative, &blob_name)?;
            if let Some(bytes) = bytes {
                total_bytes = total_bytes.saturating_add(bytes.len() as u64);
                atomic::write_atomic(
                    &directory.join("objects").join(&blob_name),
                    &bytes,
                    0o600,
                )
                .with_context(|| format!("save boot recovery data for {relative}"))?;
            }
            entries.push(entry);
        }

        let entry_count = entries.len();
        let manifest = Manifest {
            schema: SNAPSHOT_SCHEMA,
            migration_id: migration_id.to_string(),
            entries,
        };
        let bytes = toml::to_string_pretty(&manifest)?.into_bytes();
        atomic::write_atomic(&directory.join("manifest.toml"), &bytes, 0o600)
            .context("publish complete boot recovery manifest")?;
        atomic::sync_directory(&directory)?;

        Ok(SnapshotInfo {
            migration_id: migration_id.to_string(),
            entries: entry_count,
            bytes: total_bytes,
            directory: directory.clone(),
        })
    })();

    if result.is_err() {
        // No caller may mutate boot state unless capture returned success.
        // An incomplete snapshot has no authority and can be retried.
        let _ = fs::remove_dir_all(&directory);
        if let Some(parent) = directory.parent() {
            let _ = atomic::sync_directory(parent);
        }
    }
    result
}

/// Verify the retained manifest and every payload without changing the
/// installed system. Useful before a reboot and from an external rescue ISO.
pub(crate) fn verify(root: &Path, migration_id: &str) -> Result<SnapshotInfo> {
    let root = fs::canonicalize(root)?;
    let directory = snapshot_dir(&root, migration_id)?;
    let validated = load_validated(&root, migration_id, &directory)?;
    let bytes = validated.iter().fold(0u64, |sum, entry| {
        sum.saturating_add(match &entry.state {
            Restorable::File { bytes, .. } => bytes.len() as u64,
            Restorable::Symlink { .. } | Restorable::Directory { .. } | Restorable::Absent => 0,
        })
    });
    Ok(SnapshotInfo {
        migration_id: migration_id.to_string(),
        entries: validated.len(),
        bytes,
        directory,
    })
}

/// Restore the captured boot and runit service state. The entire manifest and
/// all saved file payloads are validated before the first target path is touched.
/// Re-running
/// restore after interruption is safe; already-restored entries are simply
/// written to the same captured state again.
pub(crate) fn restore(root: &Path, migration_id: &str) -> Result<SnapshotInfo> {
    let root = fs::canonicalize(root)?;
    let directory = snapshot_dir(&root, migration_id)?;
    let validated = load_validated(&root, migration_id, &directory)?;
    let bytes = validated.iter().fold(0u64, |sum, entry| {
        sum.saturating_add(match &entry.state {
            Restorable::File { bytes, .. } => bytes.len() as u64,
            Restorable::Symlink { .. } | Restorable::Directory { .. } | Restorable::Absent => 0,
        })
    });

    // Destination-parent checks are completed before applying any entry. A
    // symlinked directory such as /boot or /usr/bin must never redirect restore
    // outside the selected root.
    // Reconciliation runs after the restore writes. Validate the entire live
    // service tree now as well, so a special file or unreadable subtree cannot
    // make reconciliation fail only after boot files have already changed.
    preflight_runit_reconciliation(&root)?;

    for entry in &validated {
        // Preflight is read-only: missing parents are created only after every
        // destination has passed validation.
        let _ = ensure_target_parent(&root, &entry.path, false)?;
        if !matches!(&entry.state, Restorable::Directory { .. }) {
            reject_directory_leaf(&root.join(&entry.path))?;
        } else if let Ok(metadata) = fs::symlink_metadata(root.join(&entry.path)) {
            if metadata.file_type().is_symlink() || (!metadata.is_dir() && !metadata.is_file()) {
                bail!(
                    "refusing to replace unsafe runit service directory entry {}",
                    root.join(&entry.path).display()
                );
            }
        }
    }

    for entry in &validated {
        let create_parent = !matches!(&entry.state, Restorable::Absent);
        let _ = ensure_target_parent(&root, &entry.path, create_parent)?;
        let destination = root.join(&entry.path);
        match &entry.state {
            Restorable::Absent => {
                if fs::symlink_metadata(&destination).is_ok() {
                    fs::remove_file(&destination).with_context(|| {
                        format!(
                            "remove newly-created migration boot artifact {}",
                            destination.display()
                        )
                    })?;
                    atomic::sync_directory(destination.parent().expect("validated path has parent"))?;
                }
            }
            Restorable::File { bytes, mode } => {
                atomic::write_atomic(&destination, bytes, *mode).with_context(|| {
                    format!("restore migration boot artifact {}", destination.display())
                })?;
            }
            Restorable::Symlink { target } => {
                validate_link_target(&entry.path, target)?;
                replace_symlink(&destination, target)?;
            }
            Restorable::Directory { mode } => {
                match fs::symlink_metadata(&destination) {
                    Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
                    Ok(metadata) if metadata.is_file() => {
                        fs::remove_file(&destination)?;
                        fs::create_dir(&destination)?;
                    }
                    Ok(_) => bail!(
                        "refusing to restore runit service directory over {}",
                        destination.display()
                    ),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        fs::create_dir(&destination).with_context(|| {
                            format!("restore runit service directory {}", destination.display())
                        })?;
                    }
                    Err(error) => return Err(error).with_context(|| {
                        format!("inspect runit service directory {}", destination.display())
                    }),
                }
                fs::set_permissions(&destination, fs::Permissions::from_mode(*mode))?;
                atomic::sync_directory(destination.parent().expect("validated path has parent"))?;
            }
        }
    }

    remove_untracked_runit_entries(&root, &validated)?;

    Ok(SnapshotInfo {
        migration_id: migration_id.to_string(),
        entries: validated.len(),
        bytes,
        directory,
    })
}

/// Remove a retained snapshot only after the caller has recorded a successful
/// migration or an operator explicitly authorizes cleanup. This function
/// never participates in capture/restore itself.
pub(crate) fn discard(root: &Path, migration_id: &str) -> Result<()> {
    let root = fs::canonicalize(root)?;
    check_real_dir_chain(&root, Path::new("var/lib/nawa/migrations"))?;
    let directory = snapshot_dir(&root, migration_id)?;
    match fs::symlink_metadata(&directory) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            bail!(
                "boot recovery snapshot {} is not a real directory",
                directory.display()
            )
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error).with_context(|| {
                format!("inspect boot recovery snapshot {}", directory.display())
            })
        }
    }
    fs::remove_dir_all(&directory)
        .with_context(|| format!("discard boot recovery snapshot {}", directory.display()))?;
    if let Some(parent) = directory.parent() {
        atomic::sync_directory(parent)?;
    }
    Ok(())
}

fn capture_entry(
    root: &Path,
    relative: &str,
    blob_name: &str,
) -> Result<(Entry, Option<Vec<u8>>)> {
    let relative_path = Path::new(relative);
    if !allowed_path(relative_path) {
        bail!("boot recovery path {relative:?} is outside the migration allowlist");
    }
    if !ensure_target_parent(root, relative_path, false)? {
        return Ok((absent_entry(relative), None));
    }
    let path = root.join(relative_path);
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok((absent_entry(relative), None));
        }
        Err(error) => {
            return Err(error).with_context(|| format!("inspect boot artifact {}", path.display()))
        }
    };

    if metadata.file_type().is_symlink() {
        let target = fs::read_link(&path)
            .with_context(|| format!("read boot artifact symlink {}", path.display()))?;
        let target = target
            .to_str()
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "boot artifact symlink {} has a non-UTF-8 target; refusing to snapshot",
                    path.display()
                )
            })?
            .to_string();
        validate_link_target(relative_path, &target)?;
        let digest = hash_bytes(target.as_bytes());
        return Ok((
            Entry {
                path: relative.to_string(),
                kind: "symlink".to_string(),
                mode: None,
                blob: None,
                target: Some(target),
                sha256: Some(digest),
            },
            None,
        ));
    }

    if metadata.is_dir() {
        return Ok((
            Entry {
                path: relative.to_string(),
                kind: "directory".to_string(),
                mode: Some(metadata.permissions().mode() & 0o7777),
                blob: None,
                target: None,
                sha256: None,
            },
            None,
        ));
    }
    if !metadata.is_file() {
        bail!(
            "boot recovery artifact {} is not a regular file, directory or symlink",
            path.display()
        );
    }
    let (bytes, mode) = read_regular_file_nofollow(&path)?;
    let digest = hash_bytes(&bytes);
    Ok((
        Entry {
            path: relative.to_string(),
            kind: "file".to_string(),
            mode: Some(mode),
            blob: Some(blob_name.to_string()),
            target: None,
            sha256: Some(digest),
        },
        Some(bytes),
    ))
}

fn absent_entry(relative: &str) -> Entry {
    Entry {
        path: relative.to_string(),
        kind: "absent".to_string(),
        mode: None,
        blob: None,
        target: None,
        sha256: None,
    }
}

fn versioned_boot_target(
    root: &Path,
    alias: &Path,
    expected_prefix: &str,
) -> Result<Option<String>> {
    if !ensure_target_parent(root, alias, false)? {
        return Ok(None);
    }
    let path = root.join(alias);
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("inspect stable boot alias {}", path.display()))
        }
    };
    if !metadata.file_type().is_symlink() {
        return Ok(None);
    }
    let target = fs::read_link(&path)
        .with_context(|| format!("read stable boot alias {}", path.display()))?;
    if target.is_absolute() {
        bail!(
            "stable boot alias {} is absolute; refusing to infer a retained versioned artifact",
            path.display()
        );
    }
    let mut components = target.components();
    let Some(Component::Normal(name)) = components.next() else {
        bail!(
            "stable boot alias {} has an unsafe target {}",
            path.display(),
            target.display()
        );
    };
    if components.next().is_some() {
        bail!(
            "stable boot alias {} does not point to a single /boot file",
            path.display()
        );
    }
    let name = name
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("stable boot alias has a non-UTF-8 target"))?;
    let matches = if expected_prefix == "initramfs-" {
        name.starts_with(expected_prefix) && name.ends_with(".img")
    } else {
        name.starts_with(expected_prefix)
    };
    if !matches {
        bail!(
            "stable boot alias {} has unexpected target {name:?}",
            path.display()
        );
    }
    Ok(Some(format!("boot/{name}")))
}

fn load_validated(
    root: &Path,
    migration_id: &str,
    directory: &Path,
) -> Result<Vec<ValidatedEntry>> {
    check_real_dir_chain(root, Path::new("var/lib/nawa/migrations"))?;
    let metadata = fs::symlink_metadata(directory)
        .with_context(|| format!("inspect boot recovery snapshot {}", directory.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!(
            "boot recovery snapshot {} is not a real directory",
            directory.display()
        );
    }
    let objects = directory.join("objects");
    let object_meta = fs::symlink_metadata(&objects)
        .with_context(|| format!("inspect boot recovery objects {}", objects.display()))?;
    if object_meta.file_type().is_symlink() || !object_meta.is_dir() {
        bail!("boot recovery objects {} is not a real directory", objects.display());
    }

    let manifest_path = directory.join("manifest.toml");
    let (manifest_bytes, _) = read_regular_file_nofollow(&manifest_path)?;
    let manifest_text = std::str::from_utf8(&manifest_bytes)
        .context("boot recovery manifest is not UTF-8")?;
    let manifest: Manifest =
        toml::from_str(manifest_text).context("boot recovery manifest is not valid TOML")?;
    if manifest.schema != SNAPSHOT_SCHEMA {
        bail!(
            "unsupported boot recovery snapshot schema {} (this nawa understands schema {SNAPSHOT_SCHEMA})",
            manifest.schema
        );
    }
    if manifest.migration_id != migration_id {
        bail!(
            "boot recovery snapshot id {:?} does not match requested migration {migration_id:?}",
            manifest.migration_id
        );
    }
    if manifest.entries.len() > MAX_SNAPSHOT_ENTRIES {
        bail!(
            "boot recovery manifest has {} entries; the safe limit is {}",
            manifest.entries.len(),
            MAX_SNAPSHOT_ENTRIES
        );
    }

    let mut validated = Vec::with_capacity(manifest.entries.len());
    let mut seen = HashSet::new();
    let mut service_directories = HashSet::new();
    for entry in manifest.entries {
        let relative = PathBuf::from(&entry.path);
        if !allowed_path(&relative) {
            bail!(
                "boot recovery manifest contains non-allowlisted path {:?}",
                entry.path
            );
        }
        if !seen.insert(entry.path.clone()) {
            bail!(
                "boot recovery manifest contains duplicate path {:?}",
                entry.path
            );
        }
        let state = match entry.kind.as_str() {
            "absent" => {
                if entry.mode.is_some()
                    || entry.blob.is_some()
                    || entry.target.is_some()
                    || entry.sha256.is_some()
                {
                    bail!(
                        "absent boot recovery entry {} has unexpected payload fields",
                        entry.path
                    );
                }
                Restorable::Absent
            }
            "file" => {
                let mode = entry.mode.ok_or_else(|| {
                    anyhow::anyhow!("file entry {} has no mode", entry.path)
                })?;
                if mode > 0o7777 || entry.target.is_some() {
                    bail!(
                        "file boot recovery entry {} has invalid metadata",
                        entry.path
                    );
                }
                let blob = entry.blob.as_deref().ok_or_else(|| {
                    anyhow::anyhow!("file entry {} has no blob", entry.path)
                })?;
                if !valid_blob_name(blob) {
                    bail!(
                        "boot recovery entry {} has an unsafe blob name",
                        entry.path
                    );
                }
                let expected = entry.sha256.as_deref().ok_or_else(|| {
                    anyhow::anyhow!("file entry {} has no sha256", entry.path)
                })?;
                let blob_path = objects.join(blob);
                let (bytes, _) = read_regular_file_nofollow(&blob_path)?;
                verify_hash(&bytes, expected, &entry.path)?;
                Restorable::File { bytes, mode }
            }
            "directory" => {
                let mode = entry.mode.ok_or_else(|| {
                    anyhow::anyhow!("directory entry {} has no mode", entry.path)
                })?;
                if mode > 0o7777
                    || entry.blob.is_some()
                    || entry.target.is_some()
                    || entry.sha256.is_some()
                {
                    bail!(
                        "directory boot recovery entry {} has invalid metadata",
                        entry.path
                    );
                }
                if entry.path == "etc/runit/runsvdir/default"
                    || entry.path.starts_with("etc/runit/runsvdir/default/")
                {
                    service_directories.insert(entry.path.clone());
                }
                Restorable::Directory { mode }
            }
            "symlink" => {
                if entry.mode.is_some() || entry.blob.is_some() {
                    bail!(
                        "symlink boot recovery entry {} has invalid metadata",
                        entry.path
                    );
                }
                let target = entry.target.as_deref().ok_or_else(|| {
                    anyhow::anyhow!("symlink entry {} has no target", entry.path)
                })?;
                validate_link_target(&relative, target)?;
                let expected = entry.sha256.as_deref().ok_or_else(|| {
                    anyhow::anyhow!("symlink entry {} has no sha256", entry.path)
                })?;
                verify_hash(target.as_bytes(), expected, &entry.path)?;
                Restorable::Symlink {
                    target: target.to_string(),
                }
            }
            other => bail!(
                "boot recovery entry {} has unknown kind {other:?}",
                entry.path
            ),
        };
        validated.push(ValidatedEntry {
            path: relative,
            state,
        });
    }

    for required in FIXED_PATHS {
        if !seen.contains(*required) {
            bail!("boot recovery manifest is missing required entry {required}");
        }
    }
    if !seen.contains("etc/runit/runsvdir/default") {
        bail!("boot recovery manifest is missing the required runit service tree");
    }
    for entry in &validated {
        let value = entry.path.to_string_lossy();
        if value.starts_with("etc/runit/runsvdir/default/")
        {
            let mut parent = entry.path.parent();
            while let Some(path) = parent {
                let parent_name = path.to_string_lossy();
                if !parent_name.starts_with("etc/runit/runsvdir/default") {
                    break;
                }
                if !service_directories.contains(parent_name.as_ref()) {
                    bail!(
                        "boot recovery manifest is missing runit service directory {parent_name:?}"
                    );
                }
                if parent_name == "etc/runit/runsvdir/default" {
                    break;
                }
                parent = path.parent();
            }
        }
    }
    Ok(validated)
}

fn valid_blob_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    bytes.len() == 13
        && bytes.starts_with(b"entry-")
        && bytes.ends_with(b".bin")
        && bytes[6..9].iter().all(u8::is_ascii_digit)
}

/// Validate the current service tree before restore mutates any destination.
/// A missing tree is recoverable from the manifest; an existing tree must be
/// traversable and contain only supported leaf types.
fn preflight_runit_reconciliation(root: &Path) -> Result<()> {
    let service_root = root.join("etc/runit/runsvdir/default");
    match fs::symlink_metadata(&service_root) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            bail!(
                "required runit service directory {} is not a real directory",
                service_root.display()
            )
        }
        Ok(_) => {
            let _ = collect_runit_service_paths(root)?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| {
                format!("inspect required runit service directory {}", service_root.display())
            })
        }
    }
    Ok(())
}

/// Remove entries created under the runit service tree after capture. The
/// manifest has already been fully validated and must contain every ancestor
/// directory, so an unrecorded directory cannot contain a path we intend to
/// preserve. Symlink leaves are removed as links and are never traversed.
fn remove_untracked_runit_entries(root: &Path, validated: &[ValidatedEntry]) -> Result<()> {
    const SERVICE_ROOT: &str = "etc/runit/runsvdir/default";
    let expected: HashSet<String> = validated
        .iter()
        .map(|entry| entry.path.to_string_lossy().into_owned())
        .filter(|path| path == SERVICE_ROOT || path.starts_with("etc/runit/runsvdir/default/"))
        .collect();

    let mut extras: Vec<PathBuf> = collect_runit_service_paths(root)?
        .into_iter()
        .filter(|path| !expected.contains(path))
        .map(PathBuf::from)
        .collect();
    extras.sort_by_key(|path| std::cmp::Reverse(path.components().count()));

    for relative in extras {
        let destination = root.join(&relative);
        let metadata = fs::symlink_metadata(&destination)
            .with_context(|| format!("inspect untracked runit service entry {}", destination.display()))?;
        if metadata.file_type().is_symlink() || metadata.is_file() {
            fs::remove_file(&destination).with_context(|| {
                format!("remove untracked runit service entry {}", destination.display())
            })?;
        } else if metadata.is_dir() {
            fs::remove_dir(&destination).with_context(|| {
                format!("remove untracked runit service directory {}", destination.display())
            })?;
        } else {
            bail!(
                "untracked runit service entry {} is not a regular file, directory or symlink",
                destination.display()
            );
        }
        if let Some(parent) = destination.parent() {
            atomic::sync_directory(parent)?;
        }
    }
    Ok(())
}

/// Return every regular-file or symlink leaf below the required runit service
/// directory. Real directories are traversed; symlinks are recorded, not followed.
fn collect_runit_service_paths(root: &Path) -> Result<Vec<String>> {
    fn walk(root: &Path, directory: &Path, output: &mut Vec<String>) -> Result<()> {
        let mut children = fs::read_dir(directory)
            .with_context(|| format!("read runit service directory {}", directory.display()))?
            .collect::<std::io::Result<Vec<_>>>()?;
        children.sort_by_key(|entry| entry.file_name());

        for child in children {
            let path = child.path();
            let metadata = fs::symlink_metadata(&path)
                .with_context(|| format!("inspect runit service entry {}", path.display()))?;
            if metadata.file_type().is_symlink() || metadata.is_file() {
                let relative = path
                    .strip_prefix(root)
                    .context("runit service entry escaped the selected target root")?;
                let value = relative
                    .to_str()
                    .ok_or_else(|| anyhow::anyhow!("runit service path is not UTF-8"))?
                    .to_string();
                if !allowed_path(Path::new(&value)) {
                    bail!("runit service entry {value:?} is outside the snapshot allowlist");
                }
                output.push(value);
                if output.len() > MAX_SNAPSHOT_ENTRIES {
                    bail!(
                        "runit service tree exceeds the safe limit of {} entries",
                        MAX_SNAPSHOT_ENTRIES
                    );
                }
            } else if metadata.is_dir() {
                let relative = path
                    .strip_prefix(root)
                    .context("runit service directory escaped the selected target root")?;
                let value = relative
                    .to_str()
                    .ok_or_else(|| anyhow::anyhow!("runit service path is not UTF-8"))?
                    .to_string();
                if !allowed_path(Path::new(&value)) {
                    bail!("runit service directory {value:?} is outside the snapshot allowlist");
                }
                output.push(value);
                if output.len() > MAX_SNAPSHOT_ENTRIES {
                    bail!(
                        "runit service tree exceeds the safe limit of {} entries",
                        MAX_SNAPSHOT_ENTRIES
                    );
                }
                walk(root, &path, output)?;
            } else {
                bail!(
                    "runit service entry {} is not a regular file, directory or symlink",
                    path.display()
                );
            }
        }
        Ok(())
    }

    let relative = Path::new("etc/runit/runsvdir/default");
    let directory = root.join(relative);
    let metadata = fs::symlink_metadata(&directory).with_context(|| {
        format!("inspect required runit service directory {}", directory.display())
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!(
            "required runit service directory {} is not a real directory",
            directory.display()
        );
    }

    let mut paths = Vec::new();
    paths.push(relative.to_string_lossy().into_owned());
    walk(root, &directory, &mut paths)?;
    Ok(paths)
}

fn allowed_path(path: &Path) -> bool {
    if !path.is_relative()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return false;
    }
    let value = path.to_string_lossy();
    FIXED_PATHS.iter().any(|allowed| value == *allowed)
        || value == "etc/runit/runsvdir/default"
        || value.starts_with("etc/runit/runsvdir/default/")
        || (value.starts_with("boot/vmlinuz-") && !value[5..].contains('/'))
        || (value.starts_with("boot/initramfs-")
            && value.ends_with(".img")
            && !value[5..].contains('/'))
}

fn ensure_target_parent(root: &Path, relative: &Path, create_missing: bool) -> Result<bool> {
    if !allowed_path(relative) {
        bail!(
            "boot recovery path {} is outside the migration allowlist",
            relative.display()
        );
    }
    let parent = relative.parent().unwrap_or_else(|| Path::new(""));
    let mut current = root.to_path_buf();
    for component in parent.components() {
        let Component::Normal(name) = component else {
            bail!(
                "boot recovery path {} contains an unsafe parent",
                relative.display()
            );
        };
        current.push(name);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                bail!(
                    "boot recovery parent {} is a symlink; refusing to follow it",
                    current.display()
                )
            }
            Ok(metadata) if !metadata.is_dir() => {
                bail!("boot recovery parent {} is not a directory", current.display())
            }
            Ok(_) => {}
            Err(error)
                if error.kind() == std::io::ErrorKind::NotFound && create_missing =>
            {
                fs::create_dir(&current)
                    .with_context(|| format!("create boot recovery parent {}", current.display()))?;
                fs::set_permissions(&current, fs::Permissions::from_mode(0o755))?;
                if let Some(parent) = current.parent() {
                    atomic::sync_directory(parent)?;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("inspect boot recovery parent {}", current.display())
                })
            }
        }
    }
    Ok(true)
}

fn ensure_real_dir_chain(root: &Path, relative: &Path, final_mode: u32) -> Result<()> {
    let mut current = root.to_path_buf();
    let components: Vec<_> = relative.components().collect();
    if components.is_empty() {
        bail!("directory path is empty");
    }
    for (index, component) in components.iter().enumerate() {
        let Component::Normal(name) = component else {
            bail!("unsafe state directory path {}", relative.display());
        };
        current.push(name);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                bail!("state directory {} is not a real directory", current.display())
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                fs::create_dir(&current).with_context(|| {
                    format!("create migration state directory {}", current.display())
                })?;
                let mode = if index + 1 == components.len() {
                    final_mode
                } else {
                    0o755
                };
                fs::set_permissions(&current, fs::Permissions::from_mode(mode))?;
                if let Some(parent) = current.parent() {
                    atomic::sync_directory(parent)?;
                }
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("inspect migration state directory {}", current.display())
                })
            }
        }
    }
    fs::set_permissions(&current, fs::Permissions::from_mode(final_mode))?;
    Ok(())
}

fn check_real_dir_chain(root: &Path, relative: &Path) -> Result<()> {
    let mut current = root.to_path_buf();
    for component in relative.components() {
        let Component::Normal(name) = component else {
            bail!("unsafe state directory path {}", relative.display());
        };
        current.push(name);
        let metadata = fs::symlink_metadata(&current)
            .with_context(|| format!("inspect migration state directory {}", current.display()))?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            bail!("migration state directory {} is not a real directory", current.display());
        }
    }
    Ok(())
}

fn reject_directory_leaf(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
            bail!("refusing to replace boot recovery directory {}", path.display())
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error)
            .with_context(|| format!("inspect boot recovery destination {}", path.display())),
    }
}

fn read_regular_file_nofollow(path: &Path) -> Result<(Vec<u8>, u32)> {
    let mut options = OpenOptions::new();
    options
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    let mut file = options.open(path).with_context(|| {
        format!(
            "open boot recovery file {} without following symlinks",
            path.display()
        )
    })?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        bail!("boot recovery payload {} is not a regular file", path.display());
    }
    let mode = metadata.permissions().mode() & 0o7777;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok((bytes, mode))
}

fn validate_link_target(relative: &Path, target: &str) -> Result<()> {
    let parent = relative.parent().unwrap_or_else(|| Path::new(""));
    let candidate = if target.starts_with('/') {
        PathBuf::from(target.trim_start_matches('/'))
    } else {
        parent.join(target)
    };
    let mut normalized = Vec::new();
    for component in candidate.components() {
        match component {
            Component::Normal(name) => normalized.push(name.to_os_string()),
            Component::CurDir => {}
            Component::ParentDir => {
                if normalized.pop().is_none() {
                    bail!(
                        "boot recovery symlink {} points outside the selected root",
                        relative.display()
                    );
                }
            }
            Component::RootDir | Component::Prefix(_) => {
                bail!(
                    "boot recovery symlink {} has an invalid target",
                    relative.display()
                );
            }
        }
    }
    if normalized.is_empty() {
        bail!(
            "boot recovery symlink {} points to the root directory",
            relative.display()
        );
    }
    Ok(())
}

fn replace_symlink(destination: &Path, target: &str) -> Result<()> {
    let parent = destination
        .parent()
        .ok_or_else(|| anyhow::anyhow!("symlink destination has no parent"))?;
    let name = destination
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("symlink destination has no name"))?;
    for _ in 0..32 {
        let temporary = parent.join(format!(
            ".{}.nawa-restore-{}-{}",
            name.to_string_lossy(),
            std::process::id(),
            unique::next()
        ));
        match std::os::unix::fs::symlink(target, &temporary) {
            Ok(()) => {
                if let Err(error) = fs::rename(&temporary, destination) {
                    let _ = fs::remove_file(&temporary);
                    return Err(error).with_context(|| {
                        format!("restore boot symlink {}", destination.display())
                    });
                }
                atomic::sync_directory(parent)?;
                return Ok(());
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("create temporary symlink {}", temporary.display())
                })
            }
        }
    }
    bail!(
        "could not allocate a temporary symlink beside {}",
        destination.display()
    )
}

fn hash_bytes(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn verify_hash(bytes: &[u8], expected: &str, path: &str) -> Result<()> {
    if expected.len() != 64 || !expected.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!(
            "boot recovery entry {path} has an invalid sha256 digest"
        );
    }
    let actual = hash_bytes(bytes);
    if actual != expected.to_ascii_lowercase() {
        bail!(
            "boot recovery payload hash mismatch for {path}: expected {expected}, got {actual}"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn fixture(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "nawa-boot-recovery-{tag}-{}-{}",
            std::process::id(),
            unique::next()
        ));
        fs::create_dir_all(root.join("boot/grub")).unwrap();
        fs::create_dir_all(root.join("usr/bin")).unwrap();
        fs::create_dir_all(root.join("etc/runit/runsvdir/default/networking")).unwrap();
        fs::create_dir_all(root.join("etc/runit/runsvdir/default/agetty-tty1")).unwrap();
        fs::create_dir(root.join("etc/runit/runsvdir/default/empty-service")).unwrap();
        fs::set_permissions(
            root.join("etc/runit/runsvdir/default/empty-service"),
            fs::Permissions::from_mode(0o750),
        )
        .unwrap();
        fs::write(
            root.join("etc/runit/runsvdir/default/networking/run"),
            b"#!/bin/sh\nexec dhcpcd\n",
        )
        .unwrap();
        fs::write(
            root.join("etc/runit/runsvdir/default/agetty-tty1/run"),
            b"#!/bin/sh\nexec getty 38400 tty1\n",
        )
        .unwrap();
        symlink(
            "../agetty-tty1",
            root.join("etc/runit/runsvdir/default/networking/agetty-link"),
        )
        .unwrap();
        fs::write(root.join("boot/grub/grub.cfg"), b"old grub config\n").unwrap();
        fs::write(root.join("boot/vmlinuz-6.10.5"), b"old kernel bytes").unwrap();
        fs::write(
            root.join("boot/initramfs-6.10.5.img"),
            b"old initramfs bytes",
        )
        .unwrap();
        symlink("vmlinuz-6.10.5", root.join("boot/vmlinuz")).unwrap();
        symlink("initramfs-6.10.5.img", root.join("boot/initramfs.img")).unwrap();
        fs::write(
            root.join("etc/fstab"),
            b"UUID=old / ext4 defaults 0 1\n",
        )
        .unwrap();
        symlink("/usr/bin/runit", root.join("usr/bin/init")).unwrap();
        fs::write(root.join("usr/bin/runit"), b"old runit executable").unwrap();
        fs::write(
            root.join("etc/runit/2"),
            b"#!/bin/sh\nexec runsvdir -P /etc/runit/runsvdir/default\n",
        )
        .unwrap();
        root
    }

    fn make_snapshot(root: &Path) -> SnapshotInfo {
        capture(root, "faseela-runit-to-systemd-v1").unwrap()
    }

    #[test]
    fn blob_name_validation_rejects_non_ascii_without_panicking() {
        for name in ["entry-000.bin", "entry-💥.bin", "entry-00é.bin", "entry-999.txt"] {
            let result = std::panic::catch_unwind(|| valid_blob_name(name));
            assert!(result.is_ok(), "validator panicked for {name:?}");
            assert_eq!(result.unwrap(), name == "entry-000.bin");
        }
    }

    #[test]
    fn capture_retains_boot_alias_targets_and_old_provider_files() {
        let root = fixture("capture");
        let info = make_snapshot(&root);
        assert!(info.entries >= FIXED_PATHS.len() + 2);
        assert!(info.bytes > 0);
        assert!(info.directory.join("manifest.toml").is_file());
        assert!(info.directory.join("objects").is_dir());
        let permissions = fs::metadata(&info.directory)
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(permissions, 0o700);
    }

    #[test]
    fn snapshot_captures_nested_runit_service_tree_and_restores_it() {
        let root = fixture("runit-tree");
        let info = make_snapshot(&root);
        let service_run = root.join("etc/runit/runsvdir/default/networking/run");
        let agetty_run = root.join("etc/runit/runsvdir/default/agetty-tty1/run");

        fs::remove_file(&service_run).unwrap();
        fs::write(&agetty_run, b"changed service definition\n").unwrap();
        fs::remove_dir(root.join("etc/runit/runsvdir/default/empty-service")).unwrap();
        fs::remove_file(
            root.join("etc/runit/runsvdir/default/networking/agetty-link"),
        )
        .unwrap();
        fs::create_dir_all(root.join("etc/runit/runsvdir/default/new-service")).unwrap();
        fs::write(
            root.join("etc/runit/runsvdir/default/new-service/run"),
            b"new service after snapshot\n",
        )
        .unwrap();

        restore(&root, "faseela-runit-to-systemd-v1").unwrap();
        assert_eq!(
            fs::read(&service_run).unwrap(),
            b"#!/bin/sh\nexec dhcpcd\n"
        );
        assert_eq!(
            fs::read(&agetty_run).unwrap(),
            b"#!/bin/sh\nexec getty 38400 tty1\n"
        );
        let empty_service = root.join("etc/runit/runsvdir/default/empty-service");
        assert!(empty_service.is_dir());
        assert_eq!(
            fs::metadata(empty_service).unwrap().permissions().mode() & 0o777,
            0o750
        );
        assert_eq!(
            fs::read_link(root.join("etc/runit/runsvdir/default/networking/agetty-link")).unwrap(),
            PathBuf::from("../agetty-tty1")
        );
        assert!(!root.join("etc/runit/runsvdir/default/new-service").exists());
        assert!(info.entries >= FIXED_PATHS.len() + 8);
        let manifest: Manifest = toml::from_str(
            &fs::read_to_string(info.directory.join("manifest.toml")).unwrap(),
        )
        .unwrap();
        assert!(manifest.entries.iter().any(|entry| {
            entry.path == "etc/runit/runsvdir/default" && entry.kind == "directory"
        }));
        assert!(!manifest.entries.iter().any(|entry| {
            entry.path.contains("agetty-link/")
        }), "the snapshot walk must not follow service symlink directories");
    }

    #[test]
    fn restore_recovers_grub_alias_targets_init_and_fstab_idempotently() {
        let root = fixture("restore");
        make_snapshot(&root);

        fs::write(
            root.join("boot/grub/grub.cfg"),
            b"new systemd grub config\n",
        )
        .unwrap();
        fs::write(root.join("boot/vmlinuz-6.10.5"), b"new kernel bytes").unwrap();
        fs::write(
            root.join("boot/initramfs-6.10.5.img"),
            b"new initramfs bytes",
        )
        .unwrap();
        fs::remove_file(root.join("boot/vmlinuz")).unwrap();
        symlink("vmlinuz-6.11.0", root.join("boot/vmlinuz")).unwrap();
        fs::remove_file(root.join("usr/bin/init")).unwrap();
        symlink("../lib/systemd/systemd", root.join("usr/bin/init")).unwrap();
        fs::write(
            root.join("etc/fstab"),
            b"UUID=new / ext4 defaults 0 1\n",
        )
        .unwrap();

        restore(&root, "faseela-runit-to-systemd-v1").unwrap();
        assert_eq!(
            fs::read(root.join("boot/grub/grub.cfg")).unwrap(),
            b"old grub config\n"
        );
        assert_eq!(
            fs::read(root.join("boot/vmlinuz-6.10.5")).unwrap(),
            b"old kernel bytes"
        );
        assert_eq!(
            fs::read(root.join("boot/initramfs-6.10.5.img")).unwrap(),
            b"old initramfs bytes"
        );
        assert_eq!(
            fs::read_link(root.join("boot/vmlinuz")).unwrap(),
            PathBuf::from("vmlinuz-6.10.5")
        );
        assert_eq!(
            fs::read_link(root.join("boot/initramfs.img")).unwrap(),
            PathBuf::from("initramfs-6.10.5.img")
        );
        assert_eq!(
            fs::read_link(root.join("usr/bin/init")).unwrap(),
            PathBuf::from("/usr/bin/runit")
        );
        assert_eq!(
            fs::read(root.join("etc/fstab")).unwrap(),
            b"UUID=old / ext4 defaults 0 1\n"
        );

        restore(&root, "faseela-runit-to-systemd-v1").unwrap();
        verify(&root, "faseela-runit-to-systemd-v1").unwrap();
    }

    #[test]
    fn restore_removes_files_that_did_not_exist_before_capture() {
        let root = fixture("absent");
        let info = make_snapshot(&root);
        fs::create_dir_all(root.join("boot/efi/EFI/BOOT")).unwrap();
        fs::write(
            root.join("boot/efi/EFI/BOOT/BOOTX64.EFI"),
            b"new EFI loader",
        )
        .unwrap();
        assert!(info.entries >= FIXED_PATHS.len());
        restore(&root, "faseela-runit-to-systemd-v1").unwrap();
        assert!(!root
            .join("boot/efi/EFI/BOOT/BOOTX64.EFI")
            .exists());
    }

    #[test]
    fn failed_destination_preflight_does_not_create_missing_parent_directories() {
        let root = fixture("preflight-no-mutation");
        make_snapshot(&root);

        fs::remove_file(root.join("boot/grub/grub.cfg")).unwrap();
        fs::remove_dir(root.join("boot/grub")).unwrap();
        let outside = root.with_extension("outside");
        fs::create_dir_all(&outside).unwrap();
        fs::remove_dir_all(root.join("usr/bin")).unwrap();
        symlink(&outside, root.join("usr/bin")).unwrap();

        assert!(restore(&root, "faseela-runit-to-systemd-v1").is_err());
        assert!(!root.join("boot/grub").exists());

        let _ = fs::remove_file(root.join("usr/bin"));
        let _ = fs::remove_dir_all(outside);
    }

    #[test]
    fn unsupported_untracked_service_entry_is_rejected_before_restore_mutates_boot_files() {
        use std::os::unix::net::UnixListener;

        let root = fixture("special-untracked-service-entry");
        make_snapshot(&root);
        fs::write(root.join("boot/grub/grub.cfg"), b"new config\n").unwrap();
        let socket_path = root.join("etc/runit/runsvdir/default/unexpected.sock");
        let _socket = UnixListener::bind(&socket_path).unwrap();

        assert!(restore(&root, "faseela-runit-to-systemd-v1").is_err());
        assert_eq!(
            fs::read(root.join("boot/grub/grub.cfg")).unwrap(),
            b"new config\n",
            "preflight must reject the unsupported service entry before restoring any boot path"
        );
    }

    #[test]
    fn corrupted_snapshot_payload_is_rejected_before_any_target_is_changed() {
        let root = fixture("corrupt");
        let info = make_snapshot(&root);
        fs::write(root.join("boot/grub/grub.cfg"), b"new config\n").unwrap();

        let manifest_text = fs::read_to_string(info.directory.join("manifest.toml")).unwrap();
        let manifest: Manifest = toml::from_str(&manifest_text).unwrap();
        let blob = manifest
            .entries
            .iter()
            .find_map(|entry| entry.blob.as_deref())
            .unwrap();
        fs::write(info.directory.join("objects").join(blob), b"tampered").unwrap();

        assert!(restore(&root, "faseela-runit-to-systemd-v1").is_err());
        assert_eq!(
            fs::read(root.join("boot/grub/grub.cfg")).unwrap(),
            b"new config\n"
        );
    }

    #[test]
    fn symlinked_boot_parent_is_refused_during_capture() {
        let root = fixture("unsafe-parent");
        let outside = root.with_extension("outside");
        fs::create_dir_all(&outside).unwrap();
        fs::remove_dir_all(root.join("boot")).unwrap();
        symlink(&outside, root.join("boot")).unwrap();
        assert!(capture(&root, "faseela-runit-to-systemd-v1").is_err());
        let _ = fs::remove_file(root.join("boot"));
        let _ = fs::remove_dir_all(outside);
    }

    #[test]
    fn capture_rejects_a_runit_service_symlink_that_escapes_the_target_root() {
        let root = fixture("unsafe-runit-link");
        symlink(
            "../../../../../../etc/shadow",
            root.join("etc/runit/runsvdir/default/networking/unsafe"),
        )
        .unwrap();
        assert!(capture(&root, "faseela-runit-to-systemd-v1").is_err());
    }

    #[test]
    fn snapshot_identity_cannot_be_reused_or_traversed() {
        let root = fixture("identity");
        make_snapshot(&root);
        assert!(capture(&root, "faseela-runit-to-systemd-v1").is_err());
        assert!(snapshot_dir(&root, "../other").is_err());
    }
}
