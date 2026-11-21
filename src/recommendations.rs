//! Playlist suggestions exposed through existing native playlist cards.

use crate::catalog::Catalog;
use crate::i18n::{tr, tr_with};
use crate::items::{card, ident, reference, shelf, unbox};
use crate::json::{at, int_at, list_at, sha256_hex, str_at, text};
use crate::state::{Failure, Result};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::time::{Duration, Instant};

const LIMIT: usize = 500;
const MAX_PAGES: usize = 5;

pub fn choices(catalog: &Catalog) -> Result<Value> {
    let page = choices_page(catalog, 0)?;
    let items = list_at(&page, "items").to_vec();
    let title = tr(&catalog.locale(), "Playlist recommendations");
    let more = (!items.is_empty()).then(|| "recommendation-lists:0".to_string());
    Ok(json!({"title": title, "personal": true,
        "shelves": [shelf("playlist-recommendations", &title, "playlist", items, more)]}))
}

pub fn choices_page(catalog: &Catalog, offset: i64) -> Result<Value> {
    let library = catalog.library(&json!({"kind": "playlists", "offset": offset, "limit": 500}))?;
    let items: Vec<Value> = list_at(&library, "items").iter().filter_map(|item| {
        let (_, key) = reference(str_at(item, "ref"), Some("playlist")).ok()?;
        let mut item = item.clone();
        item["ref"] = json!(format!("recommendations:{key}"));
        item["editable"] = json!(false);
        item["copyable"] = json!(false);
        if let Some(fields) = item.as_object_mut() {
            fields.remove("tracks");
            fields.remove("favorite");
        }
        Some(item)
    }).collect();
    let after = offset + list_at(&library, "items").len() as i64;
    let next = (after > offset && after < int_at(&library, "total", after)).then(|| format!("recommendation-lists:{after}"));
    let title = tr(&catalog.locale(), "Playlist recommendations");
    Ok(json!({"title": title, "personal": true, "items": items, "next": next}))
}

pub fn playlist(catalog: &Catalog, key: &str) -> Result<Value> {
    let raw = catalog.get(&format!("playlists/{key}"), &json!({}))?;
    let mut rows = Vec::new();
    let mut seen = HashSet::new();
    let mut pages = HashSet::new();
    let mut offset = 0;
    let mut truncated = false;
    let deadline = Instant::now() + Duration::from_secs(8);
    let user = catalog.client.user();
    // Reuse the existing bounded catalog cache. Each opening asks for at
    // most five pages / 500 rows, within one eight-second network budget.
    for _ in 0..MAX_PAGES {
        let page = catalog.cache.get(&format!("{user}\u{1f}playlist-recommendations\u{1f}{key}\u{1f}{offset}"), ||
            catalog.client.get_until(&format!("playlists/{key}/recommendations/items"),
                &json!({"limit": 100, "offset": offset, "deviceType": "BROWSER", "locale": catalog.locale()}), deadline, &user))?;
        let batch = page.get("items").and_then(Value::as_array)
            .ok_or_else(|| Failure::new("The playlist recommendations could not be read."))?;
        if batch.len() > 100 {
            return Err(Failure::new("The playlist recommendations exceeded the requested page size."));
        }
        if !batch.is_empty() && !pages.insert(sha256_hex(&serde_json::to_string(batch).unwrap_or_default())) {
            return Err(Failure::new("The playlist recommendations repeated a page. Try again."));
        }
        for entry in batch {
            if str_at(entry, "type").eq_ignore_ascii_case("VIDEO") || ident(unbox(entry)).is_empty() { continue; }
            let mut row = card(entry, Some("track"), "");
            let reference = text(at(&row, "ref"));
            if seen.insert(reference.clone()) {
                row["ref"] = json!(format!("{reference}@playlist:{key}"));
                rows.push(row);
            }
        }
        offset += batch.len();
        let more = !batch.is_empty() && (offset as i64) < int_at(&page, "totalNumberOfItems", offset as i64);
        truncated = more;
        if batch.is_empty() || !more { break; }
        if offset >= LIMIT { break; }
    }
    let mut header = card(&raw, Some("playlist"), "");
    let locale = catalog.locale();
    header["ref"] = json!(format!("recommendations:{key}"));
    header["title"] = json!(tr_with(&locale, "Recommendations for {name}", &[("name", str_at(&raw, "title"))]));
    header["editable"] = json!(false);
    header["copyable"] = json!(false);
    header["tracks"] = json!(rows.len());
    header["description"] = if truncated {
        json!(tr_with(&locale, "Showing the first {count} recommendations.", &[("count", &rows.len().to_string())]))
    } else { json!("") };
    Ok(json!({"playlist": header, "tracks": rows, "total": seen.len(), "truncated": truncated}))
}
