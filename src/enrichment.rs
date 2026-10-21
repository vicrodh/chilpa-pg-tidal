//! Private album metadata and aggregate library diagnostics.

use crate::client::now;
use crate::items::{card, copyright_owner, genre_names, ident, unbox};
use crate::json::{at, sha256_hex, truthy};
use crate::state::{read, save};
use serde_json::{json, Map, Value};
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Instant;

pub const FOUND_TTL: f64 = 90.0 * 24.0 * 60.0 * 60.0;
pub const EMPTY_TTL: f64 = 7.0 * 24.0 * 60.0 * 60.0;
pub const DIAGNOSTIC_KEY: &str = "library-enrichment:v1";

type Done = Arc<(Mutex<bool>, Condvar)>;

/// The album header module of an album page, checked for the fields read from it.
pub fn album_header(raw: &Value, album: &str) -> std::result::Result<Value, &'static str> {
    let rows = raw.get("rows").and_then(Value::as_array).ok_or("Invalid album page.")?;
    for row in rows {
        let modules = row.get("modules").and_then(Value::as_array).ok_or("Invalid album modules.")?;
        for module in modules {
            if !module.is_object() {
                return Err("Invalid album module.");
            }
            if module.get("type").and_then(Value::as_str) != Some("ALBUM_HEADER") {
                continue;
            }
            let value = at(module, "album");
            let genre_bad = !matches!(at(value, "genre"), Value::Null | Value::String(_));
            let genres_bad = value.get("genres").is_some_and(|genres| !genres.as_array().is_some_and(|names| names.iter().all(Value::is_string)));
            if !value.is_object() || ident(value) != album || genre_bad || genres_bad {
                return Err("Invalid album header.");
            }
            return Ok(value.clone());
        }
    }
    Err("Missing album header.")
}

pub fn finite(value: &Value) -> Option<f64> {
    value.as_f64().filter(|n| n.is_finite())
}

struct Inner {
    accounts: Map<String, Value>,
    revision: u64,
    writer: Option<Done>,
}

pub struct Enrichment {
    path: PathBuf,
    inner: Mutex<Inner>,
}

impl Enrichment {
    pub fn new(data: &std::path::Path) -> Enrichment {
        let path = data.join("library-enrichment.json");
        let accounts = match read(&path, json!({})) {
            Ok(state) if at(&state, "version") == &json!(1) => at(&state, "accounts").as_object().cloned().unwrap_or_default(),
            _ => Map::new(),
        };
        Enrichment { path, inner: Mutex::new(Inner { accounts, revision: 0, writer: None }) }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn account<'a>(inner: &'a mut Inner, user: &str) -> &'a mut Map<String, Value> {
        let key = sha256_hex(user);
        let slot = inner.accounts.entry(key).or_insert_with(|| json!({}));
        if !slot.is_object() {
            *slot = json!({});
        }
        slot.as_object_mut().unwrap()
    }

    fn album_entry(&self, user: &str, album: &str) -> Option<Value> {
        let mut inner = self.lock();
        let account = Self::account(&mut inner, user);
        account.get("albums").and_then(|albums| albums.get(album)).filter(|entry| entry.is_object()).cloned()
    }

    /// Saves in the background and waits for the write until `deadline`.
    pub fn persist(self: &Arc<Self>, deadline: Option<Instant>) {
        let done = {
            let mut inner = self.lock();
            if inner.writer.is_none() {
                let done: Done = Arc::new((Mutex::new(false), Condvar::new()));
                inner.writer = Some(done.clone());
                let this = self.clone();
                if std::thread::Builder::new().name("enrichment-writer".into()).spawn(move || this.flush()).is_err() {
                    inner.writer = None;
                    return;
                }
            }
            inner.writer.clone().unwrap()
        };
        let (finished, signal) = &*done;
        let mut finished = finished.lock().unwrap();
        while !*finished {
            match deadline {
                None => finished = signal.wait(finished).unwrap(),
                Some(until) => {
                    let remaining = until.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        return;
                    }
                    finished = signal.wait_timeout(finished, remaining).unwrap().0;
                }
            }
        }
    }

    fn flush(&self) {
        loop {
            let (revision, state) = {
                let inner = self.lock();
                (inner.revision, json!({"version": 1, "accounts": inner.accounts.clone()}))
            };
            let _ = save(&self.path, &state);
            let mut inner = self.lock();
            if revision == inner.revision {
                if let Some(done) = inner.writer.take() {
                    drop(inner);
                    *done.0.lock().unwrap() = true;
                    done.1.notify_all();
                }
                return;
            }
        }
    }

    pub fn remember(self: &Arc<Self>, user: &str, rows: &[Value], kind: &str, genre_answer: bool, persist: bool, deadline: Option<Instant>) {
        if user.is_empty() {
            return;
        }
        let changed = {
            let mut inner = self.lock();
            if deadline.is_some_and(|until| Instant::now() >= until) {
                return;
            }
            let mut changed = false;
            let account = Self::account(&mut inner, user);
            if !account.get("albums").is_some_and(Value::is_object) {
                account.insert("albums".into(), json!({}));
            }
            let albums = account.get_mut("albums").unwrap().as_object_mut().unwrap();
            for row in rows {
                if !row.is_object() {
                    continue;
                }
                let value = unbox(row);
                if !value.is_object() {
                    continue;
                }
                let album = if kind == "albums" { value } else { at(value, "album") };
                if !album.is_object() || ident(album).is_empty() {
                    continue;
                }
                let key = ident(album);
                let mut entry = albums.get(&key).filter(|entry| entry.is_object()).cloned().unwrap_or_else(|| json!({}));
                // Sparse embedded albums do not erase a full album's copyright.
                if let Some(copyright) = at(album, "copyright").as_str() {
                    let ttl = if copyright_owner(&json!(copyright)).is_empty() { EMPTY_TTL } else { FOUND_TTL };
                    entry["copyright"] = json!(copyright);
                    entry["expires"] = json!(now() + ttl);
                    changed = true;
                }
                let names = genre_names(album);
                if genre_answer || !names.is_empty() {
                    entry["genres_expires"] = json!(now() + if names.is_empty() { EMPTY_TTL } else { FOUND_TTL });
                    entry["genres"] = json!(names);
                    changed = true;
                }
                for field in ["releaseDate", "type"] {
                    if let Some(found) = at(album, field).as_str().filter(|found| !found.trim().is_empty()) {
                        entry[field] = json!(found);
                        entry[format!("{field}_expires")] = json!(now() + FOUND_TTL);
                        changed = true;
                    }
                }
                if truthy(&entry) {
                    albums.insert(key, entry);
                }
            }
            if changed {
                inner.revision += 1;
            }
            changed
        };
        if changed && persist {
            self.persist(None);
        }
    }

    /// The copyright owner known for an album, or nothing when unknown or expired.
    pub fn label(&self, user: &str, album: &str) -> Option<String> {
        let entry = self.album_entry(user, album)?;
        let copyright = at(&entry, "copyright").as_str()?;
        let expires = finite(at(&entry, "expires"))?;
        (expires > now()).then(|| copyright_owner(&json!(copyright)))
    }

    pub fn genres(&self, user: &str, album: &str) -> Option<Vec<String>> {
        let entry = self.album_entry(user, album)?;
        let names = at(&entry, "genres").as_array()?;
        if !names.iter().all(Value::is_string) {
            return None;
        }
        let expires = finite(at(&entry, "genres_expires"))?;
        (expires > now()).then(|| names.iter().filter_map(Value::as_str).map(str::to_string).collect())
    }

    /// Release date, year and type known for an album, as card fields.
    pub fn release(&self, user: &str, album: &str) -> Map<String, Value> {
        let Some(entry) = self.album_entry(user, album) else { return Map::new() };
        let mut value = json!({});
        for field in ["releaseDate", "type"] {
            let expires = finite(at(&entry, &format!("{field}_expires")));
            if at(&entry, field).is_string() && expires.is_some_and(|expires| expires > now()) {
                value[field] = at(&entry, field).clone();
            }
        }
        let card = card(&value, Some("album"), "");
        card.as_object().unwrap().iter().filter(|(key, _)| ["release_date", "year", "release_type"].contains(&key.as_str()))
            .map(|(key, field)| (key.clone(), field.clone())).collect()
    }

    pub fn record(self: &Arc<Self>, user: &str, metrics: Value, deadline: Option<Instant>) {
        if user.is_empty() {
            return;
        }
        {
            let mut inner = self.lock();
            let account = Self::account(&mut inner, user);
            let previous = at(at(account.get(DIAGNOSTIC_KEY).unwrap_or(&Value::Null), "album_label"), "last_attempt").as_f64();
            let current = at(at(&metrics, "album_label"), "last_attempt").as_f64().unwrap_or(0.0);
            if previous.is_some_and(|previous| previous > current) {
                return;
            }
            account.insert(DIAGNOSTIC_KEY.into(), metrics);
            inner.revision += 1;
        }
        self.persist(deadline);
    }
}

pub fn metric(now: f64, outcome: &str) -> Value {
    json!({"last_attempt": now, "outcome": outcome, "asked": 0, "with_data": 0, "cached": 0})
}
