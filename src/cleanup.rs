use anyhow::{bail, Context, Result};
use nix::errno::Errno;
use nix::mount::{umount, umount2, MntFlags};
use nix::unistd::{self, Uid};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy)]
pub struct CleanupOptions {
    pub yes: bool,
    pub dry_run: bool,
}

#[derive(Debug, Clone)]
struct SandboxCandidate {
    path: PathBuf,
    mounts: Vec<PathBuf>,
}

pub fn cleanup(options: CleanupOptions) -> Result<()> {
    if unistd::geteuid() != Uid::from_raw(0) {
        bail!("vegas cleanup requires root privileges. Try: sudo vegas cleanup");
    }

    let mut candidates = discover_candidates()?;
    if candidates.is_empty() {
        println!("No stale vegas sandboxes found.");
        return Ok(());
    }

    candidates.sort_by(|a, b| a.path.cmp(&b.path));
    print_cleanup_plan(&candidates);

    if options.dry_run {
        println!("Dry-run mode enabled. No changes were made.");
        return Ok(());
    }

    if !options.yes
        && !confirm("Proceed with unmount + directory cleanup for all items above? [y/N]: ")?
    {
        println!("Cleanup cancelled.");
        return Ok(());
    }

    let mut any_errors = false;
    for candidate in &candidates {
        println!("\nCleaning {}", candidate.path.display());

        let mut busy_mounts = Vec::new();
        let mut mounts = candidate.mounts.clone();
        mounts.sort_by_key(|path| std::cmp::Reverse(path.components().count()));

        for mount_point in mounts {
            match umount(mount_point.as_path()) {
                Ok(_) => println!("  unmounted {}", mount_point.display()),
                Err(Errno::EBUSY) => {
                    println!("  busy mount {}", mount_point.display());
                    busy_mounts.push(mount_point);
                }
                Err(e) => {
                    any_errors = true;
                    eprintln!("  warning: failed to unmount {}: {e}", mount_point.display());
                }
            }
        }

        if !busy_mounts.is_empty() {
            let allow_lazy = if options.yes {
                true
            } else {
                println!(
                    "  {} mount(s) are busy and can be lazily detached (MNT_DETACH).",
                    busy_mounts.len()
                );
                confirm("  Apply lazy detach for busy mounts? [y/N]: ")?
            };

            if allow_lazy {
                for mount_point in busy_mounts {
                    match umount2(mount_point.as_path(), MntFlags::MNT_DETACH) {
                        Ok(_) => println!("  lazily detached {}", mount_point.display()),
                        Err(e) => {
                            any_errors = true;
                            eprintln!(
                                "  warning: failed lazy detach {}: {e}",
                                mount_point.display()
                            );
                        }
                    }
                }
            }
        }

        if let Err(e) = fs::remove_dir_all(&candidate.path) {
            any_errors = true;
            eprintln!("  warning: failed to remove {}: {}", candidate.path.display(), e);
        } else {
            println!("  removed {}", candidate.path.display());
        }
    }

    if any_errors {
        println!("\nCleanup finished with warnings.");
    } else {
        println!("\nCleanup complete.");
    }

    Ok(())
}

fn discover_candidates() -> Result<Vec<SandboxCandidate>> {
    let all_mounts = read_mount_points()?;
    let mut sandboxes = Vec::new();

    for root in [Path::new("/tmp"), Path::new("/var/tmp")] {
        if !root.exists() {
            continue;
        }

        let entries = match fs::read_dir(root) {
            Ok(entries) => entries,
            Err(_) => continue,
        };

        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(_) => continue,
            };
            let path = entry.path();
            let file_name = entry.file_name();
            let name = file_name.to_string_lossy();

            if !name.starts_with("vegas-") {
                continue;
            }
            if !path.is_dir() {
                continue;
            }

            let mounts: Vec<PathBuf> = all_mounts
                .iter()
                .filter(|mount| mount.starts_with(&path))
                .cloned()
                .collect();

            sandboxes.push(SandboxCandidate { path, mounts });
        }
    }

    Ok(sandboxes)
}

fn read_mount_points() -> Result<Vec<PathBuf>> {
    let content = fs::read_to_string("/proc/self/mountinfo")
        .context("Failed to read /proc/self/mountinfo")?;

    let mut mounts = Vec::new();
    for line in content.lines() {
        let Some((left, _)) = line.split_once(" - ") else {
            continue;
        };
        let fields: Vec<&str> = left.split_whitespace().collect();
        if fields.len() < 5 {
            continue;
        }
        mounts.push(PathBuf::from(unescape_mountinfo_path(fields[4])));
    }

    mounts.sort();
    mounts.dedup();
    Ok(mounts)
}

fn print_cleanup_plan(candidates: &[SandboxCandidate]) {
    let total_mounts: usize = candidates.iter().map(|candidate| candidate.mounts.len()).sum();

    println!("Found {} vegas sandbox directorie(s).", candidates.len());
    println!("Detected {} mount point(s) under these directories.", total_mounts);
    println!();

    for candidate in candidates {
        println!("- {}", candidate.path.display());
        if candidate.mounts.is_empty() {
            println!("  mounts: none");
            continue;
        }

        println!("  mounts: {}", candidate.mounts.len());
        for mount in &candidate.mounts {
            println!("    • {}", mount.display());
        }
    }
}

fn confirm(prompt: &str) -> Result<bool> {
    print!("{prompt}");
    io::stdout().flush().context("Failed to flush stdout")?;

    let mut input = String::new();
    io::stdin()
        .read_line(&mut input)
        .context("Failed to read user input")?;

    let answer = input.trim().to_ascii_lowercase();
    Ok(answer == "y" || answer == "yes")
}

fn unescape_mountinfo_path(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;

    while index < bytes.len() {
        if bytes[index] == b'\\' && index + 3 < bytes.len() {
            let oct = &raw[index + 1..index + 4];
            if let Ok(value) = u8::from_str_radix(oct, 8) {
                out.push(value);
                index += 4;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }

    String::from_utf8_lossy(&out).to_string()
}
