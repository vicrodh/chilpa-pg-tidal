//! Native JSON-RPC source operations, separate from host UI and audio policy.

use crate::account::Account;
use crate::catalog::Catalog;
use crate::client::Client;
use crate::credits::Credits;
use crate::items::card;
use crate::json::{at, str_at, text};
use crate::links::link_target;
use crate::lyrics::track_lyrics;
use crate::playlists::Playlists;
use crate::reporting::Reports;
use crate::state::{create_private_dir, Cache, Failure, Result};
use crate::streaming::{normalization, Streams};
use serde_json::{json, Value};
use std::path::Path;
use std::sync::{Arc, Mutex, RwLock};

pub struct Service {
    client: Arc<Client>,
    cache: Arc<Cache>,
    account: Arc<Account>,
    catalog: Arc<Catalog>,
    playlists: Playlists,
    reports: Arc<Reports>,
    streams: Streams,
    locale: Mutex<String>,
    gate: RwLock<()>,
}

impl Service {
    pub fn new(data: &Path) -> std::io::Result<Service> {
        create_private_dir(data)?;
        let client = Arc::new(Client::new(data));
        let cache = Arc::new(Cache::default());
        let account = Arc::new(Account::new(client.clone(), data));
        let credits = Arc::new(Credits::new(client.clone(), data));
        let catalog = Arc::new(Catalog::new(client.clone(), cache.clone(), Some(credits)));
        let playlists = Playlists::new(client.clone(), catalog.clone());
        let reports = Arc::new(Reports::new(data, client.clone()));
        let streams = Streams::new(client.clone(), account.clone())?;
        reports.start();
        Ok(Service { client, cache, account, catalog, playlists, reports, streams, locale: Mutex::new("en".into()), gate: RwLock::new(()) })
    }

    pub fn handle(&self, method: &str, params: &Value) -> Result<Value> {
        // Signing in or out waits for running calls and holds new ones. Asking
        // whether a code was approved can take seconds, so it does not.
        let polling = method == "settings.submit" && str_at(params, "action") == "poll";
        if (method == "settings.submit" && !polling) || method == "auth.logout" {
            let _exclusive = self.gate.write().unwrap_or_else(|poisoned| poisoned.into_inner());
            self.dispatch(method, params)
        } else {
            let _shared = self.gate.read().unwrap_or_else(|poisoned| poisoned.into_inner());
            self.dispatch(method, params)
        }
    }

    fn locale(&self) -> String {
        self.locale.lock().unwrap().clone()
    }

    fn dispatch(&self, method: &str, params: &Value) -> Result<Value> {
        match method {
            "initialize" => {
                let locale = match at(params, "locale") { Value::Null => "en".to_string(), other => text(other) };
                self.catalog.set_locale(&locale.replace('-', "_"));
                *self.locale.lock().unwrap() = locale;
                return Ok(json!({"protocol": 1, "capabilities": {"auth": "browser", "home": true,
                    "navigation": {"discover": "Music", "library": "Collection", "albums": "Albums"},
                    "library": ["albums", "tracks", "artists", "playlists"], "pages": true,
                    "playlist_edit": true, "track_quality": true, "search": true, "scene_search_limit": 1000,
                    "resolve": true, "favorites": true, "playback_events": 1, "track_lyrics": 1}}));
            }
            "auth.status" => return Ok(self.account.status()),
            "settings.open" | "auth.login" => return Ok(self.account.open(&self.locale())),
            "settings.submit" => {
                let answer = self.account.submit(params, &self.locale());
                self.cache.clear();
                if self.client.user().is_empty() {
                    self.streams.clear();
                    self.reports.clear();
                }
                return Ok(answer);
            }
            "auth.logout" => {
                self.client.logout();
                self.account.reset();
                self.cache.clear();
                self.streams.clear();
                self.reports.clear();
                return Ok(json!({"signed_in": false}));
            }
            "inspect" => return Ok(json!({"settings_pages": ["account"]})),
            "reporting.status" => return Ok(self.reports.status()),
            "playback.event" => return self.reports.event(params),
            // A link of this source, as what to open; null for any other.
            "link.resolve" => return Ok(link_target(str_at(params, "url"))),
            _ => {}
        }
        self.client.token()?;
        match method {
            "home" => self.catalog.discover("discover:home"),
            "page.get" => {
                let mut result = self.catalog.page(params)?;
                let history = self.reports.history(20);
                if str_at(params, "ref") == "discover:foryou" && !history.is_empty() {
                    if !result["shelves"].is_array() {
                        result["shelves"] = json!([]);
                    }
                    result["shelves"].as_array_mut().unwrap().insert(0,
                        json!({"id": "chilpa-history", "title": crate::i18n::tr(&self.locale(), "Recently played in Chilpa"), "kind": "track", "items": history}));
                }
                Ok(result)
            }
            "library.list" => self.catalog.library(params),
            "search" => self.catalog.search(params),
            "album.get" => self.catalog.album(params),
            "artist.get" => self.catalog.artist(params),
            "playlist.get" => self.catalog.playlist(params),
            "track.get" => self.catalog.track(params),
            "track.quality" => self.catalog.track_quality(params, &self.account.quality()),
            "track.lyrics" => track_lyrics(&self.catalog, params),
            "track.resolve" => self.resolve(params),
            "page.action" | "favorites.set" => {
                let mut params = params.clone();
                if method == "favorites.set" {
                    params["action"] = json!("favorite");
                }
                let result = self.playlists.action(&params);
                if str_at(&params, "action") != "playlist.search" {
                    self.cache.clear();
                }
                result
            }
            _ => Err(Failure::new("Unknown plugin operation.").code(-32601)),
        }
    }

    fn resolve(&self, params: &Value) -> Result<Value> {
        let mut result = self.streams.resolve(params)?;
        let reference = str_at(params, "ref");
        let base = reference.split('@').next().unwrap_or("");
        let key = base.split_once(':').map(|(_, key)| key).unwrap_or("");
        let raw = self.catalog.get(&format!("tracks/{key}"), &json!({}))?;
        let track = card(&raw, Some("track"), "");
        if result["normalization"].is_null() {
            result["normalization"] = normalization(&json!({}), &raw);
        }
        let mut reporting = result.as_object_mut().and_then(|fields| fields.shift_remove("reporting")).unwrap_or_else(|| json!({}));
        for context in reference.split('@').skip(1) {
            if let Some(playlist) = context.strip_prefix("playlist:").filter(|playlist| !playlist.is_empty()) {
                reporting["source_type"] = json!("PLAYLIST");
                reporting["source_id"] = json!(playlist);
                break;
            }
        }
        result["resolution_id"] = json!(self.reports.ticket(track, reporting)?);
        Ok(result)
    }

    pub fn close(&self) {
        self.reports.close();
        self.streams.close();
    }
}
