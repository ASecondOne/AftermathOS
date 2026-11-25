//! Declarative graphical boot settings, shared by the image builder and engine.
use serde::Deserialize;

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub enabled: bool,
    pub development_accounts: bool,
    /// Build local Plasma checkouts instead of using the distribution packages.
    pub source_build: bool,
}
#[derive(Default, Deserialize)]
#[serde(default)]
struct Config {
    desktop: Settings,
}
pub fn parse(text: &str) -> Result<Settings, String> {
    toml::from_str::<Config>(text)
        .map(|c| c.desktop)
        .map_err(|e| e.to_string())
}
pub fn settings() -> Result<Settings, String> {
    parse(&std::fs::read_to_string("/etc/aftermath/aftermath.toml").map_err(|e| e.to_string())?)
}

/// Keep graphical services and console allocation on the same boot policy.
pub fn enabled_for_boot(cmdline: &str) -> bool {
    settings().is_ok_and(|settings| settings.enabled)
        && !cmdline.split_whitespace().any(|arg| {
            matches!(
                arg,
                "aftermath.desktop=off"
                    | "aftermath.test=session-smoke"
                    | "aftermath.test=live-smoke"
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn graphical_boot_is_explicit_and_bootstrap_can_be_disabled() {
        assert!(!parse("").unwrap().enabled);
        let config = parse("[desktop]\nenabled=true\ndevelopment_accounts=false\n").unwrap();
        assert!(config.enabled && !config.development_accounts);
        assert!(parse("[desktop]\nenabled='yes'\n").is_err());
    }
}
