//! aftermath-ctl: everything `amctl` and the engine share.
//!
//! * [`proto`]: the control protocol on /run/aftermath/control.sock
//! * [`facts`]: system facts readable without the engine (mode, overlay,
//!   network, uptime, boot id, strata) - the offline fallback of `amctl`
//! * [`services`]: the service file format (/etc/aftermath/services/*.toml)
//! * [`locale`]: keyboard layout (/etc/aftermath/aftermath.toml `[locale]`)
//! * [`grubenv`], [`overlay`], [`disks`]: boot-next, overlay reset and
//!   disk discovery, used by the engine and by `amctl` as root when the
//!   engine is down (plan B)
//!
//! Pure parts are unit-tested; nothing here starts processes on its own.

pub mod disks;
pub mod facts;
pub mod grubenv;
pub mod locale;
pub mod overlay;
pub mod proto;
pub mod services;

pub const RUN_DIR: &str = "/run/aftermath";
/// Unix socket of the engine (mode 0666: everyone may query, only root may
/// change things; checked per request with SO_PEERCRED).
pub const SOCKET: &str = "/run/aftermath/control.sock";
/// Directory of the current boot's session logs (written by the engine).
pub const LOGDIR_FILE: &str = "/run/aftermath/logdir";
pub const SERVICES_DIR: &str = "/etc/aftermath/services";
pub const STRAT_BIN: &str = aftermath_strata::STRAT_BIN;
pub const STRATA_SERVICE: &str = "/usr/lib/aftermath/aftermath-strata";
