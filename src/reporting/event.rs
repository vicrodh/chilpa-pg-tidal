//! Provider event identity and payloads. Authentication remains in Client;
//! decoded claims only attribute an already authenticated playback.

use crate::json::{at, list_at, text};
use crate::state::{Failure, Result};
use base64::Engine;
use serde_json::{json, Value};

// The PKCE sign-in uses the provider's Android application identity. These
// event metadata pins describe that authorized client, not the host machine.
const APP_VERSION: &str = "2.205.0";

fn number(value: &Value) -> Option<u64> {
    value.as_u64().or_else(|| value.as_str()?.parse().ok()).filter(|number| *number > 0)
}

pub(super) fn identity(token: &str, expected_user: &str) -> Result<Value> {
    let missing = || Failure::new("The account token has no usable listening identity. Sign in again.");
    if token.len() > 65536 {
        return Err(missing());
    }
    let segment = token.split('.').nth(1).ok_or_else(missing)?;
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(segment)
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(segment)).map_err(|_| missing())?;
    let claims: Value = serde_json::from_slice(&decoded).map_err(|_| missing())?;
    let uid = number(at(&claims, "uid")).ok_or_else(missing)?;
    let cid = number(at(&claims, "cid")).ok_or_else(missing)?;
    let sid = at(&claims, "sid").as_str().filter(|sid| !sid.is_empty() && sid.len() <= 256).ok_or_else(missing)?;
    if uid.to_string() != expected_user {
        return Err(Failure::new("The listening event belongs to another account."));
    }
    Ok(json!({"user": {"id": uid, "clientId": cid, "sessionId": sid},
        "client": {"token": cid.to_string(), "deviceType": "mobile",
            "version": APP_VERSION, "platform": "android"}}))
}

pub(super) fn headers(client_id: &str, token: &str, sent_at_ms: i64) -> Value {
    json!({"client-id": client_id, "app-version": APP_VERSION, "os-name": "Android",
        "os-version": "35", "device-model": "Pixel 7", "device-vendor": "Google",
        "consent-category": "NECESSARY", "requested-sent-timestamp": sent_at_ms.to_string(), "authorization": token})
}

pub(crate) fn source(reference: &str) -> Option<(&str, &str)> {
    reference.split('@').skip(1).find_map(|context| {
        let (kind, id) = context.split_once(':')?;
        if id.is_empty() || id.len() > 256 || id.contains(['/', '?', '#', '\\']) {
            return None;
        }
        Some((match kind { "album" => "ALBUM", "playlist" => "PLAYLIST", "artist" => "ARTIST",
            "mix" => "MIX", "favorites" if id == "MY_TRACKS" => "MY_ITEMS", _ => return None }, id))
    })
}

pub fn event_body(entry: &Value, identity: &Value) -> Value {
    let (play, event_id) = (at(entry, "play"), at(entry, "id"));
    let spans = list_at(play, "spans");
    let reference = text(at(at(play, "track"), "ref"));
    let track = reference.split('@').next().unwrap_or("").split_once(':').map(|(_, key)| key).unwrap_or("");
    let extra = at(play, "extra");
    let (mut source_type, mut source_id) = (at(extra, "source_type").clone(), at(extra, "source_id").clone());
    let known = ["ALBUM", "PLAYLIST", "ARTIST", "MIX", "MY_ITEMS"];
    if !source_type.as_str().is_some_and(|kind| known.contains(&kind)) || source_id.as_str().is_none_or(str::is_empty) {
        source_type = json!("ITEM");
        source_id = json!(track);
    }
    if source_type == "MY_ITEMS" {
        source_id = json!("MY_TRACKS");
    }
    let seconds = |span: &Value, field: &str| at(span, field).as_f64().unwrap_or(0.0) / 1000.0;
    // Session bounds carry the first start and final stop. Only interruptions
    // belong in actions; never count a seek or a pause as listening time.
    let mut actions = Vec::new();
    for pair in spans.windows(2) {
        actions.push(json!({"actionType": "PLAYBACK_STOP", "assetPosition": seconds(&pair[0], "end"), "timestamp": pair[0]["until"]}));
        actions.push(json!({"actionType": "PLAYBACK_START", "assetPosition": seconds(&pair[1], "start"), "timestamp": pair[1]["at"]}));
    }
    let (first, last) = (spans.first().cloned().unwrap_or(Value::Null), spans.last().cloned().unwrap_or(Value::Null));
    let audio_mode = extra.get("audio_mode").cloned().unwrap_or(json!("STEREO"));
    let actual = extra.get("actual_product_id").filter(|id| id.is_string() || id.is_u64()).map(text).filter(|id| !id.is_empty()).unwrap_or_else(|| track.to_string());
    json!({"group": "play_log", "version": 2, "uuid": event_id, "user": at(identity, "user"), "client": at(identity, "client"),
        "ts": at(&last, "until"), "payload": {"actions": actions, "actualAssetPresentation": "FULL",
        "actualAudioMode": audio_mode, "actualProductId": actual,
        "actualQuality": at(extra, "quality"), "endAssetPosition": seconds(&last, "end"),
        "endTimestamp": at(&last, "until"), "isPostPaywall": true, "playbackSessionId": event_id,
        "productType": "TRACK", "requestedProductId": track, "sourceId": source_id,
        "sourceType": source_type, "startAssetPosition": seconds(&first, "start"), "startTimestamp": at(&first, "at")}})
}

#[cfg(test)]
mod tests {
    use super::*;

    fn token(claims: Value) -> String {
        format!("fixture.{}.signature", base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(claims.to_string()))
    }

    fn entry() -> Value {
        json!({"id": "event-1", "play": {"track": {"ref": "track:9"}, "extra": {"quality": "LOSSLESS"},
            "spans": [{"start": 0, "end": 31000, "at": 100000, "until": 131000}]}})
    }

    #[test]
    fn account_and_client_are_taken_from_numeric_or_string_claims() {
        for claims in [json!({"uid": 5, "cid": 123, "sid": "session"}), json!({"uid": "5", "cid": "123", "sid": "session"})] {
            let identity = identity(&token(claims), "5").unwrap();
            let body = event_body(&entry(), &identity);
            assert_eq!(body["user"], json!({"id": 5, "clientId": 123, "sessionId": "session"}));
            assert_eq!(body["client"]["token"], "123");
            assert_eq!(body["client"]["platform"], "android");
            assert_eq!(body["client"]["deviceType"], "mobile");
            assert!(!body.to_string().contains("signature"));
            assert_eq!(body["payload"]["actions"], json!([]));
            assert_eq!(body["payload"]["endAssetPosition"], 31.0);
        }
    }

    #[test]
    fn transport_headers_use_the_authorized_application_identity_and_string_values() {
        let headers = headers("fixture-client", "fixture-access", 123000);
        assert!(headers.as_object().unwrap().values().all(Value::is_string));
        assert_eq!(headers["app-version"], APP_VERSION);
        assert_eq!(headers["client-id"], "fixture-client");
        assert_eq!(headers["os-name"], "Android");
        assert_eq!(headers["requested-sent-timestamp"], "123000");
        assert_eq!(headers["authorization"], "fixture-access");
    }

    #[test]
    fn unusable_claims_never_fabricate_identity_or_expose_the_token() {
        for access in ["opaque-private-token".to_string(), "a.%%%.private-token".into(), token(json!({"uid": 5, "sid": "s"})),
            token(json!({"uid": 5, "cid": 123, "sid": ""})), token(json!({"uid": 6, "cid": 123, "sid": "s"}))] {
            let failure = identity(&access, "5").unwrap_err();
            assert!(!failure.message.contains(&access));
        }
    }

    #[test]
    fn resumed_intervals_keep_their_real_positions_and_interruptions() {
        let mut entry = entry();
        entry["play"]["extra"]["actual_product_id"] = json!(10);
        entry["play"]["spans"].as_array_mut().unwrap().push(json!({"start": 90000, "end": 100000, "at": 160000, "until": 170000}));
        let payload = event_body(&entry, &json!({}))["payload"].clone();
        assert_eq!(payload["actualProductId"], "10");
        assert_eq!(payload["requestedProductId"], "9");
        assert_eq!(payload["actions"], json!([
            {"actionType": "PLAYBACK_STOP", "assetPosition": 31.0, "timestamp": 131000},
            {"actionType": "PLAYBACK_START", "assetPosition": 90.0, "timestamp": 160000}]));
        assert_eq!(payload["endAssetPosition"], 100.0);
        assert_eq!(payload["endTimestamp"], 170000);
    }

    #[test]
    fn context_preserves_containers_and_favorites_but_rejects_ui_labels() {
        for (context, expected) in [("album:4", Some(("ALBUM", "4"))), ("playlist:p@entry:1", Some(("PLAYLIST", "p"))),
            ("artist:2", Some(("ARTIST", "2"))), ("mix:x", Some(("MIX", "x"))), ("favorites:MY_TRACKS", Some(("MY_ITEMS", "MY_TRACKS"))),
            ("search:query", None), ("album:", None), ("mix:../x", None)] {
            assert_eq!(source(&format!("track:9@{context}")), expected);
        }
    }
}
