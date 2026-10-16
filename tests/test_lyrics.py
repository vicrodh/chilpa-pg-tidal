"""track.lyrics against recorded shapes: no account, no network, placeholder text."""
import io
import json
import sys
import tempfile
import unittest
from unittest.mock import Mock, patch
import main
from tidal_plugin.catalog import Catalog
from tidal_plugin.client import Client
from tidal_plugin.lyrics import track_lyrics
from tidal_plugin.service import Service
from tidal_plugin.state import Cache, Failure
from test_contract import Fixtures

TIMED = "[00:01.00]Placeholder line 1\n[00:05.50]Placeholder line 2\n"
PLAIN = "Placeholder line 1\nPlaceholder line 2\n"
TRACK = {"id": 7, "title": "Song", "duration": 215, "isrc": "USABC1234567"}


def response(status, headers=None):
    answer = Mock()
    answer.status_code, answer.headers = status, headers or {}
    return answer


class Lyrics(unittest.TestCase):
    def setUp(self):
        self.client = Fixtures()
        self.client.responses["tracks/7"] = TRACK
        self.catalog = Catalog(self.client, Cache())

    def lyrics(self, **fields):
        self.client.responses["tracks/7/lyrics"] = {"trackId": 7, "lyricsProvider": "Provider", "isRightToLeft": False, **fields}
        return track_lyrics(self.catalog, {"ref": "track:7"})

    def test_timed_lyrics_are_preferred_and_carry_the_fingerprint(self):
        answer = self.lyrics(subtitles=TIMED, lyrics=PLAIN)
        self.assertEqual(answer, {"status": "found", "lyrics": {"format": "lrc", "text": TIMED},
                                  "fingerprint": {"duration_ms": 215000, "isrc": "USABC1234567"}})

    def test_text_without_timings_is_sent_as_plain(self):
        for empty in ("", "   ", None):
            answer = self.lyrics(subtitles=empty, lyrics=PLAIN)
            self.assertEqual(answer["lyrics"], {"format": "plain", "text": PLAIN})

    def test_a_missing_or_empty_document_is_none(self):
        self.client.responses["tracks/7/lyrics"] = Failure("No lyrics", status=404)
        self.assertEqual(track_lyrics(self.catalog, {"ref": "track:7"}), {"status": "none"})
        self.assertEqual(self.lyrics(subtitles="", lyrics=None), {"status": "none"})
        self.assertEqual([path for path, *_ in self.client.calls if path == "tracks/7"], [], "nothing else is asked without lyrics")

    def test_a_failure_is_never_none(self):
        for failure in (Failure("Limited", status=429, data={"retry_after_ms": 2000}), Failure("Down", status=503),
                        Failure("Sign in to Tidal again.", -32001, 401), Failure("Tidal could not be reached. Try again.")):
            self.client.responses["tracks/7/lyrics"] = failure
            with self.assertRaises(Failure) as raised:
                track_lyrics(self.catalog, {"ref": "track:7"})
            self.assertIs(raised.exception, failure)
        self.client.responses["tracks/7/lyrics"] = ["not", "an", "object"]
        with self.assertRaises(Failure):
            track_lyrics(self.catalog, {"ref": "track:7"})

    def test_lyrics_survive_a_track_that_cannot_be_read(self):
        self.client.responses["tracks/7"] = Failure("Down", status=503)
        answer = self.lyrics(subtitles=TIMED)
        self.assertEqual(answer, {"status": "found", "lyrics": {"format": "lrc", "text": TIMED}})
        self.client.responses["tracks/7"] = {"id": 7, "duration": 0, "isrc": " "}
        self.assertNotIn("fingerprint", self.lyrics(subtitles=TIMED), "an unknown length or code is left out, not sent as zero")

    def test_the_reference_is_the_one_resolve_takes_and_the_language_is_ignored(self):
        self.client.responses["tracks/7/lyrics"] = {"subtitles": TIMED}
        answer = track_lyrics(self.catalog, {"ref": "track:7@album:4", "language": "es"})
        self.assertEqual(answer["status"], "found")
        self.assertEqual(self.client.calls[0][:2], ("tracks/7/lyrics", None))
        for bad in ("album:4", "track:7/../x", "", "7"):
            with self.assertRaises(Failure) as raised:
                track_lyrics(self.catalog, {"ref": bad})
            self.assertEqual(raised.exception.code, -32602)

    def test_the_track_is_read_once_between_resolve_and_lyrics(self):
        self.catalog.get("tracks/7")
        self.lyrics(subtitles=TIMED)
        self.assertEqual(len([path for path, *_ in self.client.calls if path == "tracks/7"]), 1)

    def test_a_limited_response_keeps_the_wait_it_asks_for(self):
        with self.assertRaises(Failure) as raised:
            Client.checked(response(429, {"Retry-After": "7"}))
        self.assertEqual((raised.exception.status, raised.exception.data), (429, {"retry_after_ms": 7000}))
        with self.assertRaises(Failure) as raised:
            Client.checked(response(429, {"Retry-After": "86400"}))
        self.assertEqual(raised.exception.data, {"retry_after_ms": 3600000}, "a long wait is capped at an hour")
        for headers in ({}, {"Retry-After": "Wed, 21 Oct 2026 07:28:00 GMT"}, {"Retry-After": "0"}):
            with self.assertRaises(Failure) as raised:
                Client.checked(response(429, headers))
            self.assertIsNone(raised.exception.data)
        with self.assertRaises(Failure) as raised:
            Client.checked(response(404, {"Retry-After": "7"}))
        self.assertIsNone(raised.exception.data)

    def test_the_service_announces_and_routes_it(self):
        with tempfile.TemporaryDirectory() as folder, patch("requests.Session.request", side_effect=AssertionError("network")):
            service = Service(folder)
            try:
                self.assertEqual(service.handle("initialize", {})["capabilities"]["track_lyrics"], 1)
                with self.assertRaises(Failure) as raised:
                    service.handle("track.lyrics", {"ref": "track:7"})
                self.assertEqual(raised.exception.code, -32001, "signed out is an error, not none")
                service.client = self.client
                service.client.token = lambda: ("", "")
                service.catalog = self.catalog
                self.client.responses["tracks/7/lyrics"] = {"lyrics": PLAIN}
                self.assertEqual(service.handle("track.lyrics", {"ref": "track:7"})["lyrics"]["format"], "plain")
            finally:
                service.close()

    def test_the_wait_travels_in_the_error_data(self):
        class Limited:
            def __init__(self, _):
                pass
            def handle(self, method, _):
                if method == "track.lyrics":
                    raise Failure("Tidal is limiting requests. Try again shortly.", status=429, data={"retry_after_ms": 1500})
                raise Failure("Unknown plugin operation.", -32601)
            def close(self):
                pass
        requests = "".join(json.dumps({"jsonrpc": "2.0", "id": i, "method": m, "params": {}}) + "\n" for i, m in enumerate(["track.lyrics", "other"]))
        out = io.StringIO()
        with patch.object(main, "Service", Limited), patch.object(sys, "argv", ["plugin"]), \
                patch.object(sys, "stdin", io.StringIO(requests)), patch.object(sys, "stdout", out):
            main.main()
        replies = {reply["id"]: reply["error"] for reply in map(json.loads, out.getvalue().splitlines())}
        self.assertEqual(replies[0]["data"], {"retry_after_ms": 1500, "status": 429})
        self.assertEqual(replies[0]["code"], -32000)
        self.assertNotIn("data", replies[1], "an error without a wait carries no data")


if __name__ == "__main__":
    unittest.main()
