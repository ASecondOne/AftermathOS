//! Keyboard layout, configured in ONE place:
//!
//! ```toml
//! # /etc/aftermath/aftermath.toml
//! [locale]
//! keymap = "de-latin1-nodeadkeys"   # Linux console (kbd keymap name)
//! xkb_layout = "de"                 # X11 / Wayland (future desktop)
//! xkb_variant = "nodeadkeys"
//! xkb_model = "pc105"
//! xkb_options = ""
//! ```
//!
//! At boot the engine derives /etc/vconsole.conf and
//! /etc/X11/xorg.conf.d/00-keyboard.conf from it and loads the console
//! keymap: with kbd's `loadkeys` if installed, otherwise (plan B) with the
//! built-in loader below, which feeds a precompiled binary keymap
//! (/usr/share/aftermath/keymaps/<name>.bmap, made at image build time
//! with `loadkeys -b`) to the kernel with the KDSKBENT ioctl.

use std::fs::{self, File, OpenOptions};
use std::os::fd::AsRawFd;
use std::path::Path;
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};

pub const CONFIG: &str = "/etc/aftermath/aftermath.toml";
pub const BMAP_DIR: &str = "/usr/share/aftermath/keymaps";
const LOADKEYS: &str = "/usr/bin/loadkeys";

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(default)]
pub struct Locale {
    pub keymap: String,
    pub xkb_layout: String,
    pub xkb_variant: String,
    pub xkb_model: String,
    pub xkb_options: String,
}

impl Default for Locale {
    fn default() -> Self {
        Self {
            keymap: "de-latin1-nodeadkeys".into(),
            xkb_layout: "de".into(),
            xkb_variant: "nodeadkeys".into(),
            xkb_model: "pc105".into(),
            xkb_options: String::new(),
        }
    }
}

#[derive(Deserialize, Default)]
struct File_ {
    #[serde(default)]
    locale: Locale,
}

pub fn parse(text: &str) -> Result<Locale, String> {
    toml::from_str::<File_>(text)
        .map(|f| f.locale)
        .map_err(|e| e.to_string())
}

/// The configured locale; defaults (German) if the file is missing/broken.
pub fn load(root: &Path) -> (Locale, Option<String>) {
    let p = root.join(CONFIG.trim_start_matches('/'));
    match fs::read_to_string(&p) {
        Ok(t) => match parse(&t) {
            Ok(l) => (l, None),
            Err(e) => (Locale::default(), Some(format!("{}: {e}", p.display()))),
        },
        Err(_) => (Locale::default(), None),
    }
}

pub fn valid_keymap(name: &str) -> bool {
    !name.is_empty()
        && name.len() < 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
        && !name.starts_with('.')
}

/// /etc/vconsole.conf (read by tools that expect the systemd-style file).
pub fn vconsole_conf(l: &Locale) -> String {
    format!(
        "# Generated from /etc/aftermath/aftermath.toml [locale] at boot - edit that file\n\
         # (or run `amctl keymap set <name>`), not this one.\n\
         KEYMAP={}\n",
        l.keymap
    )
}

/// /etc/X11/xorg.conf.d/00-keyboard.conf. Empty settings are left out.
pub fn xorg_conf(l: &Locale) -> String {
    let mut lines = vec![
        "# Generated from /etc/aftermath/aftermath.toml [locale] at boot - edit that file,"
            .to_string(),
        "# not this one. Read by X11 and by Wayland compositors that honour it.".to_string(),
        "Section \"InputClass\"".to_string(),
        "    Identifier \"system-keyboard\"".to_string(),
        "    MatchIsKeyboard \"on\"".to_string(),
    ];

    let options = [
        ("XkbLayout", &l.xkb_layout),
        ("XkbModel", &l.xkb_model),
        ("XkbVariant", &l.xkb_variant),
        ("XkbOptions", &l.xkb_options),
    ];
    for (key, value) in options {
        if !value.is_empty() {
            lines.push(format!("    Option \"{key}\" \"{value}\""));
        }
    }

    lines.push("EndSection".to_string());
    lines.join("\n") + "\n"
}

/// Write the derived files under `root` if they differ. Returns the changed paths.
pub fn write_derived(l: &Locale, root: &Path) -> Vec<String> {
    let mut changed = Vec::new();
    for (rel, text) in [
        ("etc/vconsole.conf", vconsole_conf(l)),
        ("etc/X11/xorg.conf.d/00-keyboard.conf", xorg_conf(l)),
    ] {
        let p = root.join(rel);
        if fs::read_to_string(&p).ok().as_deref() == Some(text.as_str()) {
            continue;
        }
        if let Some(d) = p.parent() {
            let _ = fs::create_dir_all(d);
        }
        if fs::write(&p, text).is_ok() {
            changed.push(p.display().to_string());
        }
    }
    changed
}

/// Change `keymap = ...` in aftermath.toml, keeping comments and the rest.
///
/// Line by line: inside the `[locale]` table the existing `keymap` line is
/// replaced. If the table has no such line, one is added at the end of the
/// table; if there is no `[locale]` table at all, it is appended.
pub fn set_keymap_in(text: &str, keymap: &str) -> String {
    let new_line = format!("keymap = \"{keymap}\"\n");
    let mut out = String::new();
    let mut in_locale = false;
    let mut saw_locale = false;
    let mut done = false;

    for line in text.lines() {
        let trimmed = line.trim();
        let is_table_header = trimmed.starts_with('[');
        let is_keymap_key = trimmed.split('=').next().map(str::trim) == Some("keymap");

        if is_table_header {
            // Leaving [locale] without having seen `keymap`: add it here.
            if in_locale && !done {
                out.push_str(&new_line);
                done = true;
            }
            in_locale = trimmed == "[locale]";
            saw_locale |= in_locale;
        } else if in_locale && !done && is_keymap_key {
            out.push_str(&new_line);
            done = true;
            continue; // drop the old line
        }

        out.push_str(line);
        out.push('\n');
    }

    if !done {
        if !saw_locale {
            out.push_str("\n[locale]\n");
        }
        out.push_str(&new_line);
    }
    out
}

/// `amctl keymap set`: load `name` now, save it in aftermath.toml,
/// regenerate the derived files and note it in /run/aftermath/keymap.
/// Returns how it was loaded. Nothing is saved if loading fails.
pub fn apply_keymap(root: &Path, name: &str) -> Result<String, String> {
    let how = load_keymap(root, name)?;

    let config = root.join(CONFIG.trim_start_matches('/'));
    let text = fs::read_to_string(&config).unwrap_or_default();
    fs::write(&config, set_keymap_in(&text, name))
        .map_err(|e| format!("{}: {e}", config.display()))?;

    let (locale, _) = load(root);
    write_derived(&locale, root);

    let note = root.join("run/aftermath/keymap");
    let _ = fs::write(note, format!("{name}\n"));
    Ok(how)
}

// ------------------------------------------------------------- loading ----

const KDGKBENT: libc::c_ulong = 0x4B46;
const KDSKBENT: libc::c_ulong = 0x4B47;
const MAGIC: &[u8] = b"bkeymap";
const MAX_NR_KEYMAPS: usize = 256;

#[repr(C)]
struct KbEntry {
    table: u8,
    index: u8,
    value: u16,
}

/// Binary keymap (`loadkeys -b` / BusyBox loadkmap format): "bkeymap",
/// 256 flag bytes (which modifier tables are present), then for every
/// present table N little-endian u16 key values (N = 128 in practice).
pub fn parse_bmap(data: &[u8]) -> Result<Vec<(u8, u8, u16)>, String> {
    let rest = data
        .strip_prefix(MAGIC)
        .ok_or("not a binary keymap (missing bkeymap magic)")?;

    if rest.len() < MAX_NR_KEYMAPS {
        return Err("binary keymap truncated".into());
    }

    // Which modifier tables follow, in order.
    let (flags, body) = rest.split_at(MAX_NR_KEYMAPS);
    let tables: Vec<usize> = (0..MAX_NR_KEYMAPS).filter(|&i| flags[i] != 0).collect();
    if tables.is_empty() || body.len() % (tables.len() * 2) != 0 {
        return Err("binary keymap has an unexpected size".into());
    }

    // Every table has the same number of keys, two bytes each.
    let keys = body.len() / tables.len() / 2;
    if keys == 0 || keys > 256 {
        return Err("binary keymap has an unexpected size".into());
    }

    let mut out = Vec::with_capacity(tables.len() * keys);
    for (t, chunk) in tables.iter().zip(body.chunks(keys * 2)) {
        for (k, v) in chunk.chunks(2).enumerate() {
            out.push((*t as u8, k as u8, u16::from_le_bytes([v[0], v[1]])));
        }
    }
    Ok(out)
}

/// A virtual-terminal fd for keyboard ioctls.
fn vt() -> Result<File, String> {
    for p in ["/dev/tty0", "/dev/tty1", "/dev/console"] {
        if let Ok(f) = OpenOptions::new().read(true).write(true).open(p) {
            return Ok(f);
        }
    }
    Err("no virtual terminal to load the keymap into (/dev/tty0)".into())
}

/// The built-in loader (plan B when kbd is missing): send every entry of a
/// binary keymap to the kernel with KDSKBENT. Returns how many it accepted.
pub fn load_bmap(path: &Path) -> Result<usize, String> {
    let data = fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let entries = parse_bmap(&data)?;
    let f = vt()?;

    let mut n = 0;
    for (table, index, value) in entries {
        let e = KbEntry {
            table,
            index,
            value,
        };
        // SAFETY: KDSKBENT reads one struct kbentry from the pointer.
        if unsafe { libc::ioctl(f.as_raw_fd(), KDSKBENT, &e) } == 0 {
            n += 1;
        }
    }

    if n == 0 {
        return Err("the kernel accepted no key from the keymap".into());
    }
    Ok(n)
}

/// Load `name` into the console: kbd's loadkeys if present, else the
/// built-in loader. Returns a short description of how it was loaded, or
/// why both ways failed.
pub fn load_keymap(root: &Path, name: &str) -> Result<String, String> {
    if !valid_keymap(name) {
        return Err(format!("invalid keymap name `{name}`"));
    }

    let mut failures = Vec::new();

    if root.join(LOADKEYS.trim_start_matches('/')).exists() {
        match run_loadkeys(name) {
            Ok(()) => return Ok("loadkeys".into()),
            Err(e) => failures.push(e),
        }
    }

    let bmap = root
        .join(BMAP_DIR.trim_start_matches('/'))
        .join(format!("{name}.bmap"));
    match load_bmap(&bmap) {
        Ok(keys) => return Ok(format!("built-in loader, {keys} keys")),
        Err(e) => failures.push(e),
    }

    Err(failures.join("; "))
}

fn run_loadkeys(name: &str) -> Result<(), String> {
    let output = Command::new(LOADKEYS)
        .args(["-q", "-C", "/dev/tty0", name])
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("loadkeys: {e}"))?;

    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(format!("loadkeys: {}", stderr.trim()))
    }
}

/// Self-check: what the key at the US "Y" position (keycode 21) types.
/// German layouts give 'z'. None if the keymap can't be read.
pub fn probe_y_key() -> Option<char> {
    let f = vt().ok()?;
    let mut e = KbEntry {
        table: 0,
        index: 21,
        value: 0,
    };
    // SAFETY: KDGKBENT fills in kb_value of the struct we pass.
    if unsafe { libc::ioctl(f.as_raw_fd(), KDGKBENT, &mut e) } != 0 {
        return None;
    }
    char::from_u32(u32::from(e.value & 0xff))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_german() {
        let l = parse("").unwrap();
        assert_eq!(l.keymap, "de-latin1-nodeadkeys");
        assert_eq!(l.xkb_layout, "de");
        let l = parse("[locale]\nkeymap = \"us\"\n").unwrap();
        assert_eq!(l.keymap, "us");
        assert_eq!(l.xkb_layout, "de");
    }

    #[test]
    fn derived_files() {
        let l = Locale::default();
        assert!(vconsole_conf(&l).contains("KEYMAP=de-latin1-nodeadkeys\n"));
        let x = xorg_conf(&l);
        assert!(x.contains("Option \"XkbLayout\" \"de\""));
        assert!(x.contains("Option \"XkbVariant\" \"nodeadkeys\""));
        assert!(!x.contains("XkbOptions"));
    }

    #[test]
    fn set_keymap_keeps_the_rest() {
        let t =
            "# top\n[locale]\n# console\nkeymap = \"de\"\nxkb_layout = \"de\"\n[other]\nx = 1\n";
        let n = set_keymap_in(t, "us");
        assert!(n.contains("# console\nkeymap = \"us\"\nxkb_layout"));
        assert!(n.contains("[other]\nx = 1"));
        assert_eq!(parse(&n).unwrap().keymap, "us");
        let n = set_keymap_in("[locale]\nxkb_layout = \"de\"\n[x]\n", "fr");
        assert_eq!(parse(&n).unwrap().keymap, "fr");
        let n = set_keymap_in("", "fr");
        assert_eq!(parse(&n).unwrap().keymap, "fr");
    }

    #[test]
    fn bmap() {
        let mut d = MAGIC.to_vec();
        let mut flags = [0u8; 256];
        flags[0] = 1;
        flags[1] = 1;
        d.extend_from_slice(&flags);
        for t in 0..2u16 {
            for k in 0..128u16 {
                d.extend_from_slice(&(t * 1000 + k).to_le_bytes());
            }
        }
        let e = parse_bmap(&d).unwrap();
        assert_eq!(e.len(), 256);
        assert_eq!(e[21], (0, 21, 21));
        assert_eq!(e[128 + 5], (1, 5, 1005));
        assert!(parse_bmap(b"nope").is_err());
        assert!(parse_bmap(&d[..d.len() - 1]).is_err());
    }

    #[test]
    fn keymap_names() {
        assert!(valid_keymap("de-latin1-nodeadkeys"));
        assert!(!valid_keymap("../etc/passwd"));
        assert!(!valid_keymap(""));
    }
}
