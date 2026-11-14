//! Opt-in secure BLE session for development provisioning.
//!
//! A configured PSK replaces the PIN gate. There is no plaintext fallback in this mode.
//! The key is supplied through a file at process startup; production provisioning and
//! recovery are separate work and this module must not be enabled by default yet.

use duck_auth::wire::{Frame, MAX_CLIENT_LINE, MAX_ROBOT_LINE};
use duck_auth::{ProvisionedKey, RobotConfirm, Session};
use duck_ble::framing::{self, Reassembler};
use tokio::sync::mpsc;

use crate::link::{Link, QUEUE};
use crate::session;
use crate::upstream::{Pool, Sockets};
use crate::{diagnostics, hardware};
use duck_ipc_proto as proto;

/// iOS can retain the physical bond connection after its owning app exits.
/// Only authenticated inbound requests renew the application session lease.
const SESSION_IDLE: std::time::Duration = std::time::Duration::from_secs(60);

pub type Config = ProvisionedKey;

enum State {
    Start,
    Confirm(RobotConfirm),
    Ready(Session),
}

/// Serve one central; every inbound and outbound application line is Noise-protected.
pub async fn run(link: Link, sockets: Sockets, config: Config) {
    run_with_diagnostics(link, sockets, config, diagnostics::Shared::default()).await;
}

pub async fn run_with_diagnostics(
    mut link: Link,
    sockets: Sockets,
    config: Config,
    mut shared: diagnostics::Shared,
) {
    let (replies_tx, mut replies) = mpsc::channel::<String>(QUEUE);
    let config_socket = sockets.config.clone();
    let hardware_sockets = sockets.clone();
    let mut hardware_jobs = tokio::task::JoinSet::new();
    let mut last_hardware_request = None::<tokio::time::Instant>;
    let mut pool = Pool::new(sockets, replies_tx);
    let mut diagnostic_subscriber = None::<diagnostics::Subscriber>;
    let mut inbound = Reassembler::new();
    let mut state = State::Start;
    let mut authenticated = true;
    let mut unused_attempts = 0;
    let deadline = tokio::time::sleep(std::time::Duration::from_secs(15));
    tokio::pin!(deadline);

    loop {
        tokio::select! {
            () = &mut deadline => break,
            chunk = link.inbound.recv() => {
                let Some(chunk) = chunk else { break };
                let Ok(lines) = inbound.push(&chunk) else { break };
                for line in lines {
                    let Ok(frame) = Frame::decode(&line, MAX_CLIENT_LINE) else { return };
                    match (&mut state, frame) {
                        (State::Start, Frame::Start(first)) => {
                            let Ok((next, challenge)) = RobotConfirm::respond(config.key(), config.robot_id(), &first) else { return };
                            state = State::Confirm(next);
                            if send_frame(&link, Frame::Challenge(challenge)).await.is_err() { return }
                        }
                        (State::Confirm(_), Frame::Confirm(proof)) => {
                            let State::Confirm(next) = std::mem::replace(&mut state, State::Start) else { unreachable!() };
                            let Ok((ready, reply)) = next.receive(&proof) else { return };
                            if send_frame(&link, Frame::Ready(reply)).await.is_err() { return }
                            state = State::Ready(ready);
                            deadline.as_mut().reset(tokio::time::Instant::now() + SESSION_IDLE);
                        }
                        (State::Ready(secure), Frame::Data(ciphertext)) => {
                            let Ok(plain) = secure.open(&ciphertext) else { return };
                            let Ok(line) = std::str::from_utf8(&plain) else { return };
                            if line.len() > framing::MAX_LINE { return }
                            deadline.as_mut().reset(tokio::time::Instant::now() + SESSION_IDLE);
                            if let Some((reply, subscribe)) = diagnostics::request(line) {
                                if !reply.is_empty() {
                                    let Ok(ciphertext) = secure.seal(reply.as_bytes()) else { return };
                                    if send_frame(&link, Frame::Data(ciphertext)).await.is_err() { return }
                                }
                                if subscribe && diagnostic_subscriber.is_none() {
                                    let mut subscriber = diagnostics::Subscriber::default();
                                    if let Some(notification) = subscriber.notify(shared.mark_current()) {
                                        let Ok(ciphertext) = secure.seal(notification.as_bytes()) else { return };
                                        if send_frame(&link, Frame::Data(ciphertext)).await.is_err() { return }
                                    }
                                    diagnostic_subscriber = Some(subscriber);
                                }
                                continue;
                            }
                            if let Some(request) = hardware::parse(line) {
                                let refusal = match request {
                                    Ok((id, action)) => {
                                        let now = tokio::time::Instant::now();
                                        if hardware_jobs.is_empty() && last_hardware_request.is_none_or(|t| now - t >= std::time::Duration::from_millis(500)) {
                                            last_hardware_request = Some(now);
                                            let sockets = hardware_sockets.clone();
                                            let snapshot = shared.clone();
                                            hardware_jobs.spawn(async move {
                                                let result = hardware::execute(action, sockets, snapshot).await;
                                                proto::Response::ok(Some(id), &result)
                                            });
                                            continue;
                                        }
                                        Some(proto::Response::ok(Some(id), &serde_json::json!({"version":1,"state":"busy"})))
                                    }
                                    Err(Some(id)) => Some(proto::Response::err(Some(id), proto::Error::new(proto::code::INVALID_PARAMS, "expected no parameters"))),
                                    Err(None) => None,
                                };
                                if let Some(refusal) = refusal {
                                    if send_hardware_response(&link, secure, refusal).await.is_err() { return }
                                }
                                continue;
                            }
                            let outcome = session::dispatch(
                                &mut pool, &config_socket, line, &mut authenticated, &mut unused_attempts,
                            ).await;
                            if let Some(reply) = outcome.response {
                                let Ok(ciphertext) = secure.seal(reply.as_bytes()) else { return };
                                if send_frame(&link, Frame::Data(ciphertext)).await.is_err() { return }
                            }
                            if outcome.close { return }
                        }
                        _ => return,
                    }
                }
            }
            changed = shared.changed(), if diagnostic_subscriber.is_some() => {
                let State::Ready(secure) = &mut state else { return };
                if changed.is_err() {
                    diagnostic_subscriber = None;
                    continue;
                }
                let snapshot = shared.mark_current();
                if let Some(notification) = diagnostic_subscriber.as_mut().and_then(|sub| sub.notify(snapshot)) {
                    let Ok(ciphertext) = secure.seal(notification.as_bytes()) else { return };
                    if send_frame(&link, Frame::Data(ciphertext)).await.is_err() { return }
                }
            }
            job = hardware_jobs.join_next(), if !hardware_jobs.is_empty() => {
                let State::Ready(secure) = &mut state else { return };
                if let Some(Ok(reply)) = job {
                    if send_hardware_response(&link, secure, reply).await.is_err() { return }
                }
            }
            line = replies.recv(), if matches!(state, State::Ready(_)) => {
                let Some(line) = line else { break };
                let State::Ready(secure) = &mut state else { return };
                let Some(line) = crate::secure_log_budget::bound(&line) else { return };
                let Ok(ciphertext) = secure.seal(line.as_bytes()) else { return };
                if send_frame(&link, Frame::Data(ciphertext)).await.is_err() { return }
            }
        }
    }
}

async fn send_hardware_response(
    link: &Link,
    secure: &mut Session,
    reply: proto::Response,
) -> Result<(), ()> {
    let bytes = serde_json::to_vec(&reply).map_err(|_| ())?;
    let ciphertext = secure.seal(&bytes).map_err(|_| ())?;
    send_frame(link, Frame::Data(ciphertext)).await
}

async fn send_frame(link: &Link, frame: Frame) -> Result<(), ()> {
    let line = frame.encode(MAX_ROBOT_LINE).map_err(|_| ())?;
    session::send_line(link, &line).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use duck_auth::ClientStart;
    use tokio::sync::mpsc::{Receiver, Sender};

    #[tokio::test(start_paused = true)]
    async fn a_silent_unauthenticated_subscriber_cannot_hold_the_robot_busy() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("key");
        std::fs::write(&path, "71".repeat(32)).unwrap();
        let config = Config::load(&path, "test-duck".into()).unwrap();
        let sockets = Sockets {
            config: "/tmp/no-configd-secure-timeout.sock".into(),
            robot: "/tmp/no-robotd-secure-timeout.sock".into(),
            updater: "/tmp/no-updaterd-secure-timeout.sock".into(),
            nfc: "/tmp/no-nfcd-secure-timeout.sock".into(),
        };
        let (link, _to_robot, mut from_robot) = Link::pair(20, "silent phone");
        let task = tokio::spawn(run(link, sockets, config));
        tokio::time::timeout(std::time::Duration::from_secs(16), task)
            .await
            .unwrap()
            .unwrap();
        assert!(from_robot.recv().await.is_none());
    }

    async fn send(tx: &Sender<Vec<u8>>, frame: Frame) {
        let line = frame.encode(MAX_CLIENT_LINE).unwrap();
        for chunk in framing::chunks(&line, 20) {
            tx.send(chunk).await.unwrap();
        }
    }

    async fn receive(rx: &mut Receiver<Vec<u8>>, assembly: &mut Reassembler) -> Frame {
        loop {
            let chunk = rx.recv().await.unwrap();
            if let Some(line) = assembly.push(&chunk).unwrap().into_iter().next() {
                return Frame::decode(&line, MAX_ROBOT_LINE).unwrap();
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn diagnostics_needs_secure_opt_in_and_initial_snapshot_follows_ack() {
        let key = [0x71; 32];
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("key");
        std::fs::write(&path, "71".repeat(32)).unwrap();
        let config = Config::load(&path, "test-duck".into()).unwrap();
        let sockets = Sockets {
            config: temp.path().join("config"),
            robot: temp.path().join("robot"),
            updater: temp.path().join("updater"),
            nfc: temp.path().join("nfc"),
        };
        let (cache_tx, cache) = diagnostics::Shared::channel();
        let (link, to_robot, mut from_robot) = Link::pair(20, "test phone");
        let task = tokio::spawn(run_with_diagnostics(link, sockets, config, cache));
        let (phone, first) = ClientStart::begin(&key, "test-duck").unwrap();
        send(&to_robot, Frame::Start(first)).await;
        let mut assembly = Reassembler::for_replies();
        let Frame::Challenge(challenge) = receive(&mut from_robot, &mut assembly).await else {
            panic!("challenge")
        };
        let (phone, proof) = phone.receive(&challenge).unwrap();
        send(&to_robot, Frame::Confirm(proof)).await;
        let Frame::Ready(ready) = receive(&mut from_robot, &mut assembly).await else {
            panic!("ready")
        };
        let mut phone = phone.receive(&ready).unwrap();
        let capabilities = br#"{"jsonrpc":"2.0","id":1,"method":"robot.diagnosticsCapabilities"}"#;
        send(&to_robot, Frame::Data(phone.seal(capabilities).unwrap())).await;
        let Frame::Data(reply) = receive(&mut from_robot, &mut assembly).await else {
            panic!("reply")
        };
        let value: serde_json::Value =
            serde_json::from_slice(&phone.open(&reply).unwrap()).unwrap();
        assert_eq!(value["result"]["change_only"], true);
        cache_tx.send_replace(Default::default());
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), from_robot.recv())
                .await
                .is_err()
        );
        let subscribe = br#"{"jsonrpc":"2.0","id":2,"method":"robot.diagnosticsSubscribe"}"#;
        send(&to_robot, Frame::Data(phone.seal(subscribe).unwrap())).await;
        let Frame::Data(reply) = receive(&mut from_robot, &mut assembly).await else {
            panic!("reply")
        };
        let value: serde_json::Value =
            serde_json::from_slice(&phone.open(&reply).unwrap()).unwrap();
        assert_eq!(value["id"], 2);
        assert_eq!(value["result"]["subscribed"], true);
        let Frame::Data(reply) = receive(&mut from_robot, &mut assembly).await else {
            panic!("initial")
        };
        let value: serde_json::Value =
            serde_json::from_slice(&phone.open(&reply).unwrap()).unwrap();
        assert_eq!(value["method"], "robot.diagnostics");
        assert_eq!(value["params"]["sequence"], 1);
        assert!(value["params"]["battery_age_ms"].is_null());
        cache_tx.send_replace(Default::default());
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), from_robot.recv())
                .await
                .is_err()
        );
        let status = br#"{"jsonrpc":"2.0","id":3,"method":"robot.hardwareStatus"}"#;
        send(&to_robot, Frame::Data(phone.seal(status).unwrap())).await;
        let Frame::Data(reply) = receive(&mut from_robot, &mut assembly).await else {
            panic!("hardware snapshot")
        };
        let value: serde_json::Value =
            serde_json::from_slice(&phone.open(&reply).unwrap()).unwrap();
        assert_eq!(value["id"], 3);
        assert_eq!(value["result"]["version"], 1);
        assert_eq!(value["result"]["servo"]["state"], "unknown");
        assert!(!value["result"]["mic"]["state"].is_null());

        // A client cannot change recording configuration or supply an output path.
        let invalid = br#"{"jsonrpc":"2.0","id":4,"method":"robot.hardwareMicTest","params":{"path":"capture.wav"}}"#;
        send(&to_robot, Frame::Data(phone.seal(invalid).unwrap())).await;
        let Frame::Data(reply) = receive(&mut from_robot, &mut assembly).await else {
            panic!("invalid audio test")
        };
        let value: serde_json::Value =
            serde_json::from_slice(&phone.open(&reply).unwrap()).unwrap();
        assert_eq!(value["id"], 4);
        assert_eq!(value["error"]["code"], proto::code::INVALID_PARAMS);
        drop(to_robot);
        task.await.unwrap();
        assert!(from_robot.recv().await.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn secure_session_rejects_plaintext_and_decrypts_rpc() {
        let key = [0x71; 32];
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("key");
        std::fs::write(&path, "71".repeat(32)).unwrap();
        let config = Config::load(&path, "test-duck".into()).unwrap();
        let sockets = Sockets {
            config: "/tmp/no-configd-secure-test.sock".into(),
            robot: "/tmp/no-robotd-secure-test.sock".into(),
            updater: "/tmp/no-updaterd-secure-test.sock".into(),
            nfc: "/tmp/no-nfcd-secure-test.sock".into(),
        };
        let (link, to_robot, mut from_robot) = Link::pair(20, "test phone");
        let task = tokio::spawn(run(link, sockets.clone(), config.clone()));
        to_robot
            .send(b"{\"method\":\"net.connect\"}\n".to_vec())
            .await
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap();
        assert!(from_robot.recv().await.is_none());

        let (link, to_robot, mut from_robot) = Link::pair(20, "test phone");
        let task = tokio::spawn(run(link, sockets, config));
        let (phone, first) = ClientStart::begin(&key, "test-duck").unwrap();
        send(&to_robot, Frame::Start(first)).await;
        let mut assembly = Reassembler::for_replies();
        let Frame::Challenge(challenge) = receive(&mut from_robot, &mut assembly).await else {
            panic!("expected challenge")
        };
        let (phone, proof) = phone.receive(&challenge).unwrap();
        send(&to_robot, Frame::Confirm(proof)).await;
        let Frame::Ready(ready) = receive(&mut from_robot, &mut assembly).await else {
            panic!("expected ready")
        };
        let mut phone = phone.receive(&ready).unwrap();
        let request = br#"{"jsonrpc":"2.0","id":1,"method":"hello","params":{"api_version":51}}"#;
        send(&to_robot, Frame::Data(phone.seal(request).unwrap())).await;
        let Frame::Data(reply) = receive(&mut from_robot, &mut assembly).await else {
            panic!("expected encrypted reply")
        };
        let plaintext = phone.open(&reply).unwrap();
        assert!(
            std::str::from_utf8(&plaintext)
                .unwrap()
                .contains("not answering")
        );
        // A bonded physical link may outlive the app on iOS. Expire its application state,
        // but keep an actively used session alive beyond its original lease deadline.
        tokio::time::advance(SESSION_IDLE - std::time::Duration::from_secs(1)).await;
        assert!(!task.is_finished());
        send(&to_robot, Frame::Data(phone.seal(request).unwrap())).await;
        let Frame::Data(reply) = receive(&mut from_robot, &mut assembly).await else {
            panic!("expected encrypted reply")
        };
        phone.open(&reply).unwrap();
        tokio::time::advance(SESSION_IDLE - std::time::Duration::from_secs(1)).await;
        assert!(!task.is_finished());
        tokio::time::advance(std::time::Duration::from_secs(2)).await;
        task.await.unwrap();
        assert!(from_robot.recv().await.is_none());
    }
}
