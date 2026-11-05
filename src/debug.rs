//! An optional trace of the read-state calls, written to the file named by `MX_DEBUG`.

use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

fn target() -> Option<&'static PathBuf> {
    static TARGET: OnceLock<Option<PathBuf>> = OnceLock::new();
    TARGET
        .get_or_init(|| std::env::var_os("MX_DEBUG").map(PathBuf::from))
        .as_ref()
}

/// Whether a trace was asked for.
pub fn on() -> bool {
    target().is_some()
}

/// Appends a line to the trace, with the time in milliseconds.
pub fn log(line: &str) {
    let Some(path) = target() else {
        return;
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis());
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path);
    if let Ok(mut file) = file {
        let _ = writeln!(file, "{now} {line}");
    }
}
