//! Append-only log with size-capped rotation (impl spec §6).
//!
//! Timestamps, state transitions, and every herdr argv land here. This is the
//! debugging story: if a dispatch misbehaves, the exact command line that ran is
//! in `syncd.log`.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

const MAX_BYTES: u64 = 5 * 1024 * 1024;

pub struct Logger {
    path: PathBuf,
    file: Mutex<Option<File>>,
    /// Also echo to stderr — useful for `sync --once` and `dispatch` run by hand.
    echo: bool,
}

impl Logger {
    pub fn new(path: impl Into<PathBuf>, echo: bool) -> Logger {
        Logger {
            path: path.into(),
            file: Mutex::new(None),
            echo,
        }
    }

    /// A logger that only writes to stderr, for one-shot foreground commands.
    pub fn stderr_only() -> Logger {
        Logger {
            path: PathBuf::new(),
            file: Mutex::new(None),
            echo: true,
        }
    }

    pub fn write(&self, level: &str, msg: &str) {
        let line = format!(
            "{} {:<5} {}\n",
            chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ"),
            level,
            msg
        );
        if self.echo {
            eprint!("{line}");
        }
        if self.path.as_os_str().is_empty() {
            return;
        }
        let mut guard = match self.file.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        if guard.is_none() {
            self.rotate_if_needed(&self.path);
            *guard = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)
                .ok();
        }
        if let Some(f) = guard.as_mut() {
            let _ = f.write_all(line.as_bytes());
            let _ = f.flush();
            // Cheap check: only stat when we might have grown past the cap.
            if let Ok(meta) = f.metadata()
                && meta.len() > MAX_BYTES
            {
                *guard = None;
                self.rotate_if_needed(&self.path);
            }
        }
    }

    fn rotate_if_needed(&self, path: &Path) {
        let Ok(meta) = std::fs::metadata(path) else {
            return;
        };
        if meta.len() <= MAX_BYTES {
            return;
        }
        let _ = std::fs::rename(path, path.with_extension("log.1"));
    }

    pub fn info(&self, msg: impl AsRef<str>) {
        self.write("INFO", msg.as_ref());
    }
    pub fn warn(&self, msg: impl AsRef<str>) {
        self.write("WARN", msg.as_ref());
    }
    pub fn error(&self, msg: impl AsRef<str>) {
        self.write("ERROR", msg.as_ref());
    }
}
