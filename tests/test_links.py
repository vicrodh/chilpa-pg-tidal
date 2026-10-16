"""A TIDAL link names what it opens; any other link is not this source's."""
import unittest
from tidal_plugin.links import link_target


class LinksTest(unittest.TestCase):
    def test_each_shape_names_its_kind_and_reference(self):
        self.assertEqual(link_target("https://tidal.com/browse/album/123456"), {"kind": "album", "ref": "album:123456"})
        self.assertEqual(link_target("https://listen.tidal.com/album/123456/track/789"), {"kind": "album", "ref": "album:123456"})
        self.assertEqual(link_target("https://tidal.com/track/789/u"), {"kind": "track", "ref": "track:789"})
        self.assertEqual(link_target("https://tidal.com/browse/artist/42"), {"kind": "artist", "ref": "artist:42"})
        self.assertEqual(link_target("https://tidal.com/browse/playlist/0f4c-ab"), {"kind": "playlist", "ref": "playlist:0f4c-ab"})
        self.assertEqual(link_target("https://tidal.com/browse/mix/00abc"), {"kind": "playlist", "ref": "mix:00abc"})
        self.assertEqual(link_target("tidal://album/55"), {"kind": "album", "ref": "album:55"})

    def test_a_link_of_anywhere_else_is_none(self):
        for url in ["", "https://example.org/album/1", "https://nottidal.com/album/1", "https://tidal.com/", "ftp://tidal.com/album/1"]:
            self.assertIsNone(link_target(url), url)


if __name__ == "__main__":
    unittest.main()
