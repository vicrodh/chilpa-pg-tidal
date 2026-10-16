import base64
import unittest
from unittest.mock import Mock
from tidal_plugin.catalog import Catalog
from tidal_plugin.folders import action, PATH
from tidal_plugin.state import Cache, Failure
from test_contract import Fixtures

FOLDER = "00000000-0000-0000-0000-000000000001"
CHILD = "00000000-0000-0000-0000-000000000002"

class FoldersAndQuality(unittest.TestCase):
    def test_nested_folders_preserve_membership_and_paging(self):
        client = Fixtures()
        def get(path, params=None, v2=False):
            self.assertEqual(path, PATH)
            self.assertTrue(v2)
            key = params["folderId"]
            rows = {
                "root":[{"itemType":"FOLDER","data":{"id":FOLDER,"name":"Parent"}}],
                FOLDER:[{"itemType":"FOLDER","data":{"id":CHILD,"name":"Child"}}],
                CHILD:[{"itemType":"PLAYLIST","data":{"uuid":"p","title":"List","creator":{"id":5}}}],
            }[key]
            return {"items":rows,"totalNumberOfItems":len(rows)}
        client.get = Mock(side_effect=get)
        catalog = Catalog(client, Cache())
        result = catalog.library({"kind":"playlists","limit":500})
        self.assertEqual(result["items"][0]["native_folder"],"folder:"+CHILD)
        self.assertEqual(result["folders"][1]["parent"],"folder:"+FOLDER)
        self.assertEqual(result["total"],1)
        catalog.library({"kind":"playlists","offset":1})
        self.assertEqual(client.get.call_count,3)

    def test_native_actions_use_explicit_folder_and_playlist_identities(self):
        client = Fixtures(); client.request = Mock(return_value=({},""))
        action(client,{"action":"folder.create","title":"New","parent":"folder:"+FOLDER})
        self.assertEqual(client.request.call_args.kwargs["params"],{"name":"New","folderId":FOLDER})
        action(client,{"action":"playlist.folder","ref":"playlist:p","folder":"folder:"+CHILD})
        self.assertEqual(client.request.call_args.kwargs["params"],{"trns":"trn:playlist:p","folderId":CHILD})
        action(client,{"action":"folder.rename","ref":"folder:"+CHILD,"title":"Renamed"})
        self.assertEqual(client.request.call_args.args[0],PATH+"/rename")
        with self.assertRaises(Failure): action(client,{"action":"folder.create","title":"New","parent":"invalid"})

    def test_quality_is_cached_and_never_opens_audio(self):
        client = Fixtures()
        manifest = b'<MPD><Period><AdaptationSet mimeType="audio/flac"><Representation codecs="flac" audioSamplingRate="96000" /></AdaptationSet></Period></MPD>'
        raw = {"bitDepth":24,"sampleRate":96000,"manifestMimeType":"application/dash+xml","manifest":base64.b64encode(manifest).decode()}
        client.get = Mock(return_value=raw)
        catalog = Catalog(client,Cache())
        for _ in range(2):
            self.assertEqual(catalog.track_quality({"ref":"track:7@playlist:p"},"HI_RES_LOSSLESS")["quality"],{"codec":"wav","bits":24,"rate":96000})
        self.assertEqual(client.get.call_count,1)
        self.assertEqual(client.get.call_args.args[0],"tracks/7/playbackinfopostpaywall")
        self.assertEqual(client.calls,[])
