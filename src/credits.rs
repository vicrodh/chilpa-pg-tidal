//! Optional album credits with private account-scoped persistence.

use crate::client::{now, Client};
use crate::enrichment::{album_header, finite, metric, Enrichment, EMPTY_TTL, FOUND_TTL};
use crate::json::{at, list_at, text};
use crate::state::{read, save, Failure};
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

pub const MAX_ALBUMS: usize = 8;
pub const MAX_REQUESTS: usize = 8;
pub const BUDGET: Duration = Duration::from_secs(6);
pub const PAGE_SIZE: i64 = 100;

pub fn service_id(value: &Value) -> String {
    let text = match value {
        Value::String(text) => text.clone(),
        Value::Number(number) if number.is_i64() || number.is_u64() => number.to_string(),
        _ => return String::new(),
    };
    if !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit()) { text } else { String::new() }
}

/// A credits page as composer names per track, the next offset and the total.
pub fn parse_page(raw: &Value, offset: i64) -> std::result::Result<(Vec<(String, Vec<String>)>, i64, i64), &'static str> {
    let rows = raw.get("items").and_then(Value::as_array).ok_or("Invalid credits page.")?;
    let total = raw.get("totalNumberOfItems").and_then(Value::as_i64).filter(|_| raw["totalNumberOfItems"].is_i64() || raw["totalNumberOfItems"].is_u64());
    let Some(total) = total else { return Err("Invalid credits pagination.") };
    if total < offset + rows.len() as i64 || rows.len() as i64 > PAGE_SIZE || (offset < total && rows.is_empty()) {
        return Err("Invalid credits pagination.");
    }
    let mut tracks: Vec<(String, Vec<String>)> = Vec::new();
    for row in rows {
        let kind = row.get("type").and_then(Value::as_str).ok_or("Invalid credits item.")?;
        if kind.to_lowercase() != "track" {
            continue;
        }
        let item = at(row, "item");
        let credits = row.get("credits").and_then(Value::as_array);
        let key = service_id(at(item, "id"));
        let (true, Some(credits), false) = (item.is_object(), credits, key.is_empty()) else { return Err("Invalid track credits.") };
        let mut names: Vec<String> = Vec::new();
        for credit in credits {
            let role = credit.get("type").and_then(Value::as_str);
            let contributors = credit.get("contributors").and_then(Value::as_array);
            let (Some(role), Some(contributors)) = (role, contributors) else { return Err("Invalid credit role.") };
            let composer = role.to_lowercase().contains("composer");
            for contributor in contributors {
                let name = contributor.get("name").and_then(Value::as_str).ok_or("Invalid contributor.")?.trim();
                if composer && !name.is_empty() && !names.iter().any(|known| known == name) {
                    names.push(name.to_string());
                }
            }
        }
        if tracks.iter().any(|(known, _)| *known == key) {
            return Err("Duplicate credit track.");
        }
        tracks.push((key, names));
    }
    Ok((tracks, offset + rows.len() as i64, total))
}

pub struct Credits {
    client: Arc<Client>,
    path: PathBuf,
    pub metadata: Arc<Enrichment>,
    flight: AtomicBool,
    cursor: Mutex<HashMap<String, String>>,
    genre_turn: Mutex<HashSet<String>>,
    accounts: Mutex<Map<String, Value>>,
}

fn is_int(value: &Value) -> bool {
    value.is_i64() || value.is_u64()
}

/// A saved album entry, only when every field has the shape it was saved with.
fn valid(entry: &Value) -> Option<Value> {
    let tracks = entry.get("tracks")?.as_object()?;
    let expires = entry.get("expires")?.as_object()?;
    if tracks.len() != expires.len() || tracks.keys().any(|key| !expires.contains_key(key)) {
        return None;
    }
    for (key, names) in tracks {
        if !names.as_array().is_some_and(|names| names.iter().all(Value::is_string)) || finite(&expires[key]).is_none() {
            return None;
        }
    }
    let offset = entry.get("offset").filter(|v| is_int(v))?.as_i64()?;
    let total = entry.get("total").filter(|v| is_int(v))?.as_i64()?;
    finite(entry.get("checked_at")?)?;
    if !(0 <= offset && offset <= total) {
        return None;
    }
    let seen = entry.get("seen")?.as_array()?;
    if seen.iter().any(|key| !key.as_str().is_some_and(|key| tracks.contains_key(key))) {
        return None;
    }
    Some(entry.clone())
}

/// The composers known for a track: a list (maybe empty) when answered, or
/// nothing when it still has to be asked.
fn answer(entry: &Option<Value>, track: &str, now: f64) -> Option<Vec<String>> {
    let entry = entry.as_ref()?;
    if finite(at(at(entry, "expires"), track)).unwrap_or(0.0) > now {
        if let Some(names) = at(at(entry, "tracks"), track).as_array() {
            return Some(names.iter().filter_map(Value::as_str).map(str::to_string).collect());
        }
        return None;
    }
    let complete = at(entry, "offset") == at(entry, "total");
    if at(at(entry, "tracks"), track).is_null() && complete && finite(at(entry, "checked_at")).unwrap_or(0.0) + EMPTY_TTL > now {
        return Some(Vec::new());
    }
    None
}

fn album_of(item: &Value, kind: &str) -> String {
    let reference = if kind == "tracks" { text(at(item, "album_ref")) } else { text(at(item, "ref")) };
    service_id(&json!(reference.strip_prefix("album:").unwrap_or(&reference)))
}

fn outcome(error: &Failure) -> String {
    if let Some(status) = error.status {
        return format!("http status {status}");
    }
    match error.outcome {
        Some(found @ ("timeout" | "no token" | "disabled")) => found.to_string(),
        _ => "unexpected shape".to_string(),
    }
}

type Metrics = Arc<Mutex<Value>>;

fn bump(metrics: &Metrics, name: &str, field: &str) {
    let mut metrics = metrics.lock().unwrap();
    let value = metrics[name][field].as_i64().unwrap_or(0) + 1;
    metrics[name][field] = json!(value);
}

fn set_outcome(metrics: &Metrics, name: &str, value: &str) {
    metrics.lock().unwrap()[name]["outcome"] = json!(value);
}

fn read_metric(metrics: &Metrics, name: &str, field: &str) -> Value {
    metrics.lock().unwrap()[name][field].clone()
}

impl Credits {
    pub fn new(client: Arc<Client>, data: &std::path::Path) -> Credits {
        let path = data.join("credits.json");
        let accounts = match read(&path, json!({})) {
            Ok(state) if at(&state, "version") == &json!(1) => at(&state, "accounts").as_object().cloned().unwrap_or_default(),
            _ => Map::new(),
        };
        Credits { client, path, metadata: Arc::new(Enrichment::new(data)), flight: AtomicBool::new(false),
            cursor: Mutex::new(HashMap::new()), genre_turn: Mutex::new(HashSet::new()), accounts: Mutex::new(accounts) }
    }

    fn entry(&self, user: &str, album: &str) -> Option<Value> {
        let accounts = self.accounts.lock().unwrap();
        accounts.get(user).and_then(|account| account.get(album)).filter(|entry| entry.is_object()).and_then(valid)
    }

    fn store(&self, user: &str, album: &str, entry: Value) {
        let mut accounts = self.accounts.lock().unwrap();
        let account = accounts.entry(user.to_string()).or_insert_with(|| json!({}));
        if !account.is_object() {
            *account = json!({});
        }
        account[album] = entry;
        let _ = save(&self.path, &json!({"version": 1, "accounts": accounts.clone()}));
    }

    pub fn enrich(self: &Arc<Self>, items: &mut [Value], kind: &str, user: &str) {
        if kind != "tracks" && kind != "albums" {
            return;
        }
        let start = now();
        let deadline = Instant::now() + BUDGET;
        let mut label_metric = metric(start, "ok");
        label_metric["asked"] = json!(items.len());
        let metrics: Metrics = Arc::new(Mutex::new(json!({"album_label": label_metric, "genres": metric(start, "ok"),
            "track_credits": metric(start, if kind == "tracks" { "ok" } else { "disabled" })})));
        let (mut missing, mut genres): (Vec<String>, Vec<String>) = (Vec::new(), Vec::new());
        for item in items.iter() {
            let album = album_of(item, kind);
            if !crate::json::truthy(at(item, "label")) && self.metadata.label(user, &album).is_some() {
                bump(&metrics, "album_label", "cached");
            }
            if !album.is_empty() && !crate::json::truthy(at(item, "genres")) {
                if self.metadata.genres(user, &album).is_none() {
                    if !genres.contains(&album) {
                        genres.push(album.clone());
                    }
                } else {
                    bump(&metrics, "genres", "cached");
                }
            }
            if kind == "tracks" && !crate::json::truthy(at(item, "composers")) {
                let reference = text(at(item, "ref"));
                let track = service_id(&json!(reference.strip_prefix("track:").unwrap_or(&reference)));
                if !album.is_empty() && !track.is_empty() {
                    if answer(&self.entry(user, &album), &track, start).is_none() {
                        if !missing.contains(&album) {
                            missing.push(album.clone());
                        }
                    } else {
                        bump(&metrics, "track_credits", "cached");
                    }
                }
            }
        }
        let pending: Vec<&str> = [("track_credits", !missing.is_empty()), ("genres", !genres.is_empty())]
            .into_iter().filter(|(_, needed)| *needed).map(|(name, _)| name).collect();
        if !pending.is_empty() {
            if user.is_empty() {
                pending.iter().for_each(|name| set_outcome(&metrics, name, "no token"));
            } else if self.flight.swap(true, Ordering::AcqRel) {
                pending.iter().for_each(|name| set_outcome(&metrics, name, "disabled"));
            } else {
                let (sender, receiver) = mpsc::channel();
                let (this, worker_metrics, owner) = (self.clone(), metrics.clone(), user.to_string());
                let (missing, genres) = (missing.clone(), genres.clone());
                let spawned = std::thread::Builder::new().name("credits".into()).spawn(move || {
                    this.fill(&owner, &missing, &genres, deadline, &worker_metrics);
                    let _ = sender.send(());
                });
                if spawned.is_err() {
                    self.flight.store(false, Ordering::Release);
                    pending.iter().for_each(|name| set_outcome(&metrics, name, "disabled"));
                } else if receiver.recv_timeout(deadline.saturating_duration_since(Instant::now())).is_err() {
                    // Socket timeouts do not bound DNS or a response that trickles bytes.
                    pending.iter().for_each(|name| set_outcome(&metrics, name, "timeout"));
                }
            }
        }
        let current = now();
        for item in items.iter_mut() {
            let album = album_of(item, kind);
            if !crate::json::truthy(at(item, "label")) {
                if let Some(label) = self.metadata.label(user, &album).filter(|label| !label.is_empty()) {
                    item["label"] = json!(label);
                    item["label_source"] = json!("copyright");
                    if let Value::Object(fields) = item {
                        fields.shift_remove("label_ref");
                    }
                }
            }
            if !crate::json::truthy(at(item, "genres")) {
                if let Some(names) = self.metadata.genres(user, &album).filter(|names| !names.is_empty()) {
                    item["genre"] = json!(names.join(", "));
                    item["genres"] = json!(names);
                }
            }
            let entry = self.entry(user, &album);
            let names = if kind == "tracks" {
                let mut release = self.metadata.release(user, &album);
                if crate::json::truthy(at(item, "release_date")) {
                    release.shift_remove("release_date");
                    release.shift_remove("year");
                }
                if let Value::Object(fields) = item {
                    for (field, value) in release {
                        fields.entry(field).or_insert(value);
                    }
                }
                let reference = text(at(item, "ref"));
                answer(&entry, reference.strip_prefix("track:").unwrap_or(&reference), current).unwrap_or_default()
            } else {
                let mut names: Vec<String> = Vec::new();
                if let Some(entry) = &entry {
                    for (key, values) in at(entry, "tracks").as_object().into_iter().flatten() {
                        if finite(at(at(entry, "expires"), key)).unwrap_or(0.0) > current {
                            for name in values.as_array().into_iter().flatten().filter_map(Value::as_str) {
                                if !names.iter().any(|known| known == name) {
                                    names.push(name.to_string());
                                }
                            }
                        }
                    }
                }
                if !names.is_empty() {
                    bump(&metrics, "track_credits", "cached");
                }
                names
            };
            if !names.is_empty() {
                item["composers"] = json!(names);
            }
        }
        for (name, field) in [("album_label", "label"), ("genres", "genres"), ("track_credits", "composers")] {
            let count = items.iter().filter(|item| crate::json::truthy(at(item, field))).count();
            metrics.lock().unwrap()[name]["with_data"] = json!(count);
        }
        // A timed-out worker must not mutate the saved listing diagnostic later.
        let snapshot = metrics.lock().unwrap().clone();
        self.metadata.record(user, snapshot, Some(deadline));
    }

    fn fill(&self, user: &str, missing: &[String], genres: &[String], deadline: Instant, metrics: &Metrics) {
        self.fill_inner(user, missing, genres, deadline, metrics);
        let genres_asked = read_metric(metrics, "genres", "asked").as_i64().unwrap_or(0);
        let credits_outcome = read_metric(metrics, "track_credits", "outcome");
        {
            let mut turn = self.genre_turn.lock().unwrap();
            if !genres.is_empty() && genres_asked == 0 && credits_outcome != json!("ok") {
                turn.insert(user.to_string());
            } else if genres_asked > 0 {
                turn.remove(user);
            }
        }
        for (name, needed) in [("track_credits", !missing.is_empty()), ("genres", !genres.is_empty())] {
            let mut metrics = metrics.lock().unwrap();
            if needed && metrics[name]["asked"].as_i64().unwrap_or(0) == 0 && metrics[name]["outcome"] == json!("ok") {
                metrics[name]["outcome"] = json!("disabled");
            }
        }
        self.flight.store(false, Ordering::Release);
    }

    fn fill_inner(&self, user: &str, missing: &[String], genres: &[String], deadline: Instant, metrics: &Metrics) {
        let mut albums: Vec<String> = Vec::new();
        for album in missing.iter().chain(genres) {
            if !albums.contains(album) {
                albums.push(album.clone());
            }
        }
        if let Some(last) = self.cursor.lock().unwrap().get(user).cloned() {
            if let Some(position) = albums.iter().position(|album| *album == last) {
                albums.rotate_left(position + 1);
            }
        }
        albums.truncate(MAX_ALBUMS);
        let mut requests = 0;
        let credit_limit = MAX_REQUESTS - usize::from(!genres.is_empty() && self.genre_turn.lock().unwrap().contains(user));
        let expired = || Instant::now() >= deadline;
        'albums: for album in &albums {
            if !missing.contains(album) {
                continue;
            }
            if requests >= credit_limit || expired() {
                break;
            }
            self.cursor.lock().unwrap().insert(user.to_string(), album.clone());
            let mut entry = self.entry(user, album);
            let mut offset = match &entry {
                Some(entry) if at(entry, "offset").as_i64() < at(entry, "total").as_i64()
                    && finite(at(entry, "checked_at")).unwrap_or(0.0) + EMPTY_TTL > now() => at(entry, "offset").as_i64().unwrap_or(0),
                _ => 0,
            };
            while requests < credit_limit && !expired() {
                requests += 1;
                bump(metrics, "track_credits", "asked");
                let fetched = self.client.get_until(&format!("albums/{album}/items/credits"),
                    &json!({"replace": "true", "includeContributors": "true", "limit": PAGE_SIZE, "offset": offset}), deadline, user);
                let raw = match fetched {
                    Ok(raw) => raw,
                    Err(error) => {
                        set_outcome(metrics, "track_credits", &outcome(&error));
                        if error.status == Some(429) {
                            set_outcome(metrics, "genres", "disabled");
                            return;
                        }
                        break;
                    }
                };
                if expired() {
                    set_outcome(metrics, "track_credits", "timeout");
                    return;
                }
                let (tracks, next_offset, total) = match parse_page(&raw, offset) {
                    Ok(parsed) => parsed,
                    Err(_) => {
                        set_outcome(metrics, "track_credits", "unexpected shape");
                        break;
                    }
                };
                if offset > 0 {
                    let previous = entry.clone().unwrap_or_else(|| json!({}));
                    let seen = list_at(&previous, "seen");
                    if at(&previous, "total").as_i64() != Some(total) || tracks.iter().any(|(key, _)| seen.iter().any(|known| known == key)) {
                        let mut reset = previous.clone();
                        reset["offset"] = json!(0);
                        reset["total"] = json!(0);
                        reset["checked_at"] = json!(0);
                        self.store(user, album, reset);
                        set_outcome(metrics, "track_credits", "unexpected shape");
                        break;
                    }
                }
                if expired() {
                    set_outcome(metrics, "track_credits", "timeout");
                    return;
                }
                self.metadata.remember(user, list_at(&raw, "items"), "tracks", false, false, Some(deadline));
                if expired() {
                    set_outcome(metrics, "track_credits", "timeout");
                    return;
                }
                let current = now();
                let mut stored_tracks = Map::new();
                let mut stored_expires = Map::new();
                if let Some(previous) = &entry {
                    for (key, names) in at(previous, "tracks").as_object().into_iter().flatten() {
                        let until = finite(at(at(previous, "expires"), key)).unwrap_or(0.0);
                        if until > current {
                            stored_tracks.insert(key.clone(), names.clone());
                            stored_expires.insert(key.clone(), json!(until));
                        }
                    }
                }
                for (key, names) in &tracks {
                    stored_tracks.insert(key.clone(), json!(names));
                    stored_expires.insert(key.clone(), json!(current + if names.is_empty() { EMPTY_TTL } else { FOUND_TTL }));
                }
                let mut seen: Vec<Value> = if offset > 0 { entry.as_ref().map(|e| list_at(e, "seen").to_vec()).unwrap_or_default() } else { Vec::new() };
                seen.extend(tracks.iter().map(|(key, _)| json!(key)));
                let updated = json!({"tracks": stored_tracks, "expires": stored_expires, "offset": next_offset, "total": total,
                    "checked_at": current, "seen": seen});
                self.store(user, album, updated.clone());
                entry = Some(updated);
                offset = next_offset;
                if offset >= total {
                    continue 'albums;
                }
            }
        }
        for album in &albums {
            if !genres.contains(album) || self.metadata.genres(user, album).is_some() {
                continue;
            }
            if requests >= MAX_REQUESTS || expired() {
                if read_metric(metrics, "genres", "asked").as_i64().unwrap_or(0) == 0 {
                    set_outcome(metrics, "genres", "disabled");
                }
                break;
            }
            requests += 1;
            bump(metrics, "genres", "asked");
            self.cursor.lock().unwrap().insert(user.to_string(), album.clone());
            let raw = match self.client.get_until("pages/album", &json!({"albumId": album, "deviceType": "BROWSER", "locale": "en_US"}), deadline, user) {
                Ok(raw) => raw,
                Err(error) => {
                    set_outcome(metrics, "genres", &outcome(&error));
                    if error.status == Some(429) {
                        return;
                    }
                    continue;
                }
            };
            if expired() {
                set_outcome(metrics, "genres", "timeout");
                return;
            }
            let value = match album_header(&raw, album) {
                Ok(value) => value,
                Err(_) => {
                    set_outcome(metrics, "genres", "unexpected shape");
                    continue;
                }
            };
            self.metadata.remember(user, &[value], "albums", true, false, Some(deadline));
        }
        if expired() {
            set_outcome(metrics, "genres", "timeout");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_credits_page_keeps_composers_once_per_track() {
        let raw = json!({"totalNumberOfItems": 2, "items": [
            {"type": "track", "item": {"id": 1}, "credits": [{"type": "Composer", "contributors": [{"name": "A"}, {"name": "A"}]},
                                                            {"type": "Producer", "contributors": [{"name": "B"}]}]},
            {"type": "video", "item": {"id": 2}, "credits": []}]});
        let (tracks, next, total) = parse_page(&raw, 0).unwrap();
        assert_eq!(tracks, vec![("1".to_string(), vec!["A".to_string()])]);
        assert_eq!((next, total), (2, 2));
    }

    #[test]
    fn a_page_past_its_total_is_rejected() {
        assert!(parse_page(&json!({"totalNumberOfItems": 1, "items": [{"type": "video"}, {"type": "video"}]}), 0).is_err());
        assert!(parse_page(&json!({"totalNumberOfItems": 3, "items": []}), 0).is_err());
    }

    #[test]
    fn an_answered_album_without_the_track_answers_empty_until_it_expires() {
        let entry = Some(json!({"tracks": {}, "expires": {}, "offset": 2, "total": 2, "checked_at": now(), "seen": []}));
        assert_eq!(answer(&entry, "9", now()), Some(Vec::new()));
        let stale = Some(json!({"tracks": {}, "expires": {}, "offset": 2, "total": 2, "checked_at": 0.0, "seen": []}));
        assert_eq!(answer(&stale, "9", now()), None);
    }
}
