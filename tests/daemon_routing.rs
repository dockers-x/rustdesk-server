use hbb_common::{
    protobuf::Message,
    rendezvous_proto::{register_pk_response, RegisterPk, RendezvousMessage, RequestRelay},
    tcp::FramedStream,
    tokio::{self, net::TcpListener, net::UdpSocket, time::timeout},
    ResultType,
};
use hyper::{body::to_bytes, server::conn::Http, service::service_fn, Body, Request, Response};
use std::{
    convert::Infallible,
    net::{Ipv4Addr, SocketAddrV4},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

const LOCALHOST: Ipv4Addr = Ipv4Addr::LOCALHOST;
const DEVICE: &str = "route-qa-device";
const SECRET: &str = "isolated-console-routing-test-secret";
const ROUTING_TIMEOUT: Duration = Duration::from_secs(4);

struct Server(Child);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

async fn start_hbbs(api_port: u16, directory: &std::path::Path) -> ResultType<(Server, u16)> {
    for _ in 0..30 {
        let tcp = TcpListener::bind((LOCALHOST, 0)).await?;
        let port = tcp.local_addr()?.port();
        if !(3..=65533).contains(&port) {
            continue;
        }
        let (Ok(nat), Ok(ws), Ok(udp)) = (
            TcpListener::bind((LOCALHOST, port - 1)).await,
            TcpListener::bind((LOCALHOST, port + 2)).await,
            UdpSocket::bind((LOCALHOST, port)).await,
        ) else {
            continue;
        };
        drop((tcp, nat, ws, udp));
        let child = Server(
            Command::new(env!("CARGO_BIN_EXE_hbbs"))
                .args(["-b", "127.0.0.1", "-p", &port.to_string(), "-k", ""])
                .env("TEST_HBBS", "no")
                .env("HBBS_HEALTH_PORT", "0")
                .env("DISABLE_UPDATE_CHECK", "Y")
                .env("MUST_LOGIN", "Y")
                .env("RUSTDESK_API_JWT_KEY", SECRET)
                .env(
                    "RUSTDESK_API_RUSTDESK_API_SERVER",
                    format!("http://127.0.0.1:{api_port}"),
                )
                .env_remove("RUSTDESK_API_SECURITY_HBBS_ADMISSION_TOKEN")
                .current_dir(directory)
                .stdout(Stdio::null())
                .spawn()?,
        );
        return Ok((child, port));
    }
    hbb_common::bail!("No matching TCP, UDP, NAT and websocket ports for HBBS")
}

async fn register_target(port: u16) -> ResultType<UdpSocket> {
    let socket = UdpSocket::bind((LOCALHOST, 0)).await?;
    let mut message = RendezvousMessage::new();
    message.set_register_pk(RegisterPk {
        id: DEVICE.to_owned(),
        uuid: b"isolated-fixture".to_vec().into(),
        pk: vec![11; 32].into(),
        ..Default::default()
    });
    let request = message.write_to_bytes()?;
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut reply = [0; 4096];
    loop {
        socket.send_to(&request, (LOCALHOST, port)).await?;
        if let Ok(packet) = timeout(Duration::from_millis(200), socket.recv(&mut reply)).await {
            let response = RendezvousMessage::parse_from_bytes(&reply[..packet?])?;
            assert!(response.has_register_pk_response());
            assert_eq!(
                response.register_pk_response().result.enum_value().unwrap(),
                register_pk_response::Result::OK
            );
            return Ok(socket);
        }
        assert!(
            Instant::now() < deadline,
            "HBBS could not register a target"
        );
    }
}

async fn request_relay(port: u16, token: &str) -> ResultType<FramedStream> {
    let mut stream = FramedStream::new(SocketAddrV4::new(LOCALHOST, port), None, 1000).await?;
    let mut message = RendezvousMessage::new();
    message.set_request_relay(RequestRelay {
        id: DEVICE.to_owned(),
        uuid: uuid::Uuid::new_v4().to_string(),
        token: token.to_owned(),
        ..Default::default()
    });
    stream.send(&message).await?;
    Ok(stream)
}

async fn mock_console(listener: &TcpListener, token: &str) -> ResultType<()> {
    let (stream, _) = timeout(ROUTING_TIMEOUT, listener.accept()).await??;
    let expected = format!("Bearer {token}");
    let service = service_fn(move |request: Request<Body>| {
        let expected = expected.clone();
        async move {
            assert_eq!(request.method(), hyper::Method::POST);
            assert_eq!(request.uri().path(), "/api/server/authorize-connection");
            assert_eq!(request.headers()["authorization"], expected);
            let body = to_bytes(request.into_body()).await.unwrap();
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
                serde_json::json!({"id": DEVICE})
            );
            Ok::<_, Infallible>(Response::new(Body::from(r#"{"allowed":true}"#)))
        }
    });
    timeout(
        ROUTING_TIMEOUT,
        Http::new()
            .http1_only(true)
            .http1_keep_alive(false)
            .serve_connection(stream, service),
    )
    .await??;
    Ok(())
}

async fn expect_forwarded(target: &UdpSocket) -> ResultType<()> {
    let mut reply = [0; 4096];
    let size = timeout(ROUTING_TIMEOUT, target.recv(&mut reply)).await??;
    let message = RendezvousMessage::parse_from_bytes(&reply[..size])?;
    assert!(message.has_request_relay());
    assert_eq!(message.request_relay().id, DEVICE);
    Ok(())
}

#[tokio::test]
async fn real_hbbs_denies_during_console_outage_and_recovers_with_same_token() -> ResultType<()> {
    let workspace = tempfile::tempdir()?;
    let console = TcpListener::bind((LOCALHOST, 0)).await?;
    let api_addr = console.local_addr()?;
    let (_hbbs, hbbs_port) = start_hbbs(api_addr.port(), workspace.path()).await?;
    let device = register_target(hbbs_port).await?;
    let token = jsonwebtoken::encode(
        &jsonwebtoken::Header::default(),
        &serde_json::json!({"user_id": 1, "exp": chrono::Utc::now().timestamp() + 600}),
        &jsonwebtoken::EncodingKey::from_secret(SECRET.as_bytes()),
    )?;

    let active = request_relay(hbbs_port, &token).await?;
    mock_console(&console, &token).await?;
    expect_forwarded(&device).await?;
    drop((active, console));

    // Leave the port bound with no handler to exercise the HTTP deadline.
    let stalled_api = TcpListener::bind(api_addr).await?;
    let mut denied = request_relay(hbbs_port, &token).await?;
    let response = timeout(ROUTING_TIMEOUT, denied.next())
        .await?
        .expect("HBBS did not reply")?;
    let response = RendezvousMessage::parse_from_bytes(&response)?;
    assert_eq!(
        response.relay_response().refuse_reason,
        "Connection not authorized for this device"
    );
    let mut packet = [0; 4096];
    assert!(
        timeout(Duration::from_millis(100), device.recv(&mut packet))
            .await
            .is_err()
    );
    drop(stalled_api);

    let console = TcpListener::bind(api_addr).await?;
    let _active = request_relay(hbbs_port, &token).await?;
    mock_console(&console, &token).await?;
    expect_forwarded(&device).await
}
