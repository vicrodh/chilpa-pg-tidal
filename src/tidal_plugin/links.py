"""What a TIDAL link opens, for Chilpa's "Open music link": the kind and the
reference the pages carry, or None for a link of anywhere else. Chilpa asks
every source and the one that claims the link opens it."""
from urllib.parse import urlparse

# A path segment that names what follows it; a mix is a playlist kind with a
# mix reference, as the cards carry it.
KINDS = {"album": ("album", "album"), "track": ("track", "track"), "artist": ("artist", "artist"),
    "playlist": ("playlist", "playlist"), "mix": ("playlist", "mix")}


def link_target(url):
    parsed = urlparse((url or "").strip())
    host = (parsed.hostname or "").lower()
    if parsed.scheme == "tidal":
        parts = [parsed.netloc] + [part for part in parsed.path.split("/") if part]
    elif parsed.scheme in ("http", "https") and (host == "tidal.com" or host.endswith(".tidal.com")):
        parts = [part for part in parsed.path.split("/") if part]
    else:
        return None
    if parts and parts[0] == "browse":
        parts = parts[1:]
    for at, part in enumerate(parts[:-1]):
        if part in KINDS:
            kind, prefix = KINDS[part]
            return {"kind": kind, "ref": f"{prefix}:{parts[at + 1]}"}
    return None
