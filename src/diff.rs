use anyhow::Result;
use std::path::Path;
use walkdir::WalkDir;

/// The kind of change detected in the sandbox upper directory.
#[derive(Debug, PartialEq)]
pub enum ChangeKind {
    /// File or directory was created inside the sandbox.
    Added,
    /// File was modified inside the sandbox.
    Modified,
    /// File or directory was deleted inside the sandbox.
    Deleted,
}

/// A single filesystem change recorded in the overlay upper directory.
#[derive(Debug)]
pub struct Change {
    /// The absolute path on the real filesystem that was affected.
    pub real_path: std::path::PathBuf,
    /// The corresponding path inside the upper directory (source for apply).
    pub upper_path: std::path::PathBuf,
    pub kind: ChangeKind,
}

/// Collect all changes recorded in the overlay upper directory.
///
/// Overlayfs represents deletions as whiteout files: character special devices
/// with device number 0,0.  Everything else is either a new file (added) or a
/// file that was copied up from the lower layer before being modified.
pub fn collect_changes(upper: &Path) -> Result<Vec<Change>> {
    let mut changes = Vec::new();

    for entry in WalkDir::new(upper).min_depth(1) {
        let entry = entry?;
        let path = entry.path();

        let rel = path.strip_prefix(upper)?;
        let real_path = Path::new("/").join(rel);

        let metadata = entry.metadata()?;

        if is_whiteout(&metadata) {
            // This whiteout represents a deletion of the corresponding real file.
            changes.push(Change {
                real_path,
                upper_path: path.to_path_buf(),
                kind: ChangeKind::Deleted,
            });
        } else if metadata.is_dir() {
            // Directories in the upper layer are created automatically by overlayfs
            // during copy-up; skip them unless they are opaque (newly created dirs).
            if is_opaque_dir(path) && !real_path.exists() {
                changes.push(Change {
                    real_path,
                    upper_path: path.to_path_buf(),
                    kind: ChangeKind::Added,
                });
            }
        } else {
            // Regular file or symlink: added if it doesn't exist on the real fs,
            // modified otherwise.
            let kind = if real_path.exists() {
                ChangeKind::Modified
            } else {
                ChangeKind::Added
            };
            changes.push(Change {
                real_path,
                upper_path: path.to_path_buf(),
                kind,
            });
        }
    }

    changes.sort_by(|a, b| a.real_path.cmp(&b.real_path));
    Ok(changes)
}

/// Return true if `metadata` describes an overlayfs whiteout entry.
///
/// Overlayfs marks deleted files as character special devices with rdev == 0.
fn is_whiteout(metadata: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    metadata.file_type().is_char_device() && metadata.rdev() == 0
}

/// Return true if the directory at `path` has the overlayfs opaque xattr set,
/// which means the directory was newly created (not just a copy-up).
fn is_opaque_dir(path: &Path) -> bool {
    // Check for trusted.overlay.opaque or user.overlay.opaque xattr
    for attr in &["trusted.overlay.opaque", "user.overlay.opaque"] {
        if let Ok(val) = xattr_get(path, attr) {
            if val.as_deref() == Some(b"y") {
                return true;
            }
        }
    }
    false
}

/// Read an extended attribute value from a file path.
fn xattr_get(path: &Path, name: &str) -> Result<Option<Vec<u8>>> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let path_c = CString::new(path.as_os_str().as_bytes())?;
    let name_c = CString::new(name)?;

    // Call lgetxattr(2) to read without following symlinks.
    let ret = unsafe {
        libc::lgetxattr(
            path_c.as_ptr(),
            name_c.as_ptr(),
            std::ptr::null_mut(),
            0,
        )
    };
    if ret < 0 {
        return Ok(None);
    }
    let size = ret as usize;
    let mut buf = vec![0u8; size];
    let ret = unsafe {
        libc::lgetxattr(
            path_c.as_ptr(),
            name_c.as_ptr(),
            buf.as_mut_ptr() as *mut libc::c_void,
            size,
        )
    };
    if ret < 0 {
        return Ok(None);
    }
    Ok(Some(buf))
}

/// Print a human-readable summary of the detected changes to stdout.
pub fn display_changes(changes: &[Change]) {
    if changes.is_empty() {
        println!("No filesystem changes detected.");
        return;
    }

    println!("\n─── Filesystem changes ──────────────────────────────────");
    for change in changes {
        let (symbol, label) = match change.kind {
            ChangeKind::Added => ("+", "added"),
            ChangeKind::Modified => ("~", "modified"),
            ChangeKind::Deleted => ("-", "deleted"),
        };
        println!("  {} {} [{}]", symbol, change.real_path.display(), label);
    }
    println!("─────────────────────────────────────────────────────────");
    println!("  {} change(s) total", changes.len());
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn test_collect_empty_upper_dir() {
        let upper = tempdir().unwrap();
        let changes = collect_changes(upper.path()).unwrap();
        assert!(changes.is_empty());
    }

    #[test]
    fn test_collect_added_file() {
        let upper = tempdir().unwrap();
        // Create a file under upper that does NOT exist in the real /.
        let new_file = upper.path().join("definitely-does-not-exist-vegas-test-abc123.txt");
        fs::write(&new_file, b"hello").unwrap();

        let changes = collect_changes(upper.path()).unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].kind, ChangeKind::Added);
    }

    #[test]
    fn test_collect_whiteout_is_deletion() {
        let upper = tempdir().unwrap();
        // Create a character device 0,0 to simulate a whiteout.
        let whiteout = upper.path().join("whiteout-test");
        let ret = unsafe {
            libc::mknod(
                std::ffi::CString::new(whiteout.to_str().unwrap())
                    .unwrap()
                    .as_ptr(),
                libc::S_IFCHR | 0o000,
                0,
            )
        };
        if ret == 0 {
            // Only check if we could create the device node (requires root).
            let changes = collect_changes(upper.path()).unwrap();
            assert_eq!(changes.len(), 1);
            assert_eq!(changes[0].kind, ChangeKind::Deleted);
        }
        // If ret != 0 we're not root, just skip the assertion.
    }
}
