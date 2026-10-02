//! Where each camera's settled algorithm state ([`WarmStart`]) is kept between sessions.
//!
//! A pipeline that stops remembers what AE and AWB settled on under the camera's key, and the
//! next start of that camera (another session, another mode) begins there instead of at the
//! tuning's start-up values. Kept in memory for the life of the process, and on disk as well
//! (`<dir>/<key>.json`) when a state directory is set with [`set_state_dir`] or the
//! [`STATE_DIR_ENV`] environment variable, so a new process starts warm too.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use styx_algo::WarmStart;

/// Environment variable naming the directory warm starts are saved in.
pub const STATE_DIR_ENV: &str = "STYX_STATE_DIR";

struct Store {
    memory: BTreeMap<String, WarmStart>,
    dir: Option<PathBuf>,
}

fn store() -> &'static Mutex<Store> {
    static STORE: OnceLock<Mutex<Store>> = OnceLock::new();
    STORE.get_or_init(|| {
        Mutex::new(Store {
            memory: BTreeMap::new(),
            dir: std::env::var_os(STATE_DIR_ENV).map(PathBuf::from),
        })
    })
}

fn lock() -> std::sync::MutexGuard<'static, Store> {
    store().lock().unwrap_or_else(|e| e.into_inner())
}

/// Saves warm starts in `dir` from now on (and reads them from there); `None` keeps them in
/// memory only.
pub fn set_state_dir(dir: Option<PathBuf>) {
    lock().dir = dir;
}

/// The file a camera's warm start is saved in under `dir`: the key with anything but ASCII
/// letters, digits, `-` and `_` replaced by `_`.
pub fn state_file(dir: &Path, key: &str) -> PathBuf {
    let name: String = key
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    dir.join(format!("{name}.json"))
}

/// Remembers a camera's settled state (in memory, and in the state directory if one is set;
/// failing to write the file only loses the cross-process copy).
pub fn remember(key: &str, warm: WarmStart) {
    if !warm.is_valid() {
        return;
    }
    let mut s = lock();
    s.memory.insert(key.to_owned(), warm);
    if let Some(dir) = s.dir.clone() {
        drop(s);
        let path = state_file(&dir, key);
        // Best effort: without the file the next process starts from the tuning's values.
        let _ = std::fs::create_dir_all(&dir).and_then(|()| {
            let json = serde_json::to_string_pretty(&warm).map_err(std::io::Error::other)?;
            let tmp = path.with_extension("json.tmp");
            std::fs::write(&tmp, json)?;
            std::fs::rename(&tmp, &path)
        });
    }
}

/// A camera's last settled state: from memory, else from the state directory.
pub fn recall(key: &str) -> Option<WarmStart> {
    let s = lock();
    if let Some(w) = s.memory.get(key) {
        return Some(*w);
    }
    let path = state_file(s.dir.as_ref()?, key);
    drop(s);
    let w: WarmStart = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    w.is_valid().then_some(w)
}

/// Forgets a camera's state (memory and file).
pub fn forget(key: &str) {
    let mut s = lock();
    s.memory.remove(key);
    if let Some(dir) = &s.dir {
        let _ = std::fs::remove_file(state_file(dir, key));
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn remembers_in_memory_and_on_disk() {
        let w = WarmStart {
            total_exposure: 0.02,
            exposure: Duration::from_millis(10),
            analogue_gain: 2.0,
            ..Default::default()
        };
        let key = "bridge:/dev/v4l-subdev9 test";
        assert!(recall(key).is_none());
        remember(key, w);
        assert_eq!(recall(key), Some(w));
        let dir = PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../target"))
            .join(format!("styx-warm-test-{}", std::process::id()));
        set_state_dir(Some(dir.clone()));
        remember(key, w);
        lock().memory.clear();
        assert_eq!(recall(key), Some(w));
        assert!(state_file(&dir, key).ends_with("bridge__dev_v4l-subdev9_test.json"));
        forget(key);
        assert!(recall(key).is_none());
        set_state_dir(None);
        let _ = std::fs::remove_dir_all(dir);
        // Invalid values are not kept.
        remember(key, WarmStart::default());
        assert!(recall(key).is_none());
    }
}
