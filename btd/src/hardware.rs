//! Small, opt-in hardware diagnostics for a Noise-authenticated peer.
//! No servo writes, camera frames, or raw microphone data cross this endpoint.
use std::path::Path;
use std::time::Duration;

use duck_ipc_proto as proto;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::time::timeout;

use crate::diagnostics::{self, Shared};
use crate::upstream::Sockets;

const AUDIO_SOCKET: &str = "/run/duck-audio-test.sock";
const SNAPSHOT_TIMEOUT: Duration = Duration::from_millis(800);

#[derive(Clone, Copy)]
pub(crate) enum Action {
    Status,
    Speaker,
    Mic,
}

pub(crate) fn parse(line: &str) -> Option<Result<(proto::Id, Action), Option<proto::Id>>> {
    let req: proto::Request = serde_json::from_str(line).ok()?;
    let action = match req.method.as_str() {
        "robot.hardwareStatus" => Action::Status,
        "robot.hardwareSpeakerTest" => Action::Speaker,
        "robot.hardwareMicTest" => Action::Mic,
        _ => return None,
    };
    // Require an ID, and refuse any payload so the peer cannot select paths, gain or duration.
    let valid = req.jsonrpc == "2.0"
        && req
            .params
            .as_ref()
            .is_none_or(|p| p.is_null() || p.as_object().is_some_and(serde_json::Map::is_empty));
    Some(match (valid, req.id) {
        (true, Some(id)) => Ok((id, action)),
        (_, id) => Err(id),
    })
}

pub(crate) async fn execute(action: Action, sockets: Sockets, shared: Shared) -> Value {
    let audio = Path::new(AUDIO_SOCKET);
    match action {
        Action::Status => {
            let (health, tof, sound) = tokio::join!(
                diagnostics::query(&sockets.robot, proto::Call::RobotHealth),
                tof_info(Path::new(proto::socket::TOF)),
                audio_call(audio, b"status\n", SNAPSHOT_TIMEOUT),
            );
            let battery = battery_info(health.as_ref(), shared.hardware_battery());
            let (servo, imu) = robot_parts(health.as_ref());
            let camera = camera_info(proto::read_camera_stats(), proto::camera_stats_age());
            json!({
                "version": 1,
                "camera": camera, "servo": servo, "imu": imu, "battery": battery, "tof": tof,
                "speaker": audio_part(sound.as_ref(), "speaker"),
                "mic": audio_part(sound.as_ref(), "mic"),
            })
        }
        Action::Mic | Action::Speaker => {
            let cmd: &[u8] = if matches!(action, Action::Mic) {
                b"mic\n"
            } else {
                b"speaker\n"
            };
            test_result(
                action,
                audio_call(audio, cmd, Duration::from_secs(5))
                    .await
                    .as_ref(),
            )
        }
    }
}

fn metric(state: &str) -> Value {
    json!({ "state": state })
}

fn test_result(action: Action, reply: Option<&Value>) -> Value {
    let speaker = matches!(action, Action::Speaker);
    let state = match reply.and_then(|r| r.get("state")).and_then(Value::as_str) {
        Some("accepted") if speaker => "accepted",
        Some("ok") if !speaker => "ok",
        Some("quiet") if !speaker => "quiet",
        Some("unavailable") | None => "unavailable",
        Some("busy") => "busy",
        _ => "error",
    };
    let mut result = json!({ "version": 1, "state": state });
    if matches!(state, "ok" | "quiet")
        && let Some(level) = reply
            .and_then(|r| r.get("level_percent"))
            .and_then(Value::as_u64)
            .filter(|v| *v <= 100)
    {
        result["level_percent"] = json!(level);
    }
    result
}

fn audio_part(value: Option<&Value>, key: &str) -> Value {
    match value.and_then(|v| v.get(key)?.get("state")?.as_str()) {
        Some("ready") => metric("ready"),
        Some("unavailable") | None => metric("unavailable"),
        _ => metric("unknown"),
    }
}

async fn audio_call(path: &Path, command: &[u8], deadline: Duration) -> Option<Value> {
    timeout(deadline, async {
        let mut stream = UnixStream::connect(path).await.ok()?;
        stream.write_all(command).await.ok()?;
        let mut reader = BufReader::new(stream);
        let reply: Value = bounded_line(&mut reader, 512).await?;
        (reply.get("version")?.as_u64() == Some(1)).then_some(reply)
    })
    .await
    .ok()
    .flatten()
}

async fn bounded_line(reader: &mut BufReader<UnixStream>, max: u64) -> Option<Value> {
    let mut line = Vec::new();
    reader.take(max).read_until(b'\n', &mut line).await.ok()?;
    (line.last() == Some(&b'\n'))
        .then(|| serde_json::from_slice(&line).ok())
        .flatten()
}

fn camera_info(stats: Option<proto::CameraStats>, age: Option<Duration>) -> Value {
    let (Some(stats), Some(age)) = (stats, age) else {
        return metric("unknown");
    };
    if age > proto::CAMERA_STATS_MAX_AGE || stats.fps <= 0.0 {
        return metric("degraded");
    }
    let state = match stats.physical_input {
        Some(false) => "test_pattern",
        None => "unknown",
        Some(true) if stats.target_fps == 0 => "unknown",
        Some(true) if stats.fps < f64::from(stats.target_fps) * 0.9 => "degraded",
        Some(true) => "ok",
    };
    json!({"state": state, "width": stats.width, "height": stats.height, "fps": stats.fps})
}

fn robot_parts(raw: Option<&Value>) -> (Value, Value) {
    let empty = || (metric("unknown"), metric("unknown"));
    let Some(raw) = raw else { return empty() };
    match raw.pointer("/bus/physical_io").and_then(Value::as_bool) {
        Some(false) => return (metric("test_pattern"), metric("test_pattern")),
        None => return empty(),
        _ => {}
    }
    let Ok(health) = serde_json::from_value::<proto::HealthResult>(raw.clone()) else {
        return empty();
    };
    let fresh = health
        .control_loop
        .is_some_and(|lp| lp.ticks > 0 && lp.last_tick_age_ms < 1000);
    let missing = proto::JOINT_IDS
        .iter()
        .filter(|id| health.bus.missing.contains(id))
        .count();
    let expected = proto::JOINT_IDS.len();
    let servo = if missing > 0 {
        json!({"state": "degraded", "responding": expected - missing, "expected": expected})
    } else if health.bus.consecutive_errors > 0 {
        metric("degraded")
    } else if (fresh && health.bus.startup_failures == 0) || health.imu.is_some_and(|m| m.missing) {
        json!({"state":"ok", "responding": expected, "expected": expected})
    } else if health.bus.startup_failures > 0 {
        metric("degraded")
    } else {
        metric("unknown")
    };
    let imu = match health.imu {
        Some(imu) if imu.missing || imu.frozen() => metric("degraded"),
        Some(imu) if fresh && imu.ready && health.bus.consecutive_errors == 0 => metric("ok"),
        _ => metric("unknown"),
    };
    (servo, imu)
}

fn battery_info(health: Option<&Value>, cached: Option<(u64, u32, u32)>) -> Value {
    match health
        .and_then(|v| v.pointer("/bus/physical_io"))
        .and_then(Value::as_bool)
    {
        Some(false) => metric("test_pattern"),
        Some(true) if health.is_some_and(diagnostics::bus_ready_for_battery) => match cached {
            Some((age, pct, mv)) => json!({"state":"ok", "age_ms":age, "percent":pct, "mv":mv}),
            None => metric("unknown"),
        },
        _ => metric("unknown"),
    }
}

async fn tof_info(path: &Path) -> Value {
    timeout(SNAPSHOT_TIMEOUT, async {
        let mut stream = UnixStream::connect(path).await.ok()?;
        let id = proto::Id::Number(1);
        let mut line =
            serde_json::to_vec(&proto::Request::call(id.clone(), &proto::Call::TofStream)).ok()?;
        line.push(b'\n');
        stream.write_all(&line).await.ok()?;
        let mut reader = BufReader::new(stream);
        let reply: proto::Response =
            serde_json::from_value(bounded_line(&mut reader, 4096).await?).ok()?;
        if reply.jsonrpc != "2.0" || reply.id.as_ref() != Some(&id) || reply.error.is_some() {
            return None;
        }
        let ack: proto::TofStreamResult = serde_json::from_value(reply.result?).ok()?;
        if !ack.accepted || ack.sensor.is_none() {
            return Some(metric("unavailable"));
        }
        if matches!(ack.sensor.as_deref(), Some("sim" | "fake")) {
            return Some(metric("test_pattern"));
        }
        let notice: proto::Request =
            serde_json::from_value(bounded_line(&mut reader, 4096).await?).ok()?;
        if notice.jsonrpc != "2.0" || notice.id.is_some() {
            return None;
        }
        let frame = notice.as_tof_frame()?;
        Some(tof_frame(&frame))
    })
    .await
    .ok()
    .flatten()
    .unwrap_or_else(|| metric("unknown"))
}

fn tof_frame(frame: &proto::TofFrame) -> Value {
    let n = usize::from(frame.rows) * usize::from(frame.cols);
    let now = proto::clock::monotonic_ns();
    if n == 0
        || n > 64
        || frame.distance_mm.len() != n
        || frame.status.len() != n
        || frame.t_ns == 0
        || frame.t_ns > now
        || now - frame.t_ns > 2_000_000_000
    {
        return metric("unknown");
    }
    // ST statuses: 5/9 = usable measurement, 255 = valid observation with no target.
    // A frame facing open space is not itself proof the ToF is broken.
    let valid = frame
        .distance_mm
        .iter()
        .zip(&frame.status)
        .filter(|(mm, status)| **mm > 0 && matches!(**status, 5 | 9))
        .count();
    let state = if valid > 0 || frame.status.contains(&255) {
        "ok"
    } else {
        "degraded"
    };
    json!({"state":state, "valid_zones":valid})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_requests_cannot_supply_settings_or_be_notifications() {
        assert!(matches!(
            parse(r#"{"jsonrpc":"2.0","id":1,"method":"robot.hardwareMicTest"}"#),
            Some(Ok((_, Action::Mic)))
        ));
        assert!(matches!(
            parse(
                r#"{"jsonrpc":"2.0","id":1,"method":"robot.hardwareSpeakerTest","params":{"volume":100}}"#
            ),
            Some(Err(Some(_)))
        ));
        assert!(matches!(
            parse(r#"{"jsonrpc":"2.0","method":"robot.hardwareMicTest"}"#),
            Some(Err(None))
        ));
        assert!(parse(r#"{"jsonrpc":"2.0","id":1,"method":"robot.move"}"#).is_none());
    }

    #[test]
    fn healthy_simulation_and_stale_camera_do_not_pass_physical_checks() {
        let hw = json!({"bus":{"physical_io":false}});
        assert_eq!(robot_parts(Some(&hw)).0, metric("test_pattern"));
        assert_eq!(
            battery_info(Some(&hw), Some((5, 80, 7400))),
            metric("test_pattern")
        );
        let stats = proto::CameraStats {
            physical_input: Some(false),
            fps: 30.0,
            target_fps: 30,
            width: 1920,
            height: 1080,
            format: "UYVY".into(),
            frames: 30,
            dropped: 0,
            consumers: 0,
        };
        assert_eq!(
            camera_info(Some(stats.clone()), Some(Duration::from_secs(1)))["state"],
            "test_pattern"
        );
        assert_eq!(
            camera_info(Some(stats), Some(Duration::from_secs(9))),
            metric("degraded")
        );
    }

    #[test]
    fn fresh_tof_no_target_is_distinct_from_a_broken_observation() {
        let mut frame = proto::TofFrame {
            rows: 1,
            cols: 1,
            distance_mm: vec![0],
            status: vec![255],
            t_ns: proto::clock::monotonic_ns(),
            ..Default::default()
        };
        assert_eq!(tof_frame(&frame), json!({"state":"ok","valid_zones":0}));
        frame.status[0] = 3;
        assert_eq!(tof_frame(&frame)["state"], "degraded");
        frame.t_ns = 0;
        assert_eq!(tof_frame(&frame), metric("unknown"));
    }

    #[test]
    fn microphone_only_relays_bounded_aggregate() {
        let sample = json!({"state":"ok", "level_percent":22, "pcm":[42]});
        assert_eq!(
            test_result(Action::Mic, Some(&sample)),
            json!({"version":1,"state":"ok","level_percent":22})
        );
        assert_eq!(
            test_result(Action::Speaker, Some(&sample)),
            json!({"version":1,"state":"error"})
        );
    }
}
