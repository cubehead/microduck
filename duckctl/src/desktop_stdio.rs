//! One bounded Wi-Fi provisioning operation for the desktop UI. No command-line
//! credentials, disk cache, environment lookups, or debug logging on this path.
use std::io::{BufRead, Read, Write};

use btleplug::api::ValueNotification;
use futures::Stream;
use serde_json::{Value, json};
use tokio::time::{Instant as TokioInstant, timeout};

use super::*;

const REQUEST_LIMIT: u64 = 4096;
const RESPONSE_LIMIT: usize = 48 * 1024;
const MAX_DEVICES: usize = 48;
const ARG: &str = "desktop-stdio-v1";

enum Auth {
    Secure(setup_code::SetupCode),
    LegacyPin(String),
}

enum Operation {
    Scan,
    WifiStatus,
    WifiScan,
    WifiConnect {
        ssid: String,
        passphrase: Option<String>,
    },
}

struct Request {
    operation: Operation,
    target: Option<(String, String, Auth)>,
}

type Fail = &'static str;

pub(crate) fn invoked() -> bool {
    let mut args = std::env::args_os().skip(1);
    args.next().is_some_and(|arg| arg == ARG) && args.next().is_none()
}

pub(crate) async fn run() -> std::process::ExitCode {
    let reply = match read_request() {
        Err(error) => json!({"version":1,"ok":false,"error":error}),
        Ok(req) => {
            let budget = match req.operation {
                Operation::Scan => 17,
                Operation::WifiConnect { .. } => 75,
                _ => 32,
            };
            match timeout(Duration::from_secs(budget), execute(req)).await {
                Ok(Ok(result)) => json!({"version":1,"ok":true,"result":result}),
                Ok(Err(error)) => json!({"version":1,"ok":false,"error":error}),
                Err(_) => json!({"version":1,"ok":false,"error":"TIMEOUT"}),
            }
        }
    };
    let mut output = reply.to_string();
    if output.len() > RESPONSE_LIMIT {
        output = json!({"version":1,"ok":false,"error":"NETWORK"}).to_string();
    }
    match writeln!(std::io::stdout().lock(), "{output}") {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(_) => std::process::ExitCode::FAILURE,
    }
}

fn read_request() -> Result<Request, Fail> {
    let input = std::io::stdin();
    let mut bytes = Vec::new();
    input
        .lock()
        .take(REQUEST_LIMIT)
        .read_until(b'\n', &mut bytes)
        .map_err(|_| "INVALID")?;
    if bytes.last() != Some(&b'\n') {
        return Err("INVALID");
    }
    let value = serde_json::from_slice(&bytes).map_err(|_| "INVALID")?;
    parse(value)
}

fn parse(value: Value) -> Result<Request, Fail> {
    let obj = value.as_object().ok_or("INVALID")?;
    if obj.get("version").and_then(Value::as_u64) != Some(1) {
        return Err("UNSUPPORTED");
    }
    let operation = match obj.get("op").and_then(Value::as_str) {
        Some("scan") if obj.len() == 2 => {
            return Ok(Request {
                operation: Operation::Scan,
                target: None,
            });
        }
        Some("wifiStatus") => Operation::WifiStatus,
        Some("wifiScan") => Operation::WifiScan,
        Some("wifiConnect") => {
            let ssid = get_string(&value, "ssid", 32)?;
            let passphrase = match obj.get("passphrase") {
                None | Some(Value::Null) => None,
                Some(Value::String(s))
                    if !s.is_empty() && s.len() <= 64 && !s.chars().any(char::is_control) =>
                {
                    Some(s.clone())
                }
                _ => return Err("INVALID"),
            };
            Operation::WifiConnect { ssid, passphrase }
        }
        _ => return Err("INVALID"),
    };
    let allowed = |key: &str| {
        matches!(key, "version" | "op" | "name" | "device_id" | "auth")
            || (matches!(operation, Operation::WifiConnect { .. })
                && matches!(key, "ssid" | "passphrase"))
    };
    if obj.keys().any(|k| !allowed(k)) {
        return Err("INVALID");
    }
    let id = get_string(&value, "device_id", 192)?;
    let name = get_string(&value, "name", 128)?;
    let auth = obj
        .get("auth")
        .and_then(Value::as_object)
        .filter(|a| a.len() == 1)
        .ok_or("AUTH")?;
    let auth = if let Some(text) = auth.get("setup_code").and_then(Value::as_str) {
        if text.len() > 256 || text.chars().any(char::is_control) {
            return Err("AUTH");
        }
        Auth::Secure(setup_code::SetupCode::parse(text).map_err(|_| "AUTH")?)
    } else if let Some(pin) = auth.get("pin").and_then(Value::as_str)
        && pin.len() == 6
        && pin.bytes().all(|c| c.is_ascii_digit())
    {
        Auth::LegacyPin(pin.to_owned())
    } else {
        return Err("AUTH");
    };
    Ok(Request {
        operation,
        target: Some((id, name, auth)),
    })
}

fn get_string(value: &Value, key: &str, max: usize) -> Result<String, Fail> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty() && s.len() <= max && !s.chars().any(char::is_control))
        .map(str::to_owned)
        .ok_or("INVALID")
}

async fn execute(req: Request) -> Result<Value, Fail> {
    let manager = Manager::new().await.map_err(|_| "BLUETOOTH")?;
    let adapter = manager
        .adapters()
        .await
        .map_err(|_| "BLUETOOTH")?
        .into_iter()
        .next()
        .ok_or("BLUETOOTH")?;
    adapter
        .start_scan(ScanFilter::default())
        .await
        .map_err(|_| "BLUETOOTH")?;
    let candidates = discover(
        &adapter,
        req.target
            .as_ref()
            .map(|(id, name, _)| (id.as_str(), name.as_str())),
    )
    .await;
    let _ = timeout(Duration::from_secs(2), adapter.stop_scan()).await;
    let candidates = candidates?;
    let Some((id, name, auth)) = req.target else {
        return Ok(json!({"devices": candidates.into_iter().map(|d| json!({
            "id":d.id, "name":d.name, "ip":d.ip
        })).collect::<Vec<_>>() }));
    };
    let mut matching = candidates
        .into_iter()
        .filter(|d| d.id == id && d.name == name);
    let device = matching.next().ok_or("NOT_FOUND")?;
    if matching.next().is_some() {
        return Err("AMBIGUOUS");
    }
    let peripheral = device.peripheral;
    #[cfg(target_os = "linux")]
    let _agent = crate::agent::register(&peripheral.address().to_string())
        .await
        .ok();
    let result = provision(&peripheral, auth, req.operation).await;
    let _ = timeout(Duration::from_secs(2), peripheral.disconnect()).await;
    result
}

struct Device {
    id: String,
    name: String,
    ip: Option<Ipv4Addr>,
    peripheral: Peripheral,
}

async fn discover(
    adapter: &impl Central<Peripheral = Peripheral>,
    target: Option<(&str, &str)>,
) -> Result<Vec<Device>, Fail> {
    let until = TokioInstant::now() + Duration::from_secs(7);
    loop {
        let mut found = Vec::new();
        for peripheral in adapter.peripherals().await.map_err(|_| "BLUETOOTH")? {
            let Some(props) = peripheral.properties().await.ok().flatten() else {
                continue;
            };
            let Some(name) = props.local_name.as_deref().filter(|name| {
                !name.is_empty() && name.len() <= 128 && !name.chars().any(char::is_control)
            }) else {
                continue;
            };
            let id = peripheral.id().to_string();
            // For an already-bonded target the cached advert may omit services. Accept only
            // the exact previously selected OS identity; check GATT before transmitting auth.
            let wanted = target.is_some_and(|t| t == (id.as_str(), name));
            let duck = props.services.contains(&SERVICE_UUID);
            if (!wanted && (target.is_some() || !duck || props.rssi.is_none()))
                || id.len() > 192
                || id.is_empty()
                || id.chars().any(char::is_control)
            {
                continue;
            }
            let ip = match Address::read(&props, duck) {
                Address::At(address) => Some(address),
                _ => None,
            };
            found.push(Device {
                id,
                name: name.to_owned(),
                ip,
                peripheral,
            });
            if found.len() == MAX_DEVICES {
                break;
            }
        }
        if TokioInstant::now() >= until || (target.is_some() && !found.is_empty()) {
            // Keep distinct local peripheral IDs even on duplicate human names.
            found.sort_unstable_by(|a, b| (&a.name, &a.id).cmp(&(&b.name, &b.id)));
            return Ok(found);
        }
        tokio::time::sleep(SCAN_POLL).await;
    }
}

async fn provision(peripheral: &Peripheral, auth: Auth, op: Operation) -> Result<Value, Fail> {
    let version = timeout(Duration::from_secs(16), async {
        peripheral.connect().await.map_err(|_| "BLUETOOTH")?;
        peripheral
            .discover_services()
            .await
            .map_err(|_| "BLUETOOTH")?;
        let (write, read) = characteristics(peripheral).map_err(|_| "UNSUPPORTED")?;
        if read.service_uuid != SERVICE_UUID {
            return Err("UNSUPPORTED");
        }
        // The encrypted characteristic read is what prompts OS pairing on an unbonded host.
        let v = peripheral.read(&read).await.map_err(|_| "BLUETOOTH")?;
        if !matches!(v.as_slice(), [57..=255]) {
            return Err("UNSUPPORTED");
        }
        let events = peripheral.notifications().await.map_err(|_| "BLUETOOTH")?;
        peripheral.subscribe(&read).await.map_err(|_| "BLUETOOTH")?;
        Ok((write, events))
    })
    .await
    .map_err(|_| "TIMEOUT")??;
    let (tx, notifications) = version;
    let mut notifications = notifications.filter(|n| futures::future::ready(n.uuid == RPC_UUID));
    let mut session = match auth {
        Auth::Secure(code) => Some(
            secure_handshake(peripheral, &tx, &mut notifications, &code, false)
                .await
                .map_err(|_| "AUTH")?,
        ),
        Auth::LegacyPin(pin) => {
            let auth =
                json!({"jsonrpc":"2.0","id":0,"method":"system.authenticate","params":{"pin":pin}});
            write_line(peripheral, &tx, &auth.to_string())
                .await
                .map_err(|_| "AUTH")?;
            let reply = response(
                peripheral,
                &mut notifications,
                &mut None,
                0,
                Duration::from_secs(5),
            )
            .await
            .map_err(|_| "AUTH")?;
            if reply.get("authenticated").and_then(Value::as_bool) != Some(true) {
                return Err("AUTH");
            }
            None
        }
    };
    let (method, params, budget) = match op {
        Operation::WifiStatus => ("net.status", json!({}), 6),
        Operation::WifiScan => ("net.scan", json!({}), 12),
        Operation::WifiConnect { ssid, passphrase } => {
            let mut params = json!({"ssid":ssid});
            if let Some(passphrase) = passphrase {
                params["psk"] = json!(passphrase);
            }
            ("net.connect", params, 52)
        }
        _ => return Err("INVALID"),
    };
    let req = json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}).to_string();
    let wire = if let Some(session) = &mut session {
        let bytes = session.seal(req.as_bytes()).map_err(|_| "AUTH")?;
        Frame::Data(bytes)
            .encode(MAX_CLIENT_LINE)
            .map_err(|_| "INVALID")?
    } else {
        req
    };
    write_line(peripheral, &tx, &wire)
        .await
        .map_err(|_| "BLUETOOTH")?;
    response(
        peripheral,
        &mut notifications,
        &mut session,
        1,
        Duration::from_secs(budget),
    )
    .await
}

async fn response(
    peripheral: &Peripheral,
    stream: &mut (impl Stream<Item = ValueNotification> + Unpin),
    session: &mut Option<duck_auth::Session>,
    id: u64,
    duration: Duration,
) -> Result<Value, Fail> {
    let until = TokioInstant::now() + duration;
    let mut lines = Reassembler::for_replies();
    loop {
        let chunk = match next_chunk(peripheral, stream, until).await {
            Waited::Chunk(n) => n,
            Waited::Silent => return Err("TIMEOUT"),
            Waited::Dropped => return Err("BLUETOOTH"),
        };
        for line in lines.push(&chunk.value).map_err(|_| "NETWORK")? {
            let line = if let Some(session) = session {
                open_line(session, &line).map_err(|_| "AUTH")?
            } else {
                line
            };
            let v: Value = serde_json::from_str(&line).map_err(|_| "NETWORK")?;
            if v.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
                return Err("NETWORK");
            }
            if v.get("id").and_then(Value::as_u64) != Some(id) {
                continue;
            }
            if v.get("error").is_some() {
                return Err("NETWORK");
            }
            return v.get("result").cloned().ok_or("NETWORK");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requires_explicit_identity_and_exclusive_auth() {
        assert!(parse(json!({"version":1,"op":"scan"})).is_ok());
        assert!(parse(json!({"version":1,"op":"scan","auth":{"pin":"000000"}})).is_err());
        assert!(
            parse(json!({"version":1,"op":"wifiStatus","name":"duck-a","auth":{"pin":"000000"}}))
                .is_err()
        );
        let mut req = json!({"version":1,"op":"wifiStatus","name":"duck-a","device_id":"uuid","auth":{"pin":"000000"}});
        assert!(parse(req.clone()).is_ok());
        req["auth"]["setup_code"] = json!(format!("md1:duck-a:{}", "00".repeat(32)));
        assert!(parse(req).is_err());
    }

    #[test]
    fn only_wifi_methods_and_bounded_credentials() {
        let mut req = json!({"version":1,"op":"wifiConnect","device_id":"exact","name":"duck-a","ssid":"办公室","passphrase":"dry-run-fake","auth":{"setup_code": format!("md1:duck-a:{}", "22".repeat(32))}});
        assert!(parse(req.clone()).is_ok());
        req["passphrase"] = json!("x".repeat(65));
        assert!(parse(req.clone()).is_err());
        req["op"] = json!("robot.enable");
        assert!(parse(req).is_err());
    }
}
