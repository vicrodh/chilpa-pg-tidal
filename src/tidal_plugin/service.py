"""Native JSON-RPC source operations, separate from host UI and audio policy."""
from pathlib import Path
import threading
from contextlib import contextmanager
from .account import Account
from .catalog import Catalog
from .client import Client
from .items import card
from .links import link_target
from .lyrics import track_lyrics
from .playlists import Playlists
from .reporting import Reports
from .state import Cache, Failure
from .streaming import Streams, normalization


class Gate:
    def __init__(self):
        self.condition = threading.Condition()
        self.readers = 0
        self.writer = False
        self.waiting = 0

    @contextmanager
    def hold(self, write=False):
        with self.condition:
            if write:
                self.waiting += 1
            self.condition.wait_for(lambda: not self.writer and (self.readers == 0 if write else self.waiting == 0))
            if write:
                self.waiting -= 1
                self.writer = True
            else:
                self.readers += 1
        try:
            yield
        finally:
            with self.condition:
                if write:
                    self.writer = False
                else:
                    self.readers -= 1
                self.condition.notify_all()


class Service:
    def __init__(self, data):
        data = Path(data)
        data.mkdir(parents=True, exist_ok=True, mode=0o700)
        self.client = Client(data)
        self.cache = Cache()
        self.account = Account(self.client, data)
        self.catalog = Catalog(self.client, self.cache)
        self.playlists = Playlists(self.client, self.catalog)
        self.reports = Reports(data, self.client)
        self.streams = Streams(self.client, self.account)
        self.locale = "en"
        self.gate = Gate()
        self.reports.start()

    def handle(self, method, params):
        with self.gate.hold(method in {"settings.submit", "auth.logout"}):
            return self.dispatch(method, params)

    def dispatch(self, method, params):
        if method == "initialize":
            self.locale = params.get("locale", "en")
            self.catalog.locale = self.locale.replace("-", "_")
            return {"protocol": 1, "capabilities": {"auth": "browser", "home": True,
                "navigation": {"discover": "Music", "library": "Collection", "albums": "Albums"},
                "library": ["albums", "tracks", "artists", "playlists"], "pages": True,
                "playlist_edit": True, "track_quality": True, "search": True, "scene_search_limit": 1000,
                "resolve": True, "favorites": True, "playback_events": 1,
                "track_lyrics": 1}}
        if method == "auth.status":
            return self.account.status()
        if method in {"settings.open", "auth.login"}:
            with self.account.lock:
                self.account.shown = False
                self.account.revision += 1
                return self.account.open(self.locale)
        if method == "settings.submit":
            answer = self.account.submit(params, self.locale)
            self.cache.clear()
            if not self.client.user:
                self.streams.clear()
                self.reports.clear()
            return answer
        if method == "auth.logout":
            self.client.logout()
            self.account.pending = None
            self.account.redirect = ""
            self.cache.clear()
            self.streams.clear()
            self.reports.clear()
            return {"signed_in": False}
        if method == "inspect":
            return {"settings_pages": ["account"]}
        if method == "reporting.status":
            return self.reports.status()
        if method == "playback.event":
            return self.reports.event(params)
        if method == "link.resolve":
            # A link of this source, as what to open; None for any other.
            return link_target(params.get("url") or "")
        self.client.token()
        if method == "home":
            return self.catalog.discover("discover:home")
        if method == "page.get":
            result = self.catalog.page(params)
            if params.get("ref") == "discover:foryou" and self.reports.state["history"]:
                result.setdefault("shelves", []).insert(0, {"id": "chilpa-history", "title": "Recently played in Chilpa", "kind": "track", "items": self.reports.state["history"][:20]})
            return result
        methods = {"library.list": self.catalog.library, "search": self.catalog.search, "album.get": self.catalog.album,
            "artist.get": self.catalog.artist, "playlist.get": self.catalog.playlist}
        if method in methods:
            return methods[method](params)
        if method == "track.get":
            return self.catalog.track(params)
        if method == "track.quality":
            return self.catalog.track_quality(params, self.account.preferences["quality"])
        if method == "track.lyrics":
            return track_lyrics(self.catalog, params)
        if method == "track.resolve":
            result = self.streams.resolve(params)
            key = params["ref"].split("@", 1)[0].split(":", 1)[1]
            raw = self.catalog.get("tracks/" + key)
            track = card(raw, "track")
            if result.get("normalization") is None:
                result["normalization"] = normalization({}, raw)
            reporting = result.pop("reporting", {})
            for context in params["ref"].split("@")[1:]:
                if context.startswith("playlist:") and context[9:]:
                    reporting.update(source_type="PLAYLIST", source_id=context[9:])
                    break
            result["resolution_id"] = self.reports.ticket(track, reporting)
            return result
        if method in {"page.action", "favorites.set"}:
            if method == "favorites.set":
                params = {**params, "action": "favorite"}
            try:
                return self.playlists.action(params)
            finally:
                if params.get("action") != "playlist.search":
                    self.cache.clear()
        raise Failure("Unknown plugin operation.", -32601)

    def close(self):
        self.reports.close()
        self.streams.server.shutdown()
        self.streams.server.server_close()
