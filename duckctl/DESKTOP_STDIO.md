# MicroDuck desktop provisioning helper

Build the existing CLI on a Windows x64 or macOS host with
`cargo build --release --locked -p duckctl --bin duckctl`. The Flutter desktop
client uses the new `desktop-stdio-v1` entry point for Wi-Fi setup; stock
`duckctl` builds without that entry point are not substitutes.

The Flutter app starts the sibling executable with that one fixed argument and
sends one newline-terminated JSON object on a pipe. It does not put secrets in
arguments, environment variables, settings files or diagnostics. The helper does
not read the normal CLI's setup-code cache, infer a default PIN, or log requests.
`scan` connects to no device. An authenticated Wi-Fi operation requires an
explicit name and matching OS-local peripheral ID found by a scan, plus exactly
one of a Noise setup code and an explicitly selected legacy PIN. It validates
the GATT service before transmitting credentials. Setup code failures do not
fall back to PIN. The legacy PIN mode provides less peer assurance than Noise;
use it only with your known, nearby legacy board.

- Operations: `scan`, `wifiStatus`, `wifiScan`, `wifiConnect`. No generic
  method dispatch, drivetrain, shell, updater, or remote logging interface.
- Requests use `version: 1`, `op`, and, except for scanning, `device_id`,
  `name`, and `auth` containing exactly one of `setup_code` and `pin`.
  Connect also requires `ssid`, with optional `passphrase`.
- Replies contain exactly one object: `{version:1,ok:true,result:…}` or
  `{version:1,ok:false,error:"…"}`. Failures use fixed error codes and do not
  echo credentials, raw remote RPC errors, or subprocess diagnostics.

Only the local host can run this interface. A timed-out connect can have
reached the robot; read Wi-Fi status before trying again. To ship a desktop
package, build the helper for the same architecture as the app and sign
the macOS nested tool with the app's team and inherited sandbox as documented
in the Flutter project. No helper is sent to the robot.
