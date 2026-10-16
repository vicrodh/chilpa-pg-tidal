"""Lossless sample and random-range tests with generated, inaudible fixtures."""
import array
import base64
import io
import random
import struct
import unittest
import threading
import requests
from unittest.mock import patch
import av
from tidal_plugin.streaming import Dash, Streams
from types import SimpleNamespace


def fixture(bits):
    target = io.BytesIO()
    expected = bytearray()
    with av.open(target, "w", format="mp4", options={"movflags": "frag_keyframe+empty_moov+default_base_moof", "frag_duration": "100000"}) as output:
        stream = output.add_stream("flac", rate=48000)
        stream.layout = "stereo"
        stream.codec_context.format = "s16" if bits == 16 else "s32"
        for index in range(10):
            values = [(i*13+index*19) % 30000-15000 for i in range(9600)]
            if bits == 24:
                values = [v*173 for v in values]
                raw = array.array("i", [v << 8 for v in values]).tobytes()
                expected.extend(b"".join(v.to_bytes(3, "little", signed=True) for v in values))
            else:
                raw = array.array("h", values).tobytes()
                expected.extend(raw)
            frame = av.AudioFrame(format="s16" if bits == 16 else "s32", layout="stereo", samples=4800)
            frame.sample_rate, frame.pts = 48000, index*4800
            frame.planes[0].update(raw)
            for packet in stream.encode(frame):
                output.mux(packet)
        for packet in stream.encode(None):
            output.mux(packet)
    blob = target.getvalue()
    boxes, position = [], 0
    while position < len(blob):
        size = int.from_bytes(blob[position:position+4], "big")
        boxes.append((blob[position+4:position+8], blob[position:position+size]))
        position += size
    init, fragments = b"", []
    for kind, body in boxes:
        if kind in {b"ftyp", b"moov"}:
            init += body
        elif kind == b"moof":
            fragments.append(body)
        elif kind == b"mdat":
            fragments[-1] += body
    counts = []
    for fragment in fragments:
        with av.open(io.BytesIO(init+fragment), format="mp4") as decoder:
            counts.append(sum(f.samples for f in decoder.decode(audio=0)))
    urls = {"https://audio.tidal.com/init": init, **{f"https://audio.tidal.com/{i+5}.m4s": fragment for i, fragment in enumerate(fragments)}}
    timeline = "".join(f'<S d="{count}"/>' for count in counts)
    xml = f'''<MPD xmlns="urn:mpeg:dash:schema:mpd:2011"><Period><AdaptationSet mimeType="audio/mp4"><Representation id="audio" codecs="fLaC" audioSamplingRate="48000"><SegmentTemplate timescale="48000" startNumber="5" initialization="https://audio.tidal.com/init" media="https://audio.tidal.com/$Number$.m4s"><SegmentTimeline>{timeline}</SegmentTimeline></SegmentTemplate></Representation></AdaptationSet></Period></MPD>'''
    return xml, urls, bytes(expected)


class Streaming(unittest.TestCase):
    def test_resolving_more_than_eight_tracks_preserves_the_playing_range_url(self):
        xml, urls, expected = fixture(24)
        client = SimpleNamespace(get=lambda *args: {"manifestMimeType": "application/dash+xml", "bitDepth": 24,
            "manifest": base64.b64encode(xml.encode()).decode()})
        streams = Streams(client, SimpleNamespace(preferences={"quality": "HI_RES_LOSSLESS"}))
        try:
            with patch("tidal_plugin.streaming.fetch", side_effect=lambda url: urls[url]):
                playing = streams.resolve({"ref": "track:1"})["url"]
                self.assertEqual(requests.head(playing, timeout=5).status_code, 200)
                for key in range(2, 14):
                    streams.resolve({"ref": "track:" + str(key)})
                response = requests.get(playing, headers={"Range": "bytes=90001-110000"}, timeout=5)
                self.assertEqual(response.status_code, 206)
                self.assertEqual(response.content, expected[90001-44:110001-44])
        finally:
            streams.server.shutdown()
            streams.server.server_close()

    def test_16_and_24_bit_dash_preserve_every_original_sample(self):
        for bits in [16, 24]:
            with self.subTest(bits=bits):
                xml, urls, expected = fixture(bits)
                with patch("tidal_plugin.streaming.fetch", side_effect=lambda url: urls[url]):
                    asset = Dash(xml, bits)
                    self.assertEqual(asset.samples, 48000)
                    self.assertEqual(asset.length, len(expected)+44)
                    self.assertEqual(asset.read(44, asset.length), expected)
                    self.assertEqual(struct.unpack_from("<I", asset.header, 24)[0], 48000)

    def test_seek_ranges_across_fragment_boundaries_and_header(self):
        xml, urls, expected = fixture(24)
        with patch("tidal_plugin.streaming.fetch", side_effect=lambda url: urls[url]):
            asset = Dash(xml, 24)
            whole = asset.header + expected
            randomizer = random.Random(17)
            for start in [0, 20, 40, 44, 70000, asset.length-10] + [randomizer.randrange(asset.length) for _ in range(30)]:
                end = min(asset.length, start+19001)
                self.assertEqual(asset.read(start, end), whole[start:end])

    def test_http_ranges_and_head_expose_exact_length_for_player_and_cast_proxy(self):
        xml, urls, expected = fixture(16)
        with patch("tidal_plugin.streaming.fetch", side_effect=lambda url: urls[url]):
            asset = Dash(xml, 16)
            streams = Streams(None, None)
            try:
                streams.assets["ticket"] = (0, asset)
                url = f"http://127.0.0.1:{streams.server.server_port}/ticket"
                response = requests.head(url, timeout=5)
                self.assertEqual(int(response.headers["Content-Length"]), len(expected)+44)
                response = requests.get(url, headers={"Range": "bytes=90001-110000"}, timeout=5)
                self.assertEqual(response.status_code, 206)
                self.assertEqual(response.content, (asset.header+expected)[90001:110001])
                self.assertEqual(response.headers["Content-Range"], f"bytes 90001-110000/{asset.length}")
                self.assertEqual(requests.get(url, headers={"Range": "bytes=99999999-"}, timeout=5).status_code, 416)
                streams.clear()
                self.assertEqual(requests.head(url, timeout=5).status_code, 404)
            finally:
                streams.server.shutdown()
                streams.server.server_close()


if __name__ == "__main__":
    unittest.main()
