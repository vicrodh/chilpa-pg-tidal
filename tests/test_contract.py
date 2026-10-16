"""Provider fixtures exercise host contracts without a real account."""
import copy
import json
import tempfile
import threading
import time
import unittest
from pathlib import Path
from unittest.mock import Mock, patch
from urllib.parse import parse_qs, urlsplit
from tidal_plugin.account import Account
from tidal_plugin.catalog import Catalog, snapshot_id
from tidal_plugin.client import Client
from tidal_plugin.items import card, reference
from tidal_plugin.descriptions import description
from tidal_plugin.playlists import Playlists, members
from tidal_plugin.reporting import Reports
from tidal_plugin.service import Service
from tidal_plugin.state import Cache, Failure
from tidal_plugin.streaming import normalization, stream_url, template_url

TRACK = {"id": 7, "title": "Song", "artist": {"id": 3, "name": "Artist"}, "album": {"id": 4, "title": "Record", "cover": "ab-cd"},
         "duration": 120, "trackNumber": 2, "volumeNumber": 1, "streamReady": False}


class Fixtures:
    user = "5"
    def __init__(self):
        self.calls = []
        self.entries = [{"type": "track", "item": TRACK}, {"type": "track", "item": TRACK}]
        self.responses = {"playlists/p": {"uuid": "p", "title": "List", "creator": {"id": 5}},
            "genres": [{"path": "rock", "name": "Rock"}, {"path": "jazz", "name": "Jazz"}]}
    def get(self, path, params=None, v2=False):
        self.calls.append((path, params, v2))
        value = self.responses.get(path, {"items": [], "totalNumberOfItems": 0})
        if isinstance(value, Exception):
            raise value
        return copy.deepcopy(value)
    def all(self, path, params=None):
        return copy.deepcopy(self.entries)
    def request(self, path, params=None, method="GET", **kwargs):
        self.calls.append((path, method, kwargs))
        return self.responses.get(path, {}), '"v1"'


class Contract(unittest.TestCase):
    def test_album_genre_is_forwarded_only_when_tidal_supplies_it(self):
        raw = {"id": 4, "title": "Record", "genre": " Rock "}
        self.assertEqual(card(raw, "album")["genre"], "Rock")
        raw.pop("genre")
        raw["genres"] = ["Jazz", "Jazz", "Soul"]
        self.assertEqual(card(raw, "album")["genre"], "Jazz, Soul")
        raw.pop("genres")
        self.assertNotIn("genre", card(raw, "album"))

    def test_resolution_keeps_playlist_origin_for_listening_reports(self):
        service = Service.__new__(Service)
        service.client = Mock()
        service.streams = Mock()
        service.streams.resolve.return_value = {"normalization": {}, "reporting": {"quality": "LOSSLESS"}}
        service.catalog = Mock()
        service.catalog.get.return_value = TRACK
        service.reports = Mock()
        service.dispatch("track.resolve", {"ref": "track:7@playlist:fixture-list@entry:2"})
        extra = service.reports.ticket.call_args.args[1]
        self.assertEqual(extra, {"quality": "LOSSLESS", "source_type": "PLAYLIST", "source_id": "fixture-list"})

    def test_track_isrc_is_exposed_for_cross_source_matching(self):
        self.assertEqual(card(dict(TRACK, isrc="USABC1234567"), "track")["isrc"], "USABC1234567")

    def test_provider_top_hit_preserves_its_artist_identity(self):
        self.client.responses["search"] = {"topHit": {"type": "ARTISTS", "value": {"id": 9, "name": "Band", "picture": "ab-cd"}}}
        result = self.catalog.search({"text": "Band"})
        self.assertEqual(result["top"]["ref"], "artist:9")
        self.assertEqual(result["top"]["kind"], "artist")

    def test_biography_links_are_native_and_images_are_not_embedded(self):
        rendered = description('The [wimpLink artistId="9"]Band & Friends[/wimpLink]\n<b>Live</b><img src="https://example.org/x">')
        self.assertEqual(rendered, 'The <a href="artist:9">Band &amp; Friends</a><br><b>Live</b>')

    def test_web_biography_links_open_native_entities(self):
        rendered = description('<a href="https://listen.tidal.com/album/23?x=1">Record</a><a href="/browse/artist/9">Band</a>')
        self.assertIn('href="album:23"', rendered)
        self.assertIn('href="artist:9"', rendered)

    def test_missing_artist_sections_do_not_hide_the_artist(self):
        self.client.responses["artists/9"] = {"id": 9, "name": "Walküre"}
        for section in ["toptracks", "similar", "albums"]:
            self.client.responses["artists/9/" + section] = Failure("Missing section", status=404)
        page = self.catalog.artist_page("artist:9")
        self.assertEqual(page["header"]["title"], "Walküre")
        self.client.responses["artists/9"] = Failure("Missing artist", status=404)
        self.catalog.cache.clear()
        with self.assertRaises(Failure):
            self.catalog.artist_page("artist:9")

    def test_walkure_keeps_releases_when_bio_and_similar_are_missing(self):
        self.client.responses.update({
            "artists/46307078": {"id":46307078, "name":"Walkure"},
            "artists/46307078/bio": Failure("Missing bio", status=404),
            "artists/46307078/similar": Failure("Missing similar artists", status=404),
            "artists/46307078/toptracks": {"items":[TRACK]},
            "artists/46307078/albums": {"items":[{"id":4,"title":"Release","artist":{"id":46307078,"name":"Walkure"}}]},
        })
        page = self.catalog.artist_page("artist:46307078")
        self.assertEqual(page["header"]["ref"], "artist:46307078")
        shelves = {s["id"]: s for s in page["shelves"]}
        self.assertEqual(len(shelves["tracks"]["items"]), 1)
        self.assertEqual(shelves["albums"]["items"][0]["ref"], "album:4")
        self.assertEqual(shelves["similar"]["items"], [])
        self.catalog.cache.clear()
        self.client.responses["artists/46307078/similar"] = Failure("Limited", status=429)
        with self.assertRaises(Failure) as caught:
            self.catalog.artist_page("artist:46307078")
        self.assertEqual(caught.exception.status, 429)

    def test_artist_page_carries_roles_links_mix_latest_and_influences(self):
        self.client.responses.update({
            "artists/9": {"id": 9, "name": "Walküre", "artistRoles": [{"categoryId": 1, "category": "Composer"}], "mixes": {"ARTIST_MIX": "m1"}},
            "artists/9/bio": {"text": "Born.", "source": "TiVo", "lastUpdated": "2026-01-02T03:04:05.000+0000"},
            "artists/9/links": {"items": [{"url": "https://example.org", "siteName": "Site"}]},
            "artists/9/albums": {"items": [{"id": 4, "title": "Release", "artist": {"id": 9, "name": "Walküre"}}], "totalNumberOfItems": 31},
            "pages/artist": {"rows": [{"modules": [{"type": "ARTIST_LIST", "title": "Influencers", "pagedList": {"items": [{"id": 12, "name": "Elder"}]}},
                {"type": "ARTIST_LIST", "title": "Similar Artists", "pagedList": {"items": [{"id": 13, "name": "Peer"}]}}]}]},
        })
        page = self.catalog.artist_page("artist:9")
        header = page["header"]
        self.assertEqual(header["roles"], ["Composer"])
        self.assertEqual(header["links"], [{"title": "Site", "url": "https://example.org"}])
        self.assertEqual(header["bio_updated"], "2026-01-02")
        shelves = {s["id"]: s for s in page["shelves"]}
        self.assertEqual(shelves["mix"]["items"][0]["ref"], "mix:m1")
        self.assertEqual(shelves["latest"]["items"][0]["ref"], "album:4")
        self.assertEqual([i["title"] for i in shelves["influences"]["items"]], ["Elder"], "only the influence rows, not the similar ones")
        self.assertEqual(shelves["influences"]["items"][0]["role"], "Influencers")
        self.assertEqual(shelves["albums"]["more"], "artist:9:albums:1", "the next page starts after what came")
        more = self.catalog.artist_page("artist:9:albums:1")
        self.assertEqual(more["next"], "artist:9:albums:2")

    def test_a_track_alone_is_the_item_a_shelf_would_carry(self):
        self.client.responses["tracks/7"] = TRACK
        item = self.catalog.track({"ref": "track:7@playlist:p"})["track"]
        self.assertEqual(item["ref"], "track:7")
        self.assertEqual(item["artist_ref"], "artist:3")
        self.assertEqual(item["album_ref"], "album:4")

    def test_artist_portraits_use_the_artist_cdn_size(self):
        self.assertTrue(card({"id": 1, "name": "Artist", "picture": "ab-cd"}, "artist")["image"].endswith("/ab/cd/750x750.jpg"))
        self.assertTrue(card({"id": 2, "title": "Record", "cover": "ab-cd"}, "album")["image"].endswith("/ab/cd/640x640.jpg"))
        self.assertTrue(card(TRACK, "track")["image"].endswith("/ab/cd/640x640.jpg"))

    def test_catalog_quality_uses_media_tags_without_inventing_resolution(self):
        raw = {"id": 1, "title": "Record", "audioQuality": "LOSSLESS",
               "mediaMetadata": {"tags": ["LOSSLESS", "HIRES_LOSSLESS"]}}
        self.assertEqual(card(raw, "album")["quality"], {"codec": "flac", "tier": "hi_res"})
        self.assertEqual(card(raw, "track")["quality"], {"codec": "flac", "tier": "hi_res"})
        raw["mediaMetadata"]["tags"] = ["LOSSLESS"]
        self.assertEqual(card(raw, "album")["quality"], {"codec": "flac"})
        raw.update(audioQuality="HIGH", mediaMetadata=None)
        self.assertEqual(card(raw, "album")["quality"], {"codec": "aac"})
        raw.pop("audioQuality")
        self.assertNotIn("quality", card(raw, "album"))

    def setUp(self):
        self.client = Fixtures()
        self.catalog = Catalog(self.client, Cache())

    def test_track_context_availability_and_album_links(self):
        row = card(TRACK, "track")
        self.assertEqual((row["artist_ref"], row["album_ref"], row["track_number"], row["disc_number"]), ("artist:3", "album:4", 2, 1))
        self.assertFalse(row["available"])
        self.assertEqual(row["duration_ms"], 120000)
        self.assertEqual(reference("track:7@playlist:p@entry:one", "track"), ("track", "7"))

    def test_occurrences_are_distinct_and_stale_indices_are_rejected(self):
        page = self.catalog.playlist({"ref": "playlist:p"})
        a, b = page["tracks"]
        self.assertNotEqual(a["ref"], b["ref"])
        self.assertEqual(members([b["member"]], self.client.entries), [1])
        self.client.entries.append({"item": {"id": 99}})
        with self.assertRaises(Failure):
            members([b["member"]], self.client.entries)

    def test_playlist_owner_is_checked_before_writes(self):
        self.client.responses["playlists/p"]["creator"]["id"] = 9
        with self.assertRaises(Failure):
            Playlists(self.client, self.catalog).action({"action": "playlist.delete", "ref": "playlist:p"})
        self.assertFalse(any(call[1] == "DELETE" for call in self.client.calls))

    def test_move_uses_occurrence_and_etag(self):
        item = self.catalog.playlist({"ref": "playlist:p"})["tracks"][1]
        Playlists(self.client, self.catalog).action({"action": "playlist.move", "ref": "playlist:p", "members": [item["member"]], "before": 0})
        self.assertIn(("playlists/p/items/1", "POST", {"etag": '"v1"', "data": {"toIndex": 0}}), self.client.calls)

    def test_only_terminal_not_found_marks_album_unavailable(self):
        self.client.responses["albums/4"] = Failure("gone", status=404)
        self.assertEqual(self.catalog.album({"ref": "album:4"})["availability"], "unavailable")
        self.client.responses["albums/4"] = Failure("limited", status=429)
        with self.assertRaises(Failure):
            self.catalog.album({"ref": "album:4"})

    def test_search_categories_are_not_flattened(self):
        self.client.responses["search"] = {"tracks": {"items": [TRACK], "totalNumberOfItems": 80}, "artists": {"items": [{"id": 3, "name": "Artist"}]}}
        result = self.catalog.search({"text": "Artist"})
        self.assertEqual(result["artists"][0]["ref"], "artist:3")
        self.assertEqual(result["tracks"][0]["album_ref"], "album:4")
        self.assertEqual(result["totals"]["tracks"], 80)

    def test_library_pages_continue_past_provider_limit(self):
        def get(path, params=None, v2=False):
            offset, limit = params["offset"], params["limit"]
            return {"items": [{"item": {"id": n, "title": "Album"}} for n in range(offset+1, min(offset+limit, 230)+1)], "totalNumberOfItems": 230}
        self.client.get = get
        result = self.catalog.library({"kind": "albums", "limit": 500})
        self.assertEqual((len(result["items"]), result["total"]), (230, 230))

    def test_playlist_collection_unwraps_and_uses_its_50_item_limit(self):
        def get(path, params=None, v2=False):
            self.assertLessEqual(params["limit"], 50)
            offset, limit = params["offset"], params["limit"]
            self.assertTrue(v2)
            self.assertEqual(path,"my-collection/playlists/folders")
            return {"items": [{"itemType":"PLAYLIST", "data": {"uuid": str(n), "title": "List", "creator": {"id": 5}}} for n in range(offset+1, min(offset+limit, 73)+1)], "totalNumberOfItems": 73}
        self.client.get = get
        result = self.catalog.library({"kind": "playlists", "limit": 500})
        self.assertEqual(len(result["items"]), 73)
        self.assertTrue(all(row["editable"] for row in result["items"]))

    def test_v1_and_v2_home_preserve_mixed_shelves_and_more(self):
        raw = {"items": [{"moduleId": "first", "type": "HORIZONTAL_LIST", "title": "For you", "viewAll": "pages/full", "items": [
            {"type": "ALBUM", "data": {"id": 4, "title": "Record"}}, {"type": "ARTIST", "data": {"id": 3, "name": "Artist"}}]}]}
        page = self.catalog.document(raw, "home")
        self.assertEqual(len(page["shelves"]), 1)
        self.assertEqual(page["shelves"][0]["kind"], "mixed")
        self.assertEqual([row["ref"] for row in page["shelves"][0]["items"]], ["album:4", "artist:3"])
        self.assertEqual(page["shelves"][0]["previous_ids"], ["first:album", "first:artist"])
        self.assertTrue(all(s["more"] == "browse:pages%2Ffull" for s in page["shelves"]))
        page = self.catalog.document({"rows": [{"modules": [{"type": "TRACK_LIST", "title": "Popular", "pagedList": {"items": [TRACK]}}]}]}, "page")
        self.assertEqual(page["shelves"][0]["items"][0]["ref"], "track:7")

    def test_interest_module_keeps_all_kinds_in_one_customizable_section(self):
        module = {"moduleId": "interest", "type": "HORIZONTAL_LIST", "title": "Classic Metal & more",
            "viewAll": "home/pages/interest/view-all", "items": [
                {"type": "ARTIST", "data": {"id": 3, "name": "Artist"}},
                {"type": "PLAYLIST", "data": {"uuid": "list", "title": "List"}},
                {"type": "ALBUM", "data": {"id": 4, "title": "Record"}}, {"type": "TRACK", "data": TRACK}]}
        page = self.catalog.document({"items": [module]}, "home/feed/static")
        self.assertEqual(len(page["shelves"]), 1)
        section = page["shelves"][0]
        self.assertEqual(section["id"], "interest:mixed")
        self.assertEqual([row["ref"] for row in section["items"]], ["artist:3", "playlist:list", "album:4"])
        self.assertEqual(section["extra_tracks"][0]["ref"], "track:7")
        self.assertEqual(set(section["previous_ids"]), {"interest:artist", "interest:playlist", "interest:album", "interest:track"})
        self.assertFalse(section["sortable"])

    def test_feed_repeated_module_merges_records_without_collapsing_shared_titles(self):
        first = {"moduleId": "one", "type": "ALBUM_LIST", "title": "Albums", "items": [{"id": 4, "title": "Record"}]}
        repeated = dict(first, items=[{"id": 4, "title": "Record"}, {"id": 5, "title": "Another"}])
        second = dict(first, moduleId="two")
        page = self.catalog.document({"items": [first, repeated, second]}, "home/feed/static")
        self.assertEqual([section["id"] for section in page["shelves"]], ["one:album", "two:album"])
        self.assertEqual([row["ref"] for row in page["shelves"][0]["items"]], ["album:4", "album:5"])

    def test_music_uses_full_feed_and_real_editorial_page(self):
        def get(path, params=None, v2=False):
            if path == "genres":
                return []
            if path == "pages/staff_picks":
                return {"rows": []}
            self.assertEqual(path, "home/feed/static")
            self.assertTrue(v2)
            cursor = (params or {}).get("cursor")
            return {"page": {"cursor": None if cursor else "next"}, "items": [{
                "moduleId": "second" if cursor else "first", "type": "HORIZONTAL_LIST",
                "title": "Albums", "viewAll": "home/pages/albums/view-all",
                "header": {"type": "ALBUM", "data": {"id": 4, "title": "Context"}},
                "items": [{"type": "ALBUM", "data": {"id": 5 if cursor else 4, "title": "Record"}}]}]}
        self.client.get = get
        page = self.catalog.discover("discover:home")
        self.assertEqual(page["title"], "Music")
        self.assertEqual(len(page["shelves"]), 2)
        self.assertEqual(page["shelves"][0]["header"]["ref"], "album:4")
        self.assertEqual(page["shelves"][0]["more"], "browse:%2Fv2%2Fhome%2Fpages%2Falbums%2Fview-all")
        self.assertEqual([t["title"] for t in page["tabs"][:3]], ["For You", "New", "Top"])
        self.catalog.discover("discover:editors")

    def test_page_links_keep_paged_entries(self):
        page = self.catalog.document({"rows": [{"modules": [{"type": "PAGE_LINKS_CLOUD",
            "pagedList": {"items": [{"title": "Rock", "apiPath": "pages/rock"}]}}]}]}, "pages/explore")
        self.assertEqual(page["tabs"], [{"title": "Rock", "ref": "browse:pages%2Frock"}])

    def test_decades_load_only_the_selected_page_and_keep_playlist_sections(self):
        paths = []
        def get(path, params=None, v2=False):
            paths.append(path)
            return {"rows": [{"modules": [{"type": "PLAYLIST_LIST", "title": "Decade playlists",
                "pagedList": {"items": [{"type": "PLAYLIST", "data": {"uuid": "decade-list", "title": "The decade"}}]}}]}]}
        self.client.get = get
        page = self.catalog.discover("discover:decades:1970s")
        self.assertEqual(paths, ["pages/m_1970s"])
        self.assertEqual(page["shelves"][0]["items"][0]["ref"], "playlist:decade-list")
        self.assertEqual([tab["title"] for tab in page["tabs"]],
                         ["1950s", "1960s", "1970s", "1980s", "1990s", "2000s", "2010s"])
        self.assertEqual([tab["title"] for tab in page["tabs"] if tab["active"]], ["1970s"])

    def test_moods_link_opens_its_playlists_and_skips_unrelated_editorial_links(self):
        def get(path, params=None, v2=False):
            if path == "genres":
                return []
            if path == "pages/moods_page":
                return {"rows": [{"modules": [{"type": "PAGE_LINKS_CLOUD", "pagedList": {"items": [
                    {"title": "Workout", "apiPath": "pages/workout"},
                    {"title": "Record Labels", "apiPath": "pages/labels"}]}}]}]}
            self.assertEqual(path, "pages/workout")
            return {"rows": [{"modules": [{"type": "PLAYLIST_LIST", "title": "Workout playlists",
                "pagedList": {"items": [{"type": "PLAYLIST", "data": {"uuid": "workout-list", "title": "Run"}}]}}]}]}
        self.client.get = get
        page = self.catalog.discover("discover:moods")
        self.assertIn({"title": "Workout", "ref": "browse:pages%2Fworkout"}, page["tabs"])
        self.assertNotIn("Record Labels", [tab["title"] for tab in page["tabs"]])
        mood = self.catalog.page({"ref": "browse:pages%2Fworkout"})
        self.assertEqual(mood["shelves"][0]["items"][0]["ref"], "playlist:workout-list")

    def test_new_and_top_use_editorial_pages(self):
        paths = []
        def get(path, params=None, v2=False):
            paths.append(path)
            return [] if path == "genres" else {"rows": []}
        self.client.get = get
        self.catalog.discover("discover:new")
        self.catalog.discover("discover:top")
        self.assertIn("pages/explore_new_music", paths)
        self.assertIn("pages/explore_top_music", paths)

    def test_v2_requests_use_api_host_and_never_follow_redirects(self):
        with tempfile.TemporaryDirectory() as folder:
            client = Client(Path(folder))
            client.token = lambda: ("fixture", "US")
            response = Mock(status_code=200, content=b"{}", headers={})
            response.json.return_value = {}
            client.send = Mock(return_value=response)
            client.get("home/feed/static", v2=True)
            self.assertEqual(client.send.call_args.args[1], "https://api.tidal.com/v2/home/feed/static")
            self.assertFalse(client.send.call_args.kwargs["allow_redirects"])
            response.status_code = 301
            with self.assertRaises(Failure):
                client.get("home/feed/static", v2=True)

    def test_mix_artwork_and_nullable_more(self):
        row = card({"type": "MIX", "data": {"id": "mix1", "title": "Daily mix",
            "mixImages": [{"url": "https://resources.tidal.com/mix.jpg"}]}})
        self.assertEqual(row["image"], "https://resources.tidal.com/mix.jpg")
        page = self.catalog.document({"rows": [{"modules": [{"type": "TRACK_LIST", "showMore": None,
            "pagedList": {"items": [TRACK]}}]}]}, "pages/full")
        self.assertEqual(page["shelves"][0]["items"][0]["ref"], "track:7")

    def test_more_carries_required_client_parameters(self):
        self.catalog.page({"ref": "browse:%2Fv2%2Fhome%2Fpages%2Fmixes%2Fview-all"})
        path, params, _ = self.client.calls[-1]
        self.assertEqual(path, "/v2/home/pages/mixes/view-all")
        self.assertEqual(params["platform"], "WEB")
        self.assertEqual(params["locale"], "en_US")

    def test_multiple_genres_and_customization_ids(self):
        page = self.catalog.discover("discover:home:rock,jazz")
        self.assertEqual(len(page["shelves"]), 4)
        self.assertEqual(len({s["id"] for s in page["shelves"]}), 4)

    def test_gain_zero_is_present_and_missing_gain_stays_missing(self):
        self.assertEqual(normalization({"trackReplayGain": 0, "trackPeakAmplitude": .7})["gain_db"], 0)
        self.assertIsNone(normalization({"trackPeakAmplitude": .7}))
        self.assertIsNone(normalization({"trackReplayGain": float("nan")}))
        self.assertNotIn("peak", normalization({"trackReplayGain": -4, "trackPeakAmplitude": 0}))

    def test_cdn_validation_and_segment_number_expansion(self):
        with self.assertRaises(Failure):
            stream_url("https://evil.example/audio")
        with self.assertRaises(Failure):
            stream_url("http://127.0.0.1/token")
        self.assertEqual(template_url("$RepresentationID$/$Number%05d$-$Time$.m4s", 6, 100, "audio", "12"), "audio/00006-100.m4s")

    def test_account_pkce_masking_cancel_and_stale_form(self):
        with tempfile.TemporaryDirectory() as folder:
            account = Account(Client(Path(folder)), Path(folder))
            form = account.open()
            form = account.submit({"action": "sign_in", "revision": form["revision"]}, "en")
            self.assertEqual(form["form"]["fields"][0]["control"]["type"], "secret")
            query = parse_qs(urlsplit(form["links"][0]["url"]).query)
            self.assertEqual(query["code_challenge_method"], ["S256"])
            self.assertIn("state", query)
            form = account.submit({"action": "visibility", "revision": form["revision"], "values": {"redirect_url": "secret"}}, "en")
            self.assertEqual(form["form"]["fields"][0]["control"]["type"], "text")
            stale = account.submit({"action": "cancel", "revision": 0}, "en")
            self.assertTrue(stale["notices"])
            with self.assertRaises(Failure):
                account.finish("https://evil.example/?code=secret")
            account.submit({"action": "cancel", "revision": form["revision"]}, "en")
            self.assertIsNone(account.pending)

    def test_no_network_on_initialization_and_all_local_data_is_owned(self):
        with tempfile.TemporaryDirectory() as folder, patch("requests.Session.request", side_effect=AssertionError("network")):
            service = Service(folder)
            try:
                self.assertTrue(service.handle("initialize", {})["capabilities"]["playlist_edit"])
                self.assertFalse(service.handle("auth.status", {})["signed_in"])
                self.assertTrue(service.handle("settings.open", {})["form"]["actions"])
            finally:
                service.close()

    def test_history_is_audible_idempotent_and_account_bound(self):
        with tempfile.TemporaryDirectory() as folder:
            reports = Reports(Path(folder), self.client)
            ticket = reports.ticket(card(TRACK, "track"))
            self.assertFalse(reports.state["history"])
            start = {"kind": "started", "playback_id": "one", "resolution_id": ticket}
            reports.event(start)
            reports.event(start)
            reports.event({"kind": "ended", "playback_id": "one", "sequence": 1, "played_ms": 2300})
            restored = Reports(Path(folder), self.client)
            self.assertEqual(len(restored.state["history"]), 1)
            self.assertEqual(restored.state["plays"]["one"]["played_ms"], 2300)
            self.client.user = "other"
            self.assertTrue(restored.event(start)["discard"])
            self.assertTrue(restored.status()["upstream_supported"])

    def test_cache_coalesces_concurrent_reads(self):
        cache, calls, result = Cache(), [], []
        def fetch():
            calls.append(1)
            time.sleep(.05)
            return 9
        threads = [threading.Thread(target=lambda: result.append(cache.get("one", fetch))) for _ in range(4)]
        for thread in threads:
            thread.start()
        for thread in threads:
            thread.join()
        self.assertEqual((calls, result), ([1], [9]*4))


if __name__ == "__main__":
    unittest.main()
