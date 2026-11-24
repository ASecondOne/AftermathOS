# /etc/aftermath/services

One `<name>.toml` per service, supervised by aftermath-engine and controlled
with `amctl services list|status|start|stop|restart|enable|disable|reload`.
See the Aftermath README ("Services") for the file format. `heartbeat.toml`
is a small example that is off by default.
