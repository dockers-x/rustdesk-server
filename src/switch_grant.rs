use crate::peer::{PeerMap, MAX_PEER_ID_LEN};
use hbb_common::{
    protobuf::EnumOrUnknown,
    rendezvous_proto::{ConnType, RequestRelay},
    sodiumoxide::crypto::{hash::sha256, sign},
};
use once_cell::sync::Lazy;
use serde_derive::Deserialize;
use std::{
    collections::HashMap,
    sync::Mutex,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const GRANT_TTL: Duration = Duration::from_secs(10);
const CLOCK_WINDOW: u64 = 60;
const REPLAY_TTL: Duration = Duration::from_secs(2 * CLOCK_WINDOW + 10);
const MAX_GRANTS: usize = 4096;
const MAX_GRANTS_PER_ID: usize = 8;
const MAX_COMMAND_LEN: usize = 1024;
static GRANTS: Lazy<Mutex<GrantStore>> = Lazy::new(|| Mutex::new(GrantStore::default()));

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Registration {
    id: String,
    switch_code_verifier: String,
    timestamp: String,
    signature: String,
}

#[derive(Debug, PartialEq)]
struct Response {
    accepted: bool,
    server_time: Option<i64>,
}

impl Response {
    fn rejected() -> Self {
        Self {
            accepted: false,
            server_time: None,
        }
    }

    fn json(self) -> String {
        match self.server_time {
            Some(server_time) => serde_json::json!({
                "accepted": self.accepted,
                "server_time": server_time,
            }),
            None => serde_json::json!({"accepted": self.accepted}),
        }
        .to_string()
    }
}

#[derive(Default)]
struct GrantStore {
    entries: HashMap<(String, [u8; 32]), Instant>,
}

impl GrantStore {
    fn register(
        &mut self,
        request: &Registration,
        public_key: &[u8],
        unix_time: i64,
        now: Instant,
    ) -> Response {
        let Some(verifier) = canonical_digest(&request.switch_code_verifier) else {
            return Response::rejected();
        };
        if !(6..=MAX_PEER_ID_LEN).contains(&request.id.len())
            || request.id.contains('\0')
            || request.timestamp.is_empty()
            || request.timestamp.len() > 19
            || !request.timestamp.bytes().all(|byte| byte.is_ascii_digit())
        {
            return Response::rejected();
        }
        let Ok(timestamp) = request.timestamp.parse::<i64>() else {
            return Response::rejected();
        };
        if timestamp.to_string() != request.timestamp {
            return Response::rejected();
        }
        let Some(public_key) = sign::PublicKey::from_slice(public_key) else {
            return Response::rejected();
        };
        let Ok(signature_bytes) = base64::decode(&request.signature) else {
            return Response::rejected();
        };
        if base64::encode(&signature_bytes) != request.signature {
            return Response::rejected();
        }
        let Ok(signature) = sign::Signature::from_bytes(&signature_bytes) else {
            return Response::rejected();
        };
        if !sign::verify_detached(&signature, &signed_message(request), &public_key) {
            return Response::rejected();
        }
        if timestamp.abs_diff(unix_time) > CLOCK_WINDOW {
            return Response {
                accepted: false,
                server_time: Some(unix_time),
            };
        }
        self.entries
            .retain(|_, registered| now.duration_since(*registered) < REPLAY_TTL);
        let key = (request.id.clone(), verifier);
        if let Some(registered) = self.entries.get(&key) {
            return Response {
                accepted: now.duration_since(*registered) < GRANT_TTL,
                server_time: None,
            };
        }
        if self.entries.len() >= MAX_GRANTS
            || self
                .entries
                .keys()
                .filter(|key| key.0 == request.id)
                .count()
                >= MAX_GRANTS_PER_ID
        {
            return Response::rejected();
        }
        self.entries.insert(key, now);
        Response {
            accepted: true,
            server_time: None,
        }
    }

    fn allows(&self, id: &str, code: &str, conn_type: ConnType, now: Instant) -> bool {
        if conn_type != ConnType::DEFAULT_CONN || canonical_digest(code).is_none() {
            return false;
        }
        let mut message = b"switch-grant-verifier\0".to_vec();
        message.extend_from_slice(code.as_bytes());
        self.entries
            .get(&(id.to_owned(), sha256::hash(&message).0))
            .is_some_and(|registered| now.duration_since(*registered) < GRANT_TTL)
    }

    fn allows_relay(&self, request: &RequestRelay, key: &str, now: Instant) -> bool {
        if (!key.is_empty() && !request.licence_key.is_empty() && request.licence_key != key)
            || request.conn_type.enum_value() != Ok(ConnType::DEFAULT_CONN)
        {
            return false;
        }
        self.allows(
            &request.id,
            &request.switch_code,
            ConnType::DEFAULT_CONN,
            now,
        )
    }
}

fn canonical_digest(value: &str) -> Option<[u8; 32]> {
    if value.len() != 44 {
        return None;
    }
    let decoded = base64::decode(value).ok()?;
    if base64::encode(&decoded) != value {
        return None;
    }
    decoded.try_into().ok()
}

fn signed_message(request: &Registration) -> Vec<u8> {
    let mut message = b"switch-grant\0".to_vec();
    message.extend_from_slice(request.id.as_bytes());
    message.push(0);
    message.extend_from_slice(request.switch_code_verifier.as_bytes());
    message.push(0);
    message.extend_from_slice(request.timestamp.as_bytes());
    message
}

pub(crate) async fn register_command(encoded: &str, peers: &PeerMap) -> String {
    let rejected = || Response::rejected().json();
    if encoded.len() + "register-switch-grant ".len() > MAX_COMMAND_LEN {
        return rejected();
    }
    let Ok(bytes) = base64::decode(encoded) else {
        return rejected();
    };
    if base64::encode(&bytes) != encoded {
        return rejected();
    }
    let Ok(request) = serde_json::from_slice::<Registration>(&bytes) else {
        return rejected();
    };
    if request.id.len() > MAX_PEER_ID_LEN || request.signature.len() != 88 {
        return rejected();
    }
    let Some(peer) = peers.get(&request.id).await else {
        return rejected();
    };
    let public_key = peer.read().await.pk.clone();
    let Ok(elapsed) = SystemTime::now().duration_since(UNIX_EPOCH) else {
        return rejected();
    };
    let Ok(unix_time) = i64::try_from(elapsed.as_secs()) else {
        return rejected();
    };
    GRANTS
        .lock()
        .unwrap()
        .register(&request, &public_key, unix_time, Instant::now())
        .json()
}

pub(crate) fn allows(id: &str, code: &str, conn_type: EnumOrUnknown<ConnType>) -> bool {
    if conn_type.enum_value() != Ok(ConnType::DEFAULT_CONN) {
        return false;
    }
    GRANTS
        .lock()
        .unwrap()
        .allows(id, code, ConnType::DEFAULT_CONN, Instant::now())
}

pub(crate) fn allows_relay(request: &RequestRelay, key: &str) -> bool {
    if !key.is_empty() && !request.licence_key.is_empty() && request.licence_key != key {
        return false;
    }
    let valid_grant = GRANTS
        .lock()
        .unwrap()
        .allows_relay(request, key, Instant::now());
    valid_grant
        || (!crate::jwt::SECRET.is_empty() && crate::jwt::verify_token(&request.token).is_ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(secret_key: &sign::SecretKey, code: &str) -> Registration {
        let mut verifier_message = b"switch-grant-verifier\0".to_vec();
        verifier_message.extend_from_slice(code.as_bytes());
        let mut request = Registration {
            id: "123456".to_owned(),
            switch_code_verifier: base64::encode(sha256::hash(&verifier_message).0),
            timestamp: "1700000000".to_owned(),
            signature: String::new(),
        };
        request.signature =
            base64::encode(sign::sign_detached(&signed_message(&request), secret_key).to_bytes());
        request
    }

    #[test]
    fn signed_registration_authenticates_before_returning_clock_correction() {
        let (public_key, secret_key) = sign::gen_keypair();
        let code = base64::encode(sha256::hash(b"00000000-0000-4000-8000-000000000000").0);
        let mut request = request(&secret_key, &code);
        assert_eq!(code, "24BV4OAwfVoBa+xNwzjWmHXrD7fmFKixJbCPsIIJXZg=");
        assert_eq!(
            request.switch_code_verifier,
            "a/UU3PSjPR2/r6JNwsVCVMKO02VfXGd/jWKV4CNQlNo="
        );
        assert_eq!(
            signed_message(&request),
            [
                b"switch-grant\0".as_slice(),
                b"123456\0",
                request.switch_code_verifier.as_bytes(),
                b"\0",
                b"1700000000"
            ]
            .concat()
        );
        let mut store = GrantStore::default();
        let now = Instant::now();
        assert_eq!(
            store.register(&request, &public_key.0, 1700000100, now),
            Response {
                accepted: false,
                server_time: Some(1700000100),
            }
        );
        request.id = "654321".to_owned();
        assert_eq!(
            store.register(&request, &public_key.0, 1700000100, now),
            Response::rejected()
        );
        let (wrong_key, _) = sign::gen_keypair();
        request.id = "123456".to_owned();
        assert_eq!(
            store.register(&request, &wrong_key.0, 1700000000, now),
            Response::rejected()
        );
    }

    #[test]
    fn destination_bound_grant_allows_transport_retries_without_renewing() {
        let (public_key, secret_key) = sign::gen_keypair();
        let code = base64::encode(sha256::hash(b"00000000-0000-4000-8000-000000000000").0);
        let request = request(&secret_key, &code);
        let mut store = GrantStore::default();
        let now = Instant::now();
        assert!(
            store
                .register(&request, &public_key.0, 1700000000, now)
                .accepted
        );
        assert!(store.allows(&request.id, &code, ConnType::DEFAULT_CONN, now));
        assert!(store.allows(
            &request.id,
            &code,
            ConnType::DEFAULT_CONN,
            now + Duration::from_secs(9)
        ));
        assert!(!store.allows("654321", &code, ConnType::DEFAULT_CONN, now));
        assert!(!store.allows(&request.id, &code, ConnType::FILE_TRANSFER, now));
        assert!(!store.allows(
            &request.id,
            &request.switch_code_verifier,
            ConnType::DEFAULT_CONN,
            now
        ));
        assert!(
            store
                .register(
                    &request,
                    &public_key.0,
                    1700000009,
                    now + Duration::from_secs(9)
                )
                .accepted
        );
        assert!(!store.allows(&request.id, &code, ConnType::DEFAULT_CONN, now + GRANT_TTL));
        assert!(
            !store
                .register(&request, &public_key.0, 1700000010, now + GRANT_TTL)
                .accepted
        );
        assert!(
            !store
                .register(&request, &public_key.0, 1700000130, now + REPLAY_TTL)
                .accepted
        );
    }

    #[test]
    fn stock_keyless_relay_requires_grant_and_rejects_a_wrong_supplied_key() {
        let (public_key, secret_key) = sign::gen_keypair();
        let code = base64::encode(sha256::hash(b"relay-switch").0);
        let registration = request(&secret_key, &code);
        let mut store = GrantStore::default();
        let now = Instant::now();
        let mut relay = RequestRelay {
            id: registration.id.clone(),
            switch_code: code,
            uuid: "fresh-relay-pairing-uuid".to_owned(),
            ..Default::default()
        };
        assert!(!store.allows_relay(&relay, "configured-server-key", now));
        assert!(
            store
                .register(&registration, &public_key.0, 1700000000, now)
                .accepted
        );
        assert!(store.allows_relay(&relay, "configured-server-key", now));
        relay.licence_key = "wrong-server-key".to_owned();
        assert!(!store.allows_relay(&relay, "configured-server-key", now));
        relay.licence_key = "configured-server-key".to_owned();
        assert!(store.allows_relay(&relay, "configured-server-key", now));
        relay.licence_key.clear();
        assert!(!store.allows_relay(&relay, "configured-server-key", now + GRANT_TTL));
    }

    #[test]
    fn malformed_and_excess_registrations_fail_closed() {
        let (public_key, secret_key) = sign::gen_keypair();
        let now = Instant::now();
        let mut store = GrantStore::default();
        for index in 0..MAX_GRANTS_PER_ID {
            let code = base64::encode(sha256::hash(index.to_string().as_bytes()).0);
            assert!(
                store
                    .register(&request(&secret_key, &code), &public_key.0, 1700000000, now)
                    .accepted
            );
        }
        let code = base64::encode(sha256::hash(b"excess").0);
        let mut request = request(&secret_key, &code);
        assert!(
            !store
                .register(&request, &public_key.0, 1700000000, now)
                .accepted
        );
        request.switch_code_verifier.pop();
        assert!(
            !store
                .register(&request, &public_key.0, 1700000000, now)
                .accepted
        );
        assert_eq!(canonical_digest("not a digest"), None);
        assert!(!allows("123456", &code, EnumOrUnknown::from_i32(99)));
    }
}
