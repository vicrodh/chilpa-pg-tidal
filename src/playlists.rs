//! Explicit playlist writes with ownership, snapshot and ETag checks.

use crate::catalog::{snapshot_id, Catalog};
use crate::client::Client;
use crate::folders::action as folder_action;
use crate::items::{ident, reference, unbox};
use crate::json::{at, str_at, text, truthy};
use crate::state::{Failure, Result};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

pub struct Playlists {
    client: Arc<Client>,
    catalog: Arc<Catalog>,
    lock: Mutex<()>,
}

impl Playlists {
    pub fn new(client: Arc<Client>, catalog: Arc<Catalog>) -> Playlists {
        Playlists { client, catalog, lock: Mutex::new(()) }
    }

    fn write(&self, path: &str, method: &str, data: Option<Value>, etag: Option<&str>) -> Result<(Value, String)> {
        self.client.request(path, &json!({}), method, data.as_ref(), etag, false, None)
    }

    pub fn action(&self, params: &Value) -> Result<Value> {
        let action = str_at(params, "action");
        if action == "playlist.search" {
            let found = self.catalog.search(params)?;
            return Ok(json!({"read_only": true, "items": found["tracks"]}));
        }
        let _writing = self.lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if ["folder.create", "folder.rename", "playlist.folder"].contains(&action) {
            let result = folder_action(&self.client, params)?;
            self.catalog.cache.clear();
            return Ok(result);
        }
        if action == "playlist.create" || action == "playlist.copy" {
            return self.create(params, action == "playlist.copy");
        }
        let reference_text = str_at(params, "ref");
        let (kind, key) = reference(reference_text, None)?;
        if action == "favorite" {
            return self.favorite(&kind, &key, at(params, "on") == &json!(true));
        }
        if kind != "playlist" {
            return Err(Failure::invalid("This action needs a playlist."));
        }
        let raw = self.client.get(&format!("playlists/{key}"), &json!({}))?;
        if text(at(at(&raw, "creator"), "id")) != self.client.user() || at(&raw, "type").as_str() == Some("EDITORIAL") {
            return Err(Failure::new("Only your own playlists can be edited."));
        }
        let path = format!("playlists/{key}");
        match action {
            "playlist.update" => {
                self.write(&path, "POST", Some(json!({"title": title(params)?, "description": text(at(params, "description"))})), None)?;
                self.visibility(&key, truthy(at(params, "public")))?;
            }
            "playlist.delete" => {
                self.write(&path, "DELETE", None, None)?;
            }
            "playlist.add" | "playlist.remove" | "playlist.move" => {
                // Read metadata and items under one version; fetching both sides catches
                // edits racing a paginated read. The mutation is conditioned on that ETag.
                let (_, before) = self.client.request(&path, &json!({"limit": 1}), "GET", None, None, false, None)?;
                let entries = self.client.all(&format!("{path}/items"))?;
                let (_, after) = self.client.request(&path, &json!({"limit": 1}), "GET", None, None, false, None)?;
                if before.is_empty() || before != after {
                    return Err(Failure::new("The playlist changed. Refresh it before editing."));
                }
                if action == "playlist.add" {
                    let tracks = at(params, "tracks").as_array().filter(|tracks| (1..=100).contains(&tracks.len()))
                        .ok_or_else(|| Failure::invalid("Choose between 1 and 100 tracks."))?;
                    let ids = tracks.iter().map(|track| reference(&text(track), Some("track")).map(|found| found.1)).collect::<Result<Vec<_>>>()?;
                    self.write(&format!("{path}/items"), "POST",
                        Some(json!({"trackIds": ids.join(","), "onDupes": "ADD", "onArtifactNotFound": "FAIL"})), Some(&after))?;
                } else {
                    let indices = members(at(params, "members"), &entries)?;
                    let target = format!("{path}/items/{}", indices.iter().map(usize::to_string).collect::<Vec<_>>().join(","));
                    if action == "playlist.remove" {
                        self.write(&target, "DELETE", None, Some(&after))?;
                    } else {
                        let position = at(params, "before");
                        let valid = (position.is_i64() || position.is_u64()) && position.as_i64().is_some_and(|p| p >= 0 && p as usize <= entries.len());
                        if !valid {
                            return Err(Failure::invalid("Invalid destination position."));
                        }
                        self.write(&target, "POST", Some(json!({"toIndex": position})), Some(&after))?;
                    }
                }
            }
            _ => return Err(Failure::invalid("Unknown playlist action.")),
        }
        Ok(json!({"changed_refs": [reference_text]}))
    }

    fn visibility(&self, key: &str, public: bool) -> Result<()> {
        let path = format!("playlists/{key}/set-{}", if public { "public" } else { "private" });
        self.client.request(&path, &json!({}), "PUT", None, None, true, None).map(|_| ())
    }

    fn create(&self, params: &Value, copy: bool) -> Result<Value> {
        let mut tracks = Vec::new();
        if copy {
            let (_, key) = reference(str_at(params, "ref"), Some("playlist"))?;
            tracks = self.client.all(&format!("playlists/{key}/items"))?.iter()
                .filter(|value| match at(value, "type") { Value::Null => true, other => text(other).to_lowercase() == "track" })
                .map(|value| ident(unbox(value))).collect();
        }
        let name = title(params)?;
        let (result, _) = self.client.request("my-collection/playlists/folders/create-playlist",
            &json!({"name": name, "description": text(at(params, "description")), "folderId": "root"}), "PUT", None, None, true, None)?;
        let created = if truthy(at(&result, "data")) { at(&result, "data") } else { &result };
        let key = ident(created);
        if key.is_empty() {
            return Err(Failure::new("Tidal did not identify the new playlist. Refresh your library before retrying."));
        }
        let finish = || -> Result<()> {
            self.visibility(&key, truthy(at(params, "public")))?;
            for chunk in tracks.chunks(100) {
                let (_, etag) = self.client.request(&format!("playlists/{key}"), &json!({}), "GET", None, None, false, None)?;
                if etag.is_empty() {
                    return Err(Failure::new("Tidal did not return a playlist version."));
                }
                self.write(&format!("playlists/{key}/items"), "POST",
                    Some(json!({"trackIds": chunk.join(","), "onDupes": "ADD", "onArtifactNotFound": "FAIL"})), Some(&etag))?;
            }
            Ok(())
        };
        if finish().is_err() {
            self.catalog.cache.clear();
            return Err(Failure::new(format!("Playlist {key} was created, but not all changes completed. Refresh your library and inspect it before retrying.")));
        }
        Ok(json!({"changed_refs": [format!("playlist:{key}")], "ref": format!("playlist:{key}")}))
    }

    fn favorite(&self, kind: &str, key: &str, on: bool) -> Result<Value> {
        if !["album", "artist", "track", "playlist"].contains(&kind) {
            return Err(Failure::invalid("This item cannot be saved as a favorite."));
        }
        let path = format!("users/{}/favorites/{kind}s", self.client.user());
        if on {
            let field = if kind == "playlist" { "uuids".to_string() } else { format!("{kind}Ids") };
            let mut data = json!({"onArtifactNotFound": "FAIL"});
            data[field] = json!(key);
            self.write(&path, "POST", Some(data), None)?;
        } else {
            self.write(&format!("{path}/{key}"), "DELETE", None, None)?;
        }
        Ok(json!({"changed_refs": [format!("{kind}:{key}")]}))
    }
}

pub fn title(params: &Value) -> Result<String> {
    let value = text(at(params, "title")).trim().to_string();
    if value.is_empty() || value.chars().count() > 250 {
        return Err(Failure::invalid("Enter a playlist name of at most 250 characters."));
    }
    Ok(value)
}

/// The positions of the chosen occurrences, checked against the snapshot they came from.
pub fn members(values: &Value, entries: &[Value]) -> Result<Vec<usize>> {
    let values = values.as_array().filter(|values| !values.is_empty()).ok_or_else(|| Failure::invalid("Choose playlist entries first."))?;
    let expected = snapshot_id(entries);
    let mut positions = Vec::new();
    for value in values {
        let raw = text(value);
        let parts: Vec<&str> = raw.split(':').collect();
        let [snapshot, position, track] = parts[..] else { return Err(Failure::invalid("Invalid playlist entry.")) };
        let position: i64 = position.trim().parse().map_err(|_| Failure::invalid("Invalid playlist entry."))?;
        if snapshot != expected || position < 0 || position as usize >= entries.len() || ident(unbox(&entries[position as usize])) != track {
            return Err(Failure::new("The playlist changed. Refresh it before editing."));
        }
        positions.push(position as usize);
    }
    positions.sort_unstable();
    positions.dedup();
    Ok(positions)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn members_resolve_to_sorted_unique_positions_of_the_same_snapshot() {
        let entries = vec![json!({"type": "track", "item": {"id": 7}}), json!({"type": "track", "item": {"id": 7}})];
        let snapshot = snapshot_id(&entries);
        let chosen = json!([format!("{snapshot}:1:7"), format!("{snapshot}:0:7"), format!("{snapshot}:1:7")]);
        assert_eq!(members(&chosen, &entries).unwrap(), vec![0, 1]);
        assert!(members(&json!([format!("{snapshot}:1:8")]), &entries).is_err());
        assert!(members(&json!(["other:0:7"]), &entries).is_err());
    }

    #[test]
    fn a_playlist_name_is_required_and_bounded() {
        assert!(title(&json!({"title": "  "})).is_err());
        assert!(title(&json!({"title": "x".repeat(251)})).is_err());
        assert_eq!(title(&json!({"title": " Mine "})).unwrap(), "Mine");
    }
}
