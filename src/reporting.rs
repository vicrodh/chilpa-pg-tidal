//! I retain measured listening and acknowledge it before attempting network
//! delivery, then send playback_session events using the published TIDAL SDK
//! contract.

use crate::client::{now, Body, Client, DEVICE_CLIENT_ID, PKCE_CLIENT_ID};
use crate::json::{at, dumps_spaced, list_at, text, truthy};
use crate::state::{save, Failure, Result};
use base64::Engine;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

pub const ENDPOINT: &str = "https://ec.tidal.com/api/event-batch";
const RECOVERY: &str = "The listening journal needs recovery.";

pub fn token_urlsafe(bytes: usize) -> String {
    let random: Vec<u8> = (0..bytes).map(|_| rand::random::<u8>()).collect();
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(random)
}

pub fn event_body(entry: &Value) -> Value {
    let (play, event_id) = (at(entry, "play"), at(entry, "id"));
    let spans = list_at(play, "spans");
    let reference = text(at(at(play, "track"), "ref"));
    let base = reference.split('@').next().unwrap_or("");
    let track = base.split_once(':').map(|(_, key)| key).unwrap_or("").to_string();
    let extra = at(play, "extra");
    let (mut source_type, mut source_id) = (at(extra, "source_type").clone(), at(extra, "source_id").clone());
    // I attribute standalone plays to the track; an empty origin can be
    // acknowledged by ingestion without appearing in the account history.
    let known = ["ALBUM", "PLAYLIST", "ARTIST", "MIX", "MY_ITEMS"];
    if !source_type.as_str().is_some_and(|kind| known.contains(&kind)) || !truthy(&source_id) {
        source_type = json!("ITEM");
        source_id = json!(track);
    }
    let seconds = |span: &Value, field: &str| at(span, field).as_f64().unwrap_or(0.0) / 1000.0;
    let mut actions = Vec::new();
    for span in spans {
        actions.push(json!({"actionType": "PLAYBACK_START", "assetPosition": seconds(span, "start"), "timestamp": at(span, "at")}));
        actions.push(json!({"actionType": "PLAYBACK_STOP", "assetPosition": seconds(span, "end"), "timestamp": at(span, "until")}));
    }
    let (first, last) = (spans.first().cloned().unwrap_or(Value::Null), spans.last().cloned().unwrap_or(Value::Null));
    let audio_mode = match extra.get("audio_mode") { Some(mode) => mode.clone(), None => json!("STEREO") };
    json!({"group": "play_log", "name": "playback_session", "version": 2, "uuid": event_id,
        "ts": at(&last, "until"), "payload": {"actions": actions, "actualAssetPresentation": "FULL",
        "actualAudioMode": audio_mode, "actualProductId": track,
        "actualQuality": at(extra, "quality"), "endAssetPosition": seconds(&last, "end"),
        "endTimestamp": at(&last, "until"), "isPostPaywall": true, "playbackSessionId": event_id,
        "productType": "TRACK", "requestedProductId": track, "sourceId": source_id,
        "sourceType": source_type, "startAssetPosition": seconds(&first, "start"),
        "startTimestamp": at(&first, "at")}})
}

enum Delivered {
    Yes,
    Discard,
}

struct Journal {
    state: Value,
    error: Option<String>,
    blocked: bool,
}

pub struct Reports {
    path: PathBuf,
    client: Arc<Client>,
    journal: Mutex<Journal>,
    delivery: Mutex<()>,
    stop: Arc<(Mutex<bool>, Condvar)>,
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
}

fn is_int(value: &Value) -> bool {
    value.is_i64() || value.is_u64()
}

fn empty() -> Value {
    json!({"tickets": {}, "plays": {}, "history": [], "outbox": []})
}

impl Reports {
    pub fn new(data: &std::path::Path, client: Arc<Client>) -> Reports {
        let path = data.join("history.json");
        let (mut state, mut error, mut blocked) = (json!({}), None, false);
        match std::fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice::<Value>(&bytes) {
                Ok(value) => state = value,
                Err(_) => (error, blocked) = (Some(RECOVERY.to_string()), true),
            },
            Err(found) if found.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => (error, blocked) = (Some(RECOVERY.to_string()), true),
        }
        let shapes_ok = state.is_object() && [("tickets", true), ("plays", true), ("history", false), ("outbox", false)].iter()
            .all(|(name, object)| match state.get(*name) { None => true, Some(found) => if *object { found.is_object() } else { found.is_array() } });
        if !shapes_ok {
            (state, error, blocked) = (json!({}), Some(RECOVERY.to_string()), true);
        }
        for (name, default) in [("tickets", json!({})), ("plays", json!({})), ("history", json!([])), ("outbox", json!([]))] {
            if state.get(name).is_none() {
                state[name] = default;
            }
        }
        Reports { path, client, journal: Mutex::new(Journal { state, error, blocked }), delivery: Mutex::new(()),
            stop: Arc::new((Mutex::new(false), Condvar::new())), thread: Mutex::new(None) }
    }

    fn journal(&self) -> std::sync::MutexGuard<'_, Journal> {
        self.journal.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn write(&self, state: &Value) -> Result<()> {
        save(&self.path, state).map_err(|_| Failure::new("The listening journal could not be saved."))
    }

    pub fn start(self: &Arc<Self>) {
        let this = self.clone();
        let handle = std::thread::Builder::new().name("reports".into()).spawn(move || loop {
            {
                let (stopped, signal) = &*this.stop;
                let guard = stopped.lock().unwrap();
                let (guard, _) = signal.wait_timeout(guard, Duration::from_secs(2)).unwrap();
                if *guard {
                    return;
                }
            }
            this.flush();
        });
        *self.thread.lock().unwrap() = handle.ok();
    }

    pub fn close(&self) {
        *self.stop.0.lock().unwrap() = true;
        self.stop.1.notify_all();
        if let Some(handle) = self.thread.lock().unwrap().take() {
            let _ = handle.join();
        }
    }

    pub fn clear(&self) {
        let _delivery = self.delivery.lock().unwrap();
        let mut journal = self.journal();
        journal.state = empty();
        if self.path.exists() {
            let _ = save(&self.path, &journal.state);
        }
        journal.error = None;
        journal.blocked = false;
    }

    pub fn history(&self, limit: usize) -> Vec<Value> {
        list_at(&self.journal().state, "history").iter().take(limit).cloned().collect()
    }

    pub fn ticket(&self, track: Value, extra: Value) -> Result<String> {
        let mut journal = self.journal();
        if journal.blocked {
            return Err(Failure::new(RECOVERY));
        }
        let key = token_urlsafe(24);
        let mut state = journal.state.clone();
        let tickets = state["tickets"].as_object_mut().unwrap();
        tickets.insert(key.clone(), json!({"user": self.client.user(), "track": track, "extra": if extra.is_object() { extra } else { json!({}) }}));
        while tickets.len() > 512 {
            let Some(oldest) = tickets.keys().next().cloned() else { break };
            tickets.shift_remove(&oldest);
        }
        self.write(&state)?;
        journal.state = state;
        Ok(key)
    }

    pub fn event(&self, params: &Value) -> Result<Value> {
        let mut journal = self.journal();
        if journal.blocked {
            return Err(Failure::new(RECOVERY));
        }
        let key = at(params, "playback_id").as_str().unwrap_or("");
        let kind = at(params, "kind").as_str().unwrap_or("");
        if key.is_empty() || key.len() > 128 || !["started", "progress", "ended"].contains(&kind) {
            return Err(Failure::invalid("Invalid playback event."));
        }
        let mut state = journal.state.clone();
        let user = self.client.user();
        if user.is_empty() {
            return Ok(json!({"accepted": false, "discard": true}));
        }
        if kind == "started" {
            let resolution = text(at(params, "resolution_id"));
            let ticket = state["tickets"].get(&resolution).cloned();
            let Some(ticket) = ticket.filter(|ticket| text(at(ticket, "user")) == user) else {
                return Ok(json!({"accepted": false, "discard": true}));
            };
            if let Some(play) = state["plays"].get(key) {
                let same = text(at(play, "user")) == user;
                return Ok(json!({"accepted": same, "discard": !same}));
            }
            let started = at(params, "started_at").as_f64().unwrap_or_else(now);
            let mut play = ticket.clone();
            for (field, value) in [("sequence", json!(0)), ("played_ms", json!(0)), ("ended", json!(false)), ("spans", json!([])),
                                   ("position", at(params, "position_ms").clone()), ("timestamp", json!((started * 1000.0) as i64))] {
                play[field] = value;
            }
            let plays = state["plays"].as_object_mut().unwrap();
            plays.insert(key.to_string(), play);
            let reference = at(at(&ticket, "track"), "ref").clone();
            let mut history = vec![at(&ticket, "track").clone()];
            history.extend(list_at(&state, "history").iter().filter(|track| at(track, "ref") != &reference).take(99).cloned());
            state["history"] = json!(history);
            let plays = state["plays"].as_object_mut().unwrap();
            let surplus = plays.len().saturating_sub(512);
            let old: Vec<String> = plays.iter().take(surplus).filter(|(_, play)| play["ended"] == json!(true)).map(|(key, _)| key.clone()).collect();
            for key in old {
                plays.shift_remove(&key);
            }
            if plays.len() > 1024 {
                return Err(Failure::new("The listening journal is full."));
            }
        } else {
            let Some(play) = state["plays"].get(key).cloned().filter(|play| text(at(play, "user")) == user) else {
                return Ok(json!({"accepted": false, "discard": true}));
            };
            let (sequence, played) = (at(params, "sequence"), at(params, "played_ms"));
            if !is_int(sequence) || sequence.as_i64().unwrap_or(0) <= 0 || !is_int(played) || played.as_i64().unwrap_or(-1) < 0 {
                return Err(Failure::invalid("Invalid playback checkpoint."));
            }
            let (sequence, played) = (sequence.as_i64().unwrap(), played.as_i64().unwrap());
            if play["ended"] == json!(true) || sequence <= play["sequence"].as_i64().unwrap_or(0) {
                return Ok(json!({"accepted": true}));
            }
            if played < play["played_ms"].as_i64().unwrap_or(0) {
                return Err(Failure::invalid("Playback time cannot decrease."));
            }
            let mut play = play;
            checkpoint(&mut play, params, played)?;
            play["sequence"] = json!(sequence);
            play["played_ms"] = json!(played);
            play["ended"] = json!(kind == "ended");
            // Older hosts have no media cursor. I keep their local history,
            // but never invent positions or backfill historical listens.
            if kind == "ended" && truthy(&play["spans"]) {
                if list_at(&state, "outbox").len() >= 1024 {
                    return Err(Failure::new("The listening delivery queue is full."));
                }
                let entry = json!({"id": uuid::Uuid::new_v4().to_string(), "user": user, "play": play.clone(),
                    "reason": at(params, "reason"), "retry_at": 0, "attempts": 0});
                state["outbox"].as_array_mut().unwrap().push(entry);
            }
            state["plays"][key] = play;
        }
        self.write(&state)?;
        journal.state = state;
        Ok(json!({"accepted": true}))
    }

    pub fn flush(&self) {
        let Ok(_delivery) = self.delivery.try_lock() else { return };
        let entry = {
            let journal = self.journal();
            if journal.blocked {
                return;
            }
            let Some(entry) = list_at(&journal.state, "outbox").first().cloned() else { return };
            if at(&entry, "retry_at").as_f64().unwrap_or(0.0) > now() {
                return;
            }
            entry
        };
        let delivered = if text(at(&entry, "user")) == self.client.user() { self.send(&entry) } else { Ok(Delivered::Discard) };
        let mut journal = self.journal();
        match delivered {
            Err(error) => {
                let data = error.data.clone().unwrap_or(Value::Null);
                let mut wait = at(&data, "retry_after_ms").as_f64().unwrap_or(0.0) / 1000.0;
                if wait == 0.0 {
                    wait = at(&data, "retry_after").as_f64().unwrap_or(0.0);
                }
                journal.error = Some("Listening delivery failed; the event remains queued.".into());
                let mut entry = entry;
                let attempts = entry["attempts"].as_i64().unwrap_or(0) + 1;
                entry["attempts"] = json!(attempts);
                let backoff = (5.0 * 2f64.powi(attempts.min(6) as i32)).min(300.0);
                entry["retry_at"] = json!(now() + wait.min(3600.0).max(backoff));
                let mut state = journal.state.clone();
                state["outbox"][0] = entry;
                if self.write(&state).is_ok() {
                    journal.state = state;
                } else {
                    journal.error = Some("The listening journal could not be saved.".into());
                }
            }
            Ok(delivered) => {
                let mut state = journal.state.clone();
                state["outbox"].as_array_mut().unwrap().remove(0);
                let counter = if matches!(delivered, Delivered::Discard) { "discarded" } else { "delivered" };
                state[counter] = json!(state[counter].as_i64().unwrap_or(0) + 1);
                if self.write(&state).is_ok() {
                    journal.state = state;
                    journal.error = None;
                } else {
                    journal.error = Some("The listening journal could not be saved.".into());
                }
            }
        }
    }

    fn send(&self, entry: &Value) -> Result<Delivered> {
        // I snapshot the account token before sending; network latency must not
        // hold the account lock used by catalog and playback requests.
        if self.client.user() != text(at(entry, "user")) {
            return Err(Failure::new("The listening event belongs to another account."));
        }
        let (token, _) = self.client.token()?;
        let system = match std::env::consts::OS { "linux" => "Linux", "macos" => "Darwin", "windows" => "Windows", other => other };
        let client_id = if self.client.device_session() { DEVICE_CLIENT_ID } else { PKCE_CLIENT_ID };
        let headers = json!({"app-name": "Chilpa", "app-version": "0.1.11", "browser-name": "", "browser-version": "",
            "client-id": client_id, "consent-category": "NECESSARY", "os-name": system,
            "requested-sent-timestamp": (now() * 1000.0) as i64, "authorization": token});
        let id = text(at(entry, "id"));
        let key = "SendMessageBatchRequestEntry.1";
        let form = vec![(format!("{key}.Id"), id.clone()),
            (format!("{key}.MessageBody"), serde_json::to_string(&event_body(entry)).unwrap_or_default()),
            (format!("{key}.MessageAttribute.1.Name"), "Name".into()),
            (format!("{key}.MessageAttribute.1.Value.DataType"), "String".into()),
            (format!("{key}.MessageAttribute.1.Value.StringValue"), "playback_session".into()),
            (format!("{key}.MessageAttribute.2.Name"), "Headers".into()),
            (format!("{key}.MessageAttribute.2.Value.DataType"), "String".into()),
            (format!("{key}.MessageAttribute.2.Value.StringValue"), dumps_spaced(&headers))];
        let reply = Client::checked(self.client.send("POST", ENDPOINT, &[], &[("Authorization", format!("Bearer {token}"))], Body::Form(form), None)?)?;
        if reply.status != 200 || reply.body.len() > 65536 {
            return Err(Failure::new("The listening endpoint returned an unexpected response."));
        }
        let body = String::from_utf8_lossy(&reply.body);
        let document = roxmltree::Document::parse(&body).map_err(|_| Failure::new("The listening acknowledgement could not be read."))?;
        // HTTP 200 may contain per-entry errors. I remove only this event's ACK.
        let acknowledged = document.descendants().filter(|node| node.tag_name().name() == "SendMessageBatchResultEntry")
            .any(|node| node.children().any(|child| child.tag_name().name() == "Id" && child.text().unwrap_or("") == id));
        if acknowledged { Ok(Delivered::Yes) } else { Err(Failure::new("The listening event was not acknowledged.")) }
    }

    pub fn status(&self) -> Value {
        let journal = self.journal();
        json!({"upstream_supported": true, "pending": list_at(&journal.state, "outbox").len(), "local_history": true,
            "delivered": journal.state.get("delivered").cloned().unwrap_or(json!(0)), "last_error": journal.error,
            "requires_playback_positions": true})
    }
}

/// A checkpoint carries consumed time, never the seek distance or elapsed wall
/// time while paused. I retain only measured intervals.
fn checkpoint(play: &mut Value, params: &Value, played: i64) -> Result<()> {
    let (position, stamp) = (at(params, "position_ms"), at(params, "observed_at_ms"));
    let delta = played - play["played_ms"].as_i64().unwrap_or(0);
    if !is_int(position) || position.as_i64().unwrap_or(-1) < 0 || !is_int(stamp) || stamp.as_i64().unwrap_or(0) <= 0 {
        return Ok(());
    }
    let (position, stamp) = (position.as_i64().unwrap(), stamp.as_i64().unwrap());
    if !play["spans"].is_array() {
        play["spans"] = json!([]);
    }
    let spans = play["spans"].as_array_mut().unwrap();
    if delta > 0 {
        let length = delta.min(position);
        if length != 0 {
            let span = json!({"start": position - length, "end": position, "at": stamp - length, "until": stamp});
            let joins = spans.last().is_some_and(|last| last["end"] == span["start"]
                && (last["until"].as_i64().unwrap_or(0) - span["at"].as_i64().unwrap_or(0)).abs() <= 250);
            if joins {
                let last = spans.last_mut().unwrap();
                let until = last["at"].as_i64().unwrap_or(0) + last["end"].as_i64().unwrap_or(0) - last["start"].as_i64().unwrap_or(0) + length;
                last["end"] = json!(position);
                last["until"] = json!(until);
            } else {
                spans.push(span);
            }
            if spans.len() > 96 {
                return Err(Failure::new("Too many listening intervals in one playback."));
            }
        }
    }
    play["position"] = json!(position);
    play["timestamp"] = json!(stamp);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contiguous_checkpoints_extend_one_interval() {
        let mut play = json!({"played_ms": 0, "spans": []});
        checkpoint(&mut play, &json!({"position_ms": 1000, "observed_at_ms": 5000}), 1000).unwrap();
        play["played_ms"] = json!(1000);
        checkpoint(&mut play, &json!({"position_ms": 2000, "observed_at_ms": 6000}), 2000).unwrap();
        assert_eq!(play["spans"], json!([{"start": 0, "end": 2000, "at": 4000, "until": 6000}]));
    }

    #[test]
    fn a_standalone_play_is_attributed_to_its_track() {
        let entry = json!({"id": "e", "play": {"track": {"ref": "track:9@playlist:x"}, "extra": {}, "spans": [{"start": 0, "end": 1500, "at": 10, "until": 1510}]}});
        let body = event_body(&entry);
        assert_eq!(body["payload"]["sourceType"], "ITEM");
        assert_eq!(body["payload"]["sourceId"], "9");
        assert_eq!(body["payload"]["endAssetPosition"], 1.5);
    }
}
