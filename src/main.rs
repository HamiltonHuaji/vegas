use anyhow::Result;
use clap::{Parser, Subcommand};

mod apply;
mod diff;
mod sandbox;

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
    /// By default the command runs as the original calling user (the user who
    /// invoked sudo), preserving the current working directory and environment
    /// variables.  Use --root to run as root or --user to specify a uid:gid.
    ///
    /// Example: sudo vegas run -- bash
    /// Example: sudo vegas run -- sudo apt install curl
    /// Example: sudo vegas run --root -- apt install curl
    /// Example: sudo vegas run --user 1000:1000 -- my-script.sh
    /// Example: sudo vegas run --user -- bash
    Run {
        /// Run as root inside the sandbox (uid 0:0).
        /// Useful for privileged commands like package installation.
        /// Mutually exclusive with --user.
        #[arg(long, conflicts_with = "user")]
        root: bool,

        /// Run as a specific uid:gid inside the sandbox.
        /// Accepts uid (e.g. 1000) or uid:gid (e.g. 1000:1000).
        /// When given without a value, runs as the original calling user
        /// (same as the default behavior).
        #[arg(long, num_args = 0..=1, default_missing_value = "")]
        user: Option<String>,

        /// The command and arguments to run in the sandbox.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        command: Vec<String>,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Run { root, user, command } => {
            sandbox::run(&command, root, user.as_deref())?;
        }
    }
    Ok(())
}
