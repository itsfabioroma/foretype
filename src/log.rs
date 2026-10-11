//! Optional debug log (`FORETYPE_LOG=/path/to/file`). The terminal itself is never used.

use std::io::Write as _;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

static LOG: OnceLock<Mutex<(std::fs::File, Instant)>> = OnceLock::new();

pub fn open(path: &std::path::Path) -> std::io::Result<()> {
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    let _ = LOG.set(Mutex::new((file, Instant::now())));
    Ok(())
}

pub fn enabled() -> bool {
    LOG.get().is_some()
}

pub fn write(args: std::fmt::Arguments<'_>) {
    if let Some(log) = LOG.get()
        && let Ok(mut guard) = log.lock()
    {
        let elapsed = guard.1.elapsed().as_secs_f64() * 1000.0;
        let _ = writeln!(guard.0, "{elapsed:10.3} {args}");
    }
}

#[macro_export]
macro_rules! log {
    ($($arg:tt)*) => {
        if $crate::log::enabled() {
            $crate::log::write(format_args!($($arg)*));
        }
    };
}
