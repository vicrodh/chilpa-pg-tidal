//! The catalog renders through the host's existing views and navigation.

use crate::client::Client;
use crate::credits::Credits;
use crate::descriptions::description;
use crate::folders::collection as folder_collection;
use crate::items::{card, ident, reference, shelf, unbox};
use crate::json::{at, dumps_spaced, first, fold, int_at, list_at, param_int, quote_all, sha256_hex, str_at, text, truthy, unquote};
use crate::state::{Cache, Failure, Result};
use serde_json::{json, Map, Value};
use std::sync::{Arc, Mutex};

pub const DECADES: [&str; 7] = ["1950s", "1960s", "1970s", "1980s", "1990s", "2000s", "2010s"];

pub struct Catalog {
    pub client: Arc<Client>,
    pub cache: Arc<Cache>,
    pub credits: Option<Arc<Credits>>,
    locale: Mutex<String>,
}

fn cache_key(parts: &[&str]) -> String {
    parts.join("\u{1f}")
}

fn values(rows: &Value) -> Vec<Value> {
    rows.as_array().cloned().unwrap_or_default()
}

impl Catalog {
    pub fn new(client: Arc<Client>, cache: Arc<Cache>, credits: Option<Arc<Credits>>) -> Catalog {
        Catalog { client, cache, credits, locale: Mutex::new("en_US".into()) }
    }

    pub fn set_locale(&self, locale: &str) {
        *self.locale.lock().unwrap() = locale.to_string();
    }

    pub fn locale(&self) -> String {
        self.locale.lock().unwrap().clone()
    }

    fn cached(&self, path: &str, params: &Value, v2: bool) -> Result<Value> {
        let user = self.client.user();
        let key = cache_key(&[&user, path, &params.to_string(), if v2 { "v2" } else { "v1" }]);
        self.cache.get(&key, || self.client.request(path, params, "GET", None, None, v2, None).map(|reply| reply.0))
    }

    pub fn get(&self, path: &str, params: &Value) -> Result<Value> {
        self.cached(path, params, false)
    }

    pub fn get_v2(&self, path: &str, params: &Value) -> Result<Value> {
        self.cached(path, params, true)
    }

    fn all_cached(&self, name: &str, key: &str, path: &str) -> Result<Vec<Value>> {
        let user = self.client.user();
        let value = self.cache.get(&cache_key(&[&user, name, key]), || self.client.all(path).map(Value::Array))?;
        Ok(values(&value))
    }

    pub fn rows(&self, raw: &Value, kind: Option<&str>) -> Vec<Value> {
        let items: &[Value] = match raw {
            Value::Object(_) => list_at(raw, "items"),
            Value::Array(items) => items,
            _ => &[],
        };
        let user = self.client.user();
        items.iter().filter(|v| v.is_object() && !ident(unbox(v)).is_empty() && text(at(v, "type")).to_lowercase() != "video")
            .map(|v| card(v, kind, &user)).collect()
    }

    pub fn library(&self, params: &Value) -> Result<Value> {
        let user = self.client.user();
        let kind = match at(params, "kind") { Value::Null => "albums".to_string(), other => text(other) };
        let offset = param_int(params, "offset", 0)?.max(0);
        let limit = param_int(params, "limit", 100)?.clamp(1, 500);
        if !["albums", "artists", "tracks", "playlists"].contains(&kind.as_str()) {
            return Err(Failure::invalid("Invalid library category."));
        }
        if kind == "playlists" {
            let snapshot = self.cache.get(&cache_key(&[&user, "folder-collection"]), || folder_collection(self))?;
            let items = list_at(&snapshot, "items");
            let start = (offset as usize).min(items.len());
            let end = (offset as usize).saturating_add(limit as usize).min(items.len());
            return Ok(json!({"items": items[start..end].to_vec(), "total": items.len(), "folders": at(&snapshot, "folders")}));
        }
        let path = format!("users/{user}/favorites/{kind}");
        // A host page may exceed the provider's limit. Preserve all requested rows.
        let (mut rows, mut total) = (Vec::new(), 0i64);
        while (rows.len() as i64) < limit {
            let count = 100.min(limit - rows.len() as i64);
            let page = self.get(&path, &json!({"limit": count, "offset": offset + rows.len() as i64, "order": "DATE", "orderDirection": "DESC"}))?;
            let batch = list_at(&page, "items").to_vec();
            total = int_at(&page, "totalNumberOfItems", offset + rows.len() as i64 + batch.len() as i64);
            let short = (batch.len() as i64) < count;
            rows.extend(batch);
            if short || offset + rows.len() as i64 >= total {
                break;
            }
        }
        if let Some(credits) = &self.credits {
            if kind == "albums" || kind == "tracks" {
                credits.metadata.remember(&user, &rows, &kind, false, false, None);
            }
        }
        let mut items = self.rows(&Value::Array(rows), Some(&kind[..kind.len() - 1]));
        if let Some(credits) = &self.credits {
            credits.enrich(&mut items, &kind, &user);
        }
        Ok(json!({"items": items, "total": total}))
    }

    pub fn search(&self, params: &Value) -> Result<Value> {
        let text_query = text(at(params, "text")).trim().to_string();
        let keys = ["albums", "artists", "tracks", "playlists"];
        if text_query.is_empty() {
            return Ok(json!({"albums": [], "artists": [], "tracks": [], "playlists": []}));
        }
        let limit = param_int(params, "limit", 20)?.clamp(1, 100);
        let offset = param_int(params, "offset", 0)?.max(0);
        let result = self.get("search", &json!({"query": text_query, "limit": limit, "offset": offset, "types": "ALBUMS,ARTISTS,TRACKS,PLAYLISTS"}))?;
        let mut answer = Map::new();
        let mut totals = Map::new();
        for key in keys {
            let rows = self.rows(at(&result, key), Some(&key[..key.len() - 1]));
            totals.insert(key.into(), json!(int_at(at(&result, key), "totalNumberOfItems", rows.len() as i64)));
            answer.insert(key.into(), Value::Array(rows));
        }
        answer.insert("totals".into(), Value::Object(totals));
        let top = at(&result, "topHit");
        if top.is_object() && !ident(unbox(top)).is_empty() {
            answer.insert("top".into(), card(top, None, &self.client.user()));
        }
        Ok(Value::Object(answer))
    }

    pub fn track_quality(&self, params: &Value, quality: &str) -> Result<Value> {
        let (_, key) = reference(str_at(params, "ref"), Some("track"))?;
        let raw = self.get(&format!("tracks/{key}/playbackinfopostpaywall"),
            &json!({"playbackmode": "STREAM", "audioquality": quality, "assetpresentation": "FULL"}))?;
        Ok(json!({"ref": at(params, "ref"), "quality": crate::streaming::stream_format(&raw)?}))
    }

    /// One track by itself, for a list that holds it without having seen its
    /// album or artist: the same item a shelf would carry.
    pub fn track(&self, params: &Value) -> Result<Value> {
        let (_, key) = reference(str_at(params, "ref"), Some("track"))?;
        Ok(json!({"track": card(&self.get(&format!("tracks/{key}"), &json!({}))?, Some("track"), "")}))
    }

    pub fn album(&self, params: &Value) -> Result<Value> {
        let user = self.client.user();
        let (_, key) = reference(str_at(params, "ref"), Some("album"))?;
        let raw = match self.get(&format!("albums/{key}"), &json!({})) {
            Ok(raw) => raw,
            Err(error) if error.status == Some(404) => return Ok(json!({"availability": "unavailable", "tracks": []})),
            Err(error) => return Err(error),
        };
        if let Some(credits) = &self.credits {
            credits.metadata.remember(&user, std::slice::from_ref(&raw), "albums", false, true, None);
        }
        let tracks = self.all_cached("album-tracks", &key, &format!("albums/{key}/tracks"))?;
        let rows: Vec<Value> = tracks.iter().map(|entry| {
            let mut track = unbox(entry).clone();
            if let Value::Object(fields) = &mut track {
                fields.entry("album").or_insert_with(|| raw.clone());
            }
            card(&track, Some("track"), "")
        }).collect();
        let total = rows.len();
        Ok(json!({"album": card(&raw, Some("album"), ""), "tracks": rows, "total": total}))
    }

    pub fn artist(&self, params: &Value) -> Result<Value> {
        let (_, key) = reference(str_at(params, "ref"), Some("artist"))?;
        let raw = self.get(&format!("artists/{key}"), &json!({}))?;
        let albums = self.rows(&self.get(&format!("artists/{key}/albums"), &json!({"limit": 100}))?, Some("album"));
        Ok(json!({"artist": card(&raw, Some("artist"), ""), "albums": albums}))
    }

    pub fn playlist(&self, params: &Value) -> Result<Value> {
        let reference_text = str_at(params, "ref");
        let (kind, key) = reference(reference_text, None)?;
        if kind == "radio" {
            let (category, id) = key.split_once(':').unwrap_or((&key, ""));
            if !["track", "artist"].contains(&category) || id.is_empty() || !id.chars().all(|c| c.is_ascii_digit()) {
                return Err(Failure::invalid("Invalid radio reference."));
            }
            let tracks = self.rows(&self.get(&format!("{category}s/{id}/radio"), &json!({"limit": 100}))?, Some("track"));
            let total = tracks.len();
            return Ok(json!({"playlist": {"ref": at(params, "ref"), "title": "Radio", "kind": "playlist"}, "tracks": tracks, "total": total}));
        }
        if kind == "mix" {
            let page = self.get("pages/mix", &json!({"mixId": key, "deviceType": "BROWSER"}))?;
            let info = list_at(&page, "rows").iter().flat_map(|row| list_at(row, "modules"))
                .map(|module| at(module, "mix")).find(|mix| mix.is_object()).cloned()
                .unwrap_or_else(|| json!({"id": key, "title": "Mix"}));
            let rows = self.rows(&Value::Array(self.all_cached("mix", &key, &format!("mixes/{key}/items"))?), Some("track"));
            let total = rows.len();
            return Ok(json!({"playlist": card(&info, Some("mix"), ""), "tracks": rows, "total": total}));
        }
        if kind != "playlist" {
            return Err(Failure::invalid("Invalid playlist reference."));
        }
        let raw = self.get(&format!("playlists/{key}"), &json!({}))?;
        let entries = self.all_cached("playlist-tracks", &key, &format!("playlists/{key}/items"))?;
        // Bind occurrence identity to a snapshot. API writes use indices, never song IDs.
        let snapshot = snapshot_id(&entries);
        let mut rows = Vec::new();
        for (position, raw_track) in entries.iter().enumerate() {
            let entry_type = match at(raw_track, "type") { Value::Null => "track".to_string(), other => text(other) };
            if entry_type.to_lowercase() != "track" {
                continue;
            }
            let mut item = card(raw_track, Some("track"), "");
            let member = format!("{snapshot}:{position}:{}", ident(unbox(raw_track)));
            item["ref"] = json!(format!("{}@playlist:{key}@entry:{member}", text(&item["ref"])));
            item["member"] = json!(member);
            rows.push(item);
        }
        let total = rows.len();
        Ok(json!({"playlist": card(&raw, Some("playlist"), &self.client.user()), "tracks": rows, "total": total}))
    }

    pub fn optional(&self, path: &str, params: &Value) -> Result<Value> {
        match self.get(path, params) {
            Err(error) if matches!(error.status, Some(404 | 400)) => Ok(json!({})),
            other => other,
        }
    }

    pub fn page(&self, params: &Value) -> Result<Value> {
        let reference_text = match at(params, "ref") { Value::Null => "discover:home".to_string(), other => text(other) };
        if truthy(at(params, "refresh")) {
            self.cache.clear();
        }
        let r = reference_text.as_str();
        if r.starts_with("discover:") {
            return self.discover(r);
        }
        if let Some(path) = r.strip_prefix("browse:") {
            let path = unquote(path);
            let value = self.get(&path, &json!({"limit": 50, "deviceType": "BROWSER", "platform": "WEB", "locale": self.locale()}))?;
            return Ok(self.document(&value, &path));
        }
        if r.starts_with("artist:") {
            return self.artist_page(r);
        }
        if r.starts_with("album:") {
            let album = self.album(&json!({"ref": r}))?;
            if album.get("album").is_none() {
                return Ok(album);
            }
            let mut result = album["album"].clone();
            let key = reference(r, Some("album"))?.1;
            let review = self.optional(&format!("albums/{key}/review"), &json!({}))?;
            let review_text = match review.get("text") { Some(found) => text(found), None => text(at(&review, "review")) };
            result["description"] = json!(description(&review_text));
            let raw = self.get(&format!("albums/{key}"), &json!({}))?;
            let credits: Vec<Value> = list_at(&raw, "artists").iter().filter(|artist| truthy(at(artist, "id")) && truthy(at(artist, "name")))
                .map(|artist| json!({"id": at(artist, "id"), "name": at(artist, "name")})).collect();
            result["credits"] = json!(credits);
            let similar = self.rows(&self.optional(&format!("albums/{key}/similar"), &json!({}))?, Some("album"));
            result["shelves"] = json!([shelf("similar", "Related releases", "album", similar, None)]);
            return Ok(result);
        }
        if let Some(name) = r.strip_prefix("label:") {
            let name = unquote(name);
            // This API has labels as metadata, not a browsable label entity. Restrict
            // the search result to exact label matches, never show unrelated releases.
            let raw = self.get("search/albums", &json!({"query": name, "limit": 100}))?;
            let rows: Vec<Value> = self.rows(&raw, Some("album")).into_iter()
                .filter(|row| fold(str_at(row, "label")) == fold(&name)).collect();
            return Ok(json!({"title": name, "header": {"kind": "label", "ref": r, "title": name},
                "shelves": [shelf("releases", "Releases", "album", rows, None)]}));
        }
        if r.starts_with("library:") {
            let kind = r.split(':').nth(1).unwrap_or("");
            let mut result = json!({"title": title_case(kind)});
            if let Value::Object(fields) = self.library(&json!({"kind": kind, "limit": 500}))? {
                for (field, value) in fields {
                    result[field] = value;
                }
            }
            return Ok(result);
        }
        Err(Failure::invalid("Unknown catalog page."))
    }

    pub fn artist_page(&self, r: &str) -> Result<Value> {
        let parts: Vec<&str> = r.split(':').collect();
        let key = parts.get(1).copied().unwrap_or("");
        reference(&format!("artist:{key}"), Some("artist"))?;
        if parts.len() > 2 {
            let group = parts[2];
            let offset: i64 = match parts.get(3) {
                Some(value) => value.parse().map_err(|_| Failure::invalid("Invalid page."))?,
                None => 0,
            };
            let filter = match group {
                "albums" => Value::Null,
                "singles" => json!("EPSANDSINGLES"),
                "appears" => json!("COMPILATIONS"),
                _ => return Err(Failure::invalid("Unknown release group.")),
            };
            let page = self.get(&format!("artists/{key}/albums"), &json!({"limit": 100, "offset": offset, "filter": filter}))?;
            let rows = self.rows(&page, Some("album"));
            // The next page starts after what came, not a fixed hundred on.
            let after = offset + rows.len() as i64;
            let next = (!rows.is_empty() && after < int_at(&page, "totalNumberOfItems", 0)).then(|| format!("artist:{key}:{group}:{after}"));
            return Ok(json!({"title": title_case(group), "items": rows, "next": next}));
        }
        let raw = self.get(&format!("artists/{key}"), &json!({}))?;
        let mut header = card(&raw, Some("artist"), "");
        let bio = self.optional(&format!("artists/{key}/bio"), &json!({}))?;
        header["description"] = json!(description(&text(at(&bio, "text"))));
        header["description_source"] = json!(text(at(&bio, "source")));
        if truthy(at(&bio, "lastUpdated")) {
            header["bio_updated"] = json!(text(at(&bio, "lastUpdated")).chars().take(10).collect::<String>());
        }
        // What the artist does, and where it is on the web, when known.
        header["roles"] = json!(list_at(&raw, "artistRoles").iter().map(|role| text(at(role, "category"))).filter(|c| !c.is_empty()).collect::<Vec<_>>());
        let links: Vec<Value> = list_at(&self.optional(&format!("artists/{key}/links"), &json!({}))?, "items").iter()
            .filter(|link| truthy(at(link, "url")))
            .map(|link| {
                let site = text(at(link, "siteName"));
                json!({"title": if site.is_empty() { text(at(link, "url")) } else { site }, "url": at(link, "url")})
            }).collect();
        header["links"] = json!(links);
        let title = text(&header["title"]);
        let image = header["image"].clone();
        let mut shelves = vec![shelf("tracks", "Popular tracks", "track",
            self.rows(&self.optional(&format!("artists/{key}/toptracks"), &json!({"limit": 20}))?, Some("track")), None)];
        for (group, group_title, filter) in [("albums", "Albums", Value::Null), ("singles", "EPs and singles", json!("EPSANDSINGLES")), ("appears", "Appears on", json!("COMPILATIONS"))] {
            let page = self.optional(&format!("artists/{key}/albums"), &json!({"limit": 30, "filter": filter}))?;
            let rows = self.rows(&page, Some("album"));
            let more = ((rows.len() as i64) < int_at(&page, "totalNumberOfItems", 0)).then(|| format!("artist:{key}:{group}:{}", rows.len()));
            shelves.push(shelf(group, group_title, "album", rows, more));
        }
        // The newest record is the latest release.
        let newest = self.rows(&self.optional(&format!("artists/{key}/albums"), &json!({"limit": 1, "order": "DATE", "orderDirection": "DESC"}))?, Some("album"));
        if let Some(latest) = newest.into_iter().next() {
            shelves.push(shelf("latest", "Latest release", "album", vec![latest], None));
        }
        shelves.push(shelf("similar", "Similar artists", "artist",
            self.rows(&self.optional(&format!("artists/{key}/similar"), &json!({"limit": 20}))?, Some("artist")), None));
        shelves.push(shelf("radio", "Artist radio", "playlist",
            vec![json!({"kind": "playlist", "ref": format!("radio:artist:{key}"), "title": format!("{title} Radio"), "image": image})], None));
        // The artist's mix, when the service made one.
        let own_mix = at(at(&raw, "mixes"), "ARTIST_MIX");
        let mix_id = if truthy(own_mix) { own_mix.clone() } else { at(&self.optional(&format!("artists/{key}/mix"), &json!({}))?, "id").clone() };
        if truthy(&mix_id) {
            shelves.push(shelf("mix", "Artist mix", "playlist",
                vec![json!({"kind": "playlist", "ref": format!("mix:{}", text(&mix_id)), "title": format!("{title} Mix"), "image": image})], None));
        }
        // Who influenced the artist and whom it influenced: the module page
        // lists them as rows of artists under those titles.
        let page = self.get("pages/artist", &json!({"artistId": key, "deviceType": "BROWSER"})).unwrap_or_else(|_| json!({}));
        let mut influences = Vec::new();
        for module in list_at(&page, "rows").iter().flat_map(|row| list_at(row, "modules")) {
            let module_title = text(at(module, "title"));
            if !module_title.to_lowercase().contains("influenc") || !text(at(module, "type")).starts_with("ARTIST") {
                continue;
            }
            let paged = at(module, "pagedList");
            let entries = if paged.get("items").is_some() { list_at(paged, "items") } else { list_at(module, "items") };
            for entry in entries {
                let mut item = card(entry, Some("artist"), "");
                item["role"] = json!(module_title);
                influences.push(item);
            }
        }
        if !influences.is_empty() {
            shelves.push(shelf("influences", "Influences", "artist", influences, None));
        }
        Ok(json!({"title": title, "header": header, "shelves": shelves}))
    }

    pub fn discover(&self, r: &str) -> Result<Value> {
        let parts: Vec<&str> = r.split(':').collect();
        let tab = parts.get(1).copied().unwrap_or("home");
        let path = match tab {
            "home" | "foryou" => "home/feed/static",
            "editors" => "pages/staff_picks",
            "explore" => "pages/explore",
            "new" => "pages/explore_new_music",
            "top" => "pages/explore_top_music",
            "hires" => "pages/hires",
            "mixes" => "pages/my_collection_my_mixes",
            "genres" => "pages/genre_page",
            "moods" => "pages/moods_page",
            "decades" => "",
            _ => return Err(Failure::invalid("Unknown discovery tab.")),
        };
        let params = json!({"deviceType": "BROWSER", "platform": "WEB", "limit": 50, "locale": self.locale()});
        if tab == "decades" {
            let decade = parts.get(2).copied().unwrap_or(DECADES[0]);
            if !DECADES.contains(&decade) {
                return Err(Failure::invalid("Unknown decade."));
            }
            let path = format!("pages/m_{decade}");
            let mut document = self.document(&self.get(&path, &params)?, &path);
            let mut tabs: Vec<Value> = DECADES.iter().map(|label| json!({"title": label, "ref": format!("discover:decades:{label}"), "active": *label == decade})).collect();
            tabs.extend(list_at(&document, "tabs").iter().cloned());
            for (field, value) in [("title", json!("Decades")), ("tab", json!(tab)), ("personal", json!(false)), ("genre_filter", json!("")), ("genres", json!([])), ("tabs", json!(tabs))] {
                document[field] = value;
            }
            return Ok(document);
        }
        let genres = self.optional("genres", &json!({}))?;
        let genre_rows: Vec<Value> = if genres.is_array() { values(&genres) } else { list_at(&genres, "items").to_vec() };
        let genre_id = |genre: &Value| match genre.get("path") { Some(path) => text(path), None => text(at(genre, "id")) };
        let chosen = parts.get(2).filter(|part| !part.is_empty()).map(|part| unquote(part)).unwrap_or_default();
        let mut document = if !chosen.is_empty() {
            let known: Vec<String> = genre_rows.iter().map(genre_id).collect();
            let selected: Vec<&str> = chosen.split(',').collect();
            if selected.iter().any(|genre| !known.iter().any(|k| k == genre)) {
                return Err(Failure::invalid("Unknown genre."));
            }
            let mut sections = Vec::new();
            for genre in selected {
                for (kind, title) in [("albums", "Releases"), ("playlists", "Playlists")] {
                    let value = self.get(&format!("genres/{}/{kind}", quote_all(genre)), &json!({"limit": 100}))?;
                    sections.push(shelf(&format!("genre:{genre}:{kind}"), &format!("{genre} · {title}"), &kind[..kind.len() - 1],
                        self.rows(&value, Some(&kind[..kind.len() - 1])), None));
                }
            }
            json!({"title": "Music", "shelves": sections})
        } else {
            let personal = tab == "home" || tab == "foryou";
            let fetched = if personal { self.get_v2(path, &params) } else { self.get(path, &params) };
            let mut value = match fetched {
                Ok(value) => value,
                Err(error) if tab == "home" && matches!(error.status, Some(400 | 404)) => self.get("pages/home", &params)?,
                Err(error) => return Err(error),
            };
            if personal {
                let mut seen = Vec::new();
                let mut cursor = text(at(at(&value, "page"), "cursor"));
                while !cursor.is_empty() && !seen.contains(&cursor) {
                    seen.push(cursor.clone());
                    let mut next = params.clone();
                    next["cursor"] = json!(cursor);
                    let following = self.get_v2(path, &next)?;
                    let more = list_at(&following, "items").to_vec();
                    if !value["items"].is_array() {
                        value["items"] = json!([]);
                    }
                    value["items"].as_array_mut().unwrap().extend(more);
                    cursor = text(at(at(&following, "page"), "cursor"));
                }
            }
            self.document(&value, path)
        };
        if tab == "moods" {
            let excluded = ["record labels", "record label", "tidal magazine", "artist experiences"];
            let mut tabs: Vec<Value> = list_at(&document, "tabs").iter()
                .filter(|link| !excluded.contains(&fold(text(at(link, "title")).trim()).as_str())).cloned().collect();
            tabs.sort_by_key(|link| fold(&text(at(link, "title"))));
            document["tabs"] = json!(tabs);
        }
        let listed = ["home", "editors", "explore", "new", "top", "hires"].contains(&tab);
        let genres_out: Vec<Value> = if listed {
            genre_rows.iter().map(|genre| json!({"id": genre_id(genre), "name": match genre.get("name") { Some(name) => name.clone(), None => json!("") }})).collect()
        } else { Vec::new() };
        let active = if tab == "foryou" { "home" } else { tab };
        let mut tabs: Vec<Value> = [("home", "For You"), ("new", "New"), ("top", "Top"), ("editors", "Staff Picks"),
            ("explore", "Explore"), ("hires", "Hi-Res"), ("mixes", "My Mixes"), ("genres", "Genres"), ("decades", "Decades"), ("moods", "Moods")]
            .iter().map(|(key, title)| json!({"title": title, "ref": format!("discover:{key}"), "active": *key == active})).collect();
        tabs.extend(list_at(&document, "tabs").iter().cloned());
        for (field, value) in [("title", json!("Music")), ("tab", json!(tab)), ("personal", json!(["home", "foryou", "mixes"].contains(&tab))),
                               ("genre_filter", json!(chosen)), ("genres", json!(genres_out)), ("tabs", json!(tabs))] {
            document[field] = value;
        }
        Ok(document)
    }

    pub fn document(&self, value: &Value, path: &str) -> Value {
        let user = self.client.user();
        let mut modules: Vec<Value> = list_at(value, "rows").iter().flat_map(|row| list_at(row, "modules").iter().cloned()).collect();
        if modules.is_empty() {
            modules = list_at(value, "items").to_vec();
        }
        let (mut shelves, mut flat, mut links): (Vec<Value>, Vec<Value>, Vec<Value>) = (Vec::new(), Vec::new(), Vec::new());
        let mut by_id: Vec<String> = Vec::new();
        for module in &modules {
            let module_type = text(at(module, "type"));
            let paged = at(module, "pagedList");
            let entries = if paged.get("items").is_some() { list_at(paged, "items") } else { list_at(module, "items") };
            let mut groups: Vec<(String, Vec<Value>)> = Vec::new();
            let mut mapped_rows = Vec::new();
            for entry in entries {
                if text(at(entry, "type")).to_lowercase() == "video" {
                    continue;
                }
                let raw = unbox(entry);
                if ident(raw).is_empty() {
                    let api_path = at(raw, "apiPath");
                    if truthy(api_path) {
                        links.push(json!({"ref": format!("browse:{}", quote_all(&text(api_path))), "title": match raw.get("title") { Some(t) => t.clone(), None => json!("") }}));
                    }
                    continue;
                }
                let kind = ["album", "artist", "playlist", "track", "mix"].into_iter()
                    .find(|kind| module_type.starts_with(&format!("{}_", kind.to_uppercase())));
                let mapped = card(entry, kind, &user);
                let mapped_kind = text(&mapped["kind"]);
                match groups.iter_mut().find(|(name, _)| *name == mapped_kind) {
                    Some((_, rows)) => rows.push(mapped.clone()),
                    None => groups.push((mapped_kind, vec![mapped.clone()])),
                }
                mapped_rows.push(mapped);
            }
            let show_more = at(module, "showMore");
            let mut more = at(show_more, "apiPath").clone();
            if !truthy(&more) {
                more = at(module, "viewAll").clone();
            }
            if more.is_object() {
                more = at(&more, "apiPath").clone();
            }
            let mut more = if truthy(&more) { Some(text(&more)) } else { None };
            if !groups.is_empty() {
                let identity = first(module, &["moduleId", "id"]);
                let stable = if truthy(identity) { text(identity) } else {
                    sha256_hex(&format!("{path}{}{module_type}", text(at(module, "title"))))[..16].to_string()
                };
                if let Some(link) = &more {
                    if path.starts_with("home/") && !link.starts_with('/') && !link.starts_with("https://") {
                        more = Some(format!("/v2/{link}"));
                    }
                }
                let kind = if groups.len() == 1 { groups[0].0.clone() } else { "mixed".to_string() };
                let rows: Vec<Value> = if kind != "mixed" { mapped_rows.clone() } else {
                    mapped_rows.iter().filter(|row| row["kind"] != "track").cloned().collect()
                };
                let mut section = shelf(&format!("{stable}:{kind}"), &text(at(module, "title")), &kind, rows,
                    more.as_ref().map(|link| format!("browse:{}", quote_all(link))));
                if kind == "mixed" {
                    section["previous_ids"] = json!(groups.iter().map(|(name, _)| format!("{stable}:{name}")).collect::<Vec<_>>());
                    if let Some((_, tracks)) = groups.iter().find(|(name, _)| name == "track") {
                        section["extra_tracks"] = json!(tracks);
                    }
                }
                let context = at(module, "header");
                if context.is_object() && !ident(unbox(context)).is_empty() {
                    section["header"] = card(context, None, &user);
                }
                let id = text(&section["id"]);
                if let Some(index) = by_id.iter().position(|known| *known == id) {
                    let kept = &mut shelves[index];
                    for field in ["items", "extra_tracks"] {
                        let mut seen: Vec<Value> = list_at(kept, field).iter().map(|item| item["ref"].clone()).collect();
                        for item in list_at(&section, field) {
                            if !seen.contains(&item["ref"]) {
                                if !kept[field].is_array() {
                                    kept[field] = json!([]);
                                }
                                kept[field].as_array_mut().unwrap().push(item.clone());
                                seen.push(item["ref"].clone());
                            }
                        }
                    }
                    if truthy(&section["more"]) {
                        kept["more"] = section["more"].clone();
                    }
                } else {
                    by_id.push(id);
                    shelves.push(section);
                }
            }
            if entries.is_empty() && !ident(unbox(module)).is_empty() {
                flat.push(card(module, None, &user));
            }
        }
        let mut result = json!({"title": match value.get("title") { Some(title) => title.clone(), None => json!("Tidal") }});
        if !links.is_empty() {
            result["tabs"] = json!(links);
        }
        if !shelves.is_empty() {
            result["shelves"] = json!(shelves);
        } else {
            result["items"] = json!(flat);
        }
        result
    }
}

fn title_case(text: &str) -> String {
    let mut out = String::new();
    let mut start = true;
    for ch in text.chars() {
        if ch.is_alphabetic() {
            if start { out.extend(ch.to_uppercase()) } else { out.extend(ch.to_lowercase()) }
            start = false;
        } else {
            out.push(ch);
            start = true;
        }
    }
    out
}

/// The identity of a playlist's contents, bound into every occurrence reference.
pub fn snapshot_id(rows: &[Value]) -> String {
    let identities: Vec<Value> = rows.iter().map(|row| {
        let kind = match at(row, "type") { Value::Null => "track".to_string(), other => text(other) };
        json!([kind, ident(unbox(row))])
    }).collect();
    sha256_hex(&dumps_spaced(&Value::Array(identities)))[..24].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_snapshot_matches_the_identity_hash_of_earlier_versions() {
        // sha256('[["track", "7"], ["track", "7"]]')[:24], as the earlier plugin computed it.
        let rows = vec![json!({"type": "track", "item": {"id": 7}}), json!({"type": "track", "item": {"id": 7}})];
        assert_eq!(snapshot_id(&rows), sha256_hex(r#"[["track", "7"], ["track", "7"]]"#)[..24].to_string());
    }

    #[test]
    fn titles_capitalize_each_word() {
        assert_eq!(title_case("albums"), "Albums");
        assert_eq!(title_case("eps and singles"), "Eps And Singles");
    }
}
