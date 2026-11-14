//! Read-only, opt-in diagnostics for Noise-authenticated BLE sessions.
use std::time::Duration;

use duck_ipc_proto as proto;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::watch;
use tokio::time::{Instant, timeout};

use crate::upstream::Sockets;

pub const CAPABILITIES: &str = "robot.diagnosticsCapabilities";
pub const SUBSCRIBE: &str = "robot.diagnosticsSubscribe";
const NOTIFICATION: &str = "robot.diagnostics";
pub const STATUS_POLL: Duration = Duration::from_secs(2);
const BATTERY_REFRESH: Duration = Duration::from_secs(60);
const BATTERY_STALE: Duration = Duration::from_secs(65);
const IPC_TIMEOUT: Duration = Duration::from_millis(400);

#[derive(Clone)]
pub struct Shared(watch::Receiver<Snapshot>);

impl Default for Shared {
    fn default() -> Self {
        Self(watch::channel(Snapshot::default()).1)
    }
}

#[derive(Default)]
struct BatteryCache {
    last_attempt: Option<Instant>,
    sample: Option<(Instant, u32, u32)>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Display {
    battery: Option<(u32, u32)>,
    health: &'static str,
    pose: &'static str,
    network: &'static str,
}

#[derive(Clone, PartialEq, Eq)]
pub struct Snapshot {
    display: Display,
    battery_age_ms: Option<u64>,
}

impl Default for Snapshot {
    fn default() -> Self {
        Self {
            display: Display {
                battery: None,
                health: "unknown",
                pose: "unknown",
                network: "unknown",
            },
            battery_age_ms: None,
        }
    }
}

#[derive(Default)]
pub struct Subscriber {
    last: Option<Display>,
    sequence: u32,
}

impl Subscriber {
    pub fn notify(&mut self, snapshot: Snapshot) -> Option<String> {
        if self.last.as_ref() == Some(&snapshot.display) {
            return None;
        }
        if self.sequence >= i32::MAX as u32 {
            return None;
        }
        self.sequence += 1;
        let d = snapshot.display;
        let out = json!({
            "jsonrpc": "2.0", "method": NOTIFICATION,
            "params": {
                "version": 1, "sequence": self.sequence,
                "battery_age_ms": snapshot.battery_age_ms,
                "cache_age_ms": snapshot.battery_age_ms.unwrap_or(0),
                "battery_percent": d.battery.map(|x| x.0),
                "battery_mv": d.battery.map(|x| x.1),
                "health": d.health, "pose": d.pose, "network": d.network,
                "torque_enabled": null
            }
        });
        self.last = Some(d);
        Some(out.to_string())
    }
}

pub fn request(line: &str) -> Option<(String, bool)> {
    let req: proto::Request = serde_json::from_str(line).ok()?;
    let subscribe = match req.method.as_str() {
        CAPABILITIES => false,
        SUBSCRIBE => true,
        _ => return None,
    };
    // Both methods require a request id: a notification cannot silently opt in.
    let Some(id) = req.id else {
        return Some((String::new(), false));
    };
    let valid = req.jsonrpc == "2.0"
        && req
            .params
            .as_ref()
            .is_none_or(|p| p.is_null() || p.as_object().is_some_and(|o| o.is_empty()));
    let response = if !valid {
        proto::Response::err(
            Some(id),
            proto::Error::new(proto::code::INVALID_PARAMS, "expected no parameters"),
        )
    } else if subscribe {
        proto::Response::ok(Some(id), &json!({"version": 1, "subscribed": true}))
    } else {
        proto::Response::ok(
            Some(id),
            &json!({"version": 1, "change_only": true, "battery_refresh_ms": 60000}),
        )
    };
    Some((
        serde_json::to_string(&response).expect("diagnostics response"),
        subscribe && valid,
    ))
}

pub(crate) async fn query(path: &std::path::Path, call: proto::Call) -> Option<Value> {
    timeout(IPC_TIMEOUT, async {
        let mut conn = UnixStream::connect(path).await.ok()?;
        let mut bytes =
            serde_json::to_vec(&proto::Request::call(proto::Id::Number(1), &call)).ok()?;
        bytes.push(b'\n');
        conn.write_all(&bytes).await.ok()?;
        let mut reader = BufReader::new(conn);
        let mut response = Vec::new();
        use tokio::io::AsyncReadExt;
        let n = (&mut reader)
            .take(64 * 1024)
            .read_until(b'\n', &mut response)
            .await
            .ok()?;
        if n == 0 || response.last() != Some(&b'\n') {
            return None;
        }
        let reply: proto::Response = serde_json::from_slice(&response).ok()?;
        if reply.jsonrpc != "2.0" || reply.id != Some(proto::Id::Number(1)) || reply.error.is_some()
        {
            return None;
        }
        reply.result
    })
    .await
    .ok()
    .flatten()
}

fn battery(value: &Value) -> Option<(u32, u32)> {
    let percent = value.get("percent")?.as_f64()?;
    let mv = value.get("volts")?.as_f64()? * 1000.0;
    if !(0.0..=100.0).contains(&percent) || !(1.0..=100_000.0).contains(&mv) {
        return None;
    }
    Some((percent.round() as u32, mv.round() as u32))
}

pub(crate) fn bus_ready_for_battery(value: &Value) -> bool {
    value.get("healthy").and_then(Value::as_bool) == Some(true)
        && value
            .pointer("/bus/consecutive_errors")
            .and_then(Value::as_u64)
            == Some(0)
        && value
            .pointer("/bus/startup_failures")
            .and_then(Value::as_u64)
            == Some(0)
        && value
            .pointer("/bus/missing")
            .is_none_or(|missing| missing.as_array().is_some_and(Vec::is_empty))
        && value
            .pointer("/control_loop/ticks")
            .and_then(Value::as_u64)
            .is_some_and(|ticks| ticks > 0)
        && value
            .pointer("/control_loop/last_tick_age_ms")
            .and_then(Value::as_u64)
            .is_some_and(|age| age < 1000)
}

impl BatteryCache {
    fn observe(&mut self, now: Instant, health: Option<&Value>) -> Option<(u64, u32, u32)> {
        if self
            .last_attempt
            .is_none_or(|t| now.duration_since(t) >= BATTERY_REFRESH)
        {
            self.last_attempt = Some(now);
            if let Some((pct, mv)) = health.and_then(|h| h.get("battery")).and_then(battery) {
                self.sample = Some((now, pct, mv));
            }
        }
        self.sample.and_then(|(at, pct, mv)| {
            let age = now.duration_since(at);
            (age <= BATTERY_STALE).then_some((age.as_millis() as u64, pct, mv))
        })
    }
}

impl Shared {
    pub fn channel() -> (watch::Sender<Snapshot>, Self) {
        let (tx, rx) = watch::channel(Snapshot::default());
        (tx, Self(rx))
    }

    pub async fn changed(&mut self) -> Result<(), watch::error::RecvError> {
        self.0.changed().await
    }
    /// Read the same cached battery displayed by the notification, without consuming a
    /// pending change or sending a new request to the motor bus.
    pub(crate) fn hardware_battery(&self) -> Option<(u64, u32, u32)> {
        let snapshot = self.0.borrow();
        let (pct, mv) = snapshot.display.battery?;
        Some((snapshot.battery_age_ms?, pct, mv))
    }

    pub fn mark_current(&mut self) -> Snapshot {
        self.0.borrow_and_update().clone()
    }
}

/// Runs once per secure btd process, independently of BLE connections.
/// Only calls existing cached robotd/configd APIs; no UART ownership.
pub async fn collect(sockets: Sockets, snapshots: watch::Sender<Snapshot>) {
    let mut tick = tokio::time::interval(STATUS_POLL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut battery = BatteryCache::default();
    loop {
        tick.tick().await;
        let next = read_status(&sockets, &mut battery).await;
        // Update age even on unchanged readings for a new subscriber. Only
        // the secure per-peer Subscriber emits changed display values.
        snapshots.send_replace(next);
    }
}

async fn read_status(sockets: &Sockets, battery_cache: &mut BatteryCache) -> Snapshot {
    let (health, policies, network) = tokio::join!(
        query(&sockets.robot, proto::Call::RobotHealth),
        query(&sockets.robot, proto::Call::RobotPolicies),
        query(&sockets.config, proto::Call::NetStatus),
    );
    let health_status = match health
        .as_ref()
        .and_then(|v| v.get("healthy"))
        .and_then(Value::as_bool)
    {
        Some(true) => "ok",
        Some(false)
            if health
                .as_ref()
                .and_then(|v| v.get("degraded"))
                .and_then(Value::as_bool)
                == Some(true) =>
        {
            "degraded"
        }
        Some(false) => "unhealthy",
        None => "unknown",
    };
    // The sit latch is explicit. No authoritative standing or torque read.
    let pose = if policies
        .as_ref()
        .and_then(|v| v.get("sitting"))
        .and_then(Value::as_bool)
        == Some(true)
    {
        "sitting"
    } else {
        "unknown"
    };
    let network = match network
        .as_ref()
        .and_then(|v| v.get("state"))
        .and_then(Value::as_str)
    {
        Some("connected") => "connected",
        Some("connecting") => "connecting",
        Some("disconnected") => "disconnected",
        _ => "unknown",
    };
    // This age is the btd cached-read age, not hardware sample age: API57
    // does not expose the latter. Reject retained voltage on an unhealthy
    // or non-running bus; require explicit counters and recent loop evidence.
    let bus_ready = health.as_ref().is_some_and(bus_ready_for_battery);
    let sample = battery_cache.observe(
        Instant::now(),
        if bus_ready { health.as_ref() } else { None },
    );
    let sample = if bus_ready {
        sample
    } else {
        battery_cache.sample = None;
        None
    };
    Snapshot {
        battery_age_ms: sample.map(|x| x.0),
        display: Display {
            battery: sample.map(|(_, pct, mv)| (pct, mv)),
            health: health_status,
            pose,
            network,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn retained_voltage_is_not_trusted_without_recent_bus_evidence() {
        let mut state = json!({"healthy": true, "bus":{"consecutive_errors":0,"startup_failures":0},"control_loop":{"ticks":1,"last_tick_age_ms":5}});
        assert!(bus_ready_for_battery(&state));
        state["bus"]["missing"] = json!([21]);
        assert!(!bus_ready_for_battery(&state));
        state["bus"]["missing"] = json!([]);
        state["control_loop"]["last_tick_age_ms"] = json!(1000);
        assert!(!bus_ready_for_battery(&state));
        assert!(!bus_ready_for_battery(&json!({"healthy": true})));
    }

    #[test]
    fn only_request_id_can_subscribe() {
        let (reply, on) =
            request(r#"{"jsonrpc":"2.0","id":7,"method":"robot.diagnosticsSubscribe"}"#).unwrap();
        assert!(on);
        assert_eq!(
            serde_json::from_str::<Value>(&reply).unwrap()["result"],
            json!({"version":1,"subscribed":true})
        );
        assert!(
            !request(r#"{"jsonrpc":"2.0","method":"robot.diagnosticsSubscribe"}"#)
                .unwrap()
                .1
        );
        assert!(!request(r#"{"jsonrpc":"2.0","id":7,"method":"robot.diagnosticsSubscribe","params":{"unexpected":true}}"#).unwrap().1);
        assert!(request(r#"{"jsonrpc":"2.0","id":7,"method":"robot.move"}"#).is_none());
    }

    #[test]
    fn cache_ages_without_repeated_battery_updates() {
        let mut cache = BatteryCache::default();
        let t = Instant::now();
        let first = json!({"battery":{"volts":7.4,"percent":50.0}});
        let second = json!({"battery":{"volts":7.2,"percent":40.0}});
        assert_eq!(cache.observe(t, Some(&first)), Some((0, 50, 7400)));
        assert_eq!(
            cache.observe(t + Duration::from_secs(59), Some(&second)),
            Some((59000, 50, 7400))
        );
        assert_eq!(
            cache.observe(t + BATTERY_REFRESH, None),
            Some((60000, 50, 7400))
        );
        assert_eq!(
            cache.observe(t + Duration::from_secs(66), Some(&second)),
            None
        );
        assert_eq!(
            cache.observe(t + BATTERY_REFRESH * 2, Some(&second)),
            Some((0, 40, 7200))
        );
    }

    #[test]
    fn only_display_changes_and_first_snapshot_are_notified() {
        let mut sub = Subscriber::default();
        let mut display = Display {
            battery: None,
            health: "ok",
            pose: "unknown",
            network: "unknown",
        };
        let sample = |display: &Display, battery_age_ms| Snapshot {
            display: display.clone(),
            battery_age_ms,
        };
        assert!(sub.notify(sample(&display, None)).is_some());
        assert!(sub.notify(sample(&display, Some(4))).is_none());
        display.network = "connected";
        let changed = sub.notify(sample(&display, None)).unwrap();
        let data: Value = serde_json::from_str(&changed).unwrap();
        assert_eq!(data["params"]["sequence"], 2);
        assert_eq!(data["params"]["network"], "connected");
        assert!(data["params"]["torque_enabled"].is_null());
        assert!(data.get("id").is_none());
    }
}
