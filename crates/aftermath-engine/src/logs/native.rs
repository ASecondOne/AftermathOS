//! Receive package log datagrams without starting journald or a syslog daemon.
//! Only diagnostic fields are decoded; no received data is executed or trusted.
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::{
    fs::{FileTypeExt, MetadataExt, PermissionsExt},
    net::UnixDatagram,
};
use std::path::Path;

/// Some packaged programs send directly to the journal socket instead of stderr.
pub fn start_native_receivers() {
    for (path, journal) in [("/run/systemd/journal/socket", true), ("/dev/log", false)] {
        if let Err(error) = receiver(path, journal) {
            crate::warn(&format!("native log receiver {path}: {error}"));
        }
    }
}

/// Reclaim only a stale socket owned by this engine's user; keep active loggers.
fn receiver(path: &str, journal: bool) -> std::io::Result<()> {
    let path = Path::new(path);
    fs::create_dir_all(path.parent().expect("absolute socket path"))?;
    if let Ok(metadata) = fs::symlink_metadata(path) {
        if !metadata.file_type().is_socket() || metadata.uid() != unsafe { libc::geteuid() } {
            return Ok(());
        }
        let probe = UnixDatagram::unbound()?;
        match probe.send_to(
            b"MESSAGE=Aftermath log receiver probe\nSYSLOG_IDENTIFIER=aftermath\n",
            path,
        ) {
            Ok(_) => return Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::ConnectionRefused => {
                fs::remove_file(path)?
            }
            Err(_) => return Ok(()),
        }
    }
    let socket = UnixDatagram::bind(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o666))?;
    std::thread::Builder::new()
        .name("native-log".into())
        .spawn(move || {
            let mut buffer = [0; 65536];
            while let Ok(size) = socket.recv(&mut buffer) {
                let (identifier, message) = if journal {
                    decode(&buffer[..size])
                } else {
                    (
                        "syslog".into(),
                        String::from_utf8_lossy(&buffer[..size])
                            .trim_end_matches('\0')
                            .into(),
                    )
                };
                if message.is_empty() {
                    continue;
                }
                let name = if identifier.to_ascii_lowercase().starts_with("sddm") {
                    "sddm"
                } else {
                    "syslog"
                };
                if let Ok(mut file) = OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(format!("/run/aftermath/{name}.log"))
                {
                    let _ = writeln!(file, "[{identifier}] {message}");
                }
            }
        })?;
    Ok(())
}

/// Support normal and length-prefixed native journal fields, bounded by a packet.
fn decode(packet: &[u8]) -> (String, String) {
    let mut identifier = "journal".to_string();
    let mut message = String::new();
    let mut offset = 0;
    while offset < packet.len() {
        let Some(end) = packet[offset..].iter().position(|byte| *byte == b'\n') else {
            break;
        };
        let line = &packet[offset..offset + end];
        offset += end + 1;
        let (key, value) = if let Some(equal) = line.iter().position(|byte| *byte == b'=') {
            (&line[..equal], &line[equal + 1..])
        } else {
            let Some(length) = packet.get(offset..offset + 8) else {
                break;
            };
            let Ok(length) =
                usize::try_from(u64::from_le_bytes(length.try_into().expect("eight bytes")))
            else {
                break;
            };
            offset += 8;
            let Some(end) = offset.checked_add(length) else {
                break;
            };
            let Some(value) = packet.get(offset..end) else {
                break;
            };
            offset = end.saturating_add(1);
            (line, value)
        };
        match key {
            b"SYSLOG_IDENTIFIER" => identifier = String::from_utf8_lossy(value).into(),
            b"MESSAGE" => message = String::from_utf8_lossy(value).into(),
            _ => {}
        }
    }
    (identifier, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn decodes_native_fields_without_interpreting_message_contents() {
        assert_eq!(
            decode(b"PRIORITY=6\nSYSLOG_IDENTIFIER=sddm\nMESSAGE=Session starting\n"),
            ("sddm".into(), "Session starting".into())
        );
        let mut packet = b"MESSAGE\n".to_vec();
        packet.extend_from_slice(&5u64.to_le_bytes());
        packet.extend_from_slice(b"a\nb=c\n");
        assert_eq!(decode(&packet).1, "a\nb=c");
    }
}
