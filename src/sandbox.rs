use crate::apply;
use crate::diff;
use crate::mount_policy::MountPolicy;
use anyhow::{bail, Context, Result};
use nix::mount::{mount, MsFlags};
use nix::sched::{unshare, CloneFlags};
use nix::sys::wait::{waitpid, WaitStatus};
use nix::unistd::{self, fork, ForkResult, Gid, Uid};
use std::ffi::CString;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process;

#[derive(Debug, Clone)]
struct ExtraOverlay {
    mount_point: String,
    fs_type: String,
    upper: PathBuf,
    work: PathBuf,
}

#[derive(Debug, Clone)]
struct MountEntry {
    mount_point: String,
    fs_type: String,
}

/// Run `command` inside an overlayfs sandbox.
///
/// The sandbox works as follows:
///
/// 1. A temporary directory tree is created: `upper/`, `work/`, `merged/`.
/// 2. A kernel overlayfs is mounted at `merged/` with `lowerdir=/`.  This
///    gives `merged/` the same view as the real root filesystem; all writes
///    are redirected to `upper/`.
/// 3. `/proc`, `/dev`, and `/sys` are bind-mounted into `merged/` so the
///    sandboxed process still shares the live kernel interfaces.
/// 4. The process `fork()`s.  The child enters a new mount namespace
///    (`CLONE_NEWNS`), sets up the mounts above, optionally drops to a
///    specified uid/gid, `chroot(2)`s into `merged/`, and `exec(2)`s the
///    requested command.
/// 5. The parent waits for the child.  Afterwards, it walks `upper/` to
///    display what changed and asks whether to apply or discard the changes.
///
/// Root privileges are required (e.g. `sudo vegas run -- <cmd>`).
///
/// `user_spec` is an optional `uid` or `uid:gid` string.  When `None` the
/// command runs as root (uid 0) inside the sandbox, which is correct for
/// privileged operations such as package installation.  When `Some`, the
/// process drops to the specified uid/gid before exec.
///
/// `groups_spec` is an optional comma-separated list of numeric GIDs to set
/// as the supplementary group list.  Only applied when `user_spec` is `Some`
/// (i.e. when privileges are dropped).
pub fn run(command: &[String], user_spec: Option<&str>, groups_spec: Option<&str>) -> Result<()> {
    if command.is_empty() {
        bail!("No command specified");
    }

    // Require effective root for overlayfs and chroot.
    // Using euid makes setuid-root wrappers work as intended.
    if unistd::geteuid() != Uid::from_raw(0) {
        bail!(
            "vegas requires root privileges.\n\
             Try: sudo vegas run -- {}",
            command.join(" ")
        );
    }

    // Parse the user spec (if any) to determine post-exec uid/gid.
    let (run_uid, run_gid) = parse_user_spec(user_spec)?;

    // Parse the supplementary groups spec (if any).
    let run_groups = parse_groups_spec(groups_spec)?;

    // Capture the current working directory so we can restore it inside the
    // sandbox after chroot.  Ignore errors (e.g. cwd was deleted).
    let original_cwd = std::env::current_dir().ok();

    // Create the temporary directory structure for overlayfs.
    let tmp = tempfile::Builder::new()
        .prefix("vegas-")
        .tempdir()
        .context("Failed to create temporary directory")?;
    let base = tmp.path().to_path_buf();
    let upper = base.join("upper");
    let work = base.join("work");
    let merged = base.join("merged");

    fs::create_dir_all(&upper).context("Failed to create upper dir")?;
    fs::create_dir_all(&work).context("Failed to create work dir")?;
    fs::create_dir_all(&merged).context("Failed to create merged dir")?;

    let mount_policy = MountPolicy::default();
    let extra_overlays = plan_extra_overlays(&base, &mount_policy)?;

    // Fork: the child will enter a new mount namespace, set up the overlay,
    // optionally drop privileges, and exec the command.
    match unsafe { fork() }.context("fork(2) failed")? {
        ForkResult::Child => {
            // Enter a private mount namespace so our mounts stay isolated.
            if let Err(e) = unshare(CloneFlags::CLONE_NEWNS) {
                eprintln!("vegas: unshare(CLONE_NEWNS) failed: {e}");
                process::exit(1);
            }

            // Prevent mount propagation back to the host/shared namespace.
            if let Err(e) = mount(
                None::<&str>,
                Path::new("/"),
                None::<&str>,
                MsFlags::MS_REC | MsFlags::MS_PRIVATE,
                None::<&str>,
            ) {
                eprintln!("vegas: failed to mark mounts private: {e}");
                process::exit(1);
            }

            // Set up the overlay filesystem and chroot into it.
            if let Err(e) = setup_sandbox(&merged, &upper, &work, &extra_overlays, &mount_policy) {
                eprintln!("vegas: Failed to set up sandbox: {e:#}");
                process::exit(1);
            }

            // Optionally drop root privileges before executing the command.
            if let Err(e) = drop_privileges(run_uid, run_gid, &run_groups) {
                eprintln!("vegas: Failed to drop privileges: {e:#}");
                process::exit(1);
            }

            // Restore the caller's working directory inside the sandbox.
            // setup_sandbox leaves us at "/" after chroot; try to switch back
            // to the original path (which exists in the overlay lower dir).
            // Errors are silently ignored: the path may not exist or may not
            // be accessible inside the chrooted environment.
            if let Some(ref cwd) = original_cwd {
                let _ = unistd::chdir(cwd);
            }

            // exec — never returns on success.
            exec_command(command);
            process::exit(1);
        }

        ForkResult::Parent { child } => {
            // Wait for the sandboxed command to finish.
            let status = waitpid(child, None).context("waitpid failed")?;

            println!("\n─── vegas: sandbox exited ───────────────────────────────");
            match status {
                WaitStatus::Exited(_, code) => println!("  Exit code: {code}"),
                WaitStatus::Signaled(_, sig, _) => println!("  Killed by signal: {sig:?}"),
                _ => {}
            }

            // Collect and display the changes captured in the upper directory.
            let mut changes = diff::collect_changes(&upper)?;
            for overlay in &extra_overlays {
                let mut extra =
                    diff::collect_changes_with_prefix(&overlay.upper, Path::new(&overlay.mount_point))?;
                changes.append(&mut extra);
            }
            changes.sort_by(|a, b| a.real_path.cmp(&b.real_path));
            diff::display_changes(&changes);

            if changes.is_empty() {
                // Tempdir is dropped → cleaned up automatically.
                return Ok(());
            }

            // Prompt the user.
            println!();
            println!("What would you like to do with these changes?");
            println!("  [a] Apply  – copy changes to the real filesystem");
            println!("  [k] Keep   – save the sandbox at {}", base.display());
            println!("  [d] Discard – throw away all changes (default)");
            print!("Choice [a/k/d]: ");
            io::stdout().flush()?;

            let mut input = String::new();
            io::stdin().read_line(&mut input)?;

            match input.trim().to_ascii_lowercase().as_str() {
                "a" => {
                    apply::apply_changes(&changes)?;
                }
                "k" => {
                    println!("Sandbox kept at: {}", base.display());
                    // Prevent the TempDir from deleting the directory on drop.
                    let _ = tmp.keep();
                    return Ok(());
                }
                _ => {
                    println!("Changes discarded.");
                }
            }

            Ok(())
        }
    }
}

/// Mount an overlayfs at `merged` (lowerdir=/, upperdir, workdir) and bind-mount
/// the pseudo-filesystems that a real environment needs.
fn setup_sandbox(
    merged: &Path,
    upper: &Path,
    work: &Path,
    extra_overlays: &[ExtraOverlay],
    mount_policy: &MountPolicy,
) -> Result<()> {
    // Mount the overlayfs.
    let opts = format!(
        "lowerdir=/,upperdir={},workdir={}",
        upper.display(),
        work.display()
    );
    mount(
        Some("overlay"),
        merged,
        Some("overlay"),
        MsFlags::empty(),
        Some(opts.as_str()),
    )
    .context(
        "Failed to mount overlayfs. \
         Make sure the kernel has CONFIG_OVERLAY_FS enabled.",
    )?;

    // Expose additional host mount points (e.g. separate /home filesystem)
    // through nested overlays so writes are redirected to sandbox state.
    mount_additional_overlays(merged, extra_overlays)?;

    // Bind-mount selected host trees into the sandbox.
    // - /proc, /dev, /sys: live kernel interfaces.
    // - /run, /var: preserve host runtime sockets/state paths (e.g. docker.sock).
    for pseudo in &mount_policy.passthrough_dirs {
        let src = PathBuf::from("/").join(pseudo);
        let dst = merged.join(pseudo);
        mount(
            Some(src.as_path()),
            dst.as_path(),
            None::<&str>,
            MsFlags::MS_BIND | MsFlags::MS_REC,
            None::<&str>,
        )
        .with_context(|| format!("Failed to bind-mount /{pseudo}"))?;
    }

    // Chroot into the merged view.
    unistd::chroot(merged).context("chroot(2) failed")?;
    unistd::chdir("/").context("chdir(\"/\") failed")?;

    Ok(())
}

fn mount_additional_overlays(merged: &Path, overlays: &[ExtraOverlay]) -> Result<()> {
    for overlay in overlays {
        let target = merged.join(overlay.mount_point.trim_start_matches('/'));
        if let Err(e) = fs::create_dir_all(&target) {
            eprintln!(
                "vegas: warning: skipping {} ({}): cannot create mount target {}: {}",
                overlay.mount_point,
                overlay.fs_type,
                target.display(),
                e
            );
            continue;
        }

        let opts = format!(
            "lowerdir={},upperdir={},workdir={}",
            overlay.mount_point,
            overlay.upper.display(),
            overlay.work.display()
        );

        if let Err(e) = mount(
            Some("overlay"),
            target.as_path(),
            Some("overlay"),
            MsFlags::empty(),
            Some(opts.as_str()),
        )
        {
            eprintln!(
                "vegas: warning: nested overlay failed for {} ({}): {}. Falling back to read-only bind mount.",
                overlay.mount_point,
                overlay.fs_type,
                e
            );
            if let Err(bind_err) = bind_mount_readonly(Path::new(&overlay.mount_point), &target) {
                eprintln!(
                    "vegas: warning: read-only bind fallback failed for {}: {}",
                    overlay.mount_point,
                    bind_err
                );
            }
        }
    }

    Ok(())
}

fn bind_mount_readonly(src: &Path, dst: &Path) -> Result<()> {
    mount(
        Some(src),
        dst,
        None::<&str>,
        MsFlags::MS_BIND | MsFlags::MS_REC,
        None::<&str>,
    )
    .with_context(|| format!("Failed to bind-mount {}", src.display()))?;

    mount(
        None::<&str>,
        dst,
        None::<&str>,
        MsFlags::MS_BIND | MsFlags::MS_REMOUNT | MsFlags::MS_RDONLY | MsFlags::MS_REC,
        None::<&str>,
    )
    .with_context(|| format!("Failed to remount {} read-only", src.display()))?;

    Ok(())
}

fn should_skip_additional_mount(mount_point: &str, mount_policy: &MountPolicy) -> bool {
    if mount_point == "/" {
        return true;
    }

    mount_policy
        .extra_overlay_skip_prefixes
        .iter()
        .any(|prefix| is_path_under_prefix(mount_point, prefix))
}

fn is_path_under_prefix(path: &str, prefix: &str) -> bool {
    path == prefix || path.starts_with(&format!("{prefix}/"))
}

fn plan_extra_overlays(base: &Path, mount_policy: &MountPolicy) -> Result<Vec<ExtraOverlay>> {
    let overlays_root = base.join("extra-overlays");
    fs::create_dir_all(&overlays_root).context("Failed to create extra overlays dir")?;

    let mut overlays = Vec::new();
    for (idx, entry) in read_mount_entries()?.into_iter().enumerate() {
        if is_vegas_internal_mount(&entry.mount_point, base) {
            continue;
        }

        if should_skip_additional_mount(&entry.mount_point, mount_policy) {
            continue;
        }

        if is_overlay_incompatible_fs(&entry.fs_type, mount_policy) {
            continue;
        }

        let src = Path::new(&entry.mount_point);
        let Ok(meta) = fs::metadata(src) else {
            continue;
        };
        if !meta.is_dir() {
            continue;
        }

        let entry_dir = overlays_root.join(format!("{idx:04}"));
        let upper = entry_dir.join("upper");
        let work = entry_dir.join("work");

        fs::create_dir_all(&upper)
            .with_context(|| format!("Failed to create upper dir {}", upper.display()))?;
        fs::create_dir_all(&work)
            .with_context(|| format!("Failed to create work dir {}", work.display()))?;

        overlays.push(ExtraOverlay {
            mount_point: entry.mount_point,
            fs_type: entry.fs_type,
            upper,
            work,
        });
    }

    Ok(overlays)
}

fn is_vegas_internal_mount(mount_point: &str, base: &Path) -> bool {
    let base_str = base.to_string_lossy();
    if mount_point == base_str || mount_point.starts_with(&format!("{}/", base_str)) {
        return true;
    }

    // Also ignore leaked mounts from older vegas runs.
    mount_point.starts_with("/tmp/vegas-") && mount_point.contains("/merged")
}

fn is_overlay_incompatible_fs(fs_type: &str, mount_policy: &MountPolicy) -> bool {
    mount_policy
        .overlay_incompatible_fs_types
        .iter()
        .any(|item| *item == fs_type)
}

fn read_mount_entries() -> Result<Vec<MountEntry>> {
    let content = fs::read_to_string("/proc/self/mountinfo")
        .context("Failed to read /proc/self/mountinfo")?;

    let mut entries = Vec::new();

    for line in content.lines() {
        let Some((left, right)) = line.split_once(" - ") else {
            continue;
        };
        let left_fields: Vec<&str> = left.split_whitespace().collect();
        if left_fields.len() < 5 {
            continue;
        }

        let right_fields: Vec<&str> = right.split_whitespace().collect();
        if right_fields.is_empty() {
            continue;
        }

        let mount_point = unescape_mountinfo_path(left_fields[4]);
        let fs_type = right_fields[0].to_string();

        entries.push(MountEntry {
            mount_point,
            fs_type,
        });
    }

    entries.sort_by(|a, b| {
        a.mount_point
            .matches('/')
            .count()
            .cmp(&b.mount_point.matches('/').count())
            .then(a.mount_point.cmp(&b.mount_point))
            .then(a.fs_type.cmp(&b.fs_type))
    });
    entries.dedup_by(|a, b| a.mount_point == b.mount_point);

    Ok(entries)
}

fn unescape_mountinfo_path(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut idx = 0;

    while idx < bytes.len() {
        if bytes[idx] == b'\\' && idx + 3 < bytes.len() {
            let oct = &raw[idx + 1..idx + 4];
            if let Ok(val) = u8::from_str_radix(oct, 8) {
                out.push(val);
                idx += 4;
                continue;
            }
        }
        out.push(bytes[idx]);
        idx += 1;
    }

    String::from_utf8_lossy(&out).to_string()
}

/// Drop root to the given uid/gid.  When both are uid 0 / gid 0, nothing is done
/// (i.e. the command runs as root inside the sandbox).
///
/// `supp_groups` sets the supplementary group list.  When empty and privileges
/// are being dropped, all supplementary groups are cleared.
fn drop_privileges(uid: Uid, gid: Gid, supp_groups: &[Gid]) -> Result<()> {
    if uid == Uid::from_raw(0) && gid == Gid::from_raw(0) {
        return Ok(()); // Keep root – useful for privileged commands.
    }
    unistd::setgroups(supp_groups).context("setgroups failed")?;
    unistd::setgid(gid).context("setgid failed")?;
    unistd::setuid(uid).context("setuid failed")?;
    Ok(())
}

/// Parse a user spec string of the form `uid` or `uid:gid`.
///
/// Returns `(Uid(0), Gid(0))` when `spec` is `None` (run as root).
fn parse_user_spec(spec: Option<&str>) -> Result<(Uid, Gid)> {
    match spec {
        None => Ok((Uid::from_raw(0), Gid::from_raw(0))),
        Some(s) => {
            let parts: Vec<&str> = s.splitn(2, ':').collect();
            let uid: u32 = parts[0]
                .parse()
                .with_context(|| format!("Invalid uid in --user '{s}'"))?;
            let gid: u32 = if parts.len() == 2 {
                parts[1]
                    .parse()
                    .with_context(|| format!("Invalid gid in --user '{s}'"))?
            } else {
                uid // Default gid to the same value as uid.
            };
            Ok((Uid::from_raw(uid), Gid::from_raw(gid)))
        }
    }
}

/// Parse a groups spec string of comma-separated numeric GIDs.
///
/// Returns an empty `Vec` when `spec` is `None` (no supplementary groups).
fn parse_groups_spec(spec: Option<&str>) -> Result<Vec<Gid>> {
    match spec {
        None => Ok(Vec::new()),
        Some(s) => s
            .split(',')
            .filter(|part| !part.is_empty())
            .map(|part| {
                part.trim()
                    .parse::<u32>()
                    .with_context(|| format!("Invalid gid in --groups '{s}': '{part}'"))
                    .map(Gid::from_raw)
            })
            .collect(),
    }
}

/// Replace the current process with `command[0] command[1..]`.
///
/// On success this function never returns (the process image is replaced).
fn exec_command(command: &[String]) {
    let prog = match CString::new(command[0].as_bytes()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("vegas: invalid command name: {e}");
            return;
        }
    };
    let args: Vec<CString> = command
        .iter()
        .filter_map(|s| CString::new(s.as_bytes()).ok())
        .collect();

    match unistd::execvp(&prog, &args) {
        Ok(_) => unreachable!(),
        Err(e) => eprintln!("vegas: exec '{}' failed: {e}", command[0]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_user_spec_none_returns_root() {
        let (uid, gid) = parse_user_spec(None).unwrap();
        assert_eq!(uid, Uid::from_raw(0));
        assert_eq!(gid, Gid::from_raw(0));
    }

    #[test]
    fn test_parse_user_spec_uid_only() {
        let (uid, gid) = parse_user_spec(Some("1000")).unwrap();
        assert_eq!(uid, Uid::from_raw(1000));
        assert_eq!(gid, Gid::from_raw(1000)); // gid defaults to uid
    }

    #[test]
    fn test_parse_user_spec_uid_gid() {
        let (uid, gid) = parse_user_spec(Some("1000:2000")).unwrap();
        assert_eq!(uid, Uid::from_raw(1000));
        assert_eq!(gid, Gid::from_raw(2000));
    }

    #[test]
    fn test_parse_user_spec_invalid_uid() {
        let result = parse_user_spec(Some("notanumber"));
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_user_spec_invalid_gid() {
        let result = parse_user_spec(Some("1000:notanumber"));
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_groups_spec_none_returns_empty() {
        let groups = parse_groups_spec(None).unwrap();
        assert!(groups.is_empty());
    }

    #[test]
    fn test_parse_groups_spec_single() {
        let groups = parse_groups_spec(Some("1000")).unwrap();
        assert_eq!(groups, vec![Gid::from_raw(1000)]);
    }

    #[test]
    fn test_parse_groups_spec_multiple() {
        let groups = parse_groups_spec(Some("1000,1001,1002")).unwrap();
        assert_eq!(
            groups,
            vec![Gid::from_raw(1000), Gid::from_raw(1001), Gid::from_raw(1002)]
        );
    }

    #[test]
    fn test_parse_groups_spec_with_spaces() {
        let groups = parse_groups_spec(Some("1000, 2000")).unwrap();
        assert_eq!(groups, vec![Gid::from_raw(1000), Gid::from_raw(2000)]);
    }

    #[test]
    fn test_parse_groups_spec_invalid() {
        let result = parse_groups_spec(Some("1000,notanumber"));
        assert!(result.is_err());
    }

    #[test]
    fn test_overlay_incompatible_fs() {
        let policy = MountPolicy::default();
        assert!(is_overlay_incompatible_fs("vfat", &policy));
        assert!(is_overlay_incompatible_fs("exfat", &policy));
        assert!(is_overlay_incompatible_fs("squashfs", &policy));
        assert!(!is_overlay_incompatible_fs("ext4", &policy));
        assert!(!is_overlay_incompatible_fs("xfs", &policy));
    }

    #[test]
    fn test_detect_vegas_internal_mount() {
        let base = Path::new("/tmp/vegas-abc123");
        assert!(is_vegas_internal_mount("/tmp/vegas-abc123", base));
        assert!(is_vegas_internal_mount(
            "/tmp/vegas-abc123/merged/proc",
            base
        ));
        assert!(is_vegas_internal_mount(
            "/tmp/vegas-oldrun/merged/dev",
            base
        ));
        assert!(!is_vegas_internal_mount("/home", base));
    }

    #[test]
    fn test_skip_additional_mount_prefixes() {
        let policy = MountPolicy::default();
        assert!(should_skip_additional_mount("/proc", &policy));
        assert!(should_skip_additional_mount("/run/docker.sock", &policy));
        assert!(should_skip_additional_mount("/var/lib", &policy));
        assert!(!should_skip_additional_mount("/home", &policy));
    }
}
