mod rules;

use crate::runtime_config::{self, GeoConfig};
use hbb_common::log;
use maxminddb::{geoip2, Reader};
use once_cell::sync::Lazy;
use serde_derive::Serialize;
use std::{fs::OpenOptions, io::Read, net::IpAddr, path::Path, sync::RwLock};

use rules::{DbRequirements, RuleSet};

const COUNTRY_MMDB: &str = "GeoLite2-Country.mmdb";
const CITY_MMDB: &str = "GeoLite2-City.mmdb";
const ASN_MMDB: &str = "GeoLite2-ASN.mmdb";
const MAX_MMDB_BYTES: u64 = 256 * 1024 * 1024;

static STATE: Lazy<RwLock<GeoState>> = Lazy::new(|| RwLock::new(GeoState::disabled()));

struct GeoState {
    enabled: bool,
    readers: GeoReaders,
    rules: RuleSet,
    warnings: Vec<String>,
}

impl GeoState {
    fn disabled() -> Self {
        Self {
            enabled: false,
            readers: GeoReaders::default(),
            rules: RuleSet::empty(),
            warnings: Vec::new(),
        }
    }
}

#[derive(Default)]
struct GeoReaders {
    country: Option<Reader<Vec<u8>>>,
    city: Option<Reader<Vec<u8>>>,
    asn: Option<Reader<Vec<u8>>>,
}

impl GeoReaders {
    #[cfg(test)]
    fn reload_from_dir(&mut self, data_dir: &Path) -> Vec<String> {
        let updates = GeoReaderUpdates::load(data_dir);
        let mut warnings = Vec::new();
        apply_reader_update(&mut self.country, "Country", updates.country, &mut warnings);
        apply_reader_update(&mut self.city, "City", updates.city, &mut warnings);
        apply_reader_update(&mut self.asn, "ASN", updates.asn, &mut warnings);
        warnings
    }

    fn lookup(&self, ip: IpAddr) -> GeoFacts {
        let mut facts = GeoFacts::default();
        if let Some(reader) = self.city.as_ref() {
            lookup_city(reader, ip, &mut facts);
        }
        if let Some(reader) = self.country.as_ref() {
            lookup_country(reader, ip, &mut facts);
        }
        if let Some(reader) = self.asn.as_ref() {
            lookup_asn(reader, ip, &mut facts);
        }
        facts
    }

    fn missing_requirements(&self, requirements: DbRequirements) -> Vec<String> {
        let mut warnings = Vec::new();
        if requirements.city && self.city.is_none() {
            warnings.push("rules use City fields but the City MMDB is unavailable".to_owned());
        }
        if requirements.asn && self.asn.is_none() {
            warnings.push("rules use ASN/ISP fields but the ASN MMDB is unavailable".to_owned());
        }
        if requirements.country && self.country.is_none() && self.city.is_none() {
            warnings.push(
                "rules use Country/Continent fields but neither Country nor City MMDB is available"
                    .to_owned(),
            );
        }
        warnings
    }
}

struct GeoReaderUpdates {
    country: ReaderUpdate,
    city: ReaderUpdate,
    asn: ReaderUpdate,
}

impl GeoReaderUpdates {
    fn load(data_dir: &Path) -> Self {
        Self {
            country: load_reader_update("Country", &data_dir.join(COUNTRY_MMDB)),
            city: load_reader_update("City", &data_dir.join(CITY_MMDB)),
            asn: load_reader_update("ASN", &data_dir.join(ASN_MMDB)),
        }
    }
}

enum ReaderUpdate {
    Loaded(Reader<Vec<u8>>),
    Missing,
    Invalid(String),
}

#[derive(Clone, Debug, Default)]
pub(super) struct GeoFacts {
    pub(super) continent: Option<String>,
    pub(super) country: Option<String>,
    pub(super) subdivision_codes: Vec<String>,
    pub(super) subdivision_names: Vec<String>,
    pub(super) city_names: Vec<String>,
    pub(super) city_geoname_id: Option<u32>,
    pub(super) asn: Option<u32>,
    pub(super) asn_org: Option<String>,
}

pub(crate) fn validate_config(config: &GeoConfig) -> Result<(), String> {
    RuleSet::compile(config).map(|_| ())
}

pub fn reload() -> String {
    let Some(config) = runtime_config::snapshot() else {
        return "Geo relay disabled because runtime config is unavailable; using upstream relay selection"
            .to_owned();
    };
    let rules = match RuleSet::compile(&config.geo) {
        Ok(rules) => rules,
        Err(err) => {
            return format!(
                "Geo relay config is invalid; keeping the previous runtime state: {err}"
            )
        }
    };
    if !config.geo.enabled {
        let Ok(mut state) = STATE.write() else {
            return "Geo relay state lock failed".to_owned();
        };
        if !runtime_config::snapshot().is_some_and(|current| current.revision == config.revision) {
            return format!(
                "Geo relay reload for revision {} was superseded",
                config.revision
            );
        }
        state.enabled = false;
        state.rules = RuleSet::empty();
        state.warnings.clear();
        return "Geo relay disabled by runtime config; using upstream relay selection".to_owned();
    }

    let reader_updates = GeoReaderUpdates::load(Path::new("."));
    let Ok(mut state) = STATE.write() else {
        return "Geo relay state lock failed".to_owned();
    };
    if !runtime_config::snapshot().is_some_and(|current| current.revision == config.revision) {
        return format!(
            "Geo relay reload for revision {} was superseded",
            config.revision
        );
    }
    let mut warnings = Vec::new();
    apply_reader_update(
        &mut state.readers.country,
        "Country",
        reader_updates.country,
        &mut warnings,
    );
    apply_reader_update(
        &mut state.readers.city,
        "City",
        reader_updates.city,
        &mut warnings,
    );
    apply_reader_update(
        &mut state.readers.asn,
        "ASN",
        reader_updates.asn,
        &mut warnings,
    );
    warnings.extend(state.readers.missing_requirements(rules.requirements()));
    state.enabled = true;
    state.rules = rules;
    state.warnings = warnings;
    let databases = state.readers.loaded_names();
    let mut message = format!(
        "Geo relay loaded: {} ordered rules, databases={}",
        state.rules.len(),
        if databases.is_empty() {
            "none".to_owned()
        } else {
            databases.join(", ")
        }
    );
    if !state.warnings.is_empty() {
        message.push_str(&format!("; warnings: {}", state.warnings.join("; ")));
    }
    message
}

pub fn select_relay(pa: IpAddr, pb: IpAddr, eligible_relays: &[String]) -> Option<String> {
    let state = STATE.read().ok()?;
    if !state.enabled || eligible_relays.is_empty() {
        return None;
    }

    let facts_a = state.readers.lookup(pa);
    let facts_b = state.readers.lookup(pb);
    let selection = state.rules.select(&facts_a, &facts_b, eligible_relays)?;
    log::debug!(
        "Geo relay selected {} by rule '{}'",
        selection.relay,
        selection.rule_name
    );
    Some(selection.relay)
}

impl GeoReaders {
    fn loaded_names(&self) -> Vec<&'static str> {
        let mut names = Vec::new();
        if self.country.is_some() {
            names.push(COUNTRY_MMDB);
        }
        if self.city.is_some() {
            names.push(CITY_MMDB);
        }
        if self.asn.is_some() {
            names.push(ASN_MMDB);
        }
        names
    }
}

fn load_reader_update(label: &str, path: &Path) -> ReaderUpdate {
    if !path.is_file() {
        return ReaderUpdate::Missing;
    }
    match open_reader(path, label) {
        Ok(reader) => ReaderUpdate::Loaded(reader),
        Err(err) => ReaderUpdate::Invalid(err),
    }
}

fn apply_reader_update(
    current: &mut Option<Reader<Vec<u8>>>,
    label: &str,
    update: ReaderUpdate,
    warnings: &mut Vec<String>,
) {
    match update {
        ReaderUpdate::Loaded(reader) => *current = Some(reader),
        ReaderUpdate::Missing => {
            let message = if current.is_some() {
                format!("{label} MMDB is missing; retaining the last known-good reader")
            } else {
                format!("{label} MMDB is unavailable; related Geo fields are disabled")
            };
            log::info!("{message}");
            warnings.push(message);
        }
        ReaderUpdate::Invalid(err) => {
            let message = if current.is_some() {
                format!("{label} MMDB is invalid; retaining the last known-good reader: {err}")
            } else {
                format!("{label} MMDB is invalid; related Geo fields are disabled: {err}")
            };
            log::warn!("{message}");
            warnings.push(message);
        }
    }
}

fn open_reader(path: &Path, expected_type: &str) -> Result<Reader<Vec<u8>>, String> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = options
        .open(path)
        .map_err(|err| format!("cannot open MMDB: {err}"))?;
    let metadata = file
        .metadata()
        .map_err(|err| format!("cannot inspect MMDB: {err}"))?;
    if !metadata.is_file() {
        return Err("MMDB must be a regular file".to_owned());
    }
    let declared_len = metadata.len();
    if declared_len == 0 || declared_len > MAX_MMDB_BYTES {
        return Err(format!(
            "MMDB size must be between 1 and {MAX_MMDB_BYTES} bytes"
        ));
    }
    let bytes = read_mmdb(&mut file, declared_len)?;
    let reader = Reader::from_source(bytes).map_err(|err| format!("invalid MMDB: {err}"))?;
    reader
        .verify()
        .map_err(|err| format!("MMDB integrity verification failed: {err}"))?;
    let database_type = &reader.metadata().database_type;
    let type_matches = database_type
        .rsplit('-')
        .next()
        .is_some_and(|value| value.eq_ignore_ascii_case(expected_type));
    if !type_matches {
        return Err(format!(
            "MMDB type '{database_type}' does not match {expected_type}"
        ));
    }
    Ok(reader)
}

fn read_mmdb(reader: &mut impl Read, declared_len: u64) -> Result<Vec<u8>, String> {
    let read_limit = declared_len
        .checked_add(1)
        .ok_or_else(|| "MMDB size is not supported".to_owned())?;
    let capacity =
        usize::try_from(read_limit).map_err(|_| "MMDB size is not supported".to_owned())?;
    let mut bytes = Vec::with_capacity(capacity);
    reader
        .take(read_limit)
        .read_to_end(&mut bytes)
        .map_err(|err| format!("cannot read MMDB: {err}"))?;
    if bytes.len() as u64 != declared_len {
        return Err("MMDB changed while reading".to_owned());
    }
    Ok(bytes)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct GeoStatus {
    revision: u64,
    enabled: bool,
    rule_count: usize,
    country_available: bool,
    city_available: bool,
    asn_available: bool,
    warnings: Vec<String>,
}

pub fn status() -> String {
    let revision = runtime_config::snapshot()
        .map(|config| config.revision)
        .unwrap_or_default();
    let Ok(state) = STATE.read() else {
        return r#"{"error":"Geo relay state lock failed"}"#.to_owned();
    };
    let status = GeoStatus {
        revision,
        enabled: state.enabled,
        rule_count: state.rules.len(),
        country_available: state.readers.country.is_some(),
        city_available: state.readers.city.is_some(),
        asn_available: state.readers.asn.is_some(),
        warnings: state.warnings.clone(),
    };
    serde_json::to_string(&status)
        .unwrap_or_else(|err| format!(r#"{{"error":"cannot serialize Geo status: {err}"}}"#))
}

fn lookup_city(reader: &Reader<Vec<u8>>, ip: IpAddr, facts: &mut GeoFacts) {
    let Ok(result) = reader.lookup(ip) else {
        return;
    };
    let Ok(Some(record)) = result.decode::<geoip2::City>() else {
        return;
    };

    set_if_empty(&mut facts.continent, record.continent.code);
    set_if_empty(&mut facts.country, record.country.iso_code);
    facts.city_geoname_id = record.city.geoname_id;
    append_names(&mut facts.city_names, &record.city.names);
    for subdivision in record.subdivisions {
        if let Some(code) = subdivision.iso_code {
            push_unique(&mut facts.subdivision_codes, code);
        }
        append_names(&mut facts.subdivision_names, &subdivision.names);
    }
}

fn lookup_country(reader: &Reader<Vec<u8>>, ip: IpAddr, facts: &mut GeoFacts) {
    let Ok(result) = reader.lookup(ip) else {
        return;
    };
    let Ok(Some(record)) = result.decode::<geoip2::Country>() else {
        return;
    };
    set_if_empty(&mut facts.continent, record.continent.code);
    set_if_empty(&mut facts.country, record.country.iso_code);
}

fn lookup_asn(reader: &Reader<Vec<u8>>, ip: IpAddr, facts: &mut GeoFacts) {
    let Ok(result) = reader.lookup(ip) else {
        return;
    };
    let Ok(Some(record)) = result.decode::<geoip2::Asn>() else {
        return;
    };
    facts.asn = record.autonomous_system_number;
    facts.asn_org = record
        .autonomous_system_organization
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty());
}

fn append_names(target: &mut Vec<String>, names: &geoip2::Names<'_>) {
    for name in [
        names.english,
        names.simplified_chinese,
        names.japanese,
        names.german,
        names.spanish,
        names.french,
        names.brazilian_portuguese,
        names.russian,
    ]
    .into_iter()
    .flatten()
    {
        push_unique(target, name);
    }
}

fn push_unique(target: &mut Vec<String>, value: &str) {
    let value = value.trim();
    if !value.is_empty() && !target.iter().any(|old| old.eq_ignore_ascii_case(value)) {
        target.push(value.to_owned());
    }
}

fn set_if_empty(target: &mut Option<String>, value: Option<&str>) {
    if target.is_none() {
        *target = value
            .map(|value| value.trim().to_ascii_uppercase())
            .filter(|value| !value.is_empty());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_mmdb_files_are_optional() {
        let data_dir = std::env::temp_dir().join(format!(
            "rustdesk-geo-missing-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&data_dir).unwrap();
        let mut readers = GeoReaders::default();
        let warnings = readers.reload_from_dir(&data_dir);
        let facts = readers.lookup("198.51.100.1".parse().unwrap());
        assert!(facts.country.is_none());
        assert!(facts.city_names.is_empty());
        assert!(facts.asn.is_none());
        assert_eq!(warnings.len(), 3);
        std::fs::remove_dir(&data_dir).unwrap();
    }

    #[test]
    fn oversized_mmdb_is_rejected_before_reading() {
        let path =
            std::env::temp_dir().join(format!("rustdesk-geo-oversized-{}", std::process::id()));
        let file = std::fs::File::create(&path).unwrap();
        file.set_len(MAX_MMDB_BYTES + 1).unwrap();
        drop(file);

        let error = open_reader(&path, "City").unwrap_err();
        assert!(error.contains("MMDB size"));
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn mmdb_read_is_bounded_to_declared_length() {
        let mut endless = std::io::repeat(b'x');
        let error = read_mmdb(&mut endless, 4).unwrap_err();
        assert!(error.contains("changed while reading"));
    }
}
