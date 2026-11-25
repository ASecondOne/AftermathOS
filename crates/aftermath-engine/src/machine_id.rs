//! One machine identity shared by libdbus and other Linux libraries.
//! Preserve existing identities; refuse invalid or conflicting nonempty files.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{symlink, OpenOptionsExt};
use std::path::Path;

fn read_id(path: &Path) -> Result<Option<String>, String> {
    let text = match fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    let id = text.trim();
    if id.is_empty() || id == "uninitialized" {
        return Ok(None);
    }
    if id.len() != 32 || !id.bytes().all(|c| c.is_ascii_hexdigit()) || id.bytes().all(|c| c == b'0')
    {
        return Err(format!(
            "{} has an invalid machine ID; refusing to replace a nonempty identity",
            path.display()
        ));
    }
    Ok(Some(id.to_ascii_lowercase()))
}

pub(crate) fn random_id() -> Result<String, String> {
    let mut bytes = [0u8; 16];
    File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .map_err(|e| format!("machine ID randomness: {e}"))?;
    // The file format is 128 random bits encoded as 32 hexadecimal digits.
    if bytes == [0; 16] {
        return Err("machine ID randomness returned an all-zero identity".into());
    }
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

pub fn prepare(canonical: &Path, legacy: &Path) -> Result<String, String> {
    let current = read_id(canonical)?;
    let old = read_id(legacy)?;
    if current
        .as_ref()
        .zip(old.as_ref())
        .is_some_and(|(a, b)| a != b)
    {
        return Err(format!(
            "{} and {} disagree; refusing to change machine identity",
            canonical.display(),
            legacy.display()
        ));
    }
    let id = match current.as_ref().or(old.as_ref()) {
        Some(id) => id.clone(),
        None => random_id()?,
    };
    if current.is_none() {
        let parent = canonical
            .parent()
            .ok_or("machine ID has no parent directory")?;
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        let temp = parent.join(format!(".machine-id-{}", random_id()?));
        let result = (|| -> std::io::Result<()> {
            let mut f = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o444)
                .open(&temp)?;
            writeln!(f, "{id}")?;
            f.sync_all()?;
            fs::rename(&temp, canonical)?;
            File::open(parent)?.sync_all()
        })();
        if let Err(e) = result {
            let _ = fs::remove_file(temp);
            return Err(format!(
                "persist machine ID at {}: {e}",
                canonical.display()
            ));
        }
    }
    // A dangling legacy symlink may now resolve to the newly written ID.
    // Matching existing regular files are also valid and stay untouched.
    if read_id(legacy)?.is_none() {
        let parent = legacy
            .parent()
            .ok_or("legacy machine ID has no parent directory")?;
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        let temp = parent.join(format!(".machine-id-{}", random_id()?));
        symlink(canonical, &temp).map_err(|e| e.to_string())?;
        if let Err(e) = fs::rename(&temp, legacy) {
            let _ = fs::remove_file(temp);
            return Err(format!("link {}: {e}", legacy.display()));
        }
        File::open(parent)
            .and_then(|f| f.sync_all())
            .map_err(|e| e.to_string())?;
    }
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Root(PathBuf);
    impl Root {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let root = std::env::temp_dir().join(format!(
                "am-machine-id-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&root).unwrap();
            Self(root)
        }
        fn ids(&self) -> (std::path::PathBuf, std::path::PathBuf) {
            (self.0.join("machine-id"), self.0.join("legacy-id"))
        }
    }
    impl Drop for Root {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn initializes_once_and_keeps_legacy_in_sync() {
        let root = Root::new();
        let (canonical, legacy) = root.ids();
        fs::write(&canonical, "").unwrap();
        let id = prepare(&canonical, &legacy).unwrap();
        assert_eq!(id.len(), 32);
        assert!(id.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_eq!(fs::read_link(&legacy).unwrap(), canonical);
        assert_eq!(prepare(&canonical, &legacy).unwrap(), id);
        assert_eq!(fs::read_to_string(legacy).unwrap().trim(), id);
    }

    #[test]
    fn reuses_a_legacy_identity_and_accepts_dangling_canonical_link() {
        let root = Root::new();
        let (canonical, legacy) = root.ids();
        let id = "1234567890abcdef1234567890abcdef";
        fs::write(&legacy, format!("{id}\n")).unwrap();
        assert_eq!(prepare(&canonical, &legacy).unwrap(), id);
        assert_eq!(fs::read_to_string(&canonical).unwrap().trim(), id);
        fs::remove_file(&canonical).unwrap();
        fs::remove_file(&legacy).unwrap();
        symlink(&canonical, &legacy).unwrap();
        assert_eq!(
            prepare(&canonical, &legacy).unwrap(),
            read_id(&legacy).unwrap().unwrap()
        );
    }

    #[test]
    fn refuses_invalid_or_conflicting_ids_without_overwriting_them() {
        let root = Root::new();
        let (canonical, legacy) = root.ids();
        for invalid in ["wrong", "00000000000000000000000000000000"] {
            fs::write(&canonical, invalid).unwrap();
            assert!(prepare(&canonical, &legacy).is_err());
            assert_eq!(fs::read_to_string(&canonical).unwrap(), invalid);
        }
        let a = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let b = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        fs::write(&canonical, a).unwrap();
        fs::write(&legacy, b).unwrap();
        assert!(prepare(&canonical, &legacy)
            .unwrap_err()
            .contains("disagree"));
        assert_eq!(fs::read_to_string(canonical).unwrap(), a);
        assert_eq!(fs::read_to_string(legacy).unwrap(), b);
    }
}
