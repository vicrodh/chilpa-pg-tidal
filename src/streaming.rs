//! Ranged temporary playback for clear DASH audio, without an offline library.
//!
//! I decode independent FLAC fragments to their original integer PCM samples
//! and serve a virtual WAV. Its exact sample count makes any seek a direct
//! segment read. No sample-rate conversion, gain, lossy encoding or persistent
//! music file is involved. The host's usual preloader, decoder and casting
//! proxy consume it.

use crate::account::Account;
use crate::client::Client;
use crate::flac;
use crate::items::reference;
use crate::json::{at, str_at, text};
use crate::reporting::token_urlsafe;
use crate::state::{Failure, Result};
use base64::Engine;
use regex::Regex;
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Condvar, LazyLock, Mutex};
use std::time::{Duration, Instant};

const CACHE_BYTES: usize = 64 * 1024 * 1024;
const WINDOW: u64 = 512 * 1024;

pub fn stream_url(address: &str) -> Result<String> {
    let unsupported = || Failure::new("Tidal returned an unsupported streaming address.");
    let parsed = url::Url::parse(address).map_err(|_| unsupported())?;
    let host = parsed.host_str().unwrap_or("");
    let trusted = ["tidal.com", "tidalhifi.com", "akamaized.net", "cloudfront.net"].iter()
        .any(|suffix| host == *suffix || host.ends_with(&format!(".{suffix}")));
    if parsed.scheme() != "https" || !parsed.username().is_empty() || parsed.password().is_some() || !trusted {
        return Err(unsupported());
    }
    Ok(address.to_string())
}

/// A segment body. Never sends account credentials to a CDN or follows an
/// unvalidated redirect.
pub fn fetch(http: &reqwest::blocking::Client, address: &str) -> Result<Vec<u8>> {
    let mut address = address.to_string();
    for _ in 0..5 {
        let response = http.get(crate::client::endpoint(&stream_url(&address)?)).timeout(Duration::from_secs(60)).send()
            .map_err(|_| Failure::new("The audio segment could not be read."))?;
        let status = response.status().as_u16();
        if (300..400).contains(&status) {
            let location = response.headers().get("location").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
            address = url::Url::parse(&address).and_then(|base| base.join(&location)).map(|joined| joined.to_string())
                .map_err(|_| Failure::new("Tidal returned an unsupported streaming address."))?;
            continue;
        }
        if status != 200 || response.content_length().is_some_and(|length| length > 32 * 1024 * 1024) {
            return Err(Failure::new("The audio segment is unavailable."));
        }
        let body = response.bytes().map_err(|_| Failure::new("The audio segment could not be read."))?;
        if body.len() > 32 * 1024 * 1024 {
            return Err(Failure::new("The audio segment is unavailable."));
        }
        return Ok(body.to_vec());
    }
    Err(Failure::new("Too many streaming redirects."))
}

fn number(value: &Value) -> Option<f64> {
    match value {
        Value::Number(number) => number.as_f64().filter(|n| n.is_finite()),
        _ => None,
    }
}

pub fn normalization(raw: &Value, track: &Value) -> Value {
    let gain = raw.get("trackReplayGain").unwrap_or_else(|| at(track, "replayGain"));
    let peak = raw.get("trackPeakAmplitude").unwrap_or_else(|| at(track, "peak"));
    let Some(_) = number(gain) else { return Value::Null };
    let mut result = json!({"gain_db": gain, "source": "provider", "scope": "track"});
    if number(peak).is_some_and(|peak| peak > 0.0) {
        result["peak"] = peak.clone();
    }
    result
}

static TEMPLATE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\$(Number|Time)(?:%0(\d+)d)?\$").unwrap());

pub fn template_url(template: &str, number: u64, timestamp: u64, representation: &str, bandwidth: &str) -> String {
    let template = template.replace("$$", "\u{0}").replace("$RepresentationID$", representation).replace("$Bandwidth$", bandwidth);
    let replaced = TEMPLATE.replace_all(&template, |found: &regex::Captures| {
        let value = if &found[1] == "Number" { number } else { timestamp };
        let width: usize = found.get(2).and_then(|w| w.as_str().parse().ok()).unwrap_or(0);
        format!("{value:0width$}")
    });
    replaced.replace('\u{0}', "$")
}

fn manifest(raw: &Value) -> Result<Vec<u8>> {
    let data = base64::engine::general_purpose::STANDARD.decode(str_at(raw, "manifest"))
        .map_err(|_| Failure::new("Tidal returned an unsupported audio manifest."))?;
    if data.len() > 2 * 1024 * 1024 {
        return Err(Failure::new("The audio manifest is too large."));
    }
    Ok(data)
}

struct Representation {
    rate: u32,
    init_url: String,
    segments: Vec<(String, u64)>,
}

fn local<'a>(node: roxmltree::Node<'a, 'a>, name: &str) -> Option<roxmltree::Node<'a, 'a>> {
    node.children().find(|child| child.is_element() && child.tag_name().name() == name)
}

fn child_text<'a>(node: roxmltree::Node<'a, 'a>, name: &str) -> Option<&'a str> {
    local(node, name).and_then(|child| child.text()).filter(|value| !value.is_empty())
}

/// The audio representation of a DASH manifest, with its finite timeline.
fn dash(xml: &[u8], declared_rate: Option<u32>, bits: Option<i64>, require_timeline: bool) -> Result<Representation> {
    let text = std::str::from_utf8(xml).map_err(|_| Failure::new("Unsupported segmented audio manifest."))?;
    let document = roxmltree::Document::parse(text).map_err(|_| Failure::new("Unsupported segmented audio manifest."))?;
    let root = document.root_element();
    if root.descendants().any(|node| node.tag_name().name() == "ContentProtection") {
        return Err(Failure::new("This stream requires DRM. Choose a different available quality."));
    }
    let adaptation = root.descendants().filter(|node| node.tag_name().name() == "AdaptationSet")
        .find(|node| node.attribute("contentType") == Some("audio") || node.attribute("mimeType").unwrap_or("").starts_with("audio/"))
        .ok_or_else(|| Failure::new("No audio representation was returned."))?;
    let representation = local(adaptation, "Representation");
    let codecs = representation.and_then(|r| r.attribute("codecs")).or(adaptation.attribute("codecs")).unwrap_or("");
    let Some(representation) = representation.filter(|_| codecs.to_lowercase().contains("flac")) else {
        return Err(Failure::new(if require_timeline { "This segmented codec is unavailable. Choose FLAC quality." } else { "No supported audio representation was returned." }));
    };
    let rate_text = representation.attribute("audioSamplingRate").or(adaptation.attribute("audioSamplingRate"))
        .map(str::to_string).unwrap_or_else(|| declared_rate.unwrap_or(0).to_string());
    let rate: u32 = rate_text.trim().parse().map_err(|_| Failure::new("Invalid audio resolution."))?;
    if !matches!(bits, Some(16 | 24 | 32)) || !(8000..=768000).contains(&rate) {
        return Err(Failure::new("Invalid audio resolution."));
    }
    if !require_timeline {
        return Ok(Representation { rate, init_url: String::new(), segments: Vec::new() });
    }
    let unsupported = || Failure::new("Unsupported segmented audio manifest.");
    let segment = local(representation, "SegmentTemplate").or_else(|| local(adaptation, "SegmentTemplate")).ok_or_else(unsupported)?;
    let mut base: Option<url::Url> = None;
    let period = local(root, "Period");
    for node in [Some(root), period, Some(adaptation), Some(representation)].into_iter().flatten() {
        if let Some(found) = child_text(node, "BaseURL") {
            base = Some(match &base {
                Some(current) => current.join(found).map_err(|_| unsupported())?,
                None => url::Url::parse(found).map_err(|_| unsupported())?,
            });
        }
    }
    let join = |relative: &str| -> Result<String> {
        let joined = match &base {
            Some(base) => base.join(relative).map(|u| u.to_string()),
            None => url::Url::parse(relative).map(|u| u.to_string()),
        };
        stream_url(&joined.map_err(|_| Failure::new("Tidal returned an unsupported streaming address."))?)
    };
    let (rep_id, bandwidth) = (representation.attribute("id").unwrap_or(""), representation.attribute("bandwidth").unwrap_or(""));
    let init_url = join(&template_url(segment.attribute("initialization").unwrap_or(""), 0, 0, rep_id, bandwidth))?;
    let timescale: u64 = segment.attribute("timescale").unwrap_or("1").parse().map_err(|_| unsupported())?;
    let mut number: u64 = segment.attribute("startNumber").unwrap_or("1").parse().map_err(|_| unsupported())?;
    let media = segment.attribute("media").unwrap_or("");
    let timeline = local(segment, "SegmentTimeline");
    let (Some(timeline), true) = (timeline, timescale != 0) else { return Err(Failure::new("The stream has no finite audio timeline.")) };
    let mut segments = Vec::new();
    let mut timestamp: u64 = 0;
    for entry in timeline.children().filter(|node| node.is_element()) {
        if let Some(start) = entry.attribute("t") {
            timestamp = start.parse().map_err(|_| Failure::new("Unsupported audio timeline."))?;
        }
        let duration: i64 = entry.attribute("d").unwrap_or("0").parse().map_err(|_| Failure::new("Unsupported audio timeline."))?;
        let repeat: i64 = entry.attribute("r").unwrap_or("0").parse().map_err(|_| Failure::new("Unsupported audio timeline."))?;
        if duration <= 0 || !(0..=10000).contains(&repeat) {
            return Err(Failure::new("Unsupported audio timeline."));
        }
        for _ in 0..=repeat {
            let scaled = duration as u64 * u64::from(rate);
            if scaled % timescale != 0 || segments.len() >= 20000 {
                return Err(Failure::new("The stream has an invalid sample timeline."));
            }
            segments.push((join(&template_url(media, number, timestamp, rep_id, bandwidth))?, scaled / timescale));
            number += 1;
            timestamp += duration as u64;
        }
    }
    if segments.is_empty() {
        return Err(Failure::new("The audio timeline is empty."));
    }
    Ok(Representation { rate, init_url, segments })
}

/// Inspect delivery metadata without opening a stream or decoding audio.
pub fn stream_format(raw: &Value) -> Result<Value> {
    if at(raw, "assetPresentation").as_str().unwrap_or("FULL") != "FULL" {
        return Err(Failure::new("This account only has a preview of this track."));
    }
    let data = manifest(raw)?;
    let bits = at(raw, "bitDepth").clone();
    let mut rate = at(raw, "sampleRate").clone();
    let mime = str_at(raw, "manifestMimeType");
    let codec = if mime.contains("dash+xml") {
        let found = dash(&data, rate.as_u64().map(|r| r as u32), bits.as_i64(), false)?;
        rate = json!(found.rate);
        "wav".to_string()
    } else if mime.contains("bts") || mime.contains("json") {
        let value: Value = serde_json::from_slice(&data).map_err(|_| Failure::new("Tidal returned an unsupported audio manifest."))?;
        if !matches!(at(&value, "encryptionType"), Value::Null) && at(&value, "encryptionType") != &json!("NONE") {
            return Err(Failure::new("This stream requires DRM. Choose a different available quality."));
        }
        let codec = text(at(&value, "codecs")).to_lowercase().split('.').next().unwrap_or("").to_string();
        if codec == "mp4a" { "m4a".to_string() } else { codec }
    } else {
        return Err(Failure::new("Tidal returned an unsupported audio manifest."));
    };
    Ok(json!({"codec": codec, "bits": bits, "rate": rate}))
}

type Flight = Arc<(Mutex<Option<Result<Arc<Vec<u8>>>>>, Condvar)>;

struct Segments {
    cache: HashMap<usize, Arc<Vec<u8>>>,
    order: VecDeque<usize>,
    flights: HashMap<usize, Flight>,
}

pub struct Dash {
    http: reqwest::blocking::Client,
    pub rate: u32,
    pub bits: u32,
    channels: usize,
    init: Vec<u8>,
    segments: Vec<(String, u64)>,
    starts: Vec<u64>,
    width: u64,
    header: Vec<u8>,
    pub length: u64,
    state: Mutex<Segments>,
}

impl Dash {
    pub fn new(http: reqwest::blocking::Client, xml: &[u8], bits: u32) -> Result<Arc<Dash>> {
        if xml.len() > 2 * 1024 * 1024 {
            return Err(Failure::new("The audio manifest is too large."));
        }
        let found = dash(xml, None, Some(i64::from(bits)), true)?;
        let init = flac::metadata(&fetch(&http, &found.init_url)?)?;
        let (channels, declared_bits, declared_rate) = flac::stream_info(&init).ok_or_else(|| Failure::new("Invalid FLAC initialization metadata."))?;
        if declared_bits != bits || declared_rate != found.rate || !(1..=8).contains(&channels) {
            return Err(Failure::new("The FLAC fragment does not match its sample timeline."));
        }
        let mut starts = Vec::with_capacity(found.segments.len());
        let mut samples = 0u64;
        for (_, frames) in &found.segments {
            starts.push(samples);
            samples += frames;
        }
        let width = (channels as u64) * u64::from(bits / 8);
        let size = samples * width;
        if size + 36 > 0xFFFF_FFFF {
            return Err(Failure::new("This audio stream exceeds the supported WAV size."));
        }
        let mut header = Vec::with_capacity(44);
        header.extend_from_slice(b"RIFF");
        header.extend_from_slice(&((size + 36) as u32).to_le_bytes());
        header.extend_from_slice(b"WAVEfmt ");
        header.extend_from_slice(&16u32.to_le_bytes());
        header.extend_from_slice(&1u16.to_le_bytes());
        header.extend_from_slice(&(channels as u16).to_le_bytes());
        header.extend_from_slice(&found.rate.to_le_bytes());
        header.extend_from_slice(&((u64::from(found.rate) * width) as u32).to_le_bytes());
        header.extend_from_slice(&(width as u16).to_le_bytes());
        header.extend_from_slice(&(bits as u16).to_le_bytes());
        header.extend_from_slice(b"data");
        header.extend_from_slice(&(size as u32).to_le_bytes());
        let asset = Arc::new(Dash { http, rate: found.rate, bits, channels, init, segments: found.segments, starts, width, header,
            length: 44 + size, state: Mutex::new(Segments { cache: HashMap::new(), order: VecDeque::new(), flights: HashMap::new() }) });
        // A stream whose first fragment cannot be read fails here, where the
        // host expects it, not halfway through its first read.
        asset.segment(0)?;
        asset.prefetch(1);
        Ok(asset)
    }

    fn decode(&self, index: usize) -> Result<Vec<u8>> {
        let (address, frames) = &self.segments[index];
        let mut data = self.init.clone();
        data.extend_from_slice(&flac::payload(&fetch(&self.http, address)?)?);
        let (pcm, channels) = flac::decode(data, self.bits, self.rate, *frames)?;
        if channels != self.channels {
            return Err(Failure::new("The stream changed its channel count."));
        }
        Ok(pcm)
    }

    /// One decoded fragment; a fragment already being fetched is waited for,
    /// never fetched twice.
    fn segment(&self, index: usize) -> Result<Arc<Vec<u8>>> {
        let flight = {
            let mut state = self.state.lock().unwrap();
            if let Some(found) = state.cache.get(&index).cloned() {
                state.order.retain(|known| *known != index);
                state.order.push_back(index);
                return Ok(found);
            }
            match state.flights.get(&index) {
                Some(flight) => Err(flight.clone()),
                None => {
                    let flight: Flight = Arc::new((Mutex::new(None), Condvar::new()));
                    state.flights.insert(index, flight.clone());
                    Ok(flight)
                }
            }
        };
        let flight = match flight {
            Err(running) => {
                let (result, done) = &*running;
                let mut result = result.lock().unwrap();
                while result.is_none() {
                    result = done.wait(result).unwrap();
                }
                return result.clone().unwrap();
            }
            Ok(flight) => flight,
        };
        let decoded = self.decode(index).map(Arc::new);
        {
            let mut state = self.state.lock().unwrap();
            state.flights.remove(&index);
            if let Ok(pcm) = &decoded {
                state.cache.insert(index, pcm.clone());
                state.order.retain(|known| *known != index);
                state.order.push_back(index);
                while state.cache.values().map(|pcm| pcm.len()).sum::<usize>() > CACHE_BYTES && state.cache.len() > 1 {
                    let Some(oldest) = state.order.pop_front() else { break };
                    state.cache.remove(&oldest);
                }
            }
        }
        let (result, done) = &*flight;
        *result.lock().unwrap() = Some(decoded.clone());
        done.notify_all();
        decoded
    }

    /// Starts reading a fragment in the background unless it is cached or in flight.
    fn prefetch(self: &Arc<Self>, index: usize) {
        if index >= self.segments.len() {
            return;
        }
        {
            let state = self.state.lock().unwrap();
            if state.cache.contains_key(&index) || state.flights.contains_key(&index) {
                return;
            }
        }
        let this = self.clone();
        let _ = std::thread::Builder::new().name("segment".into()).spawn(move || {
            let _ = this.segment(index);
        });
    }

    pub fn drop_cache(&self) {
        let mut state = self.state.lock().unwrap();
        state.cache.clear();
        state.order.clear();
    }

    pub fn read(self: &Arc<Self>, mut start: u64, end: u64) -> Result<Vec<u8>> {
        let mut output = Vec::with_capacity((end - start) as usize);
        if start < 44 {
            output.extend_from_slice(&self.header[start as usize..end.min(44) as usize]);
            start = 44;
        }
        while start < end {
            let byte = start - 44;
            let frame = byte / self.width;
            let index = self.starts.partition_point(|first| *first <= frame) - 1;
            let pcm = self.segment(index)?;
            // Playing forward reads the next fragment before it is needed.
            self.prefetch(index + 1);
            let offset = (byte - self.starts[index] * self.width) as usize;
            let take = ((end - start) as usize).min(pcm.len().saturating_sub(offset));
            if take == 0 {
                return Err(Failure::new("Invalid audio range."));
            }
            output.extend_from_slice(&pcm[offset..offset + take]);
            start += take as u64;
        }
        Ok(output)
    }
}

struct Assets {
    entries: Vec<(String, Instant, Arc<Dash>)>,
}

pub struct Streams {
    client: Arc<Client>,
    account: Arc<Account>,
    assets: Arc<Mutex<Assets>>,
    port: u16,
    stop: Arc<std::sync::atomic::AtomicBool>,
}

impl Streams {
    pub fn new(client: Arc<Client>, account: Arc<Account>) -> std::io::Result<Streams> {
        let listener = TcpListener::bind(("127.0.0.1", 0))?;
        let port = listener.local_addr()?.port();
        let assets = Arc::new(Mutex::new(Assets { entries: Vec::new() }));
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (served, stopped) = (assets.clone(), stop.clone());
        std::thread::Builder::new().name("stream-server".into()).spawn(move || {
            for connection in listener.incoming() {
                if stopped.load(std::sync::atomic::Ordering::Acquire) {
                    return;
                }
                let Ok(connection) = connection else { continue };
                let assets = served.clone();
                let _ = std::thread::Builder::new().name("stream-client".into()).spawn(move || serve(connection, &assets));
            }
        })?;
        Ok(Streams { client, account, assets, port, stop })
    }

    pub fn clear(&self) {
        self.assets.lock().unwrap().entries.clear();
    }

    pub fn close(&self) {
        self.stop.store(true, std::sync::atomic::Ordering::Release);
        let _ = TcpStream::connect(("127.0.0.1", self.port));
    }

    pub fn resolve(&self, params: &Value) -> Result<Value> {
        let (_, key) = reference(str_at(params, "ref"), Some("track"))?;
        let quality = self.account.quality();
        let raw = self.client.get(&format!("tracks/{key}/playbackinfopostpaywall"),
            &json!({"playbackmode": "STREAM", "audioquality": quality, "assetpresentation": "FULL"}))?;
        if at(&raw, "assetPresentation").as_str().unwrap_or("FULL") != "FULL" {
            return Err(Failure::new("This account only has a preview of this track."));
        }
        let data = manifest(&raw)?;
        let (bits, rate) = (at(&raw, "bitDepth").clone(), at(&raw, "sampleRate").clone());
        let audio_mode = match raw.get("audioMode") { Some(mode) => mode.clone(), None => json!("STEREO") };
        let mut result = json!({"preview": false, "delivery": "direct", "normalization": normalization(&raw, &Value::Null),
            "reporting": {"quality": at(&raw, "audioQuality"), "audio_mode": audio_mode}});
        let mime = str_at(&raw, "manifestMimeType");
        if mime.contains("dash+xml") {
            let bits = bits.as_u64().filter(|bits| *bits > 0).ok_or_else(|| Failure::new("Tidal did not report the stream's bit depth."))?;
            let asset = Dash::new(self.client.http().clone(), &data, bits as u32)?;
            let ticket = token_urlsafe(32);
            {
                let mut assets = self.assets.lock().unwrap();
                assets.entries.push((ticket.clone(), Instant::now(), asset.clone()));
                trim(&mut assets);
            }
            result["url"] = json!(format!("http://127.0.0.1:{}/{ticket}", self.port));
            result["format"] = json!({"codec": "wav", "bits": asset.bits, "rate": asset.rate});
        } else if mime.contains("bts") || mime.contains("json") {
            let value: Value = serde_json::from_slice(&data).map_err(|_| Failure::new("Tidal returned an unsupported audio manifest."))?;
            if !matches!(at(&value, "encryptionType"), Value::Null) && at(&value, "encryptionType") != &json!("NONE") {
                return Err(Failure::new("This stream requires DRM. Choose a different available quality."));
            }
            let address = crate::json::list_at(&value, "urls").first().map(text).filter(|u| !u.is_empty())
                .ok_or_else(|| Failure::new("No streaming address was returned."))?;
            let codec = text(at(&value, "codecs")).to_lowercase().split('.').next().unwrap_or("").to_string();
            result["url"] = json!(stream_url(&address)?);
            result["format"] = json!({"codec": if codec == "mp4a" { "m4a".to_string() } else { codec }, "bits": bits, "rate": rate});
        } else {
            return Err(Failure::new("Tidal returned an unsupported audio manifest."));
        }
        Ok(result)
    }
}

/// A returned URL remains usable across range reads and pauses. I evict
/// decoded buffers, not the resource the player is still addressing.
fn trim(assets: &mut Assets) {
    let now = Instant::now();
    assets.entries.retain(|(_, accessed, _)| now.duration_since(*accessed) <= Duration::from_secs(3600));
    let keep = assets.entries.len().saturating_sub(8);
    for (_, _, asset) in &assets.entries[..keep] {
        asset.drop_cache();
    }
}

fn find_asset(assets: &Mutex<Assets>, ticket: &str) -> Option<Arc<Dash>> {
    let mut assets = assets.lock().unwrap();
    let position = assets.entries.iter().position(|(known, _, _)| known == ticket)?;
    let (key, _, asset) = assets.entries.remove(position);
    assets.entries.push((key, Instant::now(), asset.clone()));
    Some(asset)
}

fn respond_error(stream: &mut TcpStream, status: u16, reason: &str) {
    let _ = write!(stream, "HTTP/1.1 {status} {reason}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
}

fn serve(stream: TcpStream, assets: &Mutex<Assets>) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(300)));
    let Ok(mut writer) = stream.try_clone() else { return };
    let mut reader = BufReader::new(stream);
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            return;
        }
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 3 {
            respond_error(&mut writer, 400, "Bad Request");
            return;
        }
        let (method, path, version) = (parts[0].to_string(), parts[1].to_string(), parts[2].to_string());
        let (mut range, mut close) = (None::<String>, version == "HTTP/1.0");
        loop {
            let mut header = String::new();
            if reader.read_line(&mut header).unwrap_or(0) == 0 {
                return;
            }
            let header = header.trim_end();
            if header.is_empty() {
                break;
            }
            if let Some((name, value)) = header.split_once(':') {
                match name.trim().to_ascii_lowercase().as_str() {
                    "range" => range = Some(value.trim().to_string()),
                    "connection" => close = close || value.trim().eq_ignore_ascii_case("close"),
                    _ => {}
                }
            }
        }
        if method != "GET" && method != "HEAD" {
            respond_error(&mut writer, 501, "Not Implemented");
            return;
        }
        let Some(asset) = find_asset(assets, path.trim_start_matches('/')) else {
            respond_error(&mut writer, 404, "Not Found");
            return;
        };
        let (mut start, mut end) = (0u64, asset.length);
        if let Some(value) = &range {
            let parsed = value.strip_prefix("bytes=").and_then(|spec| spec.split_once('-')).and_then(|(first, last)| {
                let first: u64 = first.parse().ok()?;
                let last = if last.is_empty() { asset.length } else { last.parse::<u64>().ok()?.saturating_add(1).min(asset.length) };
                Some((first, last))
            });
            match parsed {
                Some((first, last)) if first < last => (start, end) = (first, last),
                _ => {
                    respond_error(&mut writer, 416, "Range Not Satisfiable");
                    return;
                }
            }
        }
        let head = method == "HEAD";
        // Fill the first window before committing a successful HTTP response.
        let first_end = end.min(start + WINDOW);
        let first = if head { Ok(Vec::new()) } else { asset.read(start, first_end) };
        let Ok(first) = first else { return };
        let mut response = format!("HTTP/1.1 {}\r\nContent-Type: audio/wav\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\n",
            if range.is_some() { "206 Partial Content" } else { "200 OK" }, end - start);
        if range.is_some() {
            response.push_str(&format!("Content-Range: bytes {start}-{}/{}\r\n", end - 1, asset.length));
        }
        response.push_str("\r\n");
        if writer.write_all(response.as_bytes()).is_err() {
            return;
        }
        if !head {
            if writer.write_all(&first).is_err() {
                return;
            }
            let mut position = first_end;
            while position < end {
                let next = end.min(position + WINDOW);
                let Ok(chunk) = asset.read(position, next) else { return };
                if writer.write_all(&chunk).is_err() {
                    return;
                }
                position = next;
            }
        }
        if close {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn templates_fill_numbers_with_their_width() {
        assert_eq!(template_url("seg-$Number%05d$-$RepresentationID$-$$.mp4", 7, 0, "r1", "9"), "seg-00007-r1-$.mp4");
        assert_eq!(template_url("t/$Time$/$Bandwidth$", 0, 42, "r", "320"), "t/42/320");
    }

    #[test]
    fn only_provider_and_cdn_hosts_are_streamed() {
        assert!(stream_url("https://sp-pr-fa.audio.tidal.com/x").is_ok());
        assert!(stream_url("http://sp-pr-fa.audio.tidal.com/x").is_err());
        assert!(stream_url("https://user:pw@audio.tidal.com/x").is_err());
        assert!(stream_url("https://tidal.com.example.org/x").is_err());
    }

    #[test]
    fn provider_gain_is_kept_with_a_positive_peak() {
        assert_eq!(normalization(&json!({"trackReplayGain": -7.5, "trackPeakAmplitude": 0.9}), &Value::Null),
            json!({"gain_db": -7.5, "source": "provider", "scope": "track", "peak": 0.9}));
        assert_eq!(normalization(&json!({"trackReplayGain": true}), &Value::Null), Value::Null);
        assert_eq!(normalization(&json!({}), &json!({"replayGain": -3})), json!({"gain_db": -3, "source": "provider", "scope": "track"}));
    }

    #[test]
    fn a_dash_timeline_lists_every_segment_with_its_frames() {
        let xml = br#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011"><Period><AdaptationSet contentType="audio" mimeType="audio/mp4">
            <Representation id="FLAC,44100,16" codecs="flac" bandwidth="1000" audioSamplingRate="44100">
            <SegmentTemplate timescale="44100" initialization="https://sp-ad-fa.audio.tidal.com/init.mp4" media="https://sp-ad-fa.audio.tidal.com/$Number$.mp4" startNumber="1">
            <SegmentTimeline><S d="176128" r="1"/><S d="1000"/></SegmentTimeline></SegmentTemplate></Representation></AdaptationSet></Period></MPD>"#;
        let found = dash(xml, None, Some(16), true).unwrap();
        assert_eq!(found.rate, 44100);
        assert_eq!(found.segments.iter().map(|s| s.1).collect::<Vec<_>>(), vec![176128, 176128, 1000]);
        assert_eq!(found.segments[2].0, "https://sp-ad-fa.audio.tidal.com/3.mp4");
        assert!(dash(br#"<MPD><ContentProtection/></MPD>"#, None, Some(16), true).is_err());
    }
}
