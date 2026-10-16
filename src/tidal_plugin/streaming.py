"""Ranged temporary playback for clear DASH audio, without an offline library.

I decode independent FLAC fragments to their original integer PCM samples and
serve a virtual WAV. Its exact sample count makes any seek a direct segment
read. No sample-rate conversion, gain, lossy encoding or persistent music file
is involved. The host's usual preloader, decoder and casting proxy consume it.
"""
import base64
import bisect
import json
import math
import re
import secrets
import struct
import threading
import time
import xml.etree.ElementTree as ET
from collections import OrderedDict
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import urljoin, urlsplit
import requests
from . import flac
from .items import reference
from .state import Failure


def stream_url(url):
    parsed = urlsplit(url)
    host = parsed.hostname or ""
    if parsed.scheme != "https" or parsed.username or parsed.password or not any(
            host == suffix or host.endswith("." + suffix) for suffix in ["tidal.com", "tidalhifi.com", "akamaized.net", "cloudfront.net"]):
        raise Failure("Tidal returned an unsupported streaming address.")
    return url


def fetch(url):
    # Never send account credentials to a CDN or follow an unvalidated redirect.
    for _ in range(5):
        try:
            response = requests.get(stream_url(url), timeout=(8, 25), allow_redirects=False)
        except requests.RequestException:
            raise Failure("The audio segment could not be read.") from None
        if 300 <= response.status_code < 400:
            url = urljoin(url, response.headers.get("Location", ""))
            continue
        if response.status_code != 200 or len(response.content) > 32 * 1024 * 1024:
            raise Failure("The audio segment is unavailable.")
        return response.content
    raise Failure("Too many streaming redirects.")


def normalization(raw, track=None):
    gain = raw.get("trackReplayGain", (track or {}).get("replayGain"))
    peak = raw.get("trackPeakAmplitude", (track or {}).get("peak"))
    if isinstance(gain, bool) or not isinstance(gain, (int, float)) or not math.isfinite(gain):
        return None
    result = {"gain_db": gain, "source": "provider", "scope": "track"}
    if isinstance(peak, (int, float)) and not isinstance(peak, bool) and math.isfinite(peak) and peak > 0:
        result["peak"] = peak
    return result


def template_url(template, number, timestamp, representation, bandwidth):
    template = template.replace("$$", "\x00").replace("$RepresentationID$", representation).replace("$Bandwidth$", bandwidth)
    def substitute(match):
        value = number if match[1] == "Number" else timestamp
        return str(value).zfill(int(match[2] or 0))
    template = re.sub(r"\$(Number|Time)(?:%0(\d+)d)?\$", substitute, template)
    return template.replace("\x00", "$")


def stream_format(raw):
    """Inspect delivery metadata without opening a stream or decoding audio."""
    if raw.get("assetPresentation", "FULL") != "FULL":
        raise Failure("This account only has a preview of this track.")
    data = base64.b64decode(raw.get("manifest", ""), validate=True)
    if len(data) > 2 * 1024 * 1024:
        raise Failure("The audio manifest is too large.")
    bits, rate = raw.get("bitDepth"), raw.get("sampleRate")
    mime = raw.get("manifestMimeType", "")
    if "dash+xml" in mime:
        root = ET.fromstring(data)
        for node in root.iter():
            node.tag = node.tag.rsplit("}",1)[-1]
        if root.find(".//ContentProtection") is not None:
            raise Failure("This stream requires DRM. Choose a different available quality.")
        audio = next((a for a in root.findall(".//AdaptationSet") if a.get("contentType") == "audio" or a.get("mimeType", "").startswith("audio/")), None)
        representation = audio.find("Representation") if audio is not None else None
        if representation is None or "flac" not in representation.get("codecs", audio.get("codecs", "")).lower():
            raise Failure("No supported audio representation was returned.")
        rate = int(representation.get("audioSamplingRate", audio.get("audioSamplingRate", str(rate or 0))))
        if bits not in {16,24,32} or not 8000 <= rate <= 768000:
            raise Failure("Invalid audio resolution.")
        codec = "wav"
    elif "bts" in mime or "json" in mime:
        manifest = json.loads(data)
        if manifest.get("encryptionType", "NONE") not in {None,"NONE"}:
            raise Failure("This stream requires DRM. Choose a different available quality.")
        codec = manifest.get("codecs", "").lower().split(".")[0]
        if codec == "mp4a": codec = "m4a"
    else:
        raise Failure("Tidal returned an unsupported audio manifest.")
    return {"codec":codec,"bits":bits,"rate":rate}


class Dash:
    def __init__(self, xml, bits):
        if len(xml) > 2 * 1024 * 1024:
            raise Failure("The audio manifest is too large.")
        root = ET.fromstring(xml)
        # Namespace-independent parsing, including SegmentTemplate inherited by a representation.
        for node in root.iter():
            node.tag = node.tag.rsplit("}", 1)[-1]
        if root.find(".//ContentProtection") is not None:
            raise Failure("This stream requires DRM. Choose a different available quality.")
        adaptation = next((a for a in root.findall(".//AdaptationSet") if a.get("contentType") == "audio" or a.get("mimeType", "").startswith("audio/")), None)
        if adaptation is None:
            raise Failure("No audio representation was returned.")
        representation = adaptation.find("Representation")
        if representation is None or "flac" not in representation.get("codecs", adaptation.get("codecs", "")).lower():
            raise Failure("This segmented codec is unavailable. Choose FLAC quality.")
        self.rate = int(representation.get("audioSamplingRate", adaptation.get("audioSamplingRate", "0")))
        self.bits = bits
        if bits not in {16, 24, 32} or not 8000 <= self.rate <= 768000:
            raise Failure("Invalid audio resolution.")
        segment = representation.find("SegmentTemplate")
        if segment is None:
            segment = adaptation.find("SegmentTemplate")
        if segment is None:
            raise Failure("Unsupported segmented audio manifest.")
        base = ""
        period = root.find("Period")
        for node in [root, period, adaptation, representation]:
            if node is not None and node.findtext("BaseURL"):
                base = urljoin(base, node.findtext("BaseURL"))
        rep_id, bandwidth = representation.get("id", ""), representation.get("bandwidth", "")
        self.init_url = stream_url(urljoin(base, template_url(segment.get("initialization", ""), 0, 0, rep_id, bandwidth)))
        timescale, number = int(segment.get("timescale", "1")), int(segment.get("startNumber", "1"))
        media = segment.get("media", "")
        timeline = segment.find("SegmentTimeline")
        if timeline is None or not timescale:
            raise Failure("The stream has no finite audio timeline.")
        self.segments, self.starts = [], []
        timestamp, sample = 0, 0
        for entry in timeline:
            timestamp = int(entry.get("t", timestamp))
            duration, repeat = int(entry.get("d", "0")), int(entry.get("r", "0"))
            if duration <= 0 or not 0 <= repeat <= 10000:
                raise Failure("Unsupported audio timeline.")
            for _ in range(repeat + 1):
                frames, remainder = divmod(duration * self.rate, timescale)
                if remainder or len(self.segments) >= 20000:
                    raise Failure("The stream has an invalid sample timeline.")
                url = stream_url(urljoin(base, template_url(media, number, timestamp, rep_id, bandwidth)))
                self.starts.append(sample)
                self.segments.append((url, frames))
                sample += frames
                number += 1
                timestamp += duration
        if not self.segments:
            raise Failure("The audio timeline is empty.")
        self.samples, self.channels = sample, 0
        self.init = flac.metadata(fetch(self.init_url))
        self.lock = threading.RLock()
        self.cache = OrderedDict()
        first = self.decode(0)
        self.cache[0] = first
        self.width = self.channels * (bits // 8)
        size = self.samples * self.width
        if size + 36 > 0xFFFFFFFF:
            raise Failure("This audio stream exceeds the supported WAV size.")
        self.header = struct.pack("<4sI4s4sIHHIIHH4sI", b"RIFF", size+36, b"WAVE", b"fmt ", 16, 1,
            self.channels, self.rate, self.rate*self.width, self.width, bits, b"data", size)
        self.length = 44 + size

    def decode(self, index):
        data = fetch(self.segments[index][0])
        pcm, channels = flac.decode(self.init + flac.payload(data), self.bits, self.rate, self.segments[index][1])
        if self.channels and self.channels != channels:
            raise Failure("The stream changed its channel count.")
        self.channels = channels
        return pcm

    def segment(self, index):
        with self.lock:
            if index not in self.cache:
                self.cache[index] = self.decode(index)
            self.cache.move_to_end(index)
            while sum(map(len, self.cache.values())) > 64 * 1024 * 1024 and len(self.cache) > 1:
                self.cache.popitem(last=False)
            return self.cache[index]

    def read(self, start, end):
        output = bytearray()
        if start < 44:
            output.extend(self.header[start:min(end, 44)])
            start = 44
        while start < end:
            byte = start - 44
            index = bisect.bisect_right(self.starts, byte // self.width) - 1
            pcm = self.segment(index)
            offset = byte - self.starts[index] * self.width
            chunk = pcm[offset:offset + end-start]
            if not chunk:
                raise Failure("Invalid audio range.")
            output.extend(chunk)
            start += len(chunk)
        return bytes(output)


class Streams:
    def __init__(self, client, account):
        flac.library()
        self.client, self.account = client, account
        self.lock = threading.RLock()
        self.assets = OrderedDict()
        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.server.streams = self
        self.server.daemon_threads = True
        threading.Thread(target=self.server.serve_forever, daemon=True).start()

    def clear(self):
        with self.lock:
            self.assets.clear()

    def resolve(self, params):
        _, key = reference(params.get("ref", ""), "track")
        quality = self.account.preferences["quality"]
        raw = self.client.get(f"tracks/{key}/playbackinfopostpaywall", {"playbackmode": "STREAM", "audioquality": quality, "assetpresentation": "FULL"})
        if raw.get("assetPresentation", "FULL") != "FULL":
            raise Failure("This account only has a preview of this track.")
        manifest = base64.b64decode(raw.get("manifest", ""), validate=True)
        bits, rate = raw.get("bitDepth"), raw.get("sampleRate")
        result = {"preview": False, "delivery": "direct", "normalization": normalization(raw),
                  "reporting": {"quality": raw.get("audioQuality"), "audio_mode": raw.get("audioMode", "STEREO")}}
        mime = raw.get("manifestMimeType", "")
        if "dash+xml" in mime:
            if not bits:
                raise Failure("Tidal did not report the stream's bit depth.")
            asset = Dash(manifest, int(bits))
            ticket = secrets.token_urlsafe(32)
            with self.lock:
                self.assets[ticket] = (time.monotonic(), asset)
                self.trim()
            result.update(url=f"http://127.0.0.1:{self.server.server_port}/{ticket}", format={"codec": "wav", "bits": asset.bits, "rate": asset.rate})
        elif "bts" in mime or "json" in mime:
            value = json.loads(manifest)
            if value.get("encryptionType", "NONE") not in {None, "NONE"}:
                raise Failure("This stream requires DRM. Choose a different available quality.")
            urls = value.get("urls", [])
            if not urls:
                raise Failure("No streaming address was returned.")
            codec = value.get("codecs", "").lower().split(".")[0]
            result.update(url=stream_url(urls[0]), format={"codec": "m4a" if codec == "mp4a" else codec, "bits": bits, "rate": rate})
        else:
            raise Failure("Tidal returned an unsupported audio manifest.")
        return result

    def trim(self):
        # A returned URL remains usable across range reads and pauses. I evict
        # decoded buffers, not the resource the player is still addressing.
        now = time.monotonic()
        for ticket, (accessed, asset) in list(self.assets.items()):
            if now - accessed > 3600:
                del self.assets[ticket]
        for _, asset in list(self.assets.values())[:-8]:
            if asset.lock.acquire(blocking=False):
                try:
                    asset.cache.clear()
                finally:
                    asset.lock.release()


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def do_HEAD(self):
        self.deliver(True)

    def do_GET(self):
        self.deliver(False)

    def deliver(self, head):
        with self.server.streams.lock:
            ticket = self.path.lstrip("/")
            found = self.server.streams.assets.get(ticket)
            if found:
                self.server.streams.assets[ticket] = (time.monotonic(), found[1])
                self.server.streams.assets.move_to_end(ticket)
        if not found:
            self.send_error(404)
            return
        _, asset = found
        start, end = 0, asset.length
        value = self.headers.get("Range")
        if value:
            match = re.fullmatch(r"bytes=(\d+)-(\d*)", value)
            if not match:
                self.send_error(416)
                return
            start, end = int(match[1]), min(asset.length, int(match[2])+1 if match[2] else asset.length)
            if not 0 <= start < end:
                self.send_error(416)
                return
        try:
            # Fill the first window before committing a successful HTTP response.
            first_end = min(end, start + 512 * 1024)
            first = b"" if head else asset.read(start, first_end)
            self.send_response(206 if value else 200)
            self.send_header("Content-Type", "audio/wav")
            self.send_header("Content-Length", str(end-start))
            self.send_header("Accept-Ranges", "bytes")
            if value:
                self.send_header("Content-Range", f"bytes {start}-{end-1}/{asset.length}")
            self.end_headers()
            if not head:
                self.wfile.write(first)
                for position in range(first_end, end, 512 * 1024):
                    self.wfile.write(asset.read(position, min(end, position + 512 * 1024)))
        except (Failure, OSError):
            self.close_connection = True
