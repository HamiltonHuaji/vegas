use anyhow::{bail, Context, Result};
use clap::Parser;
use nix::unistd::{self, Gid, Uid};
use std::io::{self, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

/// Run a command in vegas as the caller's own uid/gid/groups.
///
/// This binary is intended to be installed setuid root:
///   chown root:root /path/to/vegas-user
///   chmod 4755 /path/to/vegas-user
///
/// Unlike `vegas`, this wrapper does not accept `--user` / `--groups`.
/// Identity is locked to the real calling user automatically.
#[derive(Parser)]
#[command(name = "vegas-user", version)]
struct Cli {
    /// The command and arguments to run in the sandbox.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    command: Vec<String>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    if cli.command.is_empty() {
        return handle_empty_command();
    }

    let real_uid = unistd::getuid();
    let real_gid = unistd::getgid();
    let groups = unistd::getgroups().context("Failed to read supplementary groups")?;

    let user_spec = format!("{}:{}", uid_raw(real_uid), gid_raw(real_gid));
    let groups_spec = groups_to_csv(&groups);

    vegas::run(&cli.command, Some(user_spec.as_str()), Some(groups_spec.as_str()))
}

fn handle_empty_command() -> Result<()> {
    if unistd::getuid() != Uid::from_raw(0) {
        bail!("No command specified. Usage: vegas-user -- <command> [args...]");
    }

    let exe_path = std::env::current_exe().context("Failed to resolve current executable path")?;

    println!("No command provided.");
    println!(
        "You are running as root. Set setuid permissions automatically on this binary?"
    );
    println!("  Target: {}", exe_path.display());
    print!("Apply root:root + mode 4755? [y/N]: ");
    io::stdout().flush()?;

    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let answer = input.trim().to_ascii_lowercase();

    if answer == "y" || answer == "yes" {
        setup_setuid_binary(&exe_path)?;
        println!("Done. Set owner root:root and mode 4755 on {}", exe_path.display());
    } else {
        println!("Skipped.");
    }

    Ok(())
}

fn setup_setuid_binary(path: &Path) -> Result<()> {
    unistd::chown(path, Some(Uid::from_raw(0)), Some(Gid::from_raw(0)))
        .with_context(|| format!("Failed to chown root:root for {}", path.display()))?;

    let mut perms = std::fs::metadata(path)
        .with_context(|| format!("Failed to read metadata for {}", path.display()))?
        .permissions();
    perms.set_mode(0o4755);
    std::fs::set_permissions(path, perms)
        .with_context(|| format!("Failed to chmod 4755 for {}", path.display()))?;

    Ok(())
}

fn uid_raw(uid: Uid) -> u32 {
    uid.as_raw()
}

fn gid_raw(gid: Gid) -> u32 {
    gid.as_raw()
}

fn groups_to_csv(groups: &[Gid]) -> String {
    groups
        .iter()
        .map(|gid| gid_raw(*gid).to_string())
        .collect::<Vec<_>>()
        .join(",")
}
