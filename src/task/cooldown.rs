//! Cooldown store for models that hit quota limits.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::os::unix::io::AsRawFd;
use std::path::Path;

/// The cooldown store format.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct CooldownStore {
    #[serde(default)]
    pub agy: BTreeMap<String, i64>, // model -> unix expiration seconds
}

/// Checks if a model is currently in cooldown.
pub fn is_cooling_down(state_dir: &Path, model: &str) -> bool {
    let store = read_store(state_dir);
    if let Some(&until_secs) = store.agy.get(model) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        if until_secs > now {
            return true;
        }
    }
    false
}

/// Sets a cooldown for a model.
pub fn set_cooldown(state_dir: &Path, model: &str, duration_secs: u64) {
    if duration_secs == 0 {
        return;
    }
    let _ = std::fs::create_dir_all(state_dir);
    let lock_path = state_dir.join("cooldown.lock");
    let lock_file = match OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
    {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!(
                "cannot open cooldown lock file {}: {}",
                lock_path.display(),
                e
            );
            return;
        }
    };

    let rc = unsafe { libc::flock(lock_file.as_raw_fd(), libc::LOCK_EX) };
    if rc != 0 {
        let e = std::io::Error::last_os_error();
        tracing::warn!(
            "cannot lock cooldown lock file {}: {}",
            lock_path.display(),
            e
        );
        return;
    }

    let mut store = read_store(state_dir);

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let until = now + duration_secs as i64;
    store.agy.insert(model.to_string(), until);

    let json_path = state_dir.join("cooldown.json");
    let tmp_path = state_dir.join(format!("cooldown.tmp-{}", std::process::id()));

    if let Ok(json) = serde_json::to_string_pretty(&store) {
        if std::fs::write(&tmp_path, json).is_ok() {
            let _ = std::fs::rename(&tmp_path, &json_path);
        }
        let _ = std::fs::remove_file(&tmp_path);
    }

    unsafe {
        libc::flock(lock_file.as_raw_fd(), libc::LOCK_UN);
    }
}

/// Reads the current cooldown store, returning a default empty store if reading fails or data is invalid.
pub fn read_store(state_dir: &Path) -> CooldownStore {
    let json_path = state_dir.join("cooldown.json");
    if let Ok(content) = std::fs::read_to_string(&json_path) {
        if let Ok(store) = serde_json::from_str(&content) {
            return store;
        }
    }
    CooldownStore::default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_store_read_write() {
        let dir = std::env::temp_dir().join(format!("weir-cd-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        set_cooldown(&dir, "gpt-4", 3600);
        let store = read_store(&dir);
        assert!(store.agy.contains_key("gpt-4"));
        assert!(is_cooling_down(&dir, "gpt-4"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn test_corrupt_store_is_ignored() {
        let dir = std::env::temp_dir().join(format!("weir-cd-test-corrupt-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("cooldown.json"), "invalid json").unwrap();
        let store = read_store(&dir);
        assert!(store.agy.is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn test_wrong_type_store_is_ignored() {
        let dir = std::env::temp_dir().join(format!("weir-cd-test-type-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // Write non-ASCII and wrong type (string instead of integer)
        std::fs::write(dir.join("cooldown.json"), r#"{"agy":{"gpt-4": "❄️"}}"#).unwrap();
        let store = read_store(&dir);
        assert!(store.agy.is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
