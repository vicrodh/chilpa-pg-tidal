//! Tidal entities mapped to Chilpa's shared album, artist and track models.

use crate::descriptions::description;
use crate::json::{at, first, fold, int_at, list_at, text, truthy};
use crate::state::{Failure, Result};
use regex::Regex;
use serde_json::{json, Map, Value};
use std::sync::LazyLock;

pub fn ident(value: &Value) -> String {
    text(first(value, &["id", "uuid"]))
}

pub fn unbox(value: &Value) -> &Value {
    let found = first(value, &["item", "data", "playlist", "value"]);
    if truthy(found) { found } else { value }
}

pub fn image(value: &Value, kind: &str) -> String {
    let mut key = first(value, &["cover", "squareImage", "image", "picture"]).clone();
    if !truthy(&key) && kind == "track" {
        return image(at(value, "album"), "album");
    }
    if key.is_object() {
        let medium = at(at(&key, "MEDIUM"), "url");
        key = if truthy(medium) { medium.clone() } else { at(&key, "url").clone() };
    }
    if !truthy(&key) {
        let images = first(value, &["images", "mixImages"]);
        let found = match images {
            Value::Array(items) => items.iter().map(|v| at(v, "url")).find(|url| truthy(url)),
            Value::Object(fields) => fields.values().map(|v| at(v, "url")).find(|url| truthy(url)),
            _ => None,
        };
        key = found.cloned().unwrap_or(Value::Null);
    }
    let Some(key) = key.as_str() else { return String::new() };
    if key.starts_with("https://") {
        return key.to_string();
    }
    let size = if kind == "artist" { "750x750" } else { "640x640" };
    format!("https://resources.tidal.com/images/{}/{size}.jpg", key.replace('-', "/"))
}

/// Seconds since the epoch of an ISO 8601 date the provider sends.
pub fn stamp(value: &Value) -> Option<i64> {
    let raw = value.as_str()?.replace('Z', "+00:00");
    if let Ok(date) = chrono::DateTime::parse_from_rfc3339(&raw) {
        return Some(date.timestamp());
    }
    for format in ["%Y-%m-%dT%H:%M:%S%.f%z", "%Y-%m-%dT%H:%M:%S%z", "%Y-%m-%d %H:%M:%S%.f%z", "%Y-%m-%d %H:%M:%S%z"] {
        if let Ok(date) = chrono::DateTime::parse_from_str(&raw, format) {
            return Some(date.timestamp());
        }
    }
    use chrono::TimeZone;
    for format in ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%dT%H:%M:%S", "%Y-%m-%d %H:%M:%S"] {
        if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(&raw, format) {
            return chrono::Local.from_local_datetime(&naive).earliest().map(|date| date.timestamp());
        }
    }
    let day = chrono::NaiveDate::parse_from_str(&raw, "%Y-%m-%d").ok()?;
    chrono::Local.from_local_datetime(&day.and_hms_opt(0, 0, 0)?).earliest().map(|date| date.timestamp())
}

pub fn genre_names(value: &Value) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    if let Value::Array(genres) = at(value, "genres") {
        for name in genres.iter().filter_map(Value::as_str).map(str::trim) {
            if !name.is_empty() && fold(name) != "music" && !names.iter().any(|known| known == name) {
                names.push(name.to_string());
            }
        }
    }
    if names.is_empty() {
        if let Some(genre) = at(value, "genre").as_str().map(str::trim) {
            if !genre.is_empty() && fold(genre) != "music" {
                names.push(genre.to_string());
            }
        }
    }
    names
}

static YEAR: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[0-9]{4}").unwrap());
static SEPARATOR: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[-–,/\s]+").unwrap());

/// The phonographic copyright owner of a copyright line, without marks or years.
pub fn copyright_owner(value: &Value) -> String {
    let Some(value) = value.as_str() else { return String::new() };
    let lines: Vec<&str> = value.lines().flat_map(|line| line.split(" / ")).map(str::trim).filter(|part| !part.is_empty()).collect();
    let mut rest = lines.iter().find(|line| line.starts_with('℗') || line.starts_with("(P)"))
        .or_else(|| lines.first()).copied().unwrap_or("").to_string();
    for mark in ["℗", "©", "(P)", "(p)", "(C)", "(c)"] {
        if let Some(stripped) = rest.strip_prefix(mark) {
            rest = stripped.trim_start().to_string();
        }
    }
    while YEAR.is_match(&rest) && !rest[4..].starts_with(|c: char| c.is_ascii_digit()) {
        rest = SEPARATOR.replace(&rest[4..], "").into_owned();
    }
    rest.trim().to_string()
}

fn quote_label(name: &str) -> String {
    crate::json::quote_all(name)
}

pub fn card(raw: &Value, kind: Option<&str>, user: &str) -> Value {
    let value = unbox(raw);
    let hint = text(at(raw, "type")).to_lowercase();
    let kind = match kind {
        Some(kind) => kind.to_string(),
        None if ["album", "artist", "track", "playlist", "mix"].contains(&hint.as_str()) => hint,
        None => if value.get("uuid").is_some() { "playlist" }
            else if value.get("trackNumber").is_some() { "track" }
            else if value.get("numberOfTracks").is_some() { "album" } else { "artist" }.to_string(),
    };
    let kind = kind.as_str();
    let key = ident(value);
    let mut title = text(first(value, &["title", "name"]));
    if title.is_empty() {
        title = text(at(at(value, "titleTextInfo"), "text"));
    }
    if let Some(version) = at(value, "version").as_str().filter(|v| !v.is_empty()) {
        if !title.contains(version) {
            title = format!("{title} ({version})");
        }
    }
    let artist = if truthy(at(value, "artist")) { at(value, "artist").clone() }
        else { list_at(value, "artists").first().cloned().unwrap_or(json!({})) };
    let album = if truthy(at(value, "album")) { at(value, "album").clone() } else { json!({}) };
    let mut result = Map::new();
    result.insert("kind".into(), json!(if kind == "mix" { "playlist" } else { kind }));
    result.insert("ref".into(), json!(format!("{kind}:{key}")));
    result.insert("title".into(), json!(title));
    result.insert("image".into(), json!(image(value, kind)));
    result.insert("artist".into(), json!(text(at(&artist, "name"))));
    result.insert("description".into(), json!(description(&text_or_empty(at(value, "description")))));
    result.insert("explicit".into(), json!(truthy(at(value, "explicit"))));
    if kind == "album" || kind == "track" {
        let mut genres = genre_names(value);
        if genres.is_empty() && kind == "track" {
            genres = genre_names(&album);
        }
        if !genres.is_empty() {
            result.insert("genre".into(), json!(genres.join(", ")));
            result.insert("genres".into(), json!(genres));
        }
    }
    if !ident(&artist).is_empty() {
        result.insert("artist_ref".into(), json!(format!("artist:{}", ident(&artist))));
    }
    if !ident(&album).is_empty() {
        result.insert("album_ref".into(), json!(format!("album:{}", ident(&album))));
        result.insert("album".into(), json!(text(at(&album, "title"))));
    }
    for (source, target) in [("streamReady", "available"), ("copyright", "copyright"), ("upc", "upc"), ("isrc", "isrc"),
                             ("numberOfTracks", "tracks"), ("trackNumber", "track_number"), ("volumeNumber", "disc_number")] {
        if let Some(found) = value.get(source) {
            result.insert(target.into(), found.clone());
        }
    }
    if let Some(duration) = at(value, "duration").as_f64() {
        result.insert("duration_ms".into(), json!((duration * 1000.0) as i64));
    } else if let Some(duration) = at(value, "duration").as_str().and_then(|d| d.trim().parse::<f64>().ok()) {
        result.insert("duration_ms".into(), json!((duration * 1000.0) as i64));
    }
    let date = if truthy(at(value, "releaseDate")) { at(value, "releaseDate") } else { at(&album, "releaseDate") };
    if truthy(date) {
        result.insert("release_date".into(), date.clone());
        if let Some(year) = date.as_str().and_then(|d| d.get(..4)).and_then(|d| d.parse::<i64>().ok()) {
            result.insert("year".into(), json!(year));
        }
    }
    let release_types = ["ALBUM", "EP", "SINGLE", "COMPILATION", "LIVE"];
    let mut release_type = text_or_empty(at(value, "type"));
    if kind == "track" && !release_types.contains(&release_type.as_str()) {
        release_type = text_or_empty(at(&album, "type"));
    }
    if release_types.contains(&release_type.as_str()) {
        result.insert("release_type".into(), json!(release_type.to_lowercase()));
    }
    let mut label = at(value, "label").clone();
    if !truthy(&label) && kind == "track" {
        label = at(&album, "label").clone();
    }
    if truthy(&label) {
        let name = if label.is_object() { text(at(&label, "name")) } else { text(&label) };
        result.insert("label".into(), json!(name));
        if !name.is_empty() {
            result.insert("label_ref".into(), json!(format!("label:{}", quote_label(&name))));
        }
    }
    let has_label = result.get("label").is_some_and(truthy);
    if !has_label && (kind == "album" || kind == "track") {
        let owner = copyright_owner(at(if kind == "track" { &album } else { value }, "copyright"));
        if !owner.is_empty() {
            result.insert("label".into(), json!(owner));
            result.insert("label_source".into(), json!("copyright"));
        }
    }
    let added = first(raw, &["created", "createdAt"]);
    let added = if truthy(added) { added } else { at(value, "dateAdded") };
    if let Some(added) = stamp(added) {
        result.insert("added_at".into(), json!(added));
    }
    if kind == "playlist" {
        let creator = obj_or_empty(at(value, "creator"));
        let editable = !user.is_empty() && text(at(&creator, "id")) == user && at(value, "type").as_str() != Some("EDITORIAL");
        let public = match value.get("publicPlaylist") { Some(found) => truthy(found), None => truthy(at(value, "public")) };
        result.insert("editable".into(), json!(editable));
        result.insert("copyable".into(), json!(true));
        result.insert("public".into(), json!(public));
        result.insert("updated_at".into(), json!(stamp(at(value, "lastUpdated"))));
        let name = match creator.get("name") { Some(name) => name.clone(), None => creator.get("username").cloned().unwrap_or(json!("")) };
        result.insert("artist".into(), name);
    }
    if let Some(quality) = audio_quality(value) {
        result.insert("quality".into(), quality);
    }
    Value::Object(result)
}

fn text_or_empty(value: &Value) -> String {
    if value.is_null() { String::new() } else { text(value) }
}

fn obj_or_empty(value: &Value) -> Value {
    if value.is_object() && truthy(value) { value.clone() } else { json!({}) }
}

/// Catalog tiers describe availability, not a measured stream format.
pub fn audio_quality(value: &Value) -> Option<Value> {
    let level = at(value, "audioQuality").as_str().unwrap_or("");
    let tags: Vec<&str> = list_at(at(value, "mediaMetadata"), "tags").iter().filter_map(Value::as_str).collect();
    if tags.contains(&"HIRES_LOSSLESS") || level == "HI_RES_LOSSLESS" {
        return Some(json!({"codec": "flac", "tier": "hi_res"}));
    }
    if tags.contains(&"LOSSLESS") || level == "LOSSLESS" {
        return Some(json!({"codec": "flac"}));
    }
    if level == "HIGH" || level == "LOW" {
        return Some(json!({"codec": "aac"}));
    }
    None
}

/// The kind and key of a reference, without its context suffixes.
pub fn reference(value: &str, expected: Option<&str>) -> Result<(String, String)> {
    let base = value.split('@').next().unwrap_or("");
    let (kind, key) = base.split_once(':').unwrap_or((base, ""));
    if key.is_empty() || expected.is_some_and(|expected| kind != expected) || key.contains(['/', '?', '#', '\\']) {
        return Err(Failure::invalid("Invalid source reference."));
    }
    Ok((kind.to_string(), key.to_string()))
}

pub fn shelf(key: &str, title: &str, kind: &str, rows: Vec<Value>, more: Option<String>) -> Value {
    json!({"id": key, "title": title, "kind": kind, "items": rows, "more": more, "sortable": kind == "album"})
}

pub fn count(value: &Value) -> i64 {
    int_at(value, "totalNumberOfItems", 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TRACK: &str = r#"{"id": 7, "title": "Song", "artist": {"id": 3, "name": "Artist"}, "album": {"id": 4, "title": "Record", "cover": "ab-cd"},
        "duration": 120, "trackNumber": 2, "volumeNumber": 1, "streamReady": false}"#;

    #[test]
    fn a_track_carries_its_album_artist_and_cover() {
        let track: Value = serde_json::from_str(TRACK).unwrap();
        let item = card(&track, Some("track"), "");
        assert_eq!(item["ref"], "track:7");
        assert_eq!(item["artist_ref"], "artist:3");
        assert_eq!(item["album_ref"], "album:4");
        assert_eq!(item["image"], "https://resources.tidal.com/images/ab/cd/640x640.jpg");
        assert_eq!(item["duration_ms"], 120000);
        assert_eq!(item["available"], false);
        assert_eq!(item["track_number"], 2);
    }

    #[test]
    fn the_copyright_owner_drops_marks_and_years() {
        assert_eq!(copyright_owner(&json!("℗ 2017 Label Records / © 2018 Other")), "Label Records");
        assert_eq!(copyright_owner(&json!("(P) 1999-2001, Some Label")), "Some Label");
        assert_eq!(copyright_owner(&json!("℗ 2021, Label")), "Label");
    }

    #[test]
    fn a_reference_rejects_paths_and_unexpected_kinds() {
        assert_eq!(reference("track:5@playlist:x", Some("track")).unwrap(), ("track".into(), "5".into()));
        assert!(reference("album:5", Some("track")).is_err());
        assert!(reference("track:../5", None).is_err());
    }

    #[test]
    fn a_playlist_is_editable_only_by_its_creator() {
        let raw = json!({"uuid": "p", "title": "List", "creator": {"id": 5}, "type": "USER"});
        assert_eq!(card(&raw, Some("playlist"), "5")["editable"], true);
        assert_eq!(card(&raw, Some("playlist"), "6")["editable"], false);
    }
}
