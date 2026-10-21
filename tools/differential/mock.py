"""A deterministic fake Tidal for the differential tests.

Requests arrive as /<original-host>/<original-path>?query, which is how both
plugins address it in the differential run. Every request is recorded in
order. Reads answer from a fixed catalog; writes answer the shape the plugin
expects and change nothing.
"""
import base64
import ctypes as C
import ctypes.util
import json
import pathlib
import re
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qsl, unquote, urlsplit

ROOT = pathlib.Path(__file__).resolve().parents[2]
FIXTURE = ROOT / "tests" / "fixtures" / "whole-44100-16.flac"
CDN = "sp-ad-fa.audio.tidal.com"
USER = 5
PLAYLIST_MINE = "aaaaaaaa-0000-4000-8000-000000000001"
PLAYLIST_EDITORIAL = "aaaaaaaa-0000-4000-8000-000000000002"
PLAYLIST_NEW = "cccccccc-0000-4000-8000-000000000001"
FOLDER = "bbbbbbbb-0000-4000-8000-000000000001"
SEGMENT_PATH = "/mediatracks/fixture/"


# ---------------------------------------------------------------- streaming --

def flac_frames(path):
    """Byte offsets and sample counts of every frame, as libFLAC reads them."""
    lib = C.CDLL(ctypes.util.find_library("FLAC"))
    pointer = C.c_void_p
    write_type = C.CFUNCTYPE(C.c_int, pointer, pointer, C.POINTER(C.POINTER(C.c_int32)), pointer)
    error_type = C.CFUNCTYPE(None, pointer, C.c_int, pointer)
    lib.FLAC__stream_decoder_new.restype = pointer
    lib.FLAC__stream_decoder_init_file.argtypes = [pointer, C.c_char_p, write_type, pointer, error_type, pointer]
    lib.FLAC__stream_decoder_get_decode_position.argtypes = [pointer, C.POINTER(C.c_uint64)]
    lib.FLAC__stream_decoder_get_blocksize.argtypes = [pointer]
    for name in ["process_until_end_of_stream", "finish", "delete"]:
        getattr(lib, "FLAC__stream_decoder_" + name).argtypes = [pointer]
    decoder = lib.FLAC__stream_decoder_new()
    found = []

    @write_type
    def write(handle, frame, planes, _):
        position = C.c_uint64()
        lib.FLAC__stream_decoder_get_decode_position(handle, C.byref(position))
        found.append((position.value, lib.FLAC__stream_decoder_get_blocksize(handle)))
        return 0

    @error_type
    def error(*_):
        raise SystemExit("libFLAC reported a decoding error")

    assert lib.FLAC__stream_decoder_init_file(decoder, str(path).encode(), write, None, error, None) == 0
    assert lib.FLAC__stream_decoder_process_until_end_of_stream(decoder)
    lib.FLAC__stream_decoder_finish(decoder)
    lib.FLAC__stream_decoder_delete(decoder)
    # The position reported in the write callback is the end of the frame just decoded.
    starts = [42] + [position for position, _ in found[:-1]]
    return [(start, end, count) for start, (end, count) in zip(starts, found)]


def box(kind, body):
    return (8 + len(body)).to_bytes(4, "big") + kind + body


def build_stream():
    """The init segment, the media segments and their sample counts."""
    data = FIXTURE.read_bytes()
    assert data[:5] == b"fLaC\x80" and int.from_bytes(data[5:8], "big") == 34
    streaminfo = data[8:42]
    frames = flac_frames(FIXTURE)
    # Two equal segments of two frames, then the rest: the timeline needs a repeat.
    groups = [frames[0:2], frames[2:4], frames[4:]]
    dfla = box(b"dfLa", b"\x00\x00\x00\x00" + b"\x80\x00\x00\x22" + streaminfo)
    sample_entry = box(b"fLaC", bytes(28) + dfla)
    init = box(b"ftyp", b"iso6\x00\x00\x00\x00iso6dash") + box(b"moov", box(b"trak", box(b"stsd", b"\x00\x00\x00\x00\x00\x00\x00\x01" + sample_entry)))
    segments = []
    for number, group in enumerate(groups, start=1):
        payload = data[group[0][0]:group[-1][1]]
        moof = box(b"moof", box(b"mfhd", b"\x00\x00\x00\x00" + number.to_bytes(4, "big")))
        segments.append((moof + box(b"mdat", payload), sum(count for _, _, count in group)))
    return init, segments


INIT, SEGMENTS = build_stream()


def dash_manifest():
    timeline = []
    index = 0
    while index < len(SEGMENTS):
        repeat = 0
        while index + repeat + 1 < len(SEGMENTS) and SEGMENTS[index + repeat + 1][1] == SEGMENTS[index][1]:
            repeat += 1
        entry = f'<S d="{SEGMENTS[index][1]}"' + (f' r="{repeat}"' if repeat else "") + "/>"
        timeline.append(entry)
        index += repeat + 1
    base = f"https://{CDN}{SEGMENT_PATH}"
    xml = ('<?xml version="1.0" encoding="UTF-8"?>\n'
           '<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance" '
           'profiles="urn:mpeg:dash:profile:isoff-main:2011" type="static" mediaPresentationDuration="PT0.6S" minBufferTime="PT3.993S">'
           '<Period id="0"><AdaptationSet id="0" contentType="audio" mimeType="audio/mp4" segmentAlignment="true">'
           '<Representation id="FLAC,44100,16" codecs="flac" bandwidth="947000" audioSamplingRate="44100">'
           f'<SegmentTemplate timescale="44100" initialization="{base}0.mp4?token=abc" media="{base}$Number$.mp4?token=abc" startNumber="1">'
           f'<SegmentTimeline>{"".join(timeline)}</SegmentTimeline></SegmentTemplate></Representation></AdaptationSet></Period></MPD>')
    return base64.b64encode(xml.encode()).decode()


# ------------------------------------------------------------------ catalog --

ARTISTS = {
    20: {"id": 20, "name": "Main Artist", "picture": "pp-qq-rr-ss", "popularity": 50, "url": "http://www.tidal.com/artist/20",
         "artistRoles": [{"categoryId": -1, "category": "Artist"}, {"categoryId": 11, "category": "Songwriter"}],
         "mixes": {"ARTIST_MIX": "artmix20"}},
    21: {"id": 21, "name": "Second", "picture": None, "artistRoles": [{"category": "Artist"}], "mixes": {}},
    22: {"id": 22, "name": "Influence One", "picture": "i1-i1-i1-i1"},
    23: {"id": 23, "name": "Influence Two", "picture": "i2-i2-i2-i2"},
}


def short_artist(key, kind="MAIN"):
    return {"id": key, "name": ARTISTS[key]["name"], "type": kind, "picture": ARTISTS[key].get("picture")}


ALBUMS = {
    10: {"id": 10, "title": "First Album", "cover": "aa-bb-cc-dd", "releaseDate": "2019-05-01", "type": "ALBUM",
         "numberOfTracks": 3, "duration": 600, "explicit": False, "copyright": "℗ 2019 Indie Records", "upc": "000000000010",
         "audioQuality": "LOSSLESS", "mediaMetadata": {"tags": ["LOSSLESS", "HIRES_LOSSLESS"]},
         "artist": short_artist(20), "artists": [short_artist(20), short_artist(21, "FEATURED")], "streamReady": True,
         "version": None},
    11: {"id": 11, "title": "Jazz EP", "cover": "ee-ff-00-11", "releaseDate": "2021-02-03", "type": "EP",
         "numberOfTracks": 3, "explicit": True, "copyright": "© 2021 Blue Things", "audioQuality": "HIGH",
         "mediaMetadata": {"tags": ["LOSSLESS"]}, "artist": short_artist(20), "artists": [short_artist(20)],
         "version": "Deluxe"},
    12: {"id": 12, "title": "Big Box", "cover": "bb-00-11-22", "releaseDate": "2008-01-01", "type": "COMPILATION",
         "numberOfTracks": 120, "copyright": "(P) 2005-2007, Box Label / © 2008 Other", "audioQuality": "LOSSLESS",
         "artist": short_artist(21), "artists": [short_artist(21)]},
    13: {"id": 13, "title": "Sparse", "cover": None, "numberOfTracks": 150, "artist": short_artist(21), "artists": []},
}


def embedded_album(key, full=False):
    album = ALBUMS[key]
    if full:
        return dict(album)
    return {"id": key, "title": album["title"], "cover": album["cover"], "releaseDate": album.get("releaseDate")}


def make_track(key, album, number, title=None, artist=20, **extra):
    track = {"id": key, "title": title or f"Track {key}", "duration": 200 + key % 7, "trackNumber": number,
             "volumeNumber": 1 + (number - 1) // 60, "explicit": key % 5 == 0, "isrc": f"USAAA{key:07d}",
             "audioQuality": "LOSSLESS", "mediaMetadata": {"tags": ["LOSSLESS"]}, "streamReady": True,
             "replayGain": -6.5, "peak": 0.95, "artist": short_artist(artist), "artists": [short_artist(artist)],
             "album": embedded_album(album), "version": None, "popularity": 10}
    track.update(extra)
    return track


TRACKS = {
    100: make_track(100, 10, 1, "Sine Song", duration=1, copyright="℗ 2019 Indie Records",
                    mediaMetadata={"tags": ["LOSSLESS", "HIRES_LOSSLESS"]}, audioQuality="HI_RES_LOSSLESS"),
    101: make_track(101, 11, 1, "Aac Song", audioQuality="HIGH", mediaMetadata={"tags": []}),
    102: make_track(102, 10, 2, "Second Song", version="Live"),
    103: make_track(103, 10, 3, "Third Song", artist=21),
    111: make_track(111, 11, 2, "Blue One"),
    112: make_track(112, 11, 3, "Blue Two"),
}
for index in range(120):
    TRACKS[12000 + index] = make_track(12000 + index, 12, index + 1, artist=21)
for index in range(150):
    TRACKS[13000 + index] = make_track(13000 + index, 13, index + 1, artist=21)
ALBUM_TRACKS = {10: [100, 102, 103], 11: [101, 111, 112], 12: list(range(12000, 12120)), 13: list(range(13000, 13150))}


def generated_album(key):
    album = {"id": key, "title": f"Album {key}", "cover": f"c{key}-00-11-22", "releaseDate": f"20{10 + key % 10}-01-01",
             "type": "SINGLE" if key % 3 == 0 else "ALBUM", "numberOfTracks": 10, "artist": short_artist(20),
             "artists": [short_artist(20)], "audioQuality": "LOSSLESS", "explicit": False}
    if key % 2 == 0:
        album["copyright"] = f"℗ 2020 Label {key % 3}"
    return album


def album(key):
    return ALBUMS.get(key) or (generated_album(key) if 500 <= key < 630 else None)


PLAYLISTS = {
    PLAYLIST_MINE: {"uuid": PLAYLIST_MINE, "title": "Mine", "description": "My <b>own</b> list", "type": "USER",
                    "creator": {"id": USER, "name": "Tester"}, "publicPlaylist": False, "numberOfTracks": 105,
                    "lastUpdated": "2024-03-01T10:00:00.000+0000", "created": "2023-01-01T00:00:00.000+0000",
                    "squareImage": "11-22-33-44", "duration": 20000},
    PLAYLIST_EDITORIAL: {"uuid": PLAYLIST_EDITORIAL, "title": "Editors Choice", "description": "Picked by people",
                         "type": "EDITORIAL", "creator": {"id": 0}, "publicPlaylist": True, "numberOfTracks": 2,
                         "lastUpdated": "2024-04-01T10:00:00Z", "image": "55-66-77-88"},
}


def playlist_items(key):
    if key == PLAYLIST_MINE:
        entries = [{"type": "track", "cut": None, "item": TRACKS[100]}, {"type": "track", "item": TRACKS[102]},
                   {"type": "track", "item": TRACKS[100]}, {"type": "video", "item": {"id": 9000, "title": "Clip"}}]
        entries += [{"type": "track", "item": TRACKS[12000 + index]} for index in range(101)]
        return entries
    if key == PLAYLIST_EDITORIAL:
        return [{"type": "track", "item": TRACKS[101]}, {"type": "track", "item": TRACKS[111]}]
    return []


MIXES = {"mix1": {"id": "mix1", "title": "My Mix 1", "subTitle": "Main Artist and more", "mixType": "DISCOVERY_MIX",
                  "images": {"SMALL": {"width": 320, "url": "https://resources.tidal.com/images/mix1/320x320.jpg"},
                             "MEDIUM": {"width": 640, "url": "https://resources.tidal.com/images/mix1/640x640.jpg"}}},
         "mix2": {"id": "mix2", "title": "My Mix 2", "mixImages": [{"url": "https://resources.tidal.com/images/mix2/640x640.jpg"}]}}


def favorite_entries(kind):
    if kind == "albums":
        return [{"created": f"2024-01-{1 + index % 28:02d}T00:00:00.000+0000", "item": generated_album(500 + index)} for index in range(130)]
    if kind == "tracks":
        keys = list(range(12000, 12120)) + list(range(13000, 13110))
        return [{"created": "2024-02-01T00:00:00.000+0000", "item": TRACKS[key]} for key in keys]
    if kind == "artists":
        return [{"created": "2024-02-02T12:30:00Z", "item": {"id": 3000 + index, "name": f"Artist {3000 + index}",
                 "picture": f"a{index}-00-00-00" if index % 2 else None}} for index in range(105)]
    return []


def page(items, query, default_limit=50):
    limit = int(query.get("limit", default_limit))
    offset = int(query.get("offset", 0))
    return {"limit": limit, "offset": offset, "totalNumberOfItems": len(items), "items": items[offset:offset + limit]}


def module(kind, title, items, more=None, **extra):
    value = {"type": kind, "title": title, "pagedList": {"limit": 50, "offset": 0, "totalNumberOfItems": len(items), "items": items}}
    if more:
        value["showMore"] = {"apiPath": more, "title": "View all"}
    value.update(extra)
    return value


def document(title, *modules):
    return {"id": title.lower().replace(" ", "_"), "title": title, "rows": [{"modules": [entry]} for entry in modules]}


def links(*names):
    return [{"title": name, "apiPath": f"pages/{name.lower().replace(' ', '_')}", "icon": None} for name in names]


PAGES = {
    "pages/staff_picks": document("Staff Picks",
        module("ALBUM_LIST", "Staff albums", [ALBUMS[10], ALBUMS[11]], "pages/single-module-page/staff/1"),
        module("PLAYLIST_LIST", "Editorial", [PLAYLISTS[PLAYLIST_EDITORIAL]]),
        module("TRACK_LIST", "Tracks", [TRACKS[100], TRACKS[102]]),
        module("VIDEO_LIST", "Videos", [{"type": "VIDEO", "id": 77, "title": "A video"}])),
    "pages/explore": document("Explore",
        module("PAGE_LINKS_CLOUD", "Genres", links("Rock", "Jazz")),
        module("ARTIST_LIST", "Artists", [ARTISTS[20], ARTISTS[21]])),
    "pages/explore_new_music": document("New",
        module("ALBUM_LIST", "New albums", [ALBUMS[11], ALBUMS[10]], "pages/new_albums"),
        module("TRACK_LIST", "New tracks", [TRACKS[111]])),
    "pages/explore_top_music": document("Top", module("TRACK_LIST", "Top 20", [TRACKS[100], TRACKS[101], TRACKS[103]])),
    "pages/hires": document("Hi-Res", module("ALBUM_LIST", "Hi-Res albums", [ALBUMS[10]])),
    "pages/my_collection_my_mixes": document("My Mixes", module("MIX_LIST", "My Mixes", [MIXES["mix1"], MIXES["mix2"]])),
    "pages/genre_page": document("Genres", module("PAGE_LINKS_CLOUD", "Genres", links("Rock", "Jazz", "Blues"))),
    "pages/moods_page": document("Moods", module("PAGE_LINKS_CLOUD", "Moods",
        links("Chill", "Record Labels", "TIDAL Magazine", "Artist Experiences", "acoustic", "Party"))),
    "pages/m_1950s": document("1950s", module("ALBUM_LIST", "Fifties", [ALBUMS[12]]), module("PAGE_LINKS", "More", links("Fifties Jazz"))),
    "pages/m_1960s": document("1960s", module("ALBUM_LIST", "Sixties", [ALBUMS[11]])),
    "pages/chill": document("Chill", module("PLAYLIST_LIST", "Chill", [PLAYLISTS[PLAYLIST_EDITORIAL]])),
    "pages/rock": document("Rock", module("ALBUM_LIST", "Rock albums", [ALBUMS[10]])),
}

HOME_FIRST = {"page": {"cursor": "c2"}, "items": [
    {"moduleId": "m1", "type": "HORIZONTAL_LIST_WITH_CONTEXT", "title": "Because you listened",
     "items": [{"type": "ALBUM", "data": ALBUMS[10]}, {"type": "TRACK", "data": TRACKS[100]},
               {"type": "PLAYLIST", "data": PLAYLISTS[PLAYLIST_EDITORIAL]}, {"type": "TRACK", "data": TRACKS[102]}],
     "viewAll": "home/pages/m1/view-all", "header": {"type": "ARTIST", "data": ARTISTS[20]}},
    {"moduleId": "dup", "type": "HORIZONTAL_LIST", "title": "Albums", "items": [{"type": "ALBUM", "data": ALBUMS[10]}]},
    {"moduleId": "dup", "type": "HORIZONTAL_LIST", "title": "Albums",
     "items": [{"type": "ALBUM", "data": ALBUMS[11]}, {"type": "ALBUM", "data": ALBUMS[10]}], "viewAll": "home/pages/dup/view-all"},
    {"type": "SHORTCUT_LIST", "title": "Shortcuts", "items": [{"type": "MIX", "data": MIXES["mix1"]},
     {"type": "VIDEO", "data": {"id": 5, "title": "skip"}}]},
    {"type": "TEXT", "title": "Just a heading", "items": []},
]}
HOME_SECOND = {"page": {}, "items": [
    {"moduleId": "m5", "type": "TRACK_LIST", "title": "More tracks", "items": [{"type": "TRACK", "data": TRACKS[103]}],
     "viewAll": "/v2/home/pages/m5/view-all"},
]}


def credits_items(key):
    rows = []
    for track_key in ALBUM_TRACKS.get(key, []):
        track = dict(TRACKS[track_key])
        track["album"] = embedded_album(key, full=True)
        if key == 11:
            credits = []
        else:
            credits = [{"type": "Composer", "contributors": [{"name": f"Composer {track_key % 3}", "id": 1}]},
                       {"type": "Lyricist", "contributors": [{"name": "Lyric Person"}]},
                       {"type": "Producer", "contributors": [{"name": "Prod"}]}]
            if track_key % 4 == 0:
                credits.append({"type": "Composer", "contributors": [{"name": " Second Composer "}, {"name": f"Composer {track_key % 3}"}]})
        rows.append({"type": "track", "item": track, "credits": credits})
    return rows


def album_page(key):
    header = {"id": key}
    if key == 10:
        header["genre"] = "Rock"
    elif key == 11:
        header["genres"] = ["Jazz", "Music"]
    elif key == 12:
        header["genre"] = None
    elif 500 <= key < 630:
        header["genre"] = "Pop" if key % 2 == 0 else None
    else:
        return None
    return {"rows": [{"modules": [{"type": "ALBUM_HEADER", "album": header}]}, {"modules": [{"type": "ALBUM_ITEMS"}]}]}


# ------------------------------------------------------------------- server --

class World:
    def __init__(self):
        self.log = []
        self.lock = threading.Lock()

    def record(self, entry):
        with self.lock:
            self.log.append(entry)

    def snapshot(self):
        with self.lock:
            return list(self.log)

    def answer(self, method, host, path, query, form):
        """Status, extra headers and a body (bytes, or a value sent as JSON)."""
        if host == CDN:
            if path.startswith(SEGMENT_PATH):
                number = path[len(SEGMENT_PATH):].removesuffix(".mp4")
                if number == "0":
                    return 200, {"Content-Type": "audio/mp4"}, INIT
                if number.isdigit() and 1 <= int(number) <= len(SEGMENTS):
                    return 200, {"Content-Type": "audio/mp4"}, SEGMENTS[int(number) - 1][0]
            return 404, {}, b""
        if host == "sp-pr-fa.audio.tidal.com":
            return 200, {"Content-Type": "audio/mp4"}, b"\x00" * 64
        if host == "auth.tidal.com" and path == "/v1/oauth2/token":
            if form.get("grant_type") == "refresh_token":
                return 200, {}, {"access_token": "t2", "refresh_token": "r2", "expires_in": 3600, "token_type": "Bearer",
                                 "user": {"userId": USER, "countryCode": "US", "username": "Tester"}}
            return 400, {}, {"error": "invalid_grant"}
        if host == "ec.tidal.com" and path == "/api/event-batch":
            ident = form.get("SendMessageBatchRequestEntry.1.Id", "")
            body = ('<?xml version="1.0"?><SendMessageBatchResponse xmlns="http://queue.amazonaws.com/doc/2012-11-05/">'
                    f'<SendMessageBatchResult><SendMessageBatchResultEntry><Id>{ident}</Id><MessageId>m-1</MessageId>'
                    '</SendMessageBatchResultEntry></SendMessageBatchResult></SendMessageBatchResponse>')
            return 200, {"Content-Type": "text/xml"}, body.encode()
        if host != "api.tidal.com":
            return 404, {}, {"status": 404}
        if method != "GET":
            return self.write(method, path, query, form)
        if path.startswith("/v1/"):
            return self.v1(path[4:], query)
        if path.startswith("/v2/"):
            return self.v2(path[4:], query)
        return 404, {}, {"status": 404}

    def write(self, method, path, query, form):
        if path == "/v2/my-collection/playlists/folders/create-playlist":
            return 200, {}, {"trn": f"trn:playlist:{PLAYLIST_NEW}", "itemType": "PLAYLIST",
                             "data": {"uuid": PLAYLIST_NEW, "title": query.get("name"), "creator": {"id": USER}}}
        if path.startswith("/v2/my-collection/playlists/folders/"):
            return 200, {}, {}
        if re.fullmatch(r"/v2/playlists/[\w-]+/set-(public|private)", path):
            return 200, {}, b""
        if path.startswith("/v1/playlists/") or path.startswith(f"/v1/users/{USER}/favorites/"):
            return 200, {"ETag": '"etag-after-write"'}, b""
        return 404, {}, {"status": 404}

    def v2(self, path, query):
        if path == "home/feed/static":
            return 200, {}, HOME_SECOND if query.get("cursor") == "c2" else HOME_FIRST
        if path.startswith("home/pages/"):
            return 200, {}, {"title": "View all", "items": [{"type": "ALBUM", "data": ALBUMS[10]}, {"type": "ALBUM", "data": ALBUMS[11]}]}
        if path == "my-collection/playlists/folders":
            folder = query.get("folderId")
            if folder == "root":
                items = [{"itemType": "FOLDER", "name": "Sub", "data": {"id": FOLDER, "name": "Sub", "itemType": "FOLDER"}},
                         {"itemType": "PLAYLIST", "name": "Mine", "data": dict(PLAYLISTS[PLAYLIST_MINE], itemType="PLAYLIST")}]
            elif folder == FOLDER:
                items = [{"itemType": "PLAYLIST", "data": dict(PLAYLISTS[PLAYLIST_EDITORIAL])}]
            else:
                items = []
            return 200, {}, page(items, query)
        return 404, {}, {"status": 404}

    def v1(self, path, query):
        parts = path.split("/")
        if path == "sessions":
            return 200, {}, {"sessionId": "s", "userId": USER, "countryCode": "US", "channelId": 1}
        if path == "genres":
            return 200, {}, [{"name": "Rock", "path": "rock", "hasAlbums": True}, {"name": "Jazz", "path": "jazz"}]
        if len(parts) == 3 and parts[0] == "genres" and parts[2] in {"albums", "playlists"}:
            items = [ALBUMS[10], ALBUMS[11]] if parts[2] == "albums" else [PLAYLISTS[PLAYLIST_EDITORIAL]]
            return 200, {}, page(items if parts[1] == "rock" else items[:1], query)
        if path in PAGES:
            return 200, {}, PAGES[path]
        if path.startswith("pages/") and path.endswith(("_jazz", "_blues", "acoustic", "party")):
            return 200, {}, document("Linked", module("ALBUM_LIST", "Linked albums", [ALBUMS[11]]))
        if path == "pages/album":
            value = album_page(int(query.get("albumId", "0")))
            return (200, {}, value) if value else (404, {}, {"status": 404})
        if path == "pages/artist":
            if query.get("artistId") != "20":
                return 404, {}, {"status": 404}
            return 200, {}, {"rows": [{"modules": [{"type": "ARTIST_HEADER", "artist": ARTISTS[20]}]},
                                      {"modules": [module("ARTIST_LIST", "Influencers", [ARTISTS[22], ARTISTS[23]])]},
                                      {"modules": [module("ARTIST_LIST", "Fans also like", [ARTISTS[21]])]},
                                      {"modules": [module("ALBUM_LIST", "Influences albums", [ALBUMS[10]])]}]}
        if path == "pages/mix":
            key = query.get("mixId")
            if key not in MIXES:
                return 200, {}, {"rows": []}
            return 200, {}, {"rows": [{"modules": [{"type": "MIX_HEADER", "mix": MIXES[key]}]}]}
        if parts[0] == "mixes" and len(parts) == 3 and parts[2] == "items":
            return 200, {}, page([{"type": "track", "item": TRACKS[100]}, {"type": "track", "item": TRACKS[111]}], query, 100)
        if path == "search":
            text = query.get("query", "")
            found = [TRACKS[100], TRACKS[102]] if "sine" in text.lower() or "song" in text.lower() else []
            limit = int(query.get("limit", 20))
            return 200, {}, {"albums": {"items": [ALBUMS[10]][:limit], "totalNumberOfItems": 7},
                             "artists": {"items": [ARTISTS[20]][:limit], "totalNumberOfItems": 1},
                             "tracks": {"items": found[:limit]},
                             "playlists": {"items": [PLAYLISTS[PLAYLIST_EDITORIAL]], "totalNumberOfItems": 1},
                             "videos": {"items": []}, "topHit": {"type": "ARTISTS", "value": ARTISTS[20]}}
        if path == "search/albums":
            return 200, {}, page([ALBUMS[10], ALBUMS[12], dict(ALBUMS[11], copyright="℗ indie records")], query)
        if parts[0] == "users" and len(parts) == 4 and parts[1] == str(USER) and parts[2] == "favorites":
            return 200, {}, page(favorite_entries(parts[3]), query)
        if parts[0] == "tracks" and len(parts) >= 2 and parts[1].isdigit():
            return self.track(int(parts[1]), parts[2:], query)
        if parts[0] == "albums" and len(parts) >= 2 and parts[1].isdigit():
            return self.album(int(parts[1]), parts[2:], query)
        if parts[0] == "artists" and len(parts) >= 2 and parts[1].isdigit():
            return self.artist(int(parts[1]), parts[2:], query)
        if parts[0] == "playlists" and len(parts) >= 2:
            key = parts[1]
            if key == PLAYLIST_NEW and len(parts) == 2:
                return 200, {"ETag": '"etag-new"'}, {"uuid": PLAYLIST_NEW, "title": "New", "creator": {"id": USER}, "type": "USER"}
            if key not in PLAYLISTS:
                return 404, {}, {"status": 404}
            if len(parts) == 2:
                return 200, {"ETag": '"etag-1"'}, PLAYLISTS[key]
            if parts[2:] == ["items"]:
                return 200, {"ETag": '"etag-1"'}, page(playlist_items(key), query)
        return 404, {}, {"status": 404, "subStatus": 2001, "userMessage": "Not found"}

    def track(self, key, rest, query):
        if key not in TRACKS:
            return 404, {}, {"status": 404}
        if not rest:
            return 200, {}, TRACKS[key]
        if rest == ["lyrics"]:
            if key != 100:
                return 404, {}, {"status": 404, "subStatus": 2001}
            return 200, {}, {"trackId": 100, "lyricsProvider": "Fixture", "lyrics": "Line one\nLine two",
                             "subtitles": "[00:00.10] Line one\n[00:00.40] Line two", "isRightToLeft": False}
        if rest == ["radio"]:
            return 200, {}, page([TRACKS[102], TRACKS[103]], query, 100)
        if rest == ["playbackinfopostpaywall"]:
            if key == 100:
                return 200, {}, {"trackId": 100, "assetPresentation": "FULL", "audioMode": "STEREO", "audioQuality": "LOSSLESS",
                                 "manifestMimeType": "application/dash+xml", "manifestHash": "h", "manifest": dash_manifest(),
                                 "albumReplayGain": -7.0, "albumPeakAmplitude": 1.0, "trackReplayGain": -6.5,
                                 "trackPeakAmplitude": 0.95, "bitDepth": 16, "sampleRate": 44100}
            manifest = {"mimeType": "audio/mp4", "codecs": "mp4a.40.2", "encryptionType": "NONE",
                        "urls": ["https://sp-pr-fa.audio.tidal.com/mediatracks/aac/101.m4a?token=x"]}
            return 200, {}, {"trackId": key, "assetPresentation": "FULL", "audioMode": "STEREO", "audioQuality": "HIGH",
                             "manifestMimeType": "application/vnd.tidal.bts", "manifest": base64.b64encode(json.dumps(manifest).encode()).decode(),
                             "bitDepth": None, "sampleRate": 44100}
        return 404, {}, {"status": 404}

    def album(self, key, rest, query):
        value = album(key)
        if value is None:
            return 404, {}, {"status": 404, "subStatus": 2001}
        if not rest:
            return 200, {}, value
        if rest == ["tracks"]:
            tracks = [TRACKS[track] for track in ALBUM_TRACKS.get(key, [])]
            if key == 10:
                tracks = [{k: v for k, v in track.items() if k != "album"} if track["id"] == 103 else track for track in tracks]
            return 200, {}, page(tracks, query, 100)
        if rest == ["review"]:
            if key != 10:
                return 404, {}, {"status": 404}
            return 200, {}, {"source": "TiVo", "text": 'A <i>fine</i> record by [wimpLink artistId="20"]Main Artist[/wimpLink].'}
        if rest == ["similar"]:
            return (200, {}, page([ALBUMS[11]], query)) if key == 10 else (404, {}, {"status": 404})
        if rest == ["items", "credits"]:
            if key == 13:
                return 404, {}, {"status": 404}
            return 200, {}, page(credits_items(key), query, 100)
        return 404, {}, {"status": 404}

    def artist(self, key, rest, query):
        if key not in ARTISTS:
            return 404, {}, {"status": 404}
        if not rest:
            return 200, {}, ARTISTS[key]
        if rest == ["bio"]:
            if key != 20:
                return 404, {}, {"status": 404}
            return 200, {}, {"source": "TiVo", "lastUpdated": "2023-01-02T03:04:05.000+0000",
                             "text": 'Formed with [wimpLink artistId="21"]Second[/wimpLink] in <b>2001</b>.\n'
                                     'See <a href="https://tidal.com/browse/album/10">the album</a> &amp; '
                                     '<a href="https://tidal.com/browse/track/100">a song</a> or <a href="javascript:x">this</a>.'}
        if rest == ["links"]:
            if key != 20:
                return 404, {}, {"status": 404}
            return 200, {}, {"items": [{"url": "https://example.org/main", "siteName": "Homepage"}, {"url": "https://example.org/wiki"}, {"siteName": "no url"}]}
        if rest == ["toptracks"]:
            return 200, {}, page([TRACKS[100], TRACKS[102]] if key == 20 else [TRACKS[103]], query)
        if rest == ["albums"]:
            groups = {None: [ALBUMS[10], ALBUMS[11]] + [generated_album(600 + index) for index in range(38)],
                      "EPSANDSINGLES": [ALBUMS[11]], "COMPILATIONS": [ALBUMS[12]]}
            items = groups.get(query.get("filter"), [])
            if key != 20:
                items = [ALBUMS[12]] if query.get("filter") is None else []
            if query.get("order") == "DATE":
                items = sorted(items, key=lambda entry: entry.get("releaseDate") or "", reverse=True)
            return 200, {}, page(items, query)
        if rest == ["similar"]:
            return 200, {}, page([ARTISTS[21], ARTISTS[22]], query)
        if rest == ["mix"]:
            return 200, {}, {"id": f"artmix{key}"}
        if rest == ["radio"]:
            return 200, {}, page([TRACKS[100], TRACKS[103]], query, 100)
        return 404, {}, {"status": 404}


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def handle_any(self):
        parsed = urlsplit(self.path)
        segments = parsed.path.lstrip("/").split("/", 1)
        host = segments[0]
        path = "/" + (segments[1] if len(segments) > 1 else "")
        query = dict(parse_qsl(parsed.query, keep_blank_values=True))
        length = int(self.headers.get("Content-Length") or 0)
        body = self.rfile.read(length) if length else b""
        form = dict(parse_qsl(body.decode("utf-8", "replace"), keep_blank_values=True)) if body else {}
        entry = {"method": self.command, "host": host, "path": unquote(path), "query": sorted(parse_qsl(parsed.query, keep_blank_values=True)),
                 "form": sorted(form.items()), "if_none_match": self.headers.get("If-None-Match"),
                 "authorization": self.headers.get("Authorization")}
        self.server.world.record(entry)
        status, headers, value = self.server.world.answer(self.command, host, unquote(path), query, form)
        data = value if isinstance(value, bytes) else json.dumps(value).encode()
        self.send_response(status)
        if not isinstance(value, bytes):
            self.send_header("Content-Type", "application/json")
        for name, header in headers.items():
            self.send_header(name, header)
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        if self.command != "HEAD":
            self.wfile.write(data)

    do_GET = do_POST = do_PUT = do_DELETE = do_HEAD = handle_any


def serve():
    """A started mock: (server, world, base URL). Stop it with server.shutdown()."""
    world = World()
    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    server.daemon_threads = True
    server.world = world
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return server, world, f"http://127.0.0.1:{server.server_port}"


SEGMENT_FRAMES = [frames for _, frames in SEGMENTS]
