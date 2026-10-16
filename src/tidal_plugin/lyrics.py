"""The lyrics of one of this source's own tracks, for the host's track.lyrics."""
from .items import reference
from .state import Failure


def track_lyrics(catalog, params):
    """Answers found or none. Every failure stays a Failure: the host asks again later."""
    _, key = reference(params.get("ref", ""), "track")
    try:
        raw = catalog.client.get(f"tracks/{key}/lyrics")
    except Failure as error:
        if error.status == 404:
            return {"status": "none"}
        raise
    if not isinstance(raw, dict):
        raise Failure("Tidal returned unreadable lyrics.")
    timed, plain = text(raw.get("subtitles")), text(raw.get("lyrics"))
    if timed:
        lyrics = {"format": "lrc", "text": timed}
    elif plain:
        lyrics = {"format": "plain", "text": plain}
    else:
        return {"status": "none"}
    answer = {"status": "found", "lyrics": lyrics}
    found = fingerprint(catalog, key)
    if found:
        answer["fingerprint"] = found
    return answer


def text(value):
    return value if isinstance(value, str) and value.strip() else ""


def fingerprint(catalog, key):
    """What the track says it is. Resolving already fetched it, so this is normally cached."""
    try:
        raw = catalog.get("tracks/" + key)
    except Failure:
        return {}
    if not isinstance(raw, dict):
        return {}
    found = {}
    duration = raw.get("duration")
    if isinstance(duration, (int, float)) and not isinstance(duration, bool) and duration > 0:
        found["duration_ms"] = int(float(duration) * 1000)
    if text(raw.get("isrc")):
        found["isrc"] = raw["isrc"].strip()
    return found
