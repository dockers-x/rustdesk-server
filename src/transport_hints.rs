use hbb_common::{
    bytes::Bytes,
    rendezvous_proto::{rendezvous_message, PunchHoleResponse, RelayResponse, RendezvousMessage},
};

pub(crate) fn force_relay(requested: bool, required: bool) -> bool {
    requested || required
}

pub(crate) fn request(ipv6: Bytes, force_relay: bool, message: &mut RendezvousMessage) {
    let ipv6 = if force_relay { Bytes::new() } else { ipv6 };
    match message.union.as_mut() {
        Some(rendezvous_message::Union::PunchHole(punch)) => {
            punch.socket_addr_v6 = ipv6;
            punch.force_relay = force_relay;
        }
        Some(rendezvous_message::Union::FetchLocalAddr(local)) => {
            local.socket_addr_v6 = ipv6;
        }
        _ => {}
    }
}

pub(crate) fn response(ipv6: Bytes, force_relay: bool, message: &mut PunchHoleResponse) {
    message.socket_addr_v6 = if force_relay { Bytes::new() } else { ipv6 };
}

pub(crate) fn relay_response(force_relay: bool, message: &mut RelayResponse) {
    if force_relay {
        message.socket_addr_v6 = Bytes::new();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hbb_common::{
        protobuf::Message,
        rendezvous_proto::{FetchLocalAddr, PunchHole},
        AddrMangle,
    };

    #[test]
    fn direct_and_local_requests_preserve_ipv6_on_the_wire() {
        let ipv6: Bytes = AddrMangle::encode("[2001:db8::7]:21118".parse().unwrap()).into();
        for address in [Bytes::new(), ipv6] {
            let mut message = RendezvousMessage::new();
            message.set_punch_hole(PunchHole::default());
            request(address.clone(), false, &mut message);
            let received =
                RendezvousMessage::parse_from_bytes(&message.write_to_bytes().unwrap()).unwrap();
            assert_eq!(received.punch_hole().socket_addr_v6, address);

            message.set_fetch_local_addr(FetchLocalAddr::default());
            request(address.clone(), false, &mut message);
            let received =
                RendezvousMessage::parse_from_bytes(&message.write_to_bytes().unwrap()).unwrap();
            assert_eq!(received.fetch_local_addr().socket_addr_v6, address);
        }
    }

    #[test]
    fn direct_and_local_responses_preserve_ipv6_on_the_wire() {
        let ipv6: Bytes = AddrMangle::encode("[2001:db8::8]:21118".parse().unwrap()).into();
        for address in [Bytes::new(), ipv6] {
            for is_local in [false, true] {
                let mut message = PunchHoleResponse::default();
                if is_local {
                    message.set_is_local(true);
                }
                response(address.clone(), false, &mut message);
                let received =
                    PunchHoleResponse::parse_from_bytes(&message.write_to_bytes().unwrap())
                        .unwrap();
                assert_eq!(received.socket_addr_v6, address);
                assert_eq!(received.is_local(), is_local);
            }
        }
    }

    #[test]
    fn client_or_server_force_relay_is_transmitted() {
        for (requested, required) in [(false, false), (true, false), (false, true), (true, true)] {
            let mut message = RendezvousMessage::new();
            message.set_punch_hole(PunchHole::default());
            request(Bytes::new(), force_relay(requested, required), &mut message);
            let received =
                RendezvousMessage::parse_from_bytes(&message.write_to_bytes().unwrap()).unwrap();
            assert_eq!(received.punch_hole().force_relay, requested || required);
        }
    }

    #[test]
    fn mandatory_relay_does_not_advertise_ipv6_in_requests_or_responses() {
        let address: Bytes = AddrMangle::encode("[2001:db8::9]:21118".parse().unwrap()).into();
        for required in [false, true] {
            let expected = if required {
                Bytes::new()
            } else {
                address.clone()
            };
            let mut message = RendezvousMessage::new();
            message.set_punch_hole(PunchHole::default());
            request(address.clone(), required, &mut message);
            let received =
                RendezvousMessage::parse_from_bytes(&message.write_to_bytes().unwrap()).unwrap();
            assert_eq!(received.punch_hole().socket_addr_v6, expected);

            message.set_fetch_local_addr(FetchLocalAddr::default());
            request(address.clone(), required, &mut message);
            let received =
                RendezvousMessage::parse_from_bytes(&message.write_to_bytes().unwrap()).unwrap();
            assert_eq!(received.fetch_local_addr().socket_addr_v6, expected);

            let mut punch = PunchHoleResponse::default();
            response(address.clone(), required, &mut punch);
            let received =
                PunchHoleResponse::parse_from_bytes(&punch.write_to_bytes().unwrap()).unwrap();
            assert_eq!(received.socket_addr_v6, expected);

            let mut relay = RelayResponse {
                socket_addr_v6: address.clone(),
                ..Default::default()
            };
            relay_response(required, &mut relay);
            let received =
                RelayResponse::parse_from_bytes(&relay.write_to_bytes().unwrap()).unwrap();
            assert_eq!(received.socket_addr_v6, expected);
        }
    }
}
