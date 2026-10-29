use crate::{jwt, switch_grant};
use hbb_common::rendezvous_proto::{
    punch_hole_response, PunchHoleRequest, PunchHoleResponse, RelayResponse, RequestRelay,
};

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
