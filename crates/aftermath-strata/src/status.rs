//! /run/aftermath/strata: which strata are running, written by
//! aftermath-strata (`<name> <state>` per line, e.g. `ubuntu up`).

use crate::at;

pub const STATUS: &str = "/run/aftermath/strata";

/// `(name, state)` per line; empty when strata were never set up.
pub fn read() -> Vec<(String, String)> {
    std::fs::read_to_string(at(STATUS))
        .unwrap_or_default()
        .lines()
        .filter_map(|line| line.split_once(' '))
        .map(|(name, state)| (name.to_owned(), state.to_owned()))
        .collect()
}

/// The guest strata that are up right now (the host is always up).
pub fn up() -> Vec<String> {
    read()
        .into_iter()
        .filter(|(_, state)| state == "up")
        .map(|(name, _)| name)
        .collect()
}
