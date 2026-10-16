"""Tidal entities mapped to Chilpa's shared album, artist and track models."""
from datetime import datetime
from urllib.parse import quote
from .descriptions import description


def ident(value):
    return str(value.get("id") or value.get("uuid") or "")


def unbox(value):
    return value.get("item") or value.get("data") or value.get("playlist") or value.get("value") or value


def image(value, kind="album"):
    key = value.get("cover") or value.get("squareImage") or value.get("image") or value.get("picture")
    if not key and kind == "track":
        return image(value.get("album", {}))
    if isinstance(key, dict):
        key = key.get("MEDIUM", {}).get("url") or key.get("url")
    if not key:
        images = value.get("images") or value.get("mixImages") or {}
        if isinstance(images, list):
            key = next((v.get("url") for v in images if isinstance(v, dict) and v.get("url")), None)
        if isinstance(images, dict):
            key = next((v.get("url") for v in images.values() if isinstance(v, dict) and v.get("url")), None)
    if not isinstance(key, str):
        return ""
    if key.startswith("https://"):
        return key
    size = "750x750" if kind == "artist" else "640x640"
    return "https://resources.tidal.com/images/" + key.replace("-", "/") + "/" + size + ".jpg"


def stamp(value):
    try:
        return int(datetime.fromisoformat(str(value).replace("Z", "+00:00")).timestamp())
    except (ValueError, TypeError):
        return None


def card(raw, kind=None, user=""):
    value = unbox(raw)
    hint = str(raw.get("type", "")).lower()
    kind = kind or (hint if hint in {"album", "artist", "track", "playlist", "mix"} else None)
    if not kind:
        kind = "playlist" if "uuid" in value else "track" if "trackNumber" in value else "album" if "numberOfTracks" in value else "artist"
    key = ident(value)
    reference = f"{kind}:{key}"
    title = value.get("title") or value.get("name") or value.get("titleTextInfo", {}).get("text") or ""
    version = value.get("version")
    if version and version not in title:
        title += " (" + version + ")"
    artist = value.get("artist") or next(iter(value.get("artists", [])), {})
    album = value.get("album") or {}
    result = {"kind": "playlist" if kind == "mix" else kind, "ref": reference,
        "title": title, "image": image(value, kind), "artist": artist.get("name", ""),
        "description": description(value.get("description", "")), "explicit": bool(value.get("explicit", False))}
    if kind == "album":
        genre = value.get("genre")
        if isinstance(genre, str) and genre.strip():
            result["genre"] = genre.strip()
        else:
            genres = value.get("genres")
            if isinstance(genres, list):
                names = [row.strip() for row in genres if isinstance(row, str) and row.strip()]
                if names:
                    result["genre"] = ", ".join(dict.fromkeys(names))
    if ident(artist):
        result["artist_ref"] = "artist:" + ident(artist)
    if ident(album):
        result.update(album_ref="album:" + ident(album), album=album.get("title", ""))
    for source, target in [("streamReady", "available"), ("copyright", "copyright"), ("upc", "upc"), ("isrc", "isrc"),
                           ("numberOfTracks", "tracks"), ("trackNumber", "track_number"), ("volumeNumber", "disc_number")]:
        if source in value:
            result[target] = value[source]
    if value.get("duration") is not None:
        result["duration_ms"] = int(float(value["duration"]) * 1000)
    date = value.get("releaseDate") or album.get("releaseDate")
    if date:
        result["release_date"] = date
        try:
            result["year"] = int(date[:4])
        except (ValueError, TypeError):
            pass
    if value.get("type") in {"ALBUM", "EP", "SINGLE", "COMPILATION"}:
        result["release_type"] = value["type"].lower()
    label = value.get("label")
    if label:
        result["label"] = label.get("name", "") if isinstance(label, dict) else str(label)
        result["label_ref"] = "label:" + quote(result["label"], safe="")
    added = stamp(raw.get("created") or raw.get("createdAt") or value.get("dateAdded"))
    if added is not None:
        result["added_at"] = added
    if kind == "playlist":
        creator = value.get("creator") or {}
        result.update(editable=bool(user and str(creator.get("id", "")) == user and value.get("type") != "EDITORIAL"),
            copyable=True, public=bool(value.get("publicPlaylist", value.get("public", False))),
            updated_at=stamp(value.get("lastUpdated")), artist=creator.get("name", creator.get("username", "")))
    quality = audio_quality(value)
    if quality:
        result["quality"] = quality
    return result


def audio_quality(value):
    """Catalog tiers describe availability, not a measured stream format."""
    level = value.get("audioQuality")
    tags = (value.get("mediaMetadata") or {}).get("tags") or []
    if "HIRES_LOSSLESS" in tags or level == "HI_RES_LOSSLESS":
        return {"codec": "flac", "tier": "hi_res"}
    if "LOSSLESS" in tags or level == "LOSSLESS":
        return {"codec": "flac"}
    if level in {"HIGH", "LOW"}:
        return {"codec": "aac"}
    return None


def reference(value, expected=None):
    from .state import Failure
    base = str(value).split("@", 1)[0]
    kind, sep, key = base.partition(":")
    if not sep or not key or (expected and kind != expected) or any(c in key for c in "/?#\\"):
        raise Failure("Invalid source reference.", -32602)
    return kind, key


def shelf(key, title, kind, rows, more=None):
    return {"id": key, "title": title, "kind": kind, "items": rows, "more": more, "sortable": kind == "album"}
