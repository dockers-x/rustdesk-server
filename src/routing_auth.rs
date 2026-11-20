use crate::{jwt, switch_grant};
use hbb_common::rendezvous_proto::{
    punch_hole_response, PunchHoleRequest, PunchHoleResponse, RelayResponse, RequestRelay,
};

use hbb_common::tokio::sync::Semaphore;
use once_cell::sync::Lazy;
use serde::Deserialize;
use std::time::Duration;

static AUTH_SLOTS: Semaphore = Semaphore::const_new(32);
static AUTH_CLIENT: Lazy<Option<reqwest::Client>> = Lazy::new(|| {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .build()
        .ok()
});

/// Recheck current session and target access on every new routed connection.
pub(crate) async fn connection_permissions(id: &str, token: &str, must_login: bool) -> Option<u64> {
    if !must_login {
        return Some(0);
    }
    let base = std::env::var("RUSTDESK_API_RUSTDESK_API_SERVER").ok()?;
    let endpoint = authorization_endpoint(&base)?;
    let client = AUTH_CLIENT.as_ref()?;
    let _slot = AUTH_SLOTS.try_acquire().ok()?;
    check_connection(client, endpoint, id, token).await
}

#[cfg(test)]
async fn connection_allowed(id: &str, token: &str, must_login: bool) -> bool {
    connection_permissions(id, token, must_login).await.is_some()
}

fn authorization_endpoint(base: &str) -> Option<reqwest::Url> {
    console_endpoint(base, "authorize-connection")
}

fn console_endpoint(base: &str, endpoint: &str) -> Option<reqwest::Url> {
    let mut url = reqwest::Url::parse(base).ok()?;
    // Never forward user tokens to cleartext remote HTTP or embedded credentials.
    let local = url.host_str().is_some_and(|host| {
        host.trim_matches(['[', ']'])
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
    });
    if (!local && url.scheme() != "https")
        || !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return None;
    }
    let path = format!(
        "{}/api/server/{endpoint}",
        url.path().trim_end_matches('/')
    );
    url.set_path(&path);
    Some(url)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AuthorizationResponse {
    allowed: bool,
    #[serde(default)]
    control_permissions: u64,
}

async fn read_reply<T: serde::de::DeserializeOwned>(request: reqwest::RequestBuilder) -> Option<T> {
    let mut response = request.send().await.ok()?;
    if response.status() != reqwest::StatusCode::OK {
        return None;
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.ok()? {
        if body.len() + chunk.len() > 256 {
            return None;
        }
        body.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&body).ok()
}

async fn check_connection(
    client: &reqwest::Client,
    endpoint: reqwest::Url,
    id: &str,
    token: &str,
) -> Option<u64> {
    if id.is_empty() || id.len() > 128 || token.is_empty() || token.len() > 4096 {
        return None;
    }
    let response: AuthorizationResponse = read_reply(
        client.post(endpoint).bearer_auth(token).json(&serde_json::json!({"id": id})),
    ).await?;
    // The only allowed bits encode denials for keyboard, clipboard and file
    // transfer. An allow bit could weaken the target client's local policy.
    const ALLOWED_BITS: u64 = 1 | (1 << 4) | (1 << 6);
    (response.allowed && response.control_permissions & !ALLOWED_BITS == 0)
        .then_some(response.control_permissions)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Admission {
    Allowed,
    Pending,
    Unavailable,
}

static ADMISSION_TOKEN: Lazy<String> =
    Lazy::new(|| std::env::var("RUSTDESK_API_SECURITY_HBBS_ADMISSION_TOKEN").unwrap_or_default());
static ADMISSION_SLOTS: Semaphore = Semaphore::const_new(32);

pub(crate) fn uses_device_admission() -> bool {
    !ADMISSION_TOKEN.is_empty()
}

pub(crate) async fn device_admission(id: &str, uuid: &[u8], pk: &[u8], observe: bool) -> Admission {
    if !uses_device_admission() {
        return Admission::Allowed;
    }
    let Some((client, endpoint)) = AUTH_CLIENT.as_ref().zip(
        std::env::var("RUSTDESK_API_RUSTDESK_API_SERVER")
            .ok()
            .and_then(|base| console_endpoint(&base, "device-admission")),
    ) else {
        return Admission::Unavailable;
    };
    let Ok(_slot) = ADMISSION_SLOTS.try_acquire() else {
        return Admission::Unavailable;
    };
    match read_reply::<AuthorizationResponse>(
        client.post(endpoint).bearer_auth(&*ADMISSION_TOKEN).json(&serde_json::json!({
            "id": id, "uuid": base64::encode(uuid), "public_key": base64::encode(pk), "observe": observe,
        })),
    ).await {
        Some(response) if response.allowed => Admission::Allowed,
        Some(_) => Admission::Pending,
        None => Admission::Unavailable,
    }
}

pub(crate) fn punch_denial(
    request: &PunchHoleRequest,
    key: &str,
    must_login: bool,
    jwt_key: &str,
) -> Option<PunchHoleResponse> {
    if !key.is_empty() && request.licence_key != key {
        return Some(PunchHoleResponse {
            failure: punch_hole_response::Failure::LICENSE_MISMATCH.into(),
            ..Default::default()
        });
    }
    if !must_login
        || switch_grant::allows(&request.id, &request.switch_code, request.conn_type)
        || jwt::verify_token_with_secret(&request.token, jwt_key).is_ok()
    {
        return None;
    }
    Some(PunchHoleResponse {
        other_failure: if request.token.is_empty() {
            "Connection failed, please login!"
        } else {
            "Token error, please log out and log back in!"
        }
        .to_owned(),
        ..Default::default()
    })
}

pub(crate) fn relay_denial(
    request: &RequestRelay,
    key: &str,
    must_login: bool,
    jwt_key: &str,
) -> Option<RelayResponse> {
    if !must_login {
        return None;
    }
    let wrong_key =
        !key.is_empty() && !request.licence_key.is_empty() && request.licence_key != key;
    if !wrong_key
        && (switch_grant::allows_relay(request, key)
            || jwt::verify_token_with_secret(&request.token, jwt_key).is_ok())
    {
        return None;
    }
    Some(RelayResponse {
        refuse_reason: "Connection failed, please log in with a valid token or switch grant!"
            .to_owned(),
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use jsonwebtoken::{encode, EncodingKey, Header};

    fn token(secret: &str, expires_in: i64) -> String {
        encode(
            &Header::default(),
            &serde_json::json!({
                "user_id": 7,
                "exp": chrono::Utc::now().timestamp() + expires_in,
            }),
            &EncodingKey::from_secret(secret.as_bytes()),
        )
        .unwrap()
    }

    #[test]
    fn optional_login_preserves_legacy_routes_and_punch_key_check() {
        let mut punch = PunchHoleRequest::default();
        let relay = RequestRelay {
            switch_code: "old-client-switch".to_owned(),
            licence_key: "legacy-key".to_owned(),
            ..Default::default()
        };
        assert!(punch_denial(&punch, "", false, "").is_none());
        assert!(relay_denial(&relay, "server-key", false, "").is_none());
        let denial = punch_denial(&punch, "server-key", false, "").unwrap();
        assert_eq!(
            denial.failure.enum_value().unwrap(),
            punch_hole_response::Failure::LICENSE_MISMATCH,
        );
        punch.licence_key = "server-key".to_owned();
        assert!(punch_denial(&punch, "server-key", false, "").is_none());
    }

    #[test]
    fn required_login_rejects_missing_invalid_expired_and_unverifiable_tokens() {
        for (jwt_key, value) in [
            ("configured-secret", String::new()),
            ("configured-secret", "arbitrary-nonempty-token".to_owned()),
            ("configured-secret", token("wrong-secret", 3600)),
            ("configured-secret", token("configured-secret", -3600)),
            ("", token("", 3600)),
        ] {
            for switch_code in ["", "unregistered-switch"] {
                let punch = PunchHoleRequest {
                    token: value.clone(),
                    switch_code: switch_code.to_owned(),
                    ..Default::default()
                };
                let relay = RequestRelay {
                    token: value.clone(),
                    switch_code: switch_code.to_owned(),
                    ..Default::default()
                };
                assert!(!punch_denial(&punch, "", true, jwt_key)
                    .unwrap()
                    .other_failure
                    .is_empty());
                assert!(!relay_denial(&relay, "", true, jwt_key)
                    .unwrap()
                    .refuse_reason
                    .is_empty());
            }
        }
    }

    #[test]
    fn required_login_accepts_valid_tokens_including_stock_keyless_relay() {
        let token = token("configured-secret", 3600);
        for switch_code in ["", "unregistered-switch"] {
            let punch = PunchHoleRequest {
                licence_key: "server-key".to_owned(),
                token: token.clone(),
                switch_code: switch_code.to_owned(),
                ..Default::default()
            };
            let mut relay = RequestRelay {
                token: token.clone(),
                switch_code: switch_code.to_owned(),
                ..Default::default()
            };
            assert!(punch_denial(&punch, "server-key", true, "configured-secret").is_none());
            assert!(relay_denial(&relay, "server-key", true, "configured-secret").is_none());
            relay.licence_key = "wrong-key".to_owned();
            assert!(relay_denial(&relay, "server-key", true, "configured-secret").is_some());
        }
    }
}

#[cfg(test)]
mod connection_authorization_tests {
    use super::*;
    use hbb_common::tokio::{
        self,
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    #[test]
    fn authorization_urls_preserve_prefix_and_reject_token_leaks() {
        assert_eq!(
            authorization_endpoint("http://127.0.0.1:21114/prefix/")
                .unwrap()
                .as_str(),
            "http://127.0.0.1:21114/prefix/api/server/authorize-connection"
        );
        assert!(authorization_endpoint("https://console.example.com").is_some());
        for invalid in [
            "http://console.example.com",
            "https://user:pass@console.example.com",
            "https://console.example.com/?token=x",
            "file:///tmp/test",
        ] {
            assert!(authorization_endpoint(invalid).is_none(), "{invalid}");
        }
    }

    #[tokio::test]
    async fn only_explicit_bounded_success_grants_access() {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(1))
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .build()
            .unwrap();
        for (status, body, expected) in [
            ("200 OK", "{\"allowed\":true}".to_string(), true),
            ("200 OK", "{\"allowed\":false}".to_string(), false),
            ("200 OK", "{\"allowed\":true,\"control_permissions\":81}".to_string(), true),
            ("200 OK", "{\"allowed\":true,\"control_permissions\":2}".to_string(), false),
            ("401 Unauthorized", "{\"allowed\":true}".to_string(), false),
            ("200 OK", "{}".to_string(), false),
            ("200 OK", "{\"allowed\":\"true\"}".to_string(), false),
            ("200 OK", " ".repeat(300), false),
            ("302 Found", "{\"allowed\":true}".to_string(), false),
        ] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let endpoint =
                authorization_endpoint(&format!("http://{}", listener.local_addr().unwrap()))
                    .unwrap();
            let server = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = [0u8; 4096];
                let size = socket.read(&mut request).await.unwrap();
                let request = String::from_utf8_lossy(&request[..size]);
                assert!(request
                    .to_ascii_lowercase()
                    .contains("authorization: bearer dummy-token"));
                assert!(request.starts_with("POST /api/server/authorize-connection "));
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
            });
            assert_eq!(
                check_connection(&client, endpoint, "dummy-device", "dummy-token").await.is_some(),
                expected
            );
            server.await.unwrap();
        }
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint =
            authorization_endpoint(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
        drop(listener);
        assert!(check_connection(&client, endpoint, "dummy-device", "dummy-token").await.is_none());
        assert!(connection_allowed("dummy-device", "", false).await);
    }
}

#[cfg(test)]
mod live_console_contract {
    use super::*;
    use hbb_common::tokio;

    /// Run only against an isolated dummy Console, never a deployed service.
    #[tokio::test]
    #[ignore = "requires an isolated Console fixture with disposable accounts"]
    async fn current_owner_other_user_and_revocation() {
        let owner = std::env::var("TEST_OWNER_TOKEN").unwrap();
        let other = std::env::var("TEST_OTHER_TOKEN").unwrap();
        let target = std::env::var("TEST_DEVICE_ID").unwrap();
        let base = std::env::var("RUSTDESK_API_RUSTDESK_API_SERVER").unwrap();
        assert!(base.starts_with("http://127.0.0.1:"));
        assert!(connection_allowed(&target, &owner, true).await);
        assert!(!connection_allowed(&target, &other, true).await);
        let response = AUTH_CLIENT
            .as_ref()
            .unwrap()
            .post(format!("{base}/api/logout"))
            .bearer_auth(&owner)
            .send()
            .await
            .unwrap();
        assert!(response.status().is_success());
        assert!(!connection_allowed(&target, &owner, true).await);
    }
}
