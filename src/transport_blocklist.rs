//! Read-only, authenticated policies supplied by the paired Console.

use std::{collections::HashSet, net::IpAddr, sync::Arc, time::Duration};

use hbb_common::{
    log,
    tokio::{self, sync::watch},
};
use ipnetwork::IpNetwork;
use once_cell::sync::Lazy;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::routing_auth::{self, ADMISSION_TOKEN, AUTH_CLIENT};

const MAX_RULES: usize = 1024;
const MAX_SNAPSHOT_BYTES: usize = 256 * 1024;
const POLL_INTERVAL: Duration = Duration::from_secs(15);

static POLICY: Lazy<watch::Sender<Option<Arc<Policy>>>> = Lazy::new(|| watch::channel(None).0);

/// Roles are reported independently so operators can tell which service has applied a revision.
#[derive(Debug, Clone, Copy)]
pub enum Service {
    Hbbs,
    Hbbr,
}

impl Service {
    fn name(self) -> &'static str {
        match self {
            Self::Hbbs => "hbbs",
            Self::Hbbr => "hbbr",
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Snapshot {
    version: u8,
    revision: String,
    rules: Vec<Rule>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Rule {
    kind: String,
    value: String,
}

#[derive(Default)]
struct Policy {
    revision: String,
    devices: HashSet<String>,
    ips: HashSet<IpAddr>,
    networks: Vec<IpNetwork>,
}

impl Policy {
    fn from_snapshot(snapshot: Snapshot) -> Option<Self> {
        if snapshot.version != 1 || snapshot.rules.len() > MAX_RULES {
            return None;
        }
        let mut policy = Self::default();
        let mut digest = Sha256::new();
        for rule in snapshot.rules {
            let key = Sha256::digest(format!("{}\0{}", rule.kind, rule.value));
            digest.update(format!("{key:x}"));
            match rule.kind.as_str() {
                "device"
                    if !rule.value.is_empty()
                        && rule.value.len() <= 128
                        && rule
                            .value
                            .chars()
                            .all(|c| !c.is_whitespace() && !c.is_control()) =>
                {
                    policy.devices.insert(rule.value);
                }
                "ip" => {
                    policy
                        .ips
                        .insert(rule.value.parse::<IpAddr>().ok()?.to_canonical());
                }
                "cidr" if rule.value.contains('/') => {
                    let network = rule.value.parse::<IpNetwork>().ok()?;
                    if network.ip() != network.network() {
                        return None;
                    }
                    policy.networks.push(network);
                }
                _ => return None,
            }
        }
        let revision = format!("{:x}", digest.finalize());
        if snapshot.revision != revision {
            return None;
        }
        policy.revision = revision;
        Some(policy)
    }

    fn blocks_ip(&self, ip: IpAddr) -> bool {
        let ip = ip.to_canonical();
        self.ips.contains(&ip) || self.networks.iter().any(|network| network.contains(ip))
    }
}

/// Test the socket address against the currently applied IP and network rules.
pub fn blocks_ip(ip: IpAddr) -> bool {
    POLICY
        .borrow()
        .as_ref()
        .is_some_and(|policy| policy.blocks_ip(ip))
}

pub(crate) fn blocks_device(id: &str) -> bool {
    POLICY
        .borrow()
        .as_ref()
        .is_some_and(|policy| policy.devices.contains(id))
}

/// The relay checks all socket and forwarded addresses for both legs after every update.
pub async fn wait_until_blocked(addresses: [IpAddr; 4]) {
    wait_for_policy(POLICY.subscribe(), addresses).await;
}

async fn wait_for_policy(
    mut changes: watch::Receiver<Option<Arc<Policy>>>,
    addresses: [IpAddr; 4],
) {
    loop {
        if changes
            .borrow_and_update()
            .as_ref()
            .is_some_and(|policy| addresses.iter().any(|ip| policy.blocks_ip(*ip)))
        {
            return;
        }
        if changes.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

/// Poll with the shared admission credential and preserve the last applied policy on failure.
pub fn spawn_refresh(service: Service) {
    if !routing_auth::uses_device_admission() {
        return;
    }
    let Some((client, endpoint)) = AUTH_CLIENT.as_ref().zip(
        std::env::var("RUSTDESK_API_RUSTDESK_API_SERVER")
            .ok()
            .and_then(|url| routing_auth::console_endpoint(&url, "blocklist")),
    ) else {
        log::warn!(
            "{} managed blocklist has no valid paired Console URL",
            service.name()
        );
        return;
    };
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(POLL_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut failed = false;
        loop {
            interval.tick().await;
            let applied = POLICY
                .borrow()
                .as_ref()
                .map(|policy| policy.revision.clone());
            let mut request = client
                .get(endpoint.clone())
                .bearer_auth(&*ADMISSION_TOKEN)
                .query(&[("agent", service.name())]);
            if let Some(revision) = applied.as_ref() {
                request = request.query(&[("applied_revision", revision)]);
            }
            let snapshot = routing_auth::read_bounded_reply(request, MAX_SNAPSHOT_BYTES).await;
            if let Some(policy) = snapshot.and_then(Policy::from_snapshot) {
                if applied.as_ref() != Some(&policy.revision) {
                    POLICY.send_replace(Some(Arc::new(policy)));
                }
                if failed || applied.is_none() {
                    log::info!("{} managed blocklist connected to Console", service.name());
                }
                failed = false;
            } else if !failed {
                log::warn!(
                    "{} managed blocklist could not refresh from Console",
                    service.name()
                );
                failed = true;
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(rules: &[(&str, &str)]) -> Snapshot {
        let mut digest = Sha256::new();
        let rules = rules
            .iter()
            .map(|(kind, value)| {
                let key = Sha256::digest(format!("{kind}\0{value}"));
                digest.update(format!("{key:x}"));
                Rule {
                    kind: (*kind).into(),
                    value: (*value).into(),
                }
            })
            .collect();
        Snapshot {
            version: 1,
            revision: format!("{:x}", digest.finalize()),
            rules,
        }
    }

    #[test]
    fn managed_rules_match_exact_ids_and_both_address_families() {
        let policy = Policy::from_snapshot(snapshot(&[
            ("device", "Lab-01"),
            ("ip", "198.51.100.3"),
            ("cidr", "192.0.2.0/24"),
            ("cidr", "2001:db8:1::/48"),
        ]))
        .unwrap();
        assert!(policy.devices.contains("Lab-01"));
        assert!(!policy.devices.contains("lab-01"));
        for address in [
            "198.51.100.3",
            "::ffff:198.51.100.3",
            "192.0.2.255",
            "2001:db8:1::a",
        ] {
            assert!(policy.blocks_ip(address.parse().unwrap()), "{address}");
        }
        for address in ["198.51.100.4", "192.0.3.0", "2001:db8:2::a"] {
            assert!(!policy.blocks_ip(address.parse().unwrap()), "{address}");
        }
    }

    #[test]
    fn managed_snapshot_rejects_a_bad_rule_or_revision_as_a_unit() {
        for (kind, value) in [
            ("unknown", "192.0.2.1"),
            ("ip", "192.0.2.1/24"),
            ("cidr", "192.0.2.1/24"),
        ] {
            assert!(
                Policy::from_snapshot(snapshot(&[("device", "peer-01"), (kind, value)])).is_none()
            );
        }
        let mut different = snapshot(&[("ip", "192.0.2.4")]);
        different.revision = snapshot(&[]).revision;
        assert!(Policy::from_snapshot(different).is_none());
    }

    #[tokio::test]
    async fn a_refresh_blocks_any_address_used_by_an_active_pair() {
        let addresses =
            ["192.0.2.1", "192.0.2.2", "192.0.2.3", "192.0.2.4"].map(|addr| addr.parse().unwrap());
        for address in addresses {
            let (updates, receiver) = watch::channel(None);
            let task = tokio::spawn(wait_for_policy(receiver, addresses));
            updates.send_replace(Some(Arc::new(
                Policy::from_snapshot(snapshot(&[("ip", "192.0.2.100")])).unwrap(),
            )));
            assert!(!task.is_finished());
            updates.send_replace(Some(Arc::new(
                Policy::from_snapshot(snapshot(&[("ip", &address.to_string())])).unwrap(),
            )));
            assert!(tokio::time::timeout(Duration::from_secs(1), task)
                .await
                .unwrap()
                .is_ok());
        }
    }
}
