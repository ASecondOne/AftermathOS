//! Aftermath strata: shared logic for the `aftermath-strata` service and the
//! `strat` multicall launcher. Everything here is pure (no mounts, no exec),
//! so it is unit-tested.

pub mod config;
pub mod cross;
pub mod defaults;
pub mod state;
pub mod status;
pub mod users;
pub mod which;

use std::path::PathBuf;

/// Where the cross-stratum launchers and their index live.
pub const CROSS_DIR: &str = "/aftermath/cross";
/// The installed multicall launcher; cross launchers are symlinks to it.
pub const STRAT_BIN: &str = "/usr/lib/aftermath/strat";
pub const CONFIG: &str = "/etc/aftermath/strata.toml";
/// PATH used for programs running inside a (non-host) stratum.
/// Where commands live inside every stratum, in PATH order.
pub const BIN_DIRS: [&str; 6] = [
    "usr/local/sbin",
    "usr/local/bin",
    "usr/sbin",
    "usr/bin",
    "sbin",
    "bin",
];
pub const STRATUM_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// Test/development prefix: `AFTERMATH_PREFIX=/tmp/x` makes every Aftermath
/// path relative to /tmp/x (used by the unprivileged namespace test).
/// Ignored when running setuid (strat), so a user can't point it at a fake tree.
pub fn prefix() -> PathBuf {
    // SAFETY: plain syscalls without arguments.
    let setuid = unsafe { libc::geteuid() != libc::getuid() || libc::getegid() != libc::getgid() };
    if setuid {
        return PathBuf::from("/");
    }
    std::env::var_os("AFTERMATH_PREFIX")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

/// `prefix() + absolute path`.
pub fn at(path: &str) -> PathBuf {
    prefix().join(path.trim_start_matches('/'))
}
