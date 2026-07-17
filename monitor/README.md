# Jetstream reliability monitor

The monitor reports independent transport and useful-delivery availability for Messijo, Graze, and both baseline streams. A `data_idle_timeout` reconnect keeps delivery unavailable through recovery; its delay is recorded as client recovery time and excluded from unexplained transport downtime. Handshake, socket read/write, and peer-close failures remain transport reasons.

Live statistics expose both uptime models, delivery state, reconnect reason/totals, and client recovery duration. Hourly history uses additive, versioned SQLite fields. Older rows remain readable as `legacy_unknown`, and the dashboard exposes reduced coverage rather than inventing delivery causes.

Alert on delivery uptime/data-idle reconnects separately from transport uptime. A reachable silent stream is a delivery incident; a failed handshake/read/write is a transport incident. Always retain coverage when comparing selected windows.
