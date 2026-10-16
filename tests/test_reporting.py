"""I verify acknowledgements and measured intervals without a live account."""
import json
from pathlib import Path
from tempfile import TemporaryDirectory
import unittest
from unittest.mock import Mock
from tidal_plugin.event_queue import EventQueue
from tidal_plugin.reporting import Reports, event_body


class Listening(unittest.TestCase):
    def setUp(self):
        self.temp = TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.path = Path(self.temp.name) / "history.json"
        self.account = "account-a"
        self.deliver = Mock(return_value=True)
        self.queue = EventQueue(self.path, lambda: self.account, self.deliver)
        self.ticket = self.queue.ticket({"ref": "track:7", "duration_ms": 120000}, {"quality": "LOSSLESS"})

    def start(self):
        self.queue.event({"kind": "started", "playback_id": "play-1", "resolution_id": self.ticket,
                          "started_at": 100, "position_ms": 0})

    def checkpoint(self, sequence, played, position, stamp, kind="progress"):
        return self.queue.event({"kind": kind, "playback_id": "play-1", "sequence": sequence,
                                "played_ms": played, "position_ms": position, "observed_at_ms": stamp})

    def test_resolution_does_not_send_a_listen(self):
        self.queue.flush()
        self.deliver.assert_not_called()
        self.assertEqual(self.queue.state["history"], [])

    def test_pause_seek_and_duplicate_end_keep_only_consumed_intervals(self):
        self.start()
        self.checkpoint(1, 1000, 1000, 101000)
        self.checkpoint(2, 2000, 2000, 107000)  # Five seconds paused.
        self.checkpoint(3, 3000, 81000, 108000)  # Seek forward, then consume one second.
        self.checkpoint(4, 4000, 82000, 109000, "ended")
        self.checkpoint(4, 4000, 82000, 109000, "ended")
        self.assertEqual(len(self.queue.state["outbox"]), 1)
        event = event_body(self.queue.state["outbox"][0])
        self.assertEqual(event["payload"]["actualQuality"], "LOSSLESS")
        spans = self.queue.state["outbox"][0]["play"]["spans"]
        self.assertEqual(sum(s["end"] - s["start"] for s in spans), 4000)
        self.assertEqual([(s["start"], s["end"]) for s in spans], [(0, 1000), (1000, 2000), (80000, 82000)])
        self.queue.flush()
        self.assertEqual(self.queue.status()["delivered"], 1)

    def test_small_seek_does_not_merge_incompatible_intervals(self):
        self.start()
        self.checkpoint(1, 1000, 1000, 101000)
        self.checkpoint(2, 2000, 1999, 102000, "ended")
        spans = self.queue.state["outbox"][0]["play"]["spans"]
        self.assertEqual(len(spans), 2)
        for span in spans:
            self.assertEqual(span["end"] - span["start"], span["until"] - span["at"])

    def test_missing_or_invalid_origin_is_attributed_to_the_track(self):
        self.start()
        self.checkpoint(1, 31000, 31000, 131000, "ended")
        entry = self.queue.state["outbox"][0]
        for extra in ({}, {"source_type": "", "source_id": ""},
                      {"source_type": "ITEM", "source_id": "search query"},
                      {"source_type": "PLAYLIST", "source_id": ""}):
            with self.subTest(extra=extra):
                entry["play"]["extra"] = extra
                payload = event_body(entry)["payload"]
                self.assertEqual((payload["sourceType"], payload["sourceId"]), ("ITEM", "7"))

    def test_known_container_origin_is_preserved(self):
        self.start()
        self.checkpoint(1, 31000, 31000, 131000, "ended")
        entry = self.queue.state["outbox"][0]
        entry["play"]["extra"].update(source_type="PLAYLIST", source_id="fixture-playlist")
        payload = event_body(entry)["payload"]
        self.assertEqual((payload["sourceType"], payload["sourceId"]), ("PLAYLIST", "fixture-playlist"))

    def test_retry_survives_restart_with_same_id(self):
        self.start()
        self.checkpoint(1, 1000, 1000, 101000, "ended")
        event_id = self.queue.state["outbox"][0]["id"]
        self.deliver.side_effect = RuntimeError("a-secret-from-transport")
        self.queue.flush()
        self.assertNotIn("a-secret", self.path.read_text())
        self.assertNotIn("a-secret", json.dumps(self.queue.status()))
        restarted = EventQueue(self.path, lambda: self.account, Mock(return_value=True))
        self.assertEqual(restarted.state["outbox"][0]["id"], event_id)
        self.assertGreater(restarted.state["outbox"][0]["retry_at"], 0)
        restarted.state["outbox"][0]["retry_at"] = 0
        restarted.flush()
        self.assertEqual(restarted.status()["pending"], 0)

    def test_account_switch_never_delivers_old_listens(self):
        self.start()
        self.checkpoint(1, 1000, 1000, 101000, "ended")
        self.account = "account-b"
        self.queue.flush()
        self.deliver.assert_not_called()
        self.assertEqual(self.queue.status()["delivered"], 0)
        self.assertEqual(self.queue.status()["pending"], 0)

    def test_legacy_checkpoints_are_not_fabricated(self):
        self.start()
        self.queue.event({"kind": "ended", "playback_id": "play-1", "sequence": 1, "played_ms": 100000})
        self.assertEqual(self.queue.status()["pending"], 0)

    def test_http_success_requires_matching_entry_acknowledgement(self):
        self.start()
        self.checkpoint(1, 1000, 1000, 101000, "ended")
        entry = self.queue.state["outbox"][0]
        import threading
        client = Mock(user=self.account, lock=threading.RLock())
        client.config.client_id_pkce = "fixture-client"
        client.token.return_value = ("fixture-token", None)
        response = Mock(status_code=200, content=b'<SendMessageBatchResponse><BatchResultErrorEntry><Id>wrong</Id></BatchResultErrorEntry></SendMessageBatchResponse>')
        client.checked.return_value = response
        report = Reports(Path(self.temp.name), client)
        self.assertFalse(report.send(entry))
        response.content = ('<SendMessageBatchResponse xmlns="urn:sqs"><SendMessageBatchResult><SendMessageBatchResultEntry><Id>' + entry["id"] + '</Id></SendMessageBatchResultEntry></SendMessageBatchResult></SendMessageBatchResponse>').encode()
        self.assertTrue(report.send(entry))
        kwargs = client.send.call_args.kwargs
        self.assertFalse(kwargs["allow_redirects"])
        self.assertEqual(json.loads(kwargs["data"]["SendMessageBatchRequestEntry.1.MessageBody"])["uuid"], entry["id"])

    def test_corrupt_journal_is_not_overwritten(self):
        self.path.write_text("[]")
        queue = EventQueue(self.path, lambda: self.account, self.deliver)
        self.assertTrue(queue.blocked)
        queue.flush()
        self.assertEqual(self.path.read_text(), "[]")
