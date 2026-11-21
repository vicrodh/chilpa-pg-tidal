//! One optional account-scoped membership snapshot, shared by catalog reads.

use crate::client::Client;
use crate::json::text;
use crate::state::{Failure, Result};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

// One process retains at most 100,000 IDs for one account. A snapshot is valid
// for 60 seconds, then replaced on its next read. Writes, sign-out and process
// shutdown release it. No worker or polling is added.
const MAX_IDS: usize = 100_000;
const TTL: Duration = Duration::from_secs(60);
const RETRY: Duration = Duration::from_secs(15);
const BUDGET: Duration = Duration::from_secs(2);

struct State {
    user: String,
    revision: u64,
    busy: Option<u64>,
    until: Instant,
    ids: Option<Arc<HashSet<String>>>,
}

pub struct Favorites {
    client: Arc<Client>,
    state: Mutex<State>,
    changed: Condvar,
}

impl Favorites {
    pub fn new(client: Arc<Client>) -> Self {
        Self { client, state: Mutex::new(State { user: String::new(), revision: 0, busy: None,
            until: Instant::now(), ids: None }), changed: Condvar::new() }
    }

    pub fn clear(&self) {
        let mut state = self.state.lock().unwrap();
        state.revision += 1;
        state.busy = None;
        state.ids = None;
        state.until = Instant::now();
        self.changed.notify_all();
    }

    /// Applies a write the service accepted. A snapshot that is held takes the
    /// change and stays valid, so the next read cannot bring back the state
    /// from before the write; an unknown one stays unknown. A request still in
    /// flight started before the write and is discarded.
    pub fn changed(&self, reference: &str, on: bool) {
        let reference = reference.split('@').next().unwrap_or(reference);
        let mut state = self.state.lock().unwrap();
        state.revision += 1;
        state.busy = None;
        if state.user != self.client.user() || state.ids.is_none() {
            state.ids = None;
            state.until = Instant::now();
        } else if let Some(ids) = state.ids.as_mut() {
            let ids = Arc::make_mut(ids);
            if on { ids.insert(reference.to_string()); } else { ids.remove(reference); }
            state.until = Instant::now() + TTL;
        }
        self.changed.notify_all();
    }

    fn snapshot(&self) -> Option<Arc<HashSet<String>>> {
        let user = self.client.user();
        if user.is_empty() { return None; }
        let deadline = Instant::now() + BUDGET;
        let mut state = self.state.lock().unwrap();
        if state.user != user {
            state.user = user.clone();
            state.revision += 1;
            state.busy = None;
            state.ids = None;
            state.until = Instant::now();
        }
        loop {
            if state.user != user { return None; }
            if state.until > Instant::now() { return state.ids.clone(); }
            if state.busy.is_none() { break; }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() { return None; }
            state = self.changed.wait_timeout(state, remaining).unwrap().0;
        }
        let revision = state.revision;
        state.busy = Some(revision);
        // Drop an expired allocation before parsing its replacement.
        state.ids = None;
        drop(state);
        let ids = self.client.get_until(&format!("users/{user}/favorites/ids"),
            &json!({"locale": "en_US", "deviceType": "BROWSER"}), deadline, &user).and_then(|raw| parse(&raw)).ok().map(Arc::new);
        let mut state = self.state.lock().unwrap();
        if state.user != user || state.revision != revision { return None; }
        state.busy = None;
        state.until = Instant::now() + if ids.is_some() { TTL } else { RETRY };
        state.ids = ids.clone();
        self.changed.notify_all();
        ids
    }

    pub fn decorate(&self, value: &mut Value) {
        if !contains_items(value, 0) { return; }
        if let Some(ids) = self.snapshot() {
            let mut remaining = 200000;
            decorate(value, &ids, 0, &mut remaining);
        }
    }
}

/// A collection list is itself the membership: its rows are favorites whether
/// or not the snapshot could be read. A playlist the account made is listed
/// there without being one.
pub fn listed(result: &mut Value, kind: &str) {
    for item in result.get_mut("items").and_then(Value::as_array_mut).into_iter().flatten() {
        if item_reference(item).is_none() { continue; }
        let favorite = if kind == "playlists" { item.get("editable").and_then(Value::as_bool).map(|own| !own) } else { Some(true) };
        if let Some(favorite) = favorite { item["favorite"] = json!(favorite); }
    }
}

fn item_reference(value: &Value) -> Option<&str> {
    let reference = value.get("ref")?.as_str()?.split('@').next()?;
    let (kind, _) = reference.split_once(':')?;
    ["track", "album", "artist", "playlist"].contains(&kind).then_some(reference)
}

fn contains_items(value: &Value, depth: usize) -> bool {
    if depth > 12 { return false; }
    if item_reference(value).is_some() { return true; }
    match value {
        Value::Object(fields) => fields.values().any(|value| contains_items(value, depth + 1)),
        Value::Array(rows) => rows.iter().any(|value| contains_items(value, depth + 1)),
        _ => false,
    }
}

fn decorate(value: &mut Value, ids: &HashSet<String>, depth: usize, remaining: &mut usize) {
    if depth > 12 || *remaining == 0 { return; }
    *remaining -= 1;
    if let Some(reference) = item_reference(value) {
        let favorite = ids.contains(reference);
        value["favorite"] = json!(favorite);
    }
    match value {
        Value::Object(fields) => for value in fields.values_mut() { decorate(value, ids, depth + 1, remaining); },
        Value::Array(rows) => for value in rows { decorate(value, ids, depth + 1, remaining); },
        _ => {}
    }
}

fn parse(raw: &Value) -> Result<HashSet<String>> {
    let invalid = || Failure::new("The favorite membership response could not be read.");
    let fields = raw.as_object().ok_or_else(invalid)?;
    if !fields.is_empty() && !["TRACK", "ALBUM", "ARTIST", "PLAYLIST"].iter().any(|key| fields.contains_key(*key)) {
        return Err(invalid());
    }
    let mut ids = HashSet::new();
    let mut count = 0;
    for (field, kind) in [("TRACK", "track"), ("ALBUM", "album"), ("ARTIST", "artist"), ("PLAYLIST", "playlist")] {
        let Some(values) = raw.get(field) else { continue };
        let values = values.as_array().ok_or_else(invalid)?;
        count += values.len();
        if count > MAX_IDS { return Err(invalid()); }
        for value in values {
            let id = text(value);
            let valid = if kind == "playlist" { id.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '-') }
                else { id.parse::<u64>().ok().is_some_and(|id| id > 0) };
            if id.is_empty() || id.len() > 36 || !valid {
                return Err(invalid());
            }
            ids.insert(format!("{kind}:{id}"));
        }
    }
    Ok(ids)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_response_marks_each_kind_and_contextual_track_without_changing_refs() {
        let ids = parse(&json!({"TRACK": [7, "7"], "ALBUM": ["4"], "ARTIST": [3], "PLAYLIST": ["p"]})).unwrap();
        assert_eq!(ids.len(), 4);
        let mut document = json!({"header": {"ref": "album:4"}, "shelves": [{"items": [
            {"ref": "track:7@playlist:p@entry:1", "ai_generated": true}, {"ref": "track:8"},
            {"ref": "artist:3"}, {"ref": "playlist:p"}, {"ref": "mix:m"}]}]});
        decorate(&mut document, &ids, 0, &mut 10000);
        assert_eq!(document["header"]["favorite"], true);
        let rows = document["shelves"][0]["items"].as_array().unwrap();
        assert_eq!(rows[0]["ref"], "track:7@playlist:p@entry:1");
        assert_eq!(rows[0]["ai_generated"], true);
        assert_eq!(rows[0]["favorite"], true);
        assert_eq!(rows[1]["favorite"], false);
        assert_eq!(rows[2]["favorite"], true);
        assert_eq!(rows[3]["favorite"], true);
        assert!(rows[4].get("favorite").is_none());
    }

    #[test]
    fn a_collection_list_marks_its_rows_without_a_snapshot() {
        let mut albums = json!({"items": [{"ref": "album:4", "favorite": false}, {"ref": "track:7@favorites:MY_TRACKS"}, {"ref": "mix:m"}]});
        listed(&mut albums, "albums");
        assert_eq!(albums["items"][0]["favorite"], true);
        assert_eq!(albums["items"][1]["favorite"], true);
        assert!(albums["items"][2].get("favorite").is_none());
        let mut playlists = json!({"items": [{"ref": "playlist:a", "editable": true}, {"ref": "playlist:b", "editable": false}, {"ref": "playlist:c"}]});
        listed(&mut playlists, "playlists");
        assert_eq!(playlists["items"][0]["favorite"], false);
        assert_eq!(playlists["items"][1]["favorite"], true);
        assert!(playlists["items"][2].get("favorite").is_none());
        let mut empty = json!({});
        listed(&mut empty, "albums");
        assert_eq!(empty, json!({}));
    }

    #[test]
    fn an_accepted_write_changes_a_held_snapshot_and_keeps_an_unknown_one_unknown() {
        let folder = tempfile::tempdir().unwrap();
        let favorites = Favorites::new(Arc::new(Client::new(folder.path())));
        favorites.changed("album:4", true);
        assert!(favorites.state.lock().unwrap().ids.is_none());
        {
            let mut state = favorites.state.lock().unwrap();
            state.ids = Some(Arc::new(HashSet::from(["album:1".to_string()])));
            state.busy = Some(state.revision);
        }
        let before = favorites.state.lock().unwrap().revision;
        favorites.changed("track:7@playlist:p@entry:1", true);
        favorites.changed("album:1", false);
        let state = favorites.state.lock().unwrap();
        let ids = state.ids.as_ref().unwrap();
        assert!(ids.contains("track:7") && !ids.contains("album:1"));
        assert!(state.until > Instant::now());
        assert!(state.busy.is_none() && state.revision == before + 2);
    }

    #[test]
    fn malformed_membership_is_unknown_and_cannot_be_an_empty_collection() {
        for raw in [Value::Null, json!([]), json!({"TRACK": "7"}), json!({"ALBUM": [null]}),
            json!({"ARTIST": ["x".repeat(37)]}), json!({"PLAYLIST": ["../p"]}), json!({"TRACK": [-1]}), json!({"status": 500})] {
            assert!(parse(&raw).is_err());
        }
        assert!(parse(&json!({})).unwrap().is_empty());
        assert!(parse(&json!({"TRACK": vec![1; MAX_IDS + 1]})).is_err());
    }
}
