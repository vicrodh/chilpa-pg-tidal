#!/usr/bin/env python3
"""Send the same JSON-RPC calls to the Python plugin and to the Rust port,
each against its own copy of the fake Tidal, and compare what they answer,
what they request and the audio they serve.

    compare.py --rust BINARY      Python against Rust; exit 1 on any difference.
    compare.py --python-only      Python against Python; proves the run is deterministic.

The Rust binary must be built with the mock-endpoints feature. Settings pages
are not compared: the sign-in wizard was redesigned on purpose.
"""
import argparse
import hashlib
import http.client
import json
import os
import pathlib
import re
import selectors
import shutil
import subprocess
import sys
import tempfile
import time
from collections import Counter
from urllib.parse import quote, urlsplit

HERE = pathlib.Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import mock  # noqa: E402

# An interpreter with the Python plugin's requirements (tidalapi, requests).
PYTHON = os.environ.get("CHILPA_TIDAL_PYTHON", "python3")
STREAM_URL = re.compile(r"^http://127\.0\.0\.1:\d+/[A-Za-z0-9_-]+$")
ORDERED_HOSTS = {"api.tidal.com", "auth.tidal.com"}
BOUNDARY = 44 + mock.SEGMENT_FRAMES[0] * 4

NORMALIZATIONS = [
    "track.resolve url: the local stream address (port and ticket) becomes <stream-url>",
    "resolution_id: random per resolve, becomes <resolution-id>",
    "event-batch form: SendMessageBatchRequestEntry.1.Id becomes <event-id>",
    "event-batch MessageBody: uuid and playbackSessionId become <event-id>",
    "event-batch Headers attribute: requested-sent-timestamp becomes <time>",
    "request logs: CDN and event-batch requests compare as a multiset over the whole run; api/auth requests compare in order per call",
    "WAV responses: compare status, Content-Type, Content-Length, Accept-Ranges, Content-Range, body length and SHA-256 (not Date or Server)",
    "WAV error responses (status 400 and above): compare the status only, not the error page body",
]


class Plugin:
    def __init__(self, command, data, base):
        environment = {"PATH": "/usr/bin:/bin", "HOME": str(data), "CHILPA_TIDAL_MOCK": base, "NO_PROXY": "*", "no_proxy": "*",
                       "LANG": "C.UTF-8", "PYTHONIOENCODING": "utf-8", "TZ": "UTC"}
        if os.environ.get("CHILPA_TIDAL_PYTHON_SOURCE"):
            environment["CHILPA_TIDAL_PYTHON_SOURCE"] = os.environ["CHILPA_TIDAL_PYTHON_SOURCE"]
        self.process = subprocess.Popen(command + ["--data-dir", str(data)], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                        stderr=subprocess.PIPE, env=environment)
        self.selector = selectors.DefaultSelector()
        self.selector.register(self.process.stdout, selectors.EVENT_READ)
        self.serial = 0

    def call(self, method, params=None, timeout=60):
        self.serial += 1
        line = json.dumps({"jsonrpc": "2.0", "id": self.serial, "method": method, "params": params or {}}) + "\n"
        self.process.stdin.write(line.encode())
        self.process.stdin.flush()
        if not self.selector.select(timeout):
            raise RuntimeError(f"{method} got no answer in {timeout} s")
        answer = self.process.stdout.readline()
        if not answer:
            raise RuntimeError(f"the plugin ended during {method}: {self.process.stderr.read(4000).decode(errors='replace')}")
        reply = json.loads(answer)
        assert reply.pop("id") == self.serial and reply.pop("jsonrpc") == "2.0", reply
        return reply

    def close(self):
        try:
            self.process.stdin.close()
            self.process.wait(timeout=40)
        except subprocess.TimeoutExpired:
            self.process.kill()
            self.process.wait()
        return self.process.stderr.read().decode(errors="replace")


def fetch(url, method, range_header=None):
    parsed = urlsplit(url)
    connection = http.client.HTTPConnection(parsed.hostname, parsed.port, timeout=60)
    headers = {"Range": range_header} if range_header else {}
    connection.request(method, parsed.path, headers=headers)
    response = connection.getresponse()
    body = response.read()
    connection.close()
    if response.status >= 400:
        # An error page's body and headers are cosmetic; the status is the contract.
        return {"status": response.status}
    kept = {name: response.getheader(name) for name in ["Content-Type", "Content-Length", "Accept-Ranges", "Content-Range"]}
    return {"status": response.status, "headers": kept, "length": len(body), "sha256": hashlib.sha256(body).hexdigest()}


def wav_checks(url):
    if not isinstance(url, str) or not url.startswith("http://127.0.0.1"):
        return {"error": f"no local stream address: {url!r}"}
    checks = {}
    for label, method, header in [("head", "HEAD", None), ("full", "GET", None), ("first 100 bytes", "GET", "bytes=0-99"),
                                  ("across segments 1 and 2", "GET", f"bytes={BOUNDARY - 50}-{BOUNDARY + 49}"),
                                  ("open-ended tail", "GET", "bytes=100000-"), ("past the end", "GET", "bytes=999999-")]:
        try:
            checks[label] = fetch(url, method, header)
        except Exception as error:  # A refused range may close the connection without an answer.
            checks[label] = {"error": type(error).__name__}
    return checks


def members(reply, positions):
    rows = (reply.get("result") or {}).get("tracks") or []
    return [rows[position]["member"] for position in positions if position < len(rows)]


def scenario(plugin, world):
    """Every call in order, with the api/auth requests it made."""
    records = []

    def step(label, method, params=None, expect_error=False):
        before = len(world.snapshot())
        reply = plugin.call(method, params)
        requests = [entry for entry in world.snapshot()[before:] if entry["host"] in ORDERED_HOSTS]
        records.append({"label": label, "method": method, "reply": reply, "requests": requests, "expect_error": expect_error})
        return reply

    step("initialize", "initialize", {"locale": "en", "playback_events": 1})
    step("inspect", "inspect")
    step("auth.status", "auth.status")
    step("reporting.status before", "reporting.status")
    step("link.resolve album", "link.resolve", {"url": "https://tidal.com/browse/album/10"})
    step("link.resolve mix", "link.resolve", {"url": "https://listen.tidal.com/mix/00abc"})
    step("link.resolve other", "link.resolve", {"url": "https://example.com/album/1"})
    step("home", "home")
    for tab in ["home", "foryou", "editors", "explore", "new", "top", "hires", "mixes", "genres", "moods", "decades",
                "decades:1960s", "home:rock", "explore:jazz", "new:rock,jazz"]:
        step(f"page discover:{tab}", "page.get", {"ref": f"discover:{tab}"})
    step("page discover:unknown", "page.get", {"ref": "discover:unknown"}, True)
    step("page discover:home:metal", "page.get", {"ref": "discover:home:metal"}, True)
    step("page discover:decades:1930s", "page.get", {"ref": "discover:decades:1930s"}, True)
    step("page browse home view-all", "page.get", {"ref": "browse:" + quote("/v2/home/pages/m1/view-all", safe="")})
    step("page browse mood", "page.get", {"ref": "browse:" + quote("pages/chill", safe="")})
    step("page browse staff more", "page.get", {"ref": "browse:" + quote("pages/single-module-page/staff/1", safe="")}, True)
    for kind in ["tracks", "albums", "artists", "playlists"]:
        step(f"page library:{kind}", "page.get", {"ref": f"library:{kind}"})
    step("page album:10", "page.get", {"ref": "album:10"})
    step("page album:11", "page.get", {"ref": "album:11"})
    step("page album:999", "page.get", {"ref": "album:999"})
    step("page artist:20", "page.get", {"ref": "artist:20"})
    step("page artist:21", "page.get", {"ref": "artist:21"})
    step("page artist:20:singles:0", "page.get", {"ref": "artist:20:singles:0"})
    step("page artist:20:albums:30", "page.get", {"ref": "artist:20:albums:30"})
    step("page artist:20:appears", "page.get", {"ref": "artist:20:appears"})
    step("page artist:20:bogus:0", "page.get", {"ref": "artist:20:bogus:0"}, True)
    step("page label", "page.get", {"ref": "label:" + quote("Indie Records", safe="")})
    step("page unknown", "page.get", {"ref": "nothing:1"}, True)
    step("library albums 0+150", "library.list", {"kind": "albums", "offset": 0, "limit": 150})
    step("library tracks 100+120", "library.list", {"kind": "tracks", "offset": 100, "limit": 120})
    step("library tracks 0+20", "library.list", {"kind": "tracks", "limit": 20})
    step("library artists 500", "library.list", {"kind": "artists", "limit": 500})
    step("library playlists 1+1", "library.list", {"kind": "playlists", "offset": 1, "limit": 1})
    step("library playlists", "library.list", {"kind": "playlists"})
    step("library default kind", "library.list", {})
    step("library bad kind", "library.list", {"kind": "videos"}, True)
    step("library string offset", "library.list", {"kind": "artists", "offset": "5", "limit": "3"})
    step("search sine", "search", {"text": "sine", "limit": 5})
    step("search empty", "search", {"text": "   "})
    step("search wide", "search", {"text": "song", "limit": 500, "offset": 3})
    step("album.get 10", "album.get", {"ref": "album:10"})
    step("album.get 12", "album.get", {"ref": "album:12"})
    step("album.get 999", "album.get", {"ref": "album:999"})
    step("album.get bad ref", "album.get", {"ref": "album:../x"}, True)
    step("artist.get 20", "artist.get", {"ref": "artist:20"})
    mine = step("playlist.get mine", "playlist.get", {"ref": f"playlist:{mock.PLAYLIST_MINE}"})
    step("playlist.get editorial", "playlist.get", {"ref": f"playlist:{mock.PLAYLIST_EDITORIAL}"})
    step("playlist.get mix", "playlist.get", {"ref": "mix:mix1"})
    step("playlist.get unknown mix", "playlist.get", {"ref": "mix:mixzz"})
    step("playlist.get artist radio", "playlist.get", {"ref": "radio:artist:20"})
    step("playlist.get track radio", "playlist.get", {"ref": "radio:track:100"})
    step("playlist.get album radio", "playlist.get", {"ref": "radio:album:1"}, True)
    step("playlist.get album ref", "playlist.get", {"ref": "album:10"}, True)
    step("track.get 100", "track.get", {"ref": "track:100"})
    step("track.quality 100", "track.quality", {"ref": "track:100"})
    step("track.quality 101", "track.quality", {"ref": "track:101"})
    step("track.lyrics 100", "track.lyrics", {"ref": "track:100"})
    step("track.lyrics 101", "track.lyrics", {"ref": "track:101"})
    step("playlist.search", "page.action", {"action": "playlist.search", "text": "sine"})
    step("playlist.create", "page.action", {"action": "playlist.create", "title": "New", "description": "d", "public": True})
    step("playlist.create without title", "page.action", {"action": "playlist.create", "title": "  "}, True)
    step("playlist.copy", "page.action", {"action": "playlist.copy", "ref": f"playlist:{mock.PLAYLIST_MINE}", "title": "Copy"})
    step("playlist.update", "page.action", {"action": "playlist.update", "ref": f"playlist:{mock.PLAYLIST_MINE}", "title": "Renamed", "description": "new"})
    step("playlist.update editorial", "page.action", {"action": "playlist.update", "ref": f"playlist:{mock.PLAYLIST_EDITORIAL}", "title": "x"}, True)
    mine = step("playlist.get mine again", "playlist.get", {"ref": f"playlist:{mock.PLAYLIST_MINE}"})
    step("playlist.add", "page.action", {"action": "playlist.add", "ref": f"playlist:{mock.PLAYLIST_MINE}", "tracks": ["track:100", "track:101@playlist:x"]})
    step("playlist.add none", "page.action", {"action": "playlist.add", "ref": f"playlist:{mock.PLAYLIST_MINE}", "tracks": []}, True)
    step("playlist.remove", "page.action", {"action": "playlist.remove", "ref": f"playlist:{mock.PLAYLIST_MINE}", "members": members(mine, [2, 0, 2])})
    step("playlist.move", "page.action", {"action": "playlist.move", "ref": f"playlist:{mock.PLAYLIST_MINE}", "members": members(mine, [1]), "before": 3})
    step("playlist.move bad position", "page.action", {"action": "playlist.move", "ref": f"playlist:{mock.PLAYLIST_MINE}", "members": members(mine, [1]), "before": 999}, True)
    step("playlist.remove stale", "page.action", {"action": "playlist.remove", "ref": f"playlist:{mock.PLAYLIST_MINE}", "members": ["0" * 24 + ":0:100"]}, True)
    step("playlist.delete", "page.action", {"action": "playlist.delete", "ref": f"playlist:{mock.PLAYLIST_MINE}"})
    step("folder.create", "page.action", {"action": "folder.create", "title": "New Folder", "parent": "root"})
    step("folder.rename", "page.action", {"action": "folder.rename", "ref": f"folder:{mock.FOLDER}", "title": "Renamed"})
    step("folder.rename bad ref", "page.action", {"action": "folder.rename", "ref": "folder:nope", "title": "x"}, True)
    step("playlist.folder", "page.action", {"action": "playlist.folder", "ref": f"playlist:{mock.PLAYLIST_MINE}", "folder": f"folder:{mock.FOLDER}"})
    step("page.action unknown", "page.action", {"action": "playlist.explode", "ref": f"playlist:{mock.PLAYLIST_MINE}"}, True)
    step("favorite album on", "favorites.set", {"ref": "album:10", "on": True})
    step("favorite track off", "favorites.set", {"ref": "track:100", "on": False})
    step("favorite playlist on", "favorites.set", {"ref": f"playlist:{mock.PLAYLIST_EDITORIAL}", "on": True})
    step("favorite mix", "favorites.set", {"ref": "mix:mix1", "on": True}, True)
    resolved = step("track.resolve 100", "track.resolve", {"ref": f"track:100@playlist:{mock.PLAYLIST_MINE}"})
    step("track.resolve 101", "track.resolve", {"ref": "track:101"})
    step("track.resolve bad", "track.resolve", {"ref": "album:1"}, True)
    url = (resolved.get("result") or {}).get("url")
    wav = wav_checks(url)
    resolution = (resolved.get("result") or {}).get("resolution_id")
    step("event started", "playback.event", {"playback_id": "p1", "kind": "started", "resolution_id": resolution,
                                             "started_at": 1700000000.0, "position_ms": 0})
    step("event started again", "playback.event", {"playback_id": "p1", "kind": "started", "resolution_id": resolution})
    step("event unknown ticket", "playback.event", {"playback_id": "p2", "kind": "started", "resolution_id": "nope"})
    step("event progress", "playback.event", {"playback_id": "p1", "kind": "progress", "sequence": 1, "played_ms": 300,
                                              "position_ms": 300, "observed_at_ms": 1700000000300})
    step("event progress repeated", "playback.event", {"playback_id": "p1", "kind": "progress", "sequence": 1, "played_ms": 300,
                                                       "position_ms": 300, "observed_at_ms": 1700000000300})
    step("event progress decreasing", "playback.event", {"playback_id": "p1", "kind": "progress", "sequence": 2, "played_ms": 100,
                                                         "position_ms": 100, "observed_at_ms": 1700000000400}, True)
    step("event ended", "playback.event", {"playback_id": "p1", "kind": "ended", "sequence": 3, "played_ms": 600,
                                           "position_ms": 600, "observed_at_ms": 1700000000600, "reason": "finished"})
    step("event bad kind", "playback.event", {"playback_id": "p1", "kind": "paused"}, True)
    deadline = time.monotonic() + 25
    while time.monotonic() < deadline:
        status = plugin.call("reporting.status").get("result") or {}
        if status.get("pending") == 0 and status.get("delivered"):
            break
        time.sleep(0.5)
    step("reporting.status after delivery", "reporting.status")
    step("page discover:foryou with history", "page.get", {"ref": "discover:foryou"})
    step("unknown method", "something.else", {}, True)
    step("auth.logout", "auth.logout")
    step("auth.status after logout", "auth.status")
    step("home after logout", "home", None, True)
    return records, wav


def normalize(value):
    if isinstance(value, dict):
        return {key: "<resolution-id>" if key == "resolution_id" else normalize(item) for key, item in value.items()}
    if isinstance(value, list):
        return [normalize(item) for item in value]
    if isinstance(value, str) and STREAM_URL.match(value):
        return "<stream-url>"
    return value


def normalize_request(entry):
    entry = dict(entry)
    if entry["host"] == "ec.tidal.com":
        form = []
        for name, value in entry["form"]:
            if name.endswith(".Id"):
                value = "<event-id>"
            elif name.endswith(".MessageBody"):
                body = json.loads(value)
                body["uuid"] = "<event-id>"
                body["payload"]["playbackSessionId"] = "<event-id>"
                value = json.dumps(body, sort_keys=True)
            elif name.endswith("2.Value.StringValue"):
                headers = json.loads(value)
                headers["requested-sent-timestamp"] = "<time>"
                value = json.dumps(headers, sort_keys=True)
            form.append((name, value))
        entry["form"] = form
    entry["query"] = [list(pair) for pair in entry["query"]]
    entry["form"] = [list(pair) for pair in entry["form"]]
    return entry


def same_type(left, right):
    if isinstance(left, bool) or isinstance(right, bool):
        return type(left) is type(right)
    if isinstance(left, (int, float)) and isinstance(right, (int, float)):
        return type(left) is type(right)
    return type(left) is type(right)


def differences(left, right, path="$"):
    if not same_type(left, right):
        yield path, left, right
    elif isinstance(left, dict):
        for key in sorted(set(left) | set(right)):
            if key not in left or key not in right:
                yield f"{path}.{key}", left.get(key, "<missing>"), right.get(key, "<missing>")
            else:
                yield from differences(left[key], right[key], f"{path}.{key}")
    elif isinstance(left, list):
        if len(left) != len(right):
            yield f"{path}.length", len(left), len(right)
        for index, (a, b) in enumerate(zip(left, right)):
            yield from differences(a, b, f"{path}[{index}]")
    elif left != right:
        yield path, left, right


def short(value, limit=160):
    text = json.dumps(value, ensure_ascii=False, sort_keys=True) if not isinstance(value, str) else repr(value)
    return text if len(text) <= limit else text[:limit] + "..."


def run(command, label):
    server, world, base = mock.serve()
    folder = pathlib.Path(tempfile.mkdtemp(prefix=f"tidal-diff-{label}-"))
    data = folder / "data"
    data.mkdir()
    # An expired token: the first catalog call refreshes it, so the refresh is compared too.
    (data / "account.json").write_text(json.dumps({"access_token": "t1", "refresh_token": "r1", "expires": 1, "user_id": "5",
                                                   "country": "US", "name": "Tester"}))
    plugin = Plugin(command, data, base)
    try:
        records, wav = scenario(plugin, world)
    finally:
        stderr = plugin.close()
        server.shutdown()
        server.server_close()
    log = world.snapshot()
    unordered = Counter(json.dumps(normalize_request(entry), sort_keys=True) for entry in log if entry["host"] not in ORDERED_HOSTS)
    files = sorted(path.name for path in data.iterdir())
    shutil.rmtree(folder, ignore_errors=True)
    return {"records": records, "wav": wav, "unordered": unordered, "stderr": stderr, "files": files, "pid": plugin.process.pid}


def compare(left, right, names):
    problems = []
    for a, b in zip(left["records"], right["records"]):
        assert a["label"] == b["label"]
        for path, x, y in differences(normalize(a["reply"]), normalize(b["reply"])):
            problems.append((a["label"], "answer " + path, x, y))
        requests_a = [normalize_request(entry) for entry in a["requests"]]
        requests_b = [normalize_request(entry) for entry in b["requests"]]
        for path, x, y in differences(requests_a, requests_b):
            problems.append((a["label"], "requests " + path, x, y))
    for path, x, y in differences(left["wav"], right["wav"]):
        problems.append(("wav", path, x, y))
    for entry in sorted(set(left["unordered"]) | set(right["unordered"])):
        if left["unordered"][entry] != right["unordered"][entry]:
            problems.append(("cdn/events", "request count", f'{left["unordered"][entry]} x {entry}', f'{right["unordered"][entry]} x {entry}'))
    for name, result in zip(names, [left, right]):
        if result["stderr"].strip():
            problems.append(("stderr", name, result["stderr"].strip()[:400], ""))
    return problems


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--rust", help="Rust plugin binary built with the mock-endpoints feature")
    parser.add_argument("--python-only", action="store_true")
    args = parser.parse_args()
    if not args.rust and not args.python_only:
        parser.error("give --rust BINARY or --python-only")
    python = [PYTHON, str(HERE / "python-plugin")]
    names = ["python", "python again"] if args.python_only else ["python", "rust"]
    left = run(python, "python")
    right = run(python if args.python_only else [str(pathlib.Path(args.rust).resolve())], "second")
    print("Normalizations:")
    for rule in NORMALIZATIONS:
        print("  - " + rule)
    unexpected = [(record["label"], record["reply"]["error"]) for record in left["records"]
                  if "error" in record["reply"] and not record["expect_error"]]
    missing = [record["label"] for record in left["records"] if record["expect_error"] and "error" not in record["reply"]]
    print(f'\nCalls: {len(left["records"])}; requests kept in order: {sum(len(r["requests"]) for r in left["records"])}; '
          f'CDN and event requests: {sum(left["unordered"].values())}')
    for label, error in unexpected:
        print(f"Python answered an unexpected error: {label}: {short(error)}")
    for label in missing:
        print(f"Python did not fail where the scenario expects it: {label}")
    problems = compare(left, right, names)
    if not problems:
        print(f"\nNo differences between {names[0]} and {names[1]}.")
        return 0
    print(f"\n{len(problems)} differences ({names[0]} | {names[1]}):")
    for label, path, x, y in problems:
        print(f"- {label}: {path}\n    {names[0]}: {short(x)}\n    {names[1]}: {short(y)}")
    return 1


if __name__ == "__main__":
    sys.exit(main())
