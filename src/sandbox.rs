use crate::apply;
use crate::diff;
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
pub fn run(command: &[String], user_spec: Option<&str>) -> Result<()> {
    if command.is_empty() {
        bail!("No command specified");
    }

    // Require root for overlayfs and chroot.
    if unistd::getuid() != Uid::from_raw(0) {
        bail!(
            "vegas requires root privileges.\n\
             Try: sudo vegas run -- {}",
            command.join(" ")
        );
    }

    // Parse the user spec (if any) to determine post-exec uid/gid.
    let (run_uid, run_gid) = parse_user_spec(user_spec)?;

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

    // Fork: the child will enter a new mount namespace, set up the overlay,
    // optionally drop privileges, and exec the command.
    match unsafe { fork() }.context("fork(2) failed")? {
        ForkResult::Child => {
            // Enter a private mount namespace so our mounts stay isolated.
            if let Err(e) = unshare(CloneFlags::CLONE_NEWNS) {
                eprintln!("vegas: unshare(CLONE_NEWNS) failed: {e}");
                process::exit(1);
            }

            // Set up the overlay filesystem and chroot into it.
            if let Err(e) = setup_sandbox(&merged, &upper, &work) {
                eprintln!("vegas: Failed to set up sandbox: {e:#}");
                process::exit(1);
            }

            // Optionally drop root privileges before executing the command.
            if let Err(e) = drop_privileges(run_uid, run_gid) {
                eprintln!("vegas: Failed to drop privileges: {e:#}");
                process::exit(1);
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
            let changes = diff::collect_changes(&upper)?;
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
fn setup_sandbox(merged: &Path, upper: &Path, work: &Path) -> Result<()> {
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

    // Bind-mount the live pseudo-filesystems into the sandbox so processes
    // see the real /proc, /dev and /sys.
    for pseudo in &["proc", "dev", "sys"] {
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

/// Drop root to the given uid/gid.  When both are uid 0 / gid 0, nothing is done
/// (i.e. the command runs as root inside the sandbox).
fn drop_privileges(uid: Uid, gid: Gid) -> Result<()> {
    if uid == Uid::from_raw(0) && gid == Gid::from_raw(0) {
        return Ok(()); // Keep root – useful for privileged commands.
    }
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
