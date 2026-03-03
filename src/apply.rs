use crate::diff::{Change, ChangeKind};
use anyhow::{Context, Result};
use std::fs;
use std::path::Path;

/// Apply the changes captured in `upper` to the real filesystem.
///
/// Added and modified files are copied from the overlay upper directory to
/// their corresponding locations on the real filesystem.  Deleted files
/// (represented by overlayfs whiteout entries) are removed from the real
/// filesystem.
///
/// This operation requires write access to the affected paths (typically root).
pub fn apply_changes(changes: &[Change]) -> Result<()> {
    if changes.is_empty() {
        println!("Nothing to apply.");
        return Ok(());
    }

    println!("Applying {} change(s) to the real filesystem…", changes.len());

    for change in changes {
        match change.kind {
            ChangeKind::Added | ChangeKind::Modified => {
                apply_file(change)?;
            }
            ChangeKind::Deleted => {
                delete_file(change)?;
            }
        }
    }

    println!("Done.");
    Ok(())
}

/// Copy an added or modified file from the upper directory to the real filesystem.
fn apply_file(change: &Change) -> Result<()> {
    let src = &change.upper_path;
    let dst = &change.real_path;

    let src_meta = fs::symlink_metadata(src)
        .with_context(|| format!("Cannot stat upper file {}", src.display()))?;

    if src_meta.is_dir() {
        // Create the directory if it does not already exist.
        fs::create_dir_all(dst)
            .with_context(|| format!("Cannot create directory {}", dst.display()))?;
        println!("  + {} [directory]", dst.display());
        return Ok(());
    }

    // Ensure parent directory exists.
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Cannot create parent dir {}", parent.display()))?;
    }

    if src_meta.file_type().is_symlink() {
        let target = fs::read_link(src)
            .with_context(|| format!("Cannot read symlink {}", src.display()))?;
        // Remove existing destination before (re)creating the symlink.
        let _ = fs::remove_file(dst);
        std::os::unix::fs::symlink(&target, dst).with_context(|| {
            format!(
                "Cannot create symlink {} -> {}",
                dst.display(),
                target.display()
            )
        })?;
    } else {
        fs::copy(src, dst)
            .with_context(|| format!("Cannot copy {} → {}", src.display(), dst.display()))?;
        // Preserve permissions.
        fs::set_permissions(dst, src_meta.permissions())
            .with_context(|| format!("Cannot set permissions on {}", dst.display()))?;
    }

    let label = if change.kind == ChangeKind::Added {
        "added"
    } else {
        "modified"
    };
    println!("  ~ {} [{}]", dst.display(), label);
    Ok(())
}

/// Remove a file or directory that was deleted in the sandbox.
fn delete_file(change: &Change) -> Result<()> {
    let path = &change.real_path;
    if !path.exists() && !is_symlink(path) {
        // Already gone; nothing to do.
        return Ok(());
    }

    if path.is_dir() && !is_symlink(path) {
        fs::remove_dir_all(path)
            .with_context(|| format!("Cannot remove directory {}", path.display()))?;
    } else {
        fs::remove_file(path)
            .with_context(|| format!("Cannot remove {}", path.display()))?;
    }
    println!("  - {} [deleted]", path.display());
    Ok(())
}

fn is_symlink(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false)
}
