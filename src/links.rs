//! What a Tidal link opens, for Chilpa's "Open music link": the kind and the
//! reference the pages carry, or nothing for a link of anywhere else. Chilpa
//! asks every source and the one that claims the link opens it.

use serde_json::{json, Value};

/// A path segment that names what follows it; a mix is a playlist kind with a
/// mix reference, as the cards carry it.
fn kind(segment: &str) -> Option<(&'static str, &'static str)> {
    match segment {
        "album" => Some(("album", "album")),
        "track" => Some(("track", "track")),
        "artist" => Some(("artist", "artist")),
        "playlist" => Some(("playlist", "playlist")),
        "mix" => Some(("playlist", "mix")),
        _ => None,
    }
}

pub fn link_target(address: &str) -> Value {
    let Ok(parsed) = url::Url::parse(address.trim()) else { return Value::Null };
    let host = parsed.host_str().unwrap_or("").to_lowercase();
    let mut parts: Vec<String> = Vec::new();
    if parsed.scheme() == "tidal" {
        parts.push(host.clone());
        parts.extend(parsed.path().split('/').filter(|part| !part.is_empty()).map(str::to_string));
    } else if matches!(parsed.scheme(), "http" | "https") && (host == "tidal.com" || host.ends_with(".tidal.com")) {
        parts.extend(parsed.path().split('/').filter(|part| !part.is_empty()).map(str::to_string));
    } else {
        return Value::Null;
    }
    if parts.first().is_some_and(|part| part == "browse") {
        parts.remove(0);
    }
    for index in 0..parts.len().saturating_sub(1) {
        if let Some((kind, prefix)) = kind(&parts[index]) {
            return json!({"kind": kind, "ref": format!("{prefix}:{}", parts[index + 1])});
        }
    }
    Value::Null
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn web_and_app_links_open_their_items() {
        assert_eq!(link_target("https://tidal.com/browse/album/123"), json!({"kind": "album", "ref": "album:123"}));
        assert_eq!(link_target("https://listen.tidal.com/mix/00abc"), json!({"kind": "playlist", "ref": "mix:00abc"}));
        assert_eq!(link_target("tidal://track/9"), json!({"kind": "track", "ref": "track:9"}));
        assert_eq!(link_target("https://example.com/album/1"), Value::Null);
    }
}
