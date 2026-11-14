# Zero 3W Robot HAT audio and hardware diagnostics (test candidate)

The Yocto patch targets official **microduck-os-v1.4.0** (commit
`3806ef540e069d4f9e20b8d96d84a8eb2a8be487`), the Zero 3W image, not
the beta/face-board images. Use only on hardware wired like the Pollen
Robot HAT (I²C pins 3/5, AIC3104 at 0x18, I²S3 and MIC3R). Apply
`microduck-zero3-hat-audio-v1.4.0.patch` in a separate checkout of
[pollen-robotics/microduck_yocto](https://github.com/pollen-robotics/microduck_yocto).
See the added `docs/agents/zero3-hat-audio.md` in that checkout for its opt-in
KAS build and bench procedure. This is a kernel/DTB and mixer change, not
something a daemon-only OTA (or a Flutter update) can supply. It leaves
the stock hardware I²C3 pinmux and USB-C PD controller in place.

For the new app rows and audio tests, also install this repo's **matching
API58** daemon build and include `duck-audio-test` in its `bin/` with the
shipped `systemd/duck-audio-test.socket`,
`systemd/duck-audio-test@.service` and sysusers entry. The packaged daemon's
postinstall installs and starts the listener; a custom offline WIC overlay
must stage the same files/account and enable the socket at image creation.
Do not flash the Audio-only KAS build expecting the API58 helper; it still
uses the pinned upstream daemon. There is no new production OTA or WIC here.

## BLE contract

The existing Noise-authenticated BLE session exposes JSON-RPC v2 requests
with an ID and no parameters. The older PIN route cannot run them.

| Method | Result |
| --- | --- |
| `robot.hardwareStatus` | `version: 1` plus camera, servo, imu, battery, tof, speaker, mic rows |
| `robot.hardwareSpeakerTest` | A bounded test tone. `accepted` means ALSA playback completed, not that a person heard it. |
| `robot.hardwareMicTest` | `ok` or `quiet` with optional relative `level_percent` from one second of the HAT's right channel. No file, microphone stream or speech transcription. |

Rows use `ok`, `degraded`, `unavailable`, `unknown`, `test_pattern` or
`ready` (audio PCM enumerated, not acoustically verified). Camera stats are
producer-measured (not a phone video session) and report simulator/test input
separately. Servo/IMU read cached `robot.health` only and never arm, change
torque or move a joint. Battery uses the existing cache and is approximate.
ToF observes one bounded frame; zero range targets with a valid observation
doesn't imply a dead sensor. Audio tests may return `busy`, `unavailable` or
`error`; the test does not take over the PCM device from another process.

## Bench acceptance and RAM

1. Check `aplay -l` and `arecord -l` both show `aic3104`, and the mixer-init
   journal indicates successful routing. Confirm USB OTG/PD and existing
   ToF operation after boot. Verify playback by ear and microphone level
   with quiet versus speech; then verify the Flutter page on a phone.
2. Read RAM at idle, during a real 1080p stream, on reconnect, while running
   the motion policy in a safely controlled test setup, and during a separate
   on-demand speech test. Capture the minimum `MemAvailable`, not just free
   memory. Hardware camera/MPP allocation may also show in CMA. Compare
   phases on the same image and confirm no OOMs or control-loop regressions.

```sh
grep -E '^(MemTotal|MemAvailable|SwapTotal|SwapFree|CmaTotal|CmaFree):' /proc/meminfo
for unit in robotd mediad duck-gateway btd tofd configd updaterd; do
  pid="$(systemctl show -p MainPID --value "$unit")"
  case "$pid" in ''|0|*[!0-9]*) continue;; esac
  printf '%s: ' "$unit"
  sudo awk '/^Pss:/{print $2 " " $3}' "/proc/$pid/smaps_rollup"
done
sudo journalctl -k -b --no-pager | grep -Ei 'out of memory|oom-kill'
```

For independent local Mandarin recognition (no movements or network), see
[the optional Radxa voice demo](../../examples/radxa-voice/README.md).
It is not part of the boot services or memory budget above.
