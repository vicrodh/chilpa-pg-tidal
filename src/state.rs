//! Private atomic JSON state, the failure every operation reports, and a
//! bounded single-flight cache for catalog reads.

use serde_json::{Map, Value};
use std::collections::{HashMap, VecDeque};
use std::fs;
use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// What an operation failed with: the message the host shows, the JSON-RPC
/// code, the HTTP status when the provider answered, extra error data, and the
/// diagnostic outcome optional metadata records.
#[derive(Debug, Clone)]
pub struct Failure {
    pub message: String,
    pub code: i64,
    pub status: Option<u16>,
    pub data: Option<Value>,
    pub outcome: Option<&'static str>,
}

impl Failure {
    pub fn new(message: impl Into<String>) -> Self {
        Failure { message: message.into(), code: -32000, status: None, data: None, outcome: None }
    }

    pub fn invalid(message: impl Into<String>) -> Self {
        Failure { code: -32602, ..Failure::new(message) }
    }

    pub fn code(mut self, code: i64) -> Self {
        self.code = code;
        self
    }

    pub fn outcome(mut self, outcome: &'static str) -> Self {
        self.outcome = Some(outcome);
        self
    }
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

pub type Result<T> = std::result::Result<T, Failure>;

/// Writes the value through a synced temporary file in the same directory.
pub fn save(path: &Path, value: &Value) -> std::io::Result<()> {
    let parent = path.parent().unwrap_or(Path::new("."));
    create_private_dir(parent)?;
    let text = serde_json::to_vec(value).map_err(std::io::Error::other)?;
    let name = parent.join(format!(".state-{}", uuid::Uuid::new_v4().simple()));
    let result = (|| {
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        // Account tokens live in these files: only the owner reads them.
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let mut file = options.open(&name)?;
        file.write_all(&text)?;
        file.sync_all()?;
        fs::rename(&name, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&name);
    }
    result
}

pub fn create_private_dir(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new().recursive(true).mode(0o700).create(path)
    }
    #[cfg(not(unix))]
    {
        fs::create_dir_all(path)
    }
}

/// The saved value, the default when the file does not exist, and an error
/// when it exists but cannot be read as JSON.
pub fn read(path: &Path, default: Value) -> std::io::Result<Value> {
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(std::io::Error::other),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(default),
        Err(error) => Err(error),
    }
}

/// Like `read`, with the default for any problem.
pub fn read_or(path: &Path, default: Value) -> Value {
    read(path, default.clone()).unwrap_or(default)
}

pub fn object() -> Value {
    Value::Object(Map::new())
}

type Flight = Arc<(Mutex<bool>, Condvar)>;

struct CacheState {
    entries: HashMap<String, (Instant, Value)>,
    order: VecDeque<String>,
    flights: HashMap<String, Flight>,
    epoch: u64,
}

/// Catalog answers for five minutes, at most 256 of them, with one request in
/// flight per key; clearing drops answers fetched before the clear.
pub struct Cache {
    state: Mutex<CacheState>,
}

impl Default for Cache {
    fn default() -> Self {
        Cache { state: Mutex::new(CacheState { entries: HashMap::new(), order: VecDeque::new(), flights: HashMap::new(), epoch: 0 }) }
    }
}

impl Cache {
    pub fn clear(&self) {
        let mut state = self.state.lock().unwrap();
        state.entries.clear();
        state.order.clear();
        state.epoch += 1;
    }

    fn fresh(state: &CacheState, key: &str) -> Option<Value> {
        state.entries.get(key).filter(|(until, _)| *until > Instant::now()).map(|(_, value)| value.clone())
    }

    pub fn get(&self, key: &str, fetch: impl FnOnce() -> Result<Value>) -> Result<Value> {
        self.get_ttl(key, Duration::from_secs(300), fetch)
    }

    pub fn get_ttl(&self, key: &str, ttl: Duration, fetch: impl FnOnce() -> Result<Value>) -> Result<Value> {
        // One caller fetches a key; the others wait for it and read its answer.
        let (flight, owner) = loop {
            let mut state = self.state.lock().unwrap();
            if let Some(value) = Self::fresh(&state, key) {
                return Ok(value);
            }
            match state.flights.get(key).cloned() {
                Some(flight) => {
                    drop(state);
                    let (busy, done) = &*flight;
                    let mut running = busy.lock().unwrap();
                    while *running {
                        running = done.wait(running).unwrap();
                    }
                    continue;
                }
                None => {
                    let flight: Flight = Arc::new((Mutex::new(true), Condvar::new()));
                    state.flights.insert(key.to_string(), flight.clone());
                    break (flight, state.epoch);
                }
            }
        };
        let result = fetch();
        let mut state = self.state.lock().unwrap();
        if let Ok(value) = &result {
            if owner == state.epoch {
                if state.entries.insert(key.to_string(), (Instant::now() + ttl, value.clone())).is_some() {
                    state.order.retain(|existing| existing != key);
                }
                state.order.push_back(key.to_string());
                while state.order.len() > 256 {
                    if let Some(oldest) = state.order.pop_front() {
                        state.entries.remove(&oldest);
                    }
                }
            }
        }
        if state.flights.get(key).is_some_and(|current| Arc::ptr_eq(current, &flight)) {
            state.flights.remove(key);
        }
        drop(state);
        let (busy, done) = &*flight;
        *busy.lock().unwrap() = false;
        done.notify_all();
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_saved_value_reads_back_and_leaves_no_temporary_file() {
        let folder = tempfile::tempdir().unwrap();
        let path = folder.path().join("nested/state.json");
        save(&path, &json!({"a": [1, "b"]})).unwrap();
        assert_eq!(read(&path, json!({})).unwrap(), json!({"a": [1, "b"]}));
        let names: Vec<_> = fs::read_dir(path.parent().unwrap()).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(names.len(), 1);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
            assert_eq!(fs::metadata(path.parent().unwrap()).unwrap().permissions().mode() & 0o777, 0o700);
        }
    }

    #[test]
    fn a_cleared_cache_does_not_keep_an_answer_fetched_before_the_clear() {
        let cache = Cache::default();
        let value = cache.get("k", || {
            cache_clear_from_inside(&cache);
            Ok(json!(1))
        });
        assert_eq!(value.unwrap(), json!(1));
        assert_eq!(cache.get("k", || Ok(json!(2))).unwrap(), json!(2));
        assert_eq!(cache.get("k", || Ok(json!(3))).unwrap(), json!(2));
    }

    fn cache_clear_from_inside(cache: &Cache) {
        cache.clear();
    }
}
