use once_cell::sync::Lazy;
use serde_derive::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    fs::{File, OpenOptions},
    io::Read,
    path::{Component, Path},
    sync::{Arc, RwLock},
};

const CONFIG_VERSION: u8 = 1;
const MAX_CONFIG_BYTES: u64 = 512 * 1024;
const TEMP_PREFIX: &str = ".geo-config-";
const TEMP_SUFFIX: &str = ".json";

static STATE: Lazy<RwLock<Arc<RuntimeConfig>>> =
    Lazy::new(|| RwLock::new(Arc::new(RuntimeConfig::default())));

#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
pub struct RuntimeConfig {
    pub version: u8,
    pub revision: u64,
    pub relay_servers: Vec<String>,
    pub geo: GeoConfig,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            version: CONFIG_VERSION,
            revision: 0,
            relay_servers: Vec::new(),
            geo: GeoConfig::default(),
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GeoConfig {
    pub enabled: bool,
    pub rules: Vec<GeoRuleConfig>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GeoRuleConfig {
    pub name: String,
    #[serde(default = "default_true")]
    pub symmetric: bool,
    #[serde(rename = "match")]
    pub matches: EndpointExpressions,
    pub relays: Vec<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
pub struct EndpointExpressions {
    pub client_a: String,
    pub client_b: String,
}

impl Default for EndpointExpressions {
    fn default() -> Self {
        Self {
            client_a: "*".to_owned(),
            client_b: "*".to_owned(),
        }
    }
}

pub struct ApplyOutcome {
    pub revision: u64,
    pub message: String,
    pub relay_servers: Option<String>,
}

pub fn snapshot() -> Option<Arc<RuntimeConfig>> {
    STATE.read().ok().map(|state| state.clone())
}

pub fn apply_temporary(
    file_name: &str,
    declared_len: u64,
    expected_sha256: &str,
) -> Result<ApplyOutcome, String> {
    validate_temporary_name(file_name)?;
    if declared_len > MAX_CONFIG_BYTES {
        return Err(format!("Geo config exceeds {MAX_CONFIG_BYTES} bytes"));
    }
    validate_sha256(expected_sha256)?;
    let mut file = open_temporary(Path::new(file_name))?;
    let metadata = file
        .metadata()
        .map_err(|err| format!("cannot inspect Geo config handoff: {err}"))?;
    if !metadata.is_file() {
        return Err("Geo config handoff must be a regular file".to_owned());
    }
    if metadata.len() != declared_len {
        return Err("Geo config handoff length does not match".to_owned());
    }
    let bytes = read_handoff(&mut file, declared_len)?;
    let actual_sha256 = format!("{:x}", Sha256::digest(&bytes));
    if !actual_sha256.eq_ignore_ascii_case(expected_sha256) {
        return Err("Geo config handoff checksum does not match".to_owned());
    }
    let current_revision = snapshot().map(|config| config.revision).unwrap_or_default();
    let config = parse_config(&bytes, current_revision)?;
    let revision = config.revision;
    let relay_servers = (!config.relay_servers.is_empty()).then(|| config.relay_servers.join(","));
    let rule_count = config.geo.rules.len();
    let mut state = STATE
        .write()
        .map_err(|err| format!("Geo config state lock failed: {err}"))?;
    if revision <= state.revision {
        return Err(format!(
            "stale Geo config revision {revision}; current revision is {}",
            state.revision
        ));
    }
    *state = Arc::new(config);
    Ok(ApplyOutcome {
        revision,
        message: format!("Geo config revision {revision} applied with {rule_count} rules"),
        relay_servers,
    })
}

fn open_temporary(path: &Path) -> Result<File, String> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    options
        .open(path)
        .map_err(|err| format!("cannot open Geo config handoff: {err}"))
}

fn read_handoff(reader: &mut impl Read, declared_len: u64) -> Result<Vec<u8>, String> {
    let read_limit = declared_len
        .checked_add(1)
        .ok_or_else(|| "Geo config handoff length is not supported".to_owned())?;
    let capacity = usize::try_from(read_limit)
        .map_err(|_| "Geo config handoff length is not supported".to_owned())?;
    let mut bytes = Vec::with_capacity(capacity);
    reader
        .take(read_limit)
        .read_to_end(&mut bytes)
        .map_err(|err| format!("cannot read Geo config handoff: {err}"))?;
    if bytes.len() as u64 != declared_len {
        return Err("Geo config handoff changed while reading".to_owned());
    }
    Ok(bytes)
}

fn validate_sha256(value: &str) -> Result<(), String> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("Geo config handoff checksum must be 64 hexadecimal characters".to_owned());
    }
    Ok(())
}

fn validate_temporary_name(file_name: &str) -> Result<(), String> {
    let path = Path::new(file_name);
    let mut components = path.components();
    let one_component =
        matches!(components.next(), Some(Component::Normal(_))) && components.next().is_none();
    let nonce = file_name
        .strip_prefix(TEMP_PREFIX)
        .and_then(|value| value.strip_suffix(TEMP_SUFFIX))
        .filter(|value| !value.is_empty());
    if !one_component
        || nonce.is_none()
        || !nonce
            .unwrap_or_default()
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-')
    {
        return Err("invalid Geo config handoff filename".to_owned());
    }
    Ok(())
}

fn parse_config(bytes: &[u8], current_revision: u64) -> Result<RuntimeConfig, String> {
    let config: RuntimeConfig =
        serde_json::from_slice(bytes).map_err(|err| format!("invalid Geo config JSON: {err}"))?;
    if config.version != CONFIG_VERSION {
        return Err(format!(
            "unsupported Geo config version {}; expected {CONFIG_VERSION}",
            config.version
        ));
    }
    if config.revision <= current_revision {
        return Err(format!(
            "stale Geo config revision {}; current revision is {current_revision}",
            config.revision
        ));
    }
    validate(config)
}

fn validate(mut config: RuntimeConfig) -> Result<RuntimeConfig, String> {
    normalize_unique(&mut config.relay_servers, "relayServers")?;
    validate_geo(&mut config.geo, &config.relay_servers)?;
    Ok(config)
}

fn validate_geo(config: &mut GeoConfig, relay_servers: &[String]) -> Result<(), String> {
    if config.rules.len() > 256 {
        return Err("geo.rules must not exceed 256 entries".to_owned());
    }
    if !config.enabled {
        return Ok(());
    }
    if config.rules.is_empty() {
        return Err("geo.enabled is true but geo.rules is empty".to_owned());
    }
    if relay_servers.is_empty() {
        return Err("geo.enabled is true but relay_servers is empty".to_owned());
    }

    let relay_set: HashSet<String> = relay_servers
        .iter()
        .map(|relay| relay.to_ascii_lowercase())
        .collect();
    let mut names = HashSet::new();
    for (index, rule) in config.rules.iter_mut().enumerate() {
        rule.name = rule.name.trim().to_owned();
        if rule.name.is_empty() || rule.name.len() > 120 {
            return Err(format!(
                "geo.rules[{index}].name must contain 1 to 120 bytes"
            ));
        }
        if !names.insert(rule.name.to_ascii_lowercase()) {
            return Err(format!("duplicate Geo rule name: {}", rule.name));
        }
        rule.matches.client_a = normalized_expression(&rule.matches.client_a);
        rule.matches.client_b = normalized_expression(&rule.matches.client_b);
        if rule.relays.len() > 32 {
            return Err(format!(
                "Geo rule '{}' must not exceed 32 relays",
                rule.name
            ));
        }
        normalize_unique(
            &mut rule.relays,
            &format!("Geo rule '{}' relays", rule.name),
        )?;
        if rule.relays.is_empty() {
            return Err(format!("Geo rule '{}' has no relays", rule.name));
        }
        for relay in &rule.relays {
            if !relay_set.contains(&relay.to_ascii_lowercase()) {
                return Err(format!(
                    "Geo rule '{}' references relay '{}' which is absent from relay_servers",
                    rule.name, relay
                ));
            }
        }
    }
    crate::geo_relay::validate_config(config)
}

fn normalize_unique(values: &mut Vec<String>, field: &str) -> Result<(), String> {
    let mut seen = HashSet::new();
    for value in values.iter_mut() {
        *value = value.trim().to_owned();
        if value.is_empty() {
            return Err(format!("{field} contains an empty value"));
        }
        if !seen.insert(value.to_ascii_lowercase()) {
            return Err(format!("{field} contains duplicate value '{value}'"));
        }
    }
    Ok(())
}

fn normalized_expression(value: &str) -> String {
    let value = value.trim();
    if value.is_empty() {
        "*".to_owned()
    } else {
        value.to_owned()
    }
}

fn default_true() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    fn valid_json(revision: u64) -> Vec<u8> {
        format!(
            r#"{{"version":1,"revision":{revision},"relayServers":["relay-a:21117"],"geo":{{"enabled":true,"rules":[{{"name":"country","match":{{"clientA":"CN","clientB":"*"}},"relays":["relay-a:21117"]}}]}}}}"#
        )
        .into_bytes()
    }

    #[test]
    fn parses_versioned_json() {
        let config = parse_config(&valid_json(1), 0).unwrap();
        assert_eq!(config.revision, 1);
        assert_eq!(config.geo.rules.len(), 1);
    }

    #[test]
    fn rejects_stale_revision() {
        let err = parse_config(&valid_json(4), 4).unwrap_err();
        assert!(err.contains("stale Geo config revision"));
    }

    #[test]
    fn rejects_unknown_version() {
        let bytes = br#"{"version":2,"revision":1}"#;
        let err = parse_config(bytes, 0).unwrap_err();
        assert!(err.contains("unsupported Geo config version"));
    }

    #[test]
    fn rejects_path_in_handoff_name() {
        assert!(validate_temporary_name("../.geo-config-a.json").is_err());
        assert!(validate_temporary_name(".geo-config-.json").is_err());
        assert!(validate_temporary_name(".geo-config-a.json").is_ok());
    }

    #[test]
    fn handoff_read_is_bounded_to_declared_length() {
        let mut endless = std::io::repeat(b'x');
        let err = read_handoff(&mut endless, 4).unwrap_err();
        assert!(err.contains("changed while reading"));
    }

    #[test]
    fn invalid_geo_relay_reference_rejects_document() {
        let bytes = br#"{"version":1,"revision":1,"relayServers":["relay-a"],"geo":{"enabled":true,"rules":[{"name":"bad","match":{"clientA":"CN","clientB":"JP"},"relays":["relay-b"]}]}}"#;
        let err = parse_config(bytes, 0).unwrap_err();
        assert!(err.contains("absent from relay_servers"));
    }

    #[test]
    fn duplicate_rule_names_are_case_insensitive() {
        let bytes = br#"{"version":1,"revision":1,"relayServers":["relay-a"],"geo":{"enabled":true,"rules":[{"name":"China","match":{"clientA":"CN","clientB":"*"},"relays":["relay-a"]},{"name":"china","match":{"clientA":"*","clientB":"CN"},"relays":["relay-a"]}]}}"#;
        let err = parse_config(bytes, 0).unwrap_err();
        assert!(err.contains("duplicate Geo rule name"));
    }

    #[test]
    fn temporary_handoff_checks_length_checksum_and_applies() {
        let file_name = format!(".geo-config-test-{}.json", std::process::id());
        let bytes = valid_json(1);
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&file_name)
            .unwrap();
        file.write_all(&bytes).unwrap();
        file.sync_all().unwrap();
        drop(file);
        let checksum = format!("{:x}", Sha256::digest(&bytes));

        assert!(apply_temporary(&file_name, bytes.len() as u64 + 1, &checksum).is_err());
        assert!(apply_temporary(&file_name, bytes.len() as u64, "not-a-checksum").is_err());
        let outcome = apply_temporary(&file_name, bytes.len() as u64, &checksum).unwrap();
        assert!(outcome.message.contains("revision 1 applied"));

        std::fs::remove_file(file_name).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn temporary_handoff_rejects_symlink() {
        use std::os::unix::fs::symlink;

        let suffix = std::process::id();
        let target = format!(".geo-config-target-{suffix}.json");
        let link = format!(".geo-config-link-{suffix}.json");
        std::fs::write(&target, valid_json(2)).unwrap();
        symlink(&target, &link).unwrap();
        let err = open_temporary(Path::new(&link)).unwrap_err();
        assert!(err.contains("cannot open Geo config handoff"));
        std::fs::remove_file(link).unwrap();
        std::fs::remove_file(target).unwrap();
    }
}
