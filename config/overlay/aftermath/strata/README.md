# /aftermath/strata

Each directory here is one stratum (a complete userland), Bedrock style.
The host (Arch) is / itself; other strata are bind-mounted here at boot by
aftermath-strata from the strata disk (/aftermath/store). Configuration:
/etc/aftermath/strata.toml. Run a command in a stratum with
`strat <stratum> <command>`; cross launchers live in /aftermath/cross.
