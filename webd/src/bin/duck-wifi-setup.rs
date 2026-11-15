#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("duck-wifi-setup is only available on Linux");
    std::process::exit(1);
}

#[cfg(target_os = "linux")]
#[tokio::main]
async fn main() {
    if let Err(reason) = linux::run().await {
        // Only static labels; no raw child output, request, SSID, or passphrase.
        eprintln!("duck-wifi-setup: {reason}");
        std::process::exit(1);
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use axum::Router;
    use axum::extract::{ConnectInfo, DefaultBodyLimit, Form, State};
    use axum::http::{HeaderMap, StatusCode, header};
    use axum::response::{Html, IntoResponse, Response};
    use axum::routing::{get, post};
    use duck_ipc_proto::{self as proto, Call, NetConnectParams, NetStatusResult};
    use serde::Deserialize;
    use socket2::{Domain, Protocol, Socket, Type};
    use std::fmt::Write as _;
    use std::io::{Read, Write};
    use std::net::{Ipv4Addr, SocketAddr};
    use std::os::unix::fs::OpenOptionsExt;
    use std::process::Stdio;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::{TcpListener, UnixStream};
    use tokio::process::Command;
    use tokio::sync::{Mutex, mpsc};
    use tokio::time::{Instant, interval, sleep, timeout};

    type Failure = &'static str;
    const IFACE: &str = "wlan0";
    const NM: &str = "/usr/bin/nmcli";
    const NFT: &str = "/usr/sbin/nft";
    const ID: &str = "8406c356-d16e-4eb6-8d61-5c351a8511c6";
    const KEYFILE: &str = "/run/NetworkManager/system-connections/microduck-setup.nmconnection";
    const INFO: &str = "/run/microduck-wifi-setup/access";
    const ADDRESS: &str = "10.42.0.1:8090";
    const ORIGIN: &str = "http://10.42.0.1:8090";
    const WINDOW: Duration = Duration::from_secs(15 * 60);
    const NFT_RULES: &str = r#"
table inet microduck_wifi_setup {
    chain input {
        type filter hook input priority -150; policy accept;
        iifname "wlan0" ip saddr 10.42.0.0/24 ip daddr 10.42.0.1 tcp dport 8090 accept
        iifname "wlan0" udp sport 68 udp dport 67 accept
        iifname "wlan0" drop
    }
    chain forward {
        type filter hook forward priority -150; policy accept;
        iifname "wlan0" drop
        oifname "wlan0" drop
    }
}
"#;

    async fn command(program: &str, args: &[&str]) -> Result<String, Failure> {
        let mut c = Command::new(program);
        c.args(args).env("LC_ALL", "C").kill_on_drop(true);
        c.stderr(Stdio::null()).stdin(Stdio::null());
        let out = timeout(Duration::from_secs(22), c.output())
            .await
            .map_err(|_| "local network tool timed out")?
            .map_err(|_| "local network tool is unavailable")?;
        if !out.status.success() {
            return Err("local network tool refused the request");
        }
        String::from_utf8(out.stdout).map_err(|_| "unexpected local tool output")
    }

    async fn profiles(active: bool) -> Result<String, Failure> {
        let mut args = vec!["--terse", "--fields", "UUID,TYPE", "connection", "show"];
        if active {
            args.push("--active");
        }
        command(NM, &args).await
    }

    fn wifi_profile_set(output: &str) -> Result<bool, Failure> {
        let mut configured = false;
        for line in output.lines() {
            let (uuid, ty) = line
                .split_once(':')
                .ok_or("unrecognized NetworkManager profile row")?;
            if uuid.is_empty() || ty.is_empty() {
                return Err("unrecognized NetworkManager profile row");
            }
            if uuid != ID && matches!(ty, "802-11-wireless" | "wifi" | "wireless") {
                configured = true;
            }
        }
        Ok(configured)
    }

    fn has_own_profile(output: &str) -> bool {
        output
            .lines()
            .any(|row| row.split_once(':').is_some_and(|(id, _)| id == ID))
    }

    fn root_file(path: &str, bytes: &[u8]) -> Result<(), Failure> {
        let mut file = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(path)
            .map_err(|_| "cannot create a private setup file")?;
        file.write_all(bytes)
            .map_err(|_| "cannot write a private setup file")
    }

    fn random_hex() -> Result<String, Failure> {
        let mut bytes = [0u8; 16];
        std::fs::File::open("/dev/urandom")
            .and_then(|mut f| f.read_exact(&mut bytes))
            .map_err(|_| "device entropy unavailable")?;
        let mut hex = String::with_capacity(32);
        for byte in bytes {
            write!(hex, "{byte:02x}").unwrap();
        }
        Ok(hex)
    }

    fn ssid() -> String {
        let identity = robotd_params::serial::this_first_name()
            .filter(|name| {
                name.len() == 9
                    && name.starts_with("duck-")
                    && name[5..].bytes().all(|b| b.is_ascii_hexdigit())
            })
            .unwrap_or_else(|| "duck".to_owned());
        format!("{identity}-setup")
    }

    fn keyfile(ssid: &str, key: &str) -> String {
        format!(
            "[connection]\nid=microduck-setup\nuuid={ID}\ntype=wifi\ninterface-name={IFACE}\nautoconnect=false\n\n[wifi]\nssid={ssid}\nmode=ap\nband=bg\n\n[wifi-security]\nkey-mgmt=wpa-psk\nproto=rsn;\npsk={key}\n\n[ipv4]\nmethod=shared\naddress1=10.42.0.1/24\nnever-default=true\n\n[ipv6]\nmethod=disabled\n"
        )
    }

    async fn firewall() -> Result<(), Failure> {
        let mut c = Command::new(NFT);
        c.args(["-f", "-"]).kill_on_drop(true);
        c.stdout(Stdio::null())
            .stderr(Stdio::null())
            .stdin(Stdio::piped());
        let mut child = c.spawn().map_err(|_| "cannot start AP isolation")?;
        let mut stdin = child
            .stdin
            .take()
            .ok_or("cannot write AP isolation policy")?;
        stdin
            .write_all(NFT_RULES.as_bytes())
            .await
            .map_err(|_| "cannot write AP isolation policy")?;
        drop(stdin);
        if timeout(Duration::from_secs(5), child.wait())
            .await
            .ok()
            .and_then(Result::ok)
            .is_some_and(|status| status.success())
        {
            Ok(())
        } else {
            Err("cannot install AP isolation policy")
        }
    }

    async fn clear_info() {
        let _ = std::fs::remove_file(INFO);
    }

    async fn cleanup() -> Result<(), Failure> {
        let _ = std::fs::remove_file(KEYFILE);
        let saved = profiles(false).await?;
        if has_own_profile(&saved) {
            // A command can partially succeed before replying. Check NetworkManager,
            // not the CLI exit code. The keyfile on tmpfs was already removed.
            let _ = command(NM, &["--wait", "15", "connection", "delete", "uuid", ID]).await;
        }
        if has_own_profile(&profiles(true).await?) {
            return Err("temporary AP remains active; keeping isolation");
        }
        if command(NFT, &["list", "table", "inet", "microduck_wifi_setup"])
            .await
            .is_ok()
        {
            command(NFT, &["delete", "table", "inet", "microduck_wifi_setup"]).await?;
        }
        clear_info().await;
        Ok(())
    }

    async fn start_ap(ssid: &str, key: &str) -> Result<TcpListener, Failure> {
        if wifi_profile_set(&profiles(false).await?)? {
            return Err("Wi-Fi profile exists, setup AP refused");
        }
        let mode = command(
            NM,
            &[
                "--escape",
                "no",
                "--get-values",
                "GENERAL.TYPE,WIFI-PROPERTIES.AP",
                "device",
                "show",
                IFACE,
            ],
        )
        .await?;
        if !mode.lines().any(|line| line == "wifi") || !mode.lines().any(|line| line == "yes") {
            return Err("wlan0 does not advertise AP capability");
        }
        firewall().await?;
        std::fs::create_dir_all("/run/NetworkManager/system-connections")
            .map_err(|_| "cannot prepare NetworkManager runtime directory")?;
        root_file(KEYFILE, keyfile(ssid, key).as_bytes())?;
        command(NM, &["connection", "load", KEYFILE]).await?;
        command(
            NM,
            &[
                "--wait",
                "15",
                "connection",
                "up",
                "uuid",
                ID,
                "ifname",
                IFACE,
            ],
        )
        .await?;
        if !has_own_profile(&profiles(true).await?) {
            return Err("temporary setup AP did not become active");
        }

        let sock = Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP))
            .map_err(|_| "cannot create isolated setup socket")?;
        sock.set_reuse_address(true)
            .map_err(|_| "cannot prepare isolated setup socket")?;
        sock.bind_device(Some(IFACE.as_bytes()))
            .map_err(|_| "cannot bind setup socket to wlan0")?;
        let addr: SocketAddr = ADDRESS.parse().unwrap();
        sock.set_nonblocking(true)
            .map_err(|_| "cannot prepare isolated setup socket")?;
        sock.bind(&addr.into())
            .map_err(|_| "cannot bind 10.42.0.1:8090 (check updater drop-in)")?;
        sock.listen(8)
            .map_err(|_| "cannot listen on isolated setup socket")?;
        let listener =
            TcpListener::from_std(sock.into()).map_err(|_| "cannot start setup listener")?;

        let info = format!(
            "MicroDuck first-time Wi-Fi setup (15 min)\nSSID: {ssid}\nWPA: {key}\nOpen {ORIGIN}/\n"
        );
        root_file(INFO, info.as_bytes())?;
        // Never write this to stdout/stderr, a journal, or an SSH banner.
        // tty1 on this image is the physical HDMI console.
        let _ = std::fs::OpenOptions::new()
            .write(true)
            .open("/dev/tty1")
            .and_then(|mut console| console.write_all(info.as_bytes()));
        Ok(listener)
    }

    struct App {
        token: String,
        notice: &'static str,
        once: Mutex<Option<mpsc::Sender<Credentials>>>,
    }

    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Credentials {
        token: String,
        ssid: String,
        password: String,
    }

    fn in_setup_network(peer: SocketAddr) -> bool {
        match peer.ip() {
            std::net::IpAddr::V4(ip) => {
                let octets = ip.octets();
                octets[..3] == [10, 42, 0] && (2..=254).contains(&octets[3])
            }
            _ => false,
        }
    }

    fn request_gate(peer: SocketAddr, headers: &HeaderMap, needs_origin: bool) -> bool {
        in_setup_network(peer)
            && headers.get_all(header::HOST).iter().count() == 1
            && headers.get(header::HOST).is_some_and(|v| v == ADDRESS)
            && (!needs_origin
                || (headers.get_all(header::ORIGIN).iter().count() == 1
                    && headers.get(header::ORIGIN).is_some_and(|v| v == ORIGIN)))
    }

    fn valid(fields: &Credentials, token: &str) -> bool {
        fields.token == token
            && !fields.ssid.is_empty()
            && fields.ssid.len() <= 32
            && !fields.ssid.chars().any(char::is_control)
            && (fields.password.is_empty()
                || ((8..=63).contains(&fields.password.len())
                    && fields.password.bytes().all(|b| (0x20..=0x7e).contains(&b)))
                || (fields.password.len() == 64
                    && fields.password.bytes().all(|b| b.is_ascii_hexdigit())))
    }

    fn page(status: StatusCode, body: String, content_security: &'static str) -> Response {
        (
            status,
            [
                (header::CACHE_CONTROL, "no-store, no-cache, must-revalidate"),
                (header::REFERRER_POLICY, "same-origin"),
                (header::CONTENT_SECURITY_POLICY, content_security),
                (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
                (header::X_FRAME_OPTIONS, "DENY"),
            ],
            Html(body),
        )
            .into_response()
    }

    fn text(status: StatusCode, body: &'static str) -> Response {
        page(
            status,
            format!(
                "<!doctype html><meta charset=utf-8><meta name=viewport content=\"width=device-width,initial-scale=1\"><h2>{body}</h2>"
            ),
            "default-src 'none'; frame-ancestors 'none'; base-uri 'none'",
        )
    }

    async fn index(
        State(app): State<Arc<App>>,
        ConnectInfo(peer): ConnectInfo<SocketAddr>,
        headers: HeaderMap,
    ) -> Response {
        if !request_gate(peer, &headers, false) {
            return text(
                StatusCode::FORBIDDEN,
                "Setup is only accessible on the device's hotspot.",
            );
        }
        page(
            StatusCode::OK,
            format!(
                r#"<!doctype html><html lang="zh"><meta charset=utf-8>
<meta name=viewport content="width=device-width,initial-scale=1">
<title>MicroDuck 首次配网</title>
<style>body{{font:17px system-ui;max-width:32rem;margin:3rem auto;padding:1rem}}label{{display:block;margin:1rem 0}}input,button{{font:inherit;box-sizing:border-box;width:100%;padding:.65rem}}p{{line-height:1.5}}</style>
<h1>MicroDuck 首次配网</h1><p>{}</p>
<p>请填写路由器上可见的 Wi-Fi 名称和密码。开放网络密码留空；隐藏网络及企业网络请使用其他配置方式。提交后热点暂时断开。如果没有连上路由器，热点会重新出现。</p>
<form method=post action=/join autocomplete=off>
<input type=hidden name=token value="{}">
<label>Wi-Fi 名称 <input name=ssid maxlength=32 required></label>
<label>Wi-Fi 密码 <input name=password type=password maxlength=64 autocomplete=new-password></label>
<button>连接 Wi-Fi</button></form></html>"#,
                app.notice, app.token
            ),
            "default-src 'none'; style-src 'unsafe-inline'; form-action 'self'; frame-ancestors 'none'; base-uri 'none'",
        )
    }

    async fn join(
        State(app): State<Arc<App>>,
        ConnectInfo(peer): ConnectInfo<SocketAddr>,
        headers: HeaderMap,
        Form(fields): Form<Credentials>,
    ) -> Response {
        if !request_gate(peer, &headers, true) {
            return text(StatusCode::FORBIDDEN, "需要从这台设备的热点页面提交。");
        }
        if !valid(&fields, &app.token) {
            return text(
                StatusCode::BAD_REQUEST,
                "Wi-Fi 名称或密码格式有误，请返回配置页重试。",
            );
        }
        let Some(sender) = app.once.lock().await.take() else {
            return text(StatusCode::CONFLICT, "已经有一个连接请求。");
        };
        if sender.send(fields).await.is_err() {
            return text(StatusCode::SERVICE_UNAVAILABLE, "此配网窗口已结束。");
        }
        text(
            StatusCode::ACCEPTED,
            "请求已接收。热点将暂时关闭；若配网失败，可能需要约 80 秒才会重新出现。",
        )
    }

    async fn rpc(call: &Call, budget: Duration) -> Result<proto::Response, Failure> {
        match timeout(budget, async {
            let conn = UnixStream::connect(proto::socket::CONFIG)
                .await
                .map_err(|_| "configd socket unavailable")?;
            let (rx, mut tx) = conn.into_split();
            let mut wire = serde_json::to_vec(&proto::Request::call(proto::Id::Number(1), call))
                .map_err(|_| "invalid setup request")?;
            wire.push(b'\n');
            let wrote = tx.write_all(&wire).await;
            wire.fill(0);
            wrote.map_err(|_| "configd write failed")?;
            let mut reader = BufReader::new(rx);
            let mut line = Vec::new();
            let mut limited = (&mut reader).take(8193);
            let n = limited
                .read_until(b'\n', &mut line)
                .await
                .map_err(|_| "configd reply failed")?;
            if n == 0 || n > 8192 || line.last() != Some(&b'\n') {
                return Err("invalid configd reply");
            }
            serde_json::from_slice::<proto::Response>(&line).map_err(|_| "invalid configd reply")
        })
        .await
        {
            Ok(result) => result,
            Err(_) => Err("configd operation timed out"),
        }
    }

    use tokio::io::AsyncReadExt;

    async fn status() -> Result<NetStatusResult, Failure> {
        rpc(&Call::NetStatus, Duration::from_secs(6))
            .await?
            .result_as()
            .map_err(|_| "configd has no network status")
    }

    fn verified(status: &NetStatusResult, ssid: &str) -> bool {
        status.state == proto::NetState::Connected
            && status.ssid.as_deref() == Some(ssid)
            && status.ip4.as_ref().is_some_and(|v| {
                v.parse::<Ipv4Addr>().is_ok_and(|a| {
                    !a.is_unspecified()
                        && !a.is_link_local()
                        && !a.is_loopback()
                        && !a.is_multicast()
                        && a != Ipv4Addr::BROADCAST
                        && a != Ipv4Addr::new(10, 42, 0, 1)
                })
            })
    }

    async fn connect(fields: Credentials) -> Result<(), &'static str> {
        let Credentials { ssid, password, .. } = fields;
        let params = NetConnectParams {
            ssid: ssid.clone(),
            psk: (!password.is_empty()).then_some(password),
        };
        let result = rpc(&Call::NetConnect(params), Duration::from_secs(65))
            .await
            .and_then(|r| {
                r.result_as::<proto::ConnectResult>()
                    .map_err(|_| "连接结果不可用")
            });
        match result {
            Ok(proto::ConnectResult::Failed { reason, .. }) => {
                return Err(match reason {
                    proto::ConnectFailure::BadKey => "路由器拒绝了 Wi-Fi 密码，请重试。",
                    proto::ConnectFailure::NotFound => "找不到这个 Wi-Fi，请检查名称和信号。",
                    proto::ConnectFailure::Unsupported => "这个 Wi-Fi 加密模式不受支持。",
                    proto::ConnectFailure::Timeout => "连接超时，可以重试。",
                    _ => "没有连上路由器，请重试。",
                });
            }
            Ok(proto::ConnectResult::Connected { .. }) | Err(_) => {}
        }
        if status().await.is_ok_and(|value| verified(&value, &ssid)) {
            Ok(())
        } else {
            Err("未确认连上所选 Wi-Fi 和获取到 IP，请检查路由器。")
        }
    }

    // Recheck after tearing down our AP: BLE or a local admin might have started
    // provisioning while the page was open. Never replace an existing profile.
    async fn can_handoff(hotspot: &str) -> Result<bool, Failure> {
        for _ in 0..5 {
            if wifi_profile_set(&profiles(false).await?)? {
                return Ok(false);
            }
            let current = status().await?;
            if current.joining.is_some() {
                return Ok(false);
            }
            match current.state {
                proto::NetState::Disconnected => return Ok(true),
                proto::NetState::Connected if current.ssid.as_deref() == Some(hotspot) => {
                    // NetworkManager may briefly report the just-stopped AP.
                    sleep(Duration::from_millis(500)).await;
                }
                _ => return Ok(false),
            }
        }
        Ok(false)
    }

    async fn ready() -> Result<NetStatusResult, Failure> {
        timeout(Duration::from_secs(95), async {
            loop {
                if let Ok(status) = status().await
                    && status.state != proto::NetState::Unavailable
                {
                    return status;
                }
                // Some board radios enumerate late; an available configd is not
                // enough to prove NetworkManager has seen wlan0.
                sleep(Duration::from_secs(2)).await;
            }
        })
        .await
        .map_err(|_| "no managed wlan0/configd status after 95 seconds; no setup AP started")
    }

    async fn initial_run() -> Result<(), Failure> {
        let initial = ready().await?;
        if initial.state == proto::NetState::Unavailable
            || initial.joining.is_some()
            || wifi_profile_set(&profiles(false).await?)?
            || initial.state != proto::NetState::Disconnected
        {
            return Ok(());
        }
        let hotspot = ssid();
        let key = random_hex()?;
        let end = Instant::now() + WINDOW;
        let mut notice = "本页面只用于首次连接 Wi-Fi，不提供运动、更新或日志控制。";
        while Instant::now() < end {
            let listener = start_ap(&hotspot, &key).await?;
            let token = random_hex()?;
            let (send, mut recv) = mpsc::channel(1);
            let app = Arc::new(App {
                token,
                notice,
                once: Mutex::new(Some(send)),
            });
            let router = Router::new()
                .route("/", get(index))
                .route("/join", post(join))
                .layer(DefaultBodyLimit::max(768))
                .with_state(app);
            let mut serve = tokio::spawn(async move {
                axum::serve(
                    listener,
                    router.into_make_service_with_connect_info::<SocketAddr>(),
                )
                .await
            });
            let mut check = interval(Duration::from_secs(5));
            // A lost peer or secondary BLE provisioning must not keep the hotspot and firewall up.
            let watch = async {
                loop {
                    check.tick().await;
                    if !has_own_profile(&profiles(true).await?)
                        || wifi_profile_set(&profiles(false).await?)?
                    {
                        return Ok::<(), Failure>(());
                    }
                }
            };
            let picked = tokio::select! {
                _ = &mut serve => return Err("setup HTTP stopped unexpectedly"),
                result = watch => result.map(|()| None),
                _ = tokio::time::sleep_until(end) => Ok(None),
                fields = recv.recv() => Ok(fields),
            };
            // Keep accepting until the short response has been sent, then close the
            // listener/socket before handing the one radio back to station mode.
            if matches!(&picked, Ok(Some(_))) {
                sleep(Duration::from_millis(750)).await;
            }
            serve.abort();
            let _ = serve.await;
            let Some(fields) = picked? else {
                return Ok(());
            };
            cleanup().await?;
            if !can_handoff(&hotspot).await? {
                return Ok(());
            }
            match connect(fields).await {
                Ok(()) => {
                    eprintln!("duck-wifi-setup: Wi-Fi provisioned and address confirmed");
                    return Ok(());
                }
                Err(message) => notice = message,
            }
            // Never reopen when a different provisioning channel left a profile behind.
            if wifi_profile_set(&profiles(false).await?)? {
                return Ok(());
            }
        }
        Ok(())
    }

    pub async fn run() -> Result<(), Failure> {
        match std::env::args().nth(1).as_deref() {
            Some("--cleanup") => cleanup().await,
            Some(_) => Err("unexpected option"),
            None => {
                cleanup().await?;
                let setup = initial_run().await;
                let stopped = cleanup().await;
                setup.and(stopped)
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn only_own_profile_is_ignored_and_bad_results_fail_closed() {
            assert_eq!(wifi_profile_set(""), Ok(false));
            assert_eq!(
                wifi_profile_set(&format!("{ID}:wifi\nother:ethernet\n")),
                Ok(false)
            );
            assert_eq!(wifi_profile_set("other:802-11-wireless"), Ok(true));
            assert_eq!(wifi_profile_set("other:wifi"), Ok(true));
            assert!(wifi_profile_set("malformed").is_err());
        }

        #[test]
        fn request_must_arrive_from_ap_and_match_host_and_origin() {
            let peer = "10.42.0.20:48252".parse().unwrap();
            let mut headers = HeaderMap::new();
            headers.insert(header::HOST, ADDRESS.parse().unwrap());
            assert!(request_gate(peer, &headers, false));
            assert!(!request_gate(peer, &headers, true));
            headers.insert(header::ORIGIN, ORIGIN.parse().unwrap());
            assert!(request_gate(peer, &headers, true));
            assert!(!request_gate(
                "192.168.1.20:48252".parse().unwrap(),
                &headers,
                true
            ));
            headers.append(header::ORIGIN, "http://wrong".parse().unwrap());
            assert!(!request_gate(peer, &headers, true));
        }

        #[test]
        fn input_is_bounded_and_requires_current_csrf_token() {
            let fields = |ssid: &str, password: &str| Credentials {
                token: "expected".into(),
                ssid: ssid.into(),
                password: password.into(),
            };
            assert!(valid(&fields("Home 5 GHz", "password"), "expected"));
            assert!(valid(&fields("Open", ""), "expected"));
            assert!(!valid(&fields("Home", "password"), "stale"));
            assert!(!valid(&fields("line\nfeed", "password"), "expected"));
            assert!(!valid(&fields("Home", "short"), "expected"));
            assert!(!valid(&fields("Home", &"a".repeat(65)), "expected"));
        }

        #[test]
        fn ap_profile_never_autoconnects_and_uses_wpa() {
            let profile = keyfile("duck-abcd-setup", "exactly-a-testing-secret");
            assert!(profile.contains("autoconnect=false"));
            assert!(profile.contains("key-mgmt=wpa-psk"));
            assert!(profile.contains("psk=exactly-a-testing-secret"));
            assert!(profile.contains("method=shared"));
        }
    }
}
