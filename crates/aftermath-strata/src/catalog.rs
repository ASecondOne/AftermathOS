//! Optional userlands that `amctl enable <name>` can download.
//!
//! Image references are explicit and pinned; enabling an installed stratum
//! never upgrades or replaces it. Package updates happen inside it (`dnf`).

use crate::config::Stratum;

/// Fedora 44's official base image (it includes bash, rpm and dnf), pinned
/// to the multi-arch index digest of `quay.io/fedora/fedora:44` as of
/// 2026-11-25; skopeo picks the entry for the running architecture.
///
/// What this guarantees: TLS to quay.io, and every byte downloaded must
/// match this digest (skopeo checks the manifests and layer digests). What
/// it doesn't: there is no publisher signature check (no sigstore policy).
/// To move to a newer image, look up the tag's digest and update it here.
pub const FEDORA_IMAGE: &str = "docker://quay.io/fedora/fedora@sha256:88551ae57826f9eaeb70b9016bb54f5758cbe1a724bb731d9ec211c274f55f05";

/// The image to download for `s`, if Aftermath knows how to install it.
/// Matching on both name and kind keeps a renamed entry from pulling Fedora.
pub fn image(s: &Stratum) -> Option<&'static str> {
    match (s.name.as_str(), s.kind.as_str()) {
        ("fedora", "fedora") => Some(FEDORA_IMAGE),
        _ => None,
    }
}
