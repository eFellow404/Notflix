//! WAL-safe SQLite database backup creation and snapshot pruning for updates.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use chrono::Utc;
use rusqlite::Connection;

use crate::error::Result;
use crate::util::{canonicalize_clean, expand_tilde, utc_now};

use super::{UpdateResult, DEFAULT_BACKUP_KEEP};

const BACKUP_BUSY_TIMEOUT: Duration = Duration::from_secs(10);
const BACKUP_PAGES_PER_STEP: i32 = 100;
const BACKUP_STEP_PAUSE: Duration = Duration::from_millis(10);
const BACKUP_LABEL_MAX_LEN: usize = 24;

fn unique_backup_path(backup_dir: &Path, stem: &str, label: &str) -> PathBuf {
    let stamp = Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    let safe_label_raw: String = label
        .chars()
        .filter(|ch| ch.is_alphanumeric() || *ch == '-' || *ch == '_')
        .take(BACKUP_LABEL_MAX_LEN)
        .collect();
    let safe_label = if safe_label_raw.is_empty() {
        "snapshot"
    } else {
        safe_label_raw.as_str()
    };

    let mut dest = backup_dir.join(format!("{stem}-{safe_label}-{stamp}.db"));
    let mut suffix = 1usize;
    while dest.exists() {
        dest = backup_dir.join(format!("{stem}-{safe_label}-{stamp}-{suffix}.db"));
        suffix += 1;
    }
    dest
}

fn prune_old_backups(backup_dir: &Path, stem: &str, keep: usize) {
    if keep == 0 {
        return;
    }
    let prefix = format!("{stem}-");
    let mut existing: Vec<(SystemTime, String, PathBuf)> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(backup_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if name.starts_with(&prefix) && name.ends_with(".db") {
                let mtime = entry
                    .metadata()
                    .and_then(|m| m.modified())
                    .unwrap_or(SystemTime::UNIX_EPOCH);
                existing.push((mtime, name.to_string(), path));
            }
        }
    }
    existing.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
    for (_, _, stale) in existing.into_iter().skip(keep) {
        let _ = std::fs::remove_file(stale);
    }
}

/// Create a consistent WAL-safe SQLite backup before applying an update.
///
/// Returns `Ok(None)` if `db_path` does not exist yet. Prunes older backups beyond `keep`.
pub fn backup_database(db_path: &Path, label: &str, keep: usize) -> Result<Option<PathBuf>> {
    let expanded = expand_tilde(db_path);
    let resolved = canonicalize_clean(&expanded).unwrap_or(expanded);
    if !resolved.is_file() {
        return Ok(None);
    }

    let backup_dir = resolved
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("backups");
    std::fs::create_dir_all(&backup_dir)?;

    let stem = resolved
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("notflix");
    let dest = unique_backup_path(&backup_dir, stem, label);

    let backup_result: rusqlite::Result<()> = (|| {
        let src = Connection::open(&resolved)?;
        src.busy_timeout(BACKUP_BUSY_TIMEOUT)?;
        let mut dst = Connection::open(&dest)?;
        dst.busy_timeout(BACKUP_BUSY_TIMEOUT)?;
        let backup = rusqlite::backup::Backup::new(&src, &mut dst)?;
        backup.run_to_completion(BACKUP_PAGES_PER_STEP, BACKUP_STEP_PAUSE, None)?;
        Ok(())
    })();

    if let Err(err) = backup_result {
        let _ = std::fs::remove_file(&dest);
        return Err(err.into());
    }

    prune_old_backups(&backup_dir, stem, keep);
    Ok(Some(dest))
}

/// Back up `db_path` (if provided and existing) before applying an update.
///
/// Returns an `"error"` [`UpdateResult`] preserving metadata from `check` if backup fails.
pub(super) fn run_pre_update_backup(
    db_path: Option<&Path>,
    fallback_label: &str,
    check: &UpdateResult,
) -> std::result::Result<Option<String>, Box<UpdateResult>> {
    let Some(db) = db_path else {
        return Ok(None);
    };
    let label = format!(
        "pre-{}",
        check.local_short_sha.as_deref().unwrap_or(fallback_label)
    );
    match backup_database(db, &label, DEFAULT_BACKUP_KEEP) {
        Ok(Some(path)) => Ok(Some(path.to_string_lossy().into_owned())),
        Ok(None) => Ok(None),
        Err(err) => Err(Box::new(UpdateResult::from_check(
            "error",
            format!("Aborted update because database backup failed: {err}"),
            utc_now(),
            check,
        ))),
    }
}
