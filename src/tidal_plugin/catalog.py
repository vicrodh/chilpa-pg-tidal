"""The catalog renders through the host's existing views and navigation."""
import copy
import hashlib
import json
from urllib.parse import quote, unquote
from .items import card, ident, unbox, reference, shelf
from .state import Failure
from .descriptions import description
from .folders import collection as folder_collection

DECADES = tuple(f"{year}s" for year in range(1950, 2020, 10))


class Catalog:
    def __init__(self, client, cache):
        self.client, self.cache = client, cache
        self.locale = "en_US"

    def get(self, path, params=None, v2=False):
        key = (self.client.user, path, json.dumps(params or {}, sort_keys=True), v2)
        return self.cache.get(key, lambda: self.client.get(path, params, v2))

    def rows(self, raw, kind=None):
        if isinstance(raw, dict):
            raw = raw.get("items", [])
        return [card(v, kind, self.client.user) for v in raw if isinstance(v, dict) and ident(unbox(v))
                and str(v.get("type", "")).lower() != "video"]

    def library(self, params):
        kind = params.get("kind", "albums")
        offset, limit = max(0, int(params.get("offset", 0))), min(500, max(1, int(params.get("limit", 100))))
        if kind not in {"albums", "artists", "tracks", "playlists"}:
            raise Failure("Invalid library category.", -32602)
        if kind == "playlists":
            snapshot = self.cache.get((self.client.user, "folder-collection"), lambda: folder_collection(self))
            return {"items": snapshot["items"][offset:offset + limit], "total": len(snapshot["items"]), "folders": snapshot["folders"]}
        path = f"users/{self.client.user}/" + ("playlistsAndFavoritePlaylists" if kind == "playlists" else f"favorites/{kind}")
        # A host page may exceed the provider's limit. Preserve all requested rows.
        rows, total = [], 0
        while len(rows) < limit:
            count = min(50 if kind == "playlists" else 100, limit - len(rows))
            page = self.get(path, {"limit": count, "offset": offset + len(rows), "order": "DATE", "orderDirection": "DESC"})
            batch = page.get("items", [])
            total = page.get("totalNumberOfItems", offset + len(rows) + len(batch))
            rows.extend(batch)
            if len(batch) < count or offset + len(rows) >= total:
                break
        return {"items": self.rows(rows, kind[:-1]), "total": total}

    def search(self, params):
        text = str(params.get("text", "")).strip()
        keys = ["albums", "artists", "tracks", "playlists"]
        if not text:
            return dict.fromkeys(keys, [])
        result = self.get("search", {"query": text, "limit": min(100, max(1, int(params.get("limit", 20)))),
            "offset": max(0, int(params.get("offset", 0))), "types": "ALBUMS,ARTISTS,TRACKS,PLAYLISTS"})
        answer = {k: self.rows(result.get(k, {}), k[:-1]) for k in keys}
        answer["totals"] = {k: result.get(k, {}).get("totalNumberOfItems", len(answer[k])) for k in keys}
        top = result.get("topHit")
        if isinstance(top, dict) and ident(unbox(top)):
            answer["top"] = card(top, user=self.client.user)
        return answer

    def track_quality(self, params, quality):
        from .streaming import stream_format
        _, key = reference(params.get("ref", ""), "track")
        raw = self.get(f"tracks/{key}/playbackinfopostpaywall", {"playbackmode":"STREAM", "audioquality":quality, "assetpresentation":"FULL"})
        return {"ref":params["ref"], "quality":stream_format(raw)}

    def track(self, params):
        # One track by itself, for a list that holds it without having seen
        # its album or artist: the same item a shelf would carry.
        _, key = reference(params.get("ref", ""), "track")
        return {"track": card(self.get("tracks/" + key), "track")}

    def album(self, params):
        _, key = reference(params.get("ref", ""), "album")
        try:
            raw = self.get("albums/" + key)
        except Failure as error:
            if error.status == 404:
                return {"availability": "unavailable", "tracks": []}
            raise
        tracks = self.cache.get((self.client.user, "album-tracks", key), lambda: self.client.all(f"albums/{key}/tracks"))
        rows = []
        for entry in tracks:
            track = dict(unbox(entry))
            track.setdefault("album", raw)
            rows.append(card(track, "track"))
        return {"album": card(raw, "album"), "tracks": rows, "total": len(rows)}

    def artist(self, params):
        _, key = reference(params.get("ref", ""), "artist")
        raw = self.get("artists/" + key)
        return {"artist": card(raw, "artist"), "albums": self.rows(self.get(f"artists/{key}/albums", {"limit": 100}), "album")}

    def playlist(self, params):
        kind, key = reference(params.get("ref", ""))
        if kind == "radio":
            category, _, key = key.partition(":")
            if category not in {"track", "artist"} or not key.isdigit():
                raise Failure("Invalid radio reference.", -32602)
            path = f"{category}s/{key}/radio"
            tracks = self.rows(self.get(path, {"limit": 100}), "track")
            return {"playlist": {"ref": params["ref"], "title": "Radio", "kind": "playlist"}, "tracks": tracks, "total": len(tracks)}
        if kind == "mix":
            page = self.get("pages/mix", {"mixId": key, "deviceType": "BROWSER"})
            modules = [m for row in page.get("rows", []) for m in row.get("modules", [])]
            info = next((m["mix"] for m in modules if isinstance(m.get("mix"), dict)), {"id": key, "title": "Mix"})
            rows = self.rows(self.cache.get((self.client.user, "mix", key), lambda: self.client.all(f"mixes/{key}/items")), "track")
            return {"playlist": card(info, "mix"), "tracks": rows, "total": len(rows)}
        if kind != "playlist":
            raise Failure("Invalid playlist reference.", -32602)
        raw = self.get("playlists/" + key)
        entries = self.cache.get((self.client.user, "playlist-tracks", key), lambda: self.client.all(f"playlists/{key}/items"))
        # Bind occurrence identity to a snapshot. API writes use indices, never song IDs.
        snapshot = snapshot_id(entries)
        rows = []
        for position, raw_track in enumerate(entries):
            if str(raw_track.get("type", "track")).lower() != "track":
                continue
            item = card(raw_track, "track")
            member = f"{snapshot}:{position}:{ident(unbox(raw_track))}"
            item.update(member=member, ref=item["ref"] + "@playlist:" + key + "@entry:" + member)
            rows.append(item)
        return {"playlist": card(raw, "playlist", self.client.user), "tracks": rows, "total": len(rows)}

    def optional(self, path, params=None):
        try:
            return self.get(path, params)
        except Failure as error:
            if error.status in {404, 400}:
                return {}
            raise

    def page(self, params):
        ref = params.get("ref", "discover:home")
        if params.get("refresh"):
            self.cache.clear()
        if ref.startswith("discover:"):
            return self.discover(ref)
        if ref.startswith("browse:"):
            path = unquote(ref[7:])
            value = self.get(path, {"limit": 50, "deviceType": "BROWSER", "platform": "WEB", "locale": self.locale})
            return self.document(value, path)
        if ref.startswith("artist:"):
            return self.artist_page(ref)
        if ref.startswith("album:"):
            album = self.album({"ref": ref})
            if "album" not in album:
                return album
            result = copy.deepcopy(album["album"])
            key = reference(ref, "album")[1]
            review = self.optional(f"albums/{key}/review")
            result["description"] = description(review.get("text", review.get("review", "")))
            raw = self.get("albums/" + key)
            result["credits"] = [{"id": artist["id"], "name": artist["name"]} for artist in raw.get("artists", []) if artist.get("id") and artist.get("name")]
            result["shelves"] = [shelf("similar", "Related releases", "album", self.rows(self.optional(f"albums/{key}/similar"), "album"))]
            return result
        if ref.startswith("label:"):
            name = unquote(ref[6:])
            # This API has labels as metadata, not a browsable label entity. Restrict
            # the search result to exact label matches, never show unrelated releases.
            raw = self.get("search/albums", {"query": name, "limit": 100})
            rows = [v for v in self.rows(raw, "album") if v.get("label", "").casefold() == name.casefold()]
            return {"title": name, "header": {"kind": "label", "ref": ref, "title": name},
                "shelves": [shelf("releases", "Releases", "album", rows)]}
        if ref.startswith("library:"):
            kind = ref.split(":")[1]
            return {"title": kind.title(), **self.library({"kind": kind, "limit": 500})}
        raise Failure("Unknown catalog page.", -32602)

    def artist_page(self, ref):
        parts = ref.split(":")
        key = parts[1]
        reference("artist:" + key, "artist")
        if len(parts) > 2:
            group = parts[2]
            offset = int(parts[3]) if len(parts) > 3 else 0
            filters = {"albums": None, "singles": "EPSANDSINGLES", "appears": "COMPILATIONS"}
            if group not in filters:
                raise Failure("Unknown release group.", -32602)
            page = self.get(f"artists/{key}/albums", {"limit": 100, "offset": offset, "filter": filters[group]})
            rows = self.rows(page, "album")
            # The next page starts after what came, not a fixed hundred on.
            after = offset + len(rows)
            return {"title": group.title(), "items": rows, "next": f"artist:{key}:{group}:{after}" if rows and after < page.get("totalNumberOfItems", 0) else None}
        raw = self.get("artists/" + key)
        header = card(raw, "artist")
        bio = self.optional(f"artists/{key}/bio")
        header["description"] = description(bio.get("text", ""))
        header["description_source"] = bio.get("source", "")
        if bio.get("lastUpdated"):
            header["bio_updated"] = str(bio["lastUpdated"])[:10]
        # What the artist does, and where it is on the web, when known.
        header["roles"] = [role.get("category", "") for role in raw.get("artistRoles", []) if role.get("category")]
        header["links"] = [{"title": link.get("siteName") or link.get("url", ""), "url": link["url"]}
            for link in self.optional(f"artists/{key}/links").get("items", []) if link.get("url")]
        shelves = [shelf("tracks", "Popular tracks", "track", self.rows(self.optional(f"artists/{key}/toptracks", {"limit": 20}), "track"))]
        for group, title, filter_value in [("albums", "Albums", None), ("singles", "EPs and singles", "EPSANDSINGLES"), ("appears", "Appears on", "COMPILATIONS")]:
            page = self.optional(f"artists/{key}/albums", {"limit": 30, "filter": filter_value})
            rows = self.rows(page, "album")
            more = f"artist:{key}:{group}:{len(rows)}" if len(rows) < page.get("totalNumberOfItems", 0) else None
            shelves.append(shelf(group, title, "album", rows, more))
        # The newest record is the latest release.
        newest = self.rows(self.optional(f"artists/{key}/albums", {"limit": 1, "order": "DATE", "orderDirection": "DESC"}), "album")
        if newest:
            shelves.append(shelf("latest", "Latest release", "album", newest[:1]))
        shelves.append(shelf("similar", "Similar artists", "artist", self.rows(self.optional(f"artists/{key}/similar", {"limit": 20}), "artist")))
        shelves.append(shelf("radio", "Artist radio", "playlist", [{"kind": "playlist", "ref": "radio:artist:" + key, "title": header["title"] + " Radio", "image": header["image"]}]))
        # The artist's mix, when the service made one.
        mix_id = (raw.get("mixes") or {}).get("ARTIST_MIX") or self.optional(f"artists/{key}/mix").get("id")
        if mix_id:
            shelves.append(shelf("mix", "Artist mix", "playlist", [{"kind": "playlist", "ref": "mix:" + str(mix_id), "title": header["title"] + " Mix", "image": header["image"]}]))
        # Who influenced the artist and whom it influenced: the module page
        # lists them as rows of artists under those titles.
        influences = []
        try:
            page = self.get("pages/artist", {"artistId": key, "deviceType": "BROWSER"})
        except Failure:
            page = {}
        for row in page.get("rows", []):
            for module in row.get("modules", []):
                title = str(module.get("title", ""))
                if "influenc" not in title.lower() or not str(module.get("type", "")).startswith("ARTIST"):
                    continue
                for entry in module.get("pagedList", {}).get("items", module.get("items", [])):
                    item = card(entry, "artist")
                    item["role"] = title
                    influences.append(item)
        if influences:
            shelves.append(shelf("influences", "Influences", "artist", influences))
        return {"title": header["title"], "header": header, "shelves": shelves}

    def discover(self, ref):
        parts = ref.split(":")
        tab = parts[1] if len(parts) > 1 else "home"
        paths = {"home": "home/feed/static", "editors": "pages/staff_picks", "foryou": "home/feed/static",
                 "explore": "pages/explore", "new": "pages/explore_new_music", "top": "pages/explore_top_music",
                 "hires": "pages/hires", "mixes": "pages/my_collection_my_mixes",
                 "genres": "pages/genre_page", "moods": "pages/moods_page", "decades": ""}
        if tab not in paths:
            raise Failure("Unknown discovery tab.", -32602)
        path = paths[tab]
        params = {"deviceType": "BROWSER", "platform": "WEB", "limit": 50, "locale": self.locale}
        if tab == "decades":
            decade = parts[2] if len(parts) > 2 else DECADES[0]
            if decade not in DECADES:
                raise Failure("Unknown decade.", -32602)
            path = "pages/m_" + decade
            document = self.document(self.get(path, params), path)
            document.update(title="Decades", tab=tab, personal=False, genre_filter="", genres=[],
                tabs=[{"title": label, "ref": "discover:decades:" + label, "active": label == decade}
                      for label in DECADES] + document.get("tabs", []))
            return document
        genres = self.optional("genres")
        genre_rows = genres if isinstance(genres, list) else genres.get("items", [])
        chosen = unquote(parts[2]) if len(parts) > 2 and parts[2] else ""
        if chosen:
            selected = chosen.split(",")
            if any(g not in {str(g.get("path", g.get("id", ""))) for g in genre_rows} for g in selected):
                raise Failure("Unknown genre.", -32602)
            sections = []
            for genre in selected:
                for kind, title in [("albums", "Releases"), ("playlists", "Playlists")]:
                    value = self.get(f"genres/{quote(genre, safe='')}/{kind}", {"limit": 100})
                    sections.append(shelf("genre:" + genre + ":" + kind, genre + " · " + title, kind[:-1], self.rows(value, kind[:-1])))
            document = {"title": "Music", "shelves": sections}
        else:
            try:
                value = self.get(path, params, v2=tab in {"home", "foryou"})
            except Failure as error:
                if tab != "home" or error.status not in {400, 404}:
                    raise
                value = self.get("pages/home", params)
            if tab in {"home", "foryou"}:
                value = copy.deepcopy(value)
                seen = set()
                cursor = value.get("page", {}).get("cursor")
                while cursor and cursor not in seen:
                    seen.add(cursor)
                    following = self.get(path, {**params, "cursor": cursor}, v2=True)
                    value.setdefault("items", []).extend(following.get("items", []))
                    cursor = following.get("page", {}).get("cursor")
            document = self.document(value, path)
        if tab == "moods":
            excluded = {"record labels", "record label", "tidal magazine", "artist experiences"}
            document["tabs"] = sorted((link for link in document.get("tabs", [])
                if link["title"].strip().casefold() not in excluded), key=lambda link: link["title"].casefold())
        document.update(title="Music", tab=tab, personal=tab in {"home", "foryou", "mixes"}, genre_filter=chosen,
            genres=[{"id": str(g.get("path", g.get("id", ""))), "name": g.get("name", "")} for g in genre_rows]
                if tab in {"home", "editors", "explore", "new", "top", "hires"} else [],
            tabs=[{"title": title, "ref": "discover:" + key, "active": key == ("home" if tab == "foryou" else tab)} for key, title in
                  [("home", "For You"), ("new", "New"), ("top", "Top"), ("editors", "Staff Picks"),
                   ("explore", "Explore"), ("hires", "Hi-Res"), ("mixes", "My Mixes"),
                   ("genres", "Genres"), ("decades", "Decades"), ("moods", "Moods")]] + document.get("tabs", []))
        return document

    def document(self, value, path):
        modules = [m for row in value.get("rows", []) for m in row.get("modules", [])]
        modules = modules or value.get("items", [])
        shelves, flat, links, by_id = [], [], [], {}
        for module in modules:
            module_type = str(module.get("type", ""))
            entries = module.get("pagedList", {}).get("items", module.get("items", []))
            groups, mapped_rows = {}, []
            for entry in entries:
                if str(entry.get("type", "")).lower() == "video":
                    continue
                raw = unbox(entry)
                if not ident(raw):
                    api_path = raw.get("apiPath")
                    if api_path:
                        links.append({"ref": "browse:" + quote(api_path, safe=""), "title": raw.get("title", "")})
                        continue
                    else:
                        continue
                else:
                    kind = next((k for k in ["album", "artist", "playlist", "track", "mix"] if module_type.startswith(k.upper() + "_")), None)
                    mapped = card(entry, kind, self.client.user)
                groups.setdefault(mapped["kind"], []).append(mapped)
                mapped_rows.append(mapped)
            more = (module.get("showMore") or {}).get("apiPath") or module.get("viewAll")
            if isinstance(more, dict):
                more = more.get("apiPath")
            if groups:
                stable = str(module.get("moduleId") or module.get("id") or hashlib.sha256((path + module.get("title", "") + module_type).encode()).hexdigest()[:16])
                if more and path.startswith("home/") and not more.startswith(("/", "https://")):
                    more = "/v2/" + more
                kind = next(iter(groups)) if len(groups) == 1 else "mixed"
                rows = mapped_rows if kind != "mixed" else [row for row in mapped_rows if row["kind"] != "track"]
                section = shelf(stable + ":" + kind, module.get("title", ""), kind, rows, "browse:" + quote(more, safe="") if more else None)
                if kind == "mixed":
                    section["previous_ids"] = [stable + ":" + previous for previous in groups]
                    if "track" in groups:
                        section["extra_tracks"] = groups["track"]
                context = module.get("header")
                if isinstance(context, dict) and ident(unbox(context)):
                    section["header"] = card(context, user=self.client.user)
                if section["id"] in by_id:
                    kept = by_id[section["id"]]
                    for field in ("items", "extra_tracks"):
                        seen = {item["ref"] for item in kept.get(field, [])}
                        for item in section.get(field, []):
                            if item["ref"] not in seen:
                                kept.setdefault(field, []).append(item)
                                seen.add(item["ref"])
                    kept["more"] = section["more"] or kept["more"]
                else:
                    by_id[section["id"]] = section
                    shelves.append(section)
            if not entries and ident(unbox(module)):
                flat.append(card(module, user=self.client.user))
        result = {"title": value.get("title", "Tidal")}
        if links:
            result["tabs"] = links
        if shelves:
            result["shelves"] = shelves
        else:
            result["items"] = flat
        return result


def snapshot_id(rows):
    identities = [(str(v.get("type", "track")), ident(unbox(v))) for v in rows]
    return hashlib.sha256(json.dumps(identities).encode()).hexdigest()[:24]
