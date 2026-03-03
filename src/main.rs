use anyhow::Result;
use clap::{Parser, Subcommand};

/// What happens in Vegas, stays in Vegas — unless you decide to bring it home.
///
/// A filesystem sandboxing tool using Linux namespaces and overlayfs.
/// Run programs without permanently changing the system, then review and
/// choose to apply or discard the changes.
///
/// Requires root privileges. Run with: sudo vegas run -- <command>
#[derive(Parser)]
#[command(name = "vegas", version)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Run a command inside a filesystem sandbox.
    ///
    /// The command runs in an overlayfs environment where all filesystem
    /// changes are captured in a temporary upper directory. When the command
    /// exits, you can review the changes and choose to apply or discard them.
    ///
    /// By default the command runs as root inside the sandbox so it can freely
    /// modify system paths (changes still only go to the overlay).  Use
    /// --user to run as a specific uid instead.
    ///
    /// Example: sudo vegas run -- bash
    /// Example: sudo vegas run -- apt install curl
    /// Example: sudo vegas run --user 1000 -- my-script.sh
    Run {
        /// Drop to this uid:gid inside the sandbox instead of running as root.
        /// Accepts a plain uid (e.g. 1000) or uid:gid (e.g. 1000:1000).
        /// Defaults to keeping root so privileged commands work correctly.
        #[arg(long)]
        user: Option<String>,

        /// Supplementary groups to set inside the sandbox.
        /// Accepts a comma-separated list of numeric GIDs (e.g. 1000,1001,1002).
        /// Only effective when --user is also specified.
        #[arg(long)]
        groups: Option<String>,

        /// The command and arguments to run in the sandbox.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        command: Vec<String>,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Run {
            user,
            groups,
            command,
        } => vegas::run(&command, user.as_deref(), groups.as_deref())?,
    }
    Ok(())
}
