"""Explicit playlist writes with ownership, snapshot and ETag checks."""
import threading
from .catalog import snapshot_id
from .items import reference, ident, unbox
from .state import Failure
from .folders import action as folder_action


class Playlists:
    def __init__(self, client, catalog):
        self.client, self.catalog = client, catalog
        self.lock = threading.Lock()

    def action(self, params):
        action = params.get("action", "")
        if action == "playlist.search":
            return {"read_only": True, "items": self.catalog.search(params)["tracks"]}
        with self.lock:
            if action in {"folder.create", "folder.rename", "playlist.folder"}:
                result = folder_action(self.client, params)
                self.catalog.cache.clear()
                return result
            if action in {"playlist.create", "playlist.copy"}:
                return self.create(params, action == "playlist.copy")
            ref = params.get("ref", "")
            kind, key = reference(ref)
            if action == "favorite":
                return self.favorite(kind, key, params.get("on") is True)
            if kind != "playlist":
                raise Failure("This action needs a playlist.", -32602)
            raw = self.client.get("playlists/" + key)
            if str(raw.get("creator", {}).get("id", "")) != self.client.user or raw.get("type") == "EDITORIAL":
                raise Failure("Only your own playlists can be edited.")
            path = "playlists/" + key
            if action == "playlist.update":
                self.client.request(path, method="POST", data={"title": title(params), "description": str(params.get("description", ""))})
                self.visibility(key, params.get("public", False))
            elif action == "playlist.delete":
                self.client.request(path, method="DELETE")
            elif action in {"playlist.add", "playlist.remove", "playlist.move"}:
                # Read metadata and items under one version; fetching both sides catches
                # edits racing a paginated read. The mutation is conditioned on that ETag.
                _, before = self.client.request(path, {"limit": 1})
                entries = self.client.all(path + "/items")
                _, after = self.client.request(path, {"limit": 1})
                if not before or before != after:
                    raise Failure("The playlist changed. Refresh it before editing.")
                if action == "playlist.add":
                    tracks = params.get("tracks", [])
                    if not isinstance(tracks, list) or not 1 <= len(tracks) <= 100:
                        raise Failure("Choose between 1 and 100 tracks.", -32602)
                    ids = [reference(t, "track")[1] for t in tracks]
                    self.client.request(path + "/items", method="POST", etag=after,
                        data={"trackIds": ",".join(ids), "onDupes": "ADD", "onArtifactNotFound": "FAIL"})
                else:
                    indices = members(params.get("members"), entries)
                    path += "/items/" + ",".join(str(i) for i in indices)
                    if action == "playlist.remove":
                        self.client.request(path, method="DELETE", etag=after)
                    else:
                        position = params.get("before")
                        if not isinstance(position, int) or not 0 <= position <= len(entries):
                            raise Failure("Invalid destination position.", -32602)
                        self.client.request(path, method="POST", etag=after, data={"toIndex": position})
            else:
                raise Failure("Unknown playlist action.", -32602)
            return {"changed_refs": [ref]}

    def visibility(self, key, public):
        self.client.request(f"playlists/{key}/set-{'public' if public else 'private'}", method="PUT", v2=True)

    def create(self, params, copy):
        tracks = []
        if copy:
            _, key = reference(params.get("ref", ""), "playlist")
            tracks = [ident(unbox(v)) for v in self.client.all(f"playlists/{key}/items") if v.get("type", "track").lower() == "track"]
        result, _ = self.client.request("my-collection/playlists/folders/create-playlist", method="PUT", v2=True,
            params={"name": title(params), "description": str(params.get("description", "")), "folderId": "root"})
        key = ident(result.get("data", result))
        if not key:
            raise Failure("Tidal did not identify the new playlist. Refresh your library before retrying.")
        try:
            self.visibility(key, params.get("public", False))
            for start in range(0, len(tracks), 100):
                _, etag = self.client.request("playlists/" + key)
                if not etag:
                    raise Failure("Tidal did not return a playlist version.")
                self.client.request(f"playlists/{key}/items", method="POST", etag=etag,
                    data={"trackIds": ",".join(tracks[start:start+100]), "onDupes": "ADD", "onArtifactNotFound": "FAIL"})
        except Failure:
            self.catalog.cache.clear()
            raise Failure(f"Playlist {key} was created, but not all changes completed. Refresh your library and inspect it before retrying.") from None
        return {"changed_refs": ["playlist:" + key], "ref": "playlist:" + key}

    def favorite(self, kind, key, on):
        if kind not in {"album", "artist", "track", "playlist"}:
            raise Failure("This item cannot be saved as a favorite.", -32602)
        path = f"users/{self.client.user}/favorites/{kind}s"
        if on:
            self.client.request(path, method="POST", data={"uuids" if kind == "playlist" else kind + "Ids": key, "onArtifactNotFound": "FAIL"})
        else:
            self.client.request(path + "/" + key, method="DELETE")
        return {"changed_refs": [kind + ":" + key]}


def title(params):
    value = str(params.get("title", "")).strip()
    if not value or len(value) > 250:
        raise Failure("Enter a playlist name of at most 250 characters.", -32602)
    return value


def members(values, entries):
    if not isinstance(values, list) or not values:
        raise Failure("Choose playlist entries first.", -32602)
    expected = snapshot_id(entries)
    positions = []
    for value in values:
        try:
            snapshot, position, track = str(value).split(":")
            position = int(position)
        except (ValueError, TypeError):
            raise Failure("Invalid playlist entry.", -32602) from None
        if snapshot != expected or not 0 <= position < len(entries) or ident(unbox(entries[position])) != track:
            raise Failure("The playlist changed. Refresh it before editing.")
        positions.append(position)
    return sorted(set(positions))
