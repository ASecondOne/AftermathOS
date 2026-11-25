# Aftermath development rules

- Write all new Aftermath implementation, test helpers and automation in Rust.
  Do not add Python, shell or JavaScript scripts, including embedded scripts
  used as test programs. Declarative configuration and documentation are fine.
- Existing external components (Linux, Plasma, PAM, udev, D-Bus and system
  utilities) may remain and may be called from Rust. Reusing them does not mean
  their implementation is Rust; describe that distinction accurately.
- Advance desktop support one concrete, tested step at a time. Preserve the
  existing rescue/fallback paths and report remaining Plasma prerequisites.
