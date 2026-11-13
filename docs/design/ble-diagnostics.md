# Opt-in BLE diagnostics v1

This is a read-only extension to API 57. The client must have completed the
existing Noise setup-key handshake, then request
`robot.diagnosticsCapabilities` (result:
`{"version":1,"change_only":true,"battery_refresh_ms":60000}`) and
`robot.diagnosticsSubscribe` (result: `{"version":1,"subscribed":true}`).
Requests use JSON-RPC IDs and no parameters. Older firmware and legacy PIN
sessions have no such methods; do not downgrade, guess, or poll on the phone.
The acknowledgement precedes the first encrypted `robot.diagnostics`
notification on the same GATT Write/Notify characteristic. Duplicate subscribe
is idempotent for the current connection. A new session starts again at
sequence 1. A read-only capability request can maintain the existing secure
lease; outgoing notifications do not renew it.

One collector for secure btd reads existing local software state via
`robot.health`, `robot.policies` and `net.status`, at most once every two
seconds, with bounded IPC and no catch-up. No GATT subscription can add
polling load. Nothing opens the motor bus or adds a physical sensor read.
Battery values are adopted from the cached health reply only once per
60 seconds, provided healthy and recent bus/loop evidence is present.
`battery_age_ms` is age of btd's cached health value, not age of a physical
measurement (API 57 provides no sample timestamp). Missing or unhealthy
bus evidence clears the displayed value; no good cached sample past 65 seconds
is presented as current. No battery is JSON null; `cache_age_ms` is a
compatibility alias using zero when unknown.

Notify once for the initial snapshot, then only when rendered state changes.
Age changes alone do not send anything. The health and network values are
existing service observations, so change notification follows the next
collector tick rather than being interrupt-driven. `pose=sitting` is the
robot's explicit sitting latch; standing and actual torque are not measured
by these interfaces and are reported as unknown/null. Quiet is not offline:
track secure connection state separately. No calibration, register write,
motion or configuration call is added by this extension.
