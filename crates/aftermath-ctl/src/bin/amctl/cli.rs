//! Command-line interface of amctl (clap). Only the shape of the commands
//! lives here; what they do is in the sibling modules.

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "amctl",
    bin_name = "amctl",
    version,
    about = "Aftermath control: status, strata, services, logs, boot and power",
    after_help = "Examples:\n  amctl status\n  amctl list\n  amctl enable fedora\n  amctl strata enter ubuntu\n  \
                  amctl which vim\n  amctl default vim ubuntu\n  \
                  amctl services restart heartbeat\n  amctl logs -f --service heartbeat\n  \
                  amctl boot next host\n  amctl overlay reset"
)]
pub struct Cli {
    /// Machine-readable JSON output (for scripts)
    #[arg(long, global = true)]
    pub json: bool,
    /// No colours
    #[arg(long, global = true)]
    pub no_color: bool,
    #[command(subcommand)]
    pub cmd: Cmd,
}

#[derive(Subcommand)]
pub enum Cmd {
    /// List strata, including installation and live integration status
    List,
    /// Download an optional stratum if missing, then start it now and at boot
    Enable { name: String },
    /// Stop a stratum now and keep it off at boot (keeps its installed files)
    Disable { name: String },
    /// Overview: mode, root/overlay, network, strata, services, uptime
    Status,
    /// Strata (Arch host, Ubuntu, ...)
    #[command(subcommand)]
    Strata(StrataCmd),
    /// Services from /etc/aftermath/services/*.toml
    #[command(subcommand, visible_alias = "svc")]
    Services(SvcCmd),
    /// Every copy of a command across the strata, and which one wins (and why)
    Which {
        /// Command name, e.g. vim
        cmd: String,
    },
    /// Choose which stratum runs a command: `amctl default vim ubuntu`,
    /// `amctl default vim --reset`, `amctl default list`, `amctl default order ...`
    Default(DefaultArgs),
    /// Session logs of this boot (consoles, kernel, services)
    Logs(LogsArgs),
    /// Power off cleanly
    Poweroff,
    /// Reboot cleanly
    Reboot,
    /// GRUB: pick the next boot once
    #[command(subcommand)]
    Boot(BootCmd),
    /// The persistent host root (overlay disk)
    #[command(subcommand)]
    Overlay(OverlayCmd),
    /// Keyboard layout: show, or `amctl keymap set <name>`
    Keymap {
        #[command(subcommand)]
        cmd: Option<KeymapCmd>,
    },
    /// Subsystems the engine offers
    Subsystems,
    /// Print shell completions (bash, zsh, fish, ...)
    Completions { shell: clap_complete::Shell },
}

#[derive(Subcommand)]
pub enum StrataCmd {
    /// All strata and their state
    List,
    /// One stratum in detail
    Status {
        name: String,
    },
    Start {
        name: String,
    },
    Stop {
        name: String,
    },
    Restart {
        name: String,
    },
    /// Download if supported and missing, then start now and at every boot
    Enable {
        name: String,
    },
    /// Stop now and keep it off at boot
    Disable {
        name: String,
    },
    /// A shell (or a command) inside a stratum
    Enter {
        name: String,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        cmd: Vec<String>,
    },
}

#[derive(Subcommand)]
pub enum SvcCmd {
    List,
    Status {
        name: String,
    },
    Start {
        name: String,
    },
    Stop {
        name: String,
    },
    Restart {
        name: String,
    },
    /// Start now and at every boot
    Enable {
        name: String,
    },
    /// Stop now and keep it off at boot
    Disable {
        name: String,
    },
    /// Re-read /etc/aftermath/services
    Reload,
}

#[derive(Subcommand)]
pub enum BootCmd {
    /// normal | host | busybox | clean | serial | debug | none (cancel)
    Next { target: String },
    /// What GRUB will do next
    Status,
}

#[derive(Subcommand)]
pub enum OverlayCmd {
    Status,
    /// Throw away all saved changes on the next boot
    Reset {
        /// Withdraw a pending reset
        #[arg(long)]
        cancel: bool,
    },
}

#[derive(Subcommand)]
pub enum KeymapCmd {
    /// Load a console keymap now and save it in /etc/aftermath/aftermath.toml
    Set { name: String },
}

#[derive(clap::Args)]
pub struct LogsArgs {
    /// Keep printing new lines
    #[arg(short, long)]
    pub follow: bool,
    /// Number of lines to show
    #[arg(short = 'n', long, default_value_t = 40)]
    pub lines: usize,
    /// A service's log
    #[arg(long, conflicts_with_all = ["kernel", "console"])]
    pub service: Option<String>,
    /// The whole kernel log (default: only Aftermath's own messages)
    #[arg(long, conflicts_with = "console")]
    pub kernel: bool,
    /// A console's log, e.g. tty1 or ttyS0
    #[arg(long)]
    pub console: Option<String>,
    /// List the log files of this boot
    #[arg(long)]
    pub list: bool,
}

/// `amctl default <cmd> <stratum>` / `<cmd> --reset`, or a subcommand.
#[derive(clap::Args)]
#[command(args_conflicts_with_subcommands = true, subcommand_negates_reqs = true)]
pub struct DefaultArgs {
    #[command(subcommand)]
    pub sub: Option<DefaultSub>,
    /// Command name, e.g. vim
    pub cmd: Option<String>,
    /// Stratum that should run it, e.g. ubuntu
    pub stratum: Option<String>,
    /// Forget your default for <cmd>: the shipped rules apply again
    #[arg(long, conflicts_with = "stratum")]
    pub reset: bool,
}

#[derive(Subcommand)]
pub enum DefaultSub {
    /// Your defaults, the lookup order and the shipped pins
    List,
    /// Lookup order for commands nothing pins, e.g. `ubuntu arch`
    Order {
        strata: Vec<String>,
        /// Back to the order in /etc/aftermath/strata.toml
        #[arg(long, conflicts_with = "strata")]
        reset: bool,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strata_shortcuts_and_existing_commands_parse() {
        assert!(matches!(
            Cli::try_parse_from(["amctl", "list"]).unwrap().cmd,
            Cmd::List
        ));
        assert!(
            matches!(Cli::try_parse_from(["amctl", "enable", "fedora"]).unwrap().cmd,
            Cmd::Enable { name } if name == "fedora")
        );
        assert!(
            matches!(Cli::try_parse_from(["amctl", "strata", "enable", "fedora"]).unwrap().cmd,
            Cmd::Strata(StrataCmd::Enable { name }) if name == "fedora")
        );
        assert!(
            matches!(Cli::try_parse_from(["amctl", "disable", "fedora", "--json"]).unwrap().cmd,
            Cmd::Disable { name } if name == "fedora")
        );
        assert!(Cli::try_parse_from(["amctl", "enable"]).is_err());
    }
}
