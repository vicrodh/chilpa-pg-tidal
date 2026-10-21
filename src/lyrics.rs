//! The lyrics of one of this source's own tracks, for the host's track.lyrics.

use crate::catalog::Catalog;
use crate::items::reference;
use crate::json::{at, str_at};
use crate::state::{Failure, Result};
use serde_json::{json, Map, Value};

/// Answers found or none. Every failure stays a failure: the host asks again later.
pub fn track_lyrics(catalog: &Catalog, params: &Value) -> Result<Value> {
    let (_, key) = reference(str_at(params, "ref"), Some("track"))?;
    let raw = match catalog.client.get(&format!("tracks/{key}/lyrics"), &json!({})) {
        Ok(raw) => raw,
        Err(error) if error.status == Some(404) => return Ok(json!({"status": "none"})),
        Err(error) => return Err(error),
    };
    if !raw.is_object() {
        return Err(Failure::new("Tidal returned unreadable lyrics."));
    }
    let (timed, plain) = (text(at(&raw, "subtitles")), text(at(&raw, "lyrics")));
    let lyrics = if !timed.is_empty() {
        json!({"format": "lrc", "text": timed})
    } else if !plain.is_empty() {
        json!({"format": "plain", "text": plain})
    } else {
        return Ok(json!({"status": "none"}));
    };
    let mut answer = json!({"status": "found", "lyrics": lyrics});
    let found = fingerprint(catalog, &key);
    if !found.is_empty() {
        answer["fingerprint"] = Value::Object(found);
    }
    Ok(answer)
}

fn text(value: &Value) -> String {
    value.as_str().filter(|text| !text.trim().is_empty()).unwrap_or("").to_string()
}

/// What the track says it is. Resolving already fetched it, so this is normally cached.
fn fingerprint(catalog: &Catalog, key: &str) -> Map<String, Value> {
    let mut found = Map::new();
    let Ok(raw) = catalog.get(&format!("tracks/{key}"), &json!({})) else { return found };
    if let Some(duration) = at(&raw, "duration").as_f64().filter(|duration| *duration > 0.0) {
        found.insert("duration_ms".into(), json!((duration * 1000.0) as i64));
    }
    let isrc = text(at(&raw, "isrc"));
    if !isrc.is_empty() {
        found.insert("isrc".into(), json!(isrc.trim()));
    }
    found
}
