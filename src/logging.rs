use flexi_logger::{opt_format, FormatFunction, Logger, LoggerHandle, WriteMode};
use hbb_common::{log::Record, ResultType};
use std::io::{self, Write};

/// Starts the daemon logger with text output by default.
pub fn start(component: &str) -> ResultType<LoggerHandle> {
    let format: FormatFunction = match std::env::var("LOG_FORMAT").as_deref() {
        Ok(value) if value.eq_ignore_ascii_case("json") => match component {
            "hbbr" => |writer, _, record| json(writer, record, "hbbr"),
            _ => |writer, _, record| json(writer, record, "hbbs"),
        },
        _ => opt_format,
    };
    Ok(Logger::try_with_env_or_str("info")?
        .log_to_stdout()
        .format(format)
        .write_mode(WriteMode::Async)
        .start()?)
}

fn json(writer: &mut dyn Write, record: &Record<'_>, component: &str) -> io::Result<()> {
    // Legacy log arguments can contain credentials, protocol frames, or remote input.
    // Include source metadata and static text only.
    let event = serde_json::json!({
        "timestamp": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        "level": record.level().as_str(),
        "component": component,
        "event": record.target(),
        "module": record.module_path(),
        "line": record.line(),
        "message": record.args().as_str(),
    });
    serde_json::to_writer(writer, &event).map_err(io::Error::other)
}
