//! Optional userlands. Keep image references explicit; enabling an installed
//! stratum never upgrades or replaces it. Package updates happen inside it.

use crate::config::Stratum;

/// Fedora's official base image includes bash, rpm and dnf. Skopeo selects
/// the running architecture and verifies TLS and OCI content digests.
pub const FEDORA_IMAGE: &str = "docker://quay.io/fedora/fedora:44";

pub fn image(s: &Stratum) -> Option<&'static str> {
    match (s.name.as_str(), s.kind.as_str()) {
        ("fedora", "fedora") => Some(FEDORA_IMAGE),
        _ => None,
    }
}
