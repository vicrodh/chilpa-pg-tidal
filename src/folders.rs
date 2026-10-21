//! Native collection folders retain their own identities and membership.

use crate::catalog::Catalog;
use crate::client::Client;
use crate::items::{card, reference};
use crate::json::{at, first, int_at, list_at, str_at, text};
use crate::state::{Failure, Result};
use serde_json::{json, Value};
use std::collections::{HashSet, VecDeque};

pub const PATH: &str = "my-collection/playlists/folders";

pub fn folder_id(value: &Value) -> Result<String> {
    let value = if value.is_null() || text(value).is_empty() { "root".to_string() } else { text(value) };
    if value == "root" {
        return Ok(value);
    }
    let key = value.strip_prefix("folder:").unwrap_or(&value);
    uuid::Uuid::parse_str(key).map(|id| id.hyphenated().to_string()).map_err(|_| Failure::invalid("Invalid folder reference."))
}

pub fn collection(catalog: &Catalog) -> Result<Value> {
    let mut pending = VecDeque::from(["root".to_string()]);
    let mut seen = HashSet::new();
    let (mut folders, mut playlists) = (Vec::new(), Vec::new());
    let user = catalog.client.user();
    while let Some(parent) = pending.pop_front() {
        if !seen.insert(parent.clone()) {
            continue;
        }
        if seen.len() > 1000 {
            return Err(Failure::new("The folder collection exceeds the current limit."));
        }
        let native = if parent == "root" { String::new() } else { format!("folder:{parent}") };
        let mut offset = 0;
        loop {
            if offset >= 100000 {
                return Err(Failure::new("The playlist collection exceeds the current limit."));
            }
            let page = catalog.get_v2(PATH, &json!({"folderId": parent, "limit": 50, "offset": offset, "order": "NAME"}))?;
            let batch = list_at(&page, "items");
            for entry in batch {
                let raw = if at(entry, "data").is_object() { at(entry, "data").clone() } else { json!({}) };
                let kind = {
                    let own = at(entry, "itemType");
                    text(if own.is_null() { at(&raw, "itemType") } else { own }).to_uppercase()
                };
                if kind == "FOLDER" {
                    let key = folder_id(at(&raw, "id"))?;
                    if key == "root" {
                        continue;
                    }
                    let name = first(entry, &["name"]);
                    let name = if name.is_null() { text(at(&raw, "name")) } else { text(name) };
                    folders.push(json!({"ref": format!("folder:{key}"), "name": name, "parent": native}));
                    pending.push_back(key);
                } else if kind == "PLAYLIST" {
                    let mut item = card(&raw, Some("playlist"), &user);
                    item["native_folder"] = json!(native);
                    playlists.push(item);
                }
            }
            if batch.len() < 50 || offset + batch.len() as i64 >= int_at(&page, "totalNumberOfItems", 100001) {
                break;
            }
            offset += 50;
        }
    }
    Ok(json!({"items": playlists, "folders": folders}))
}

pub fn action(client: &Client, params: &Value) -> Result<Value> {
    let operation = str_at(params, "action");
    let put = |path: &str, query: Value| client.request(&format!("{PATH}/{path}"), &query, "PUT", None, None, true, None);
    match operation {
        "folder.create" => {
            let name = text(at(params, "title")).trim().to_string();
            if name.is_empty() {
                return Err(Failure::invalid("Enter a folder name."));
            }
            put("create-folder", json!({"name": name, "folderId": folder_id(at(params, "parent"))?}))?;
        }
        "folder.rename" => {
            let key = folder_id(at(params, "ref"))?;
            let name = text(at(params, "title")).trim().to_string();
            if key == "root" || name.is_empty() {
                return Err(Failure::invalid("Choose a folder and a name."));
            }
            put("rename", json!({"trn": format!("trn:folder:{key}"), "name": name}))?;
        }
        "playlist.folder" => {
            let key = reference(str_at(params, "ref"), Some("playlist"))?.1;
            put("move", json!({"trns": format!("trn:playlist:{key}"), "folderId": folder_id(at(params, "folder"))?}))?;
        }
        _ => return Err(Failure::invalid("Unknown folder action.")),
    }
    Ok(json!({"changed_refs": [str_at(params, "ref")], "folders_changed": true}))
}
