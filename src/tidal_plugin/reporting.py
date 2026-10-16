"""I send measured playback_session events using the published TIDAL SDK contract."""
import json
import platform
import time
from xml.etree import ElementTree
from .event_queue import EventQueue
from .state import Failure

ENDPOINT = "https://ec.tidal.com/api/event-batch"


def event_body(entry):
    play, event_id = entry["play"], entry["id"]
    spans = play["spans"]
    track = play["track"]["ref"].split("@", 1)[0].split(":", 1)[1]
    extra = play.get("extra", {})
    source_type, source_id = extra.get("source_type"), extra.get("source_id")
    # I attribute standalone plays to the track; an empty origin can be
    # acknowledged by ingestion without appearing in the account history.
    if source_type not in {"ALBUM", "PLAYLIST", "ARTIST", "MIX", "MY_ITEMS"} or not source_id:
        source_type, source_id = "ITEM", track
    actions = [{"actionType": kind, "assetPosition": span[position] / 1000, "timestamp": span[stamp]}
               for span in spans for kind, position, stamp in [("PLAYBACK_START", "start", "at"), ("PLAYBACK_STOP", "end", "until")]]
    return {"group": "play_log", "name": "playback_session", "version": 2, "uuid": event_id,
            "ts": spans[-1]["until"], "payload": {"actions": actions, "actualAssetPresentation": "FULL",
            "actualAudioMode": extra.get("audio_mode", "STEREO"), "actualProductId": track,
            "actualQuality": extra.get("quality"), "endAssetPosition": spans[-1]["end"] / 1000,
            "endTimestamp": spans[-1]["until"], "isPostPaywall": True, "playbackSessionId": event_id,
            "productType": "TRACK", "requestedProductId": track, "sourceId": source_id,
            "sourceType": source_type, "startAssetPosition": spans[0]["start"] / 1000,
            "startTimestamp": spans[0]["at"]}}


class Reports(EventQueue):
    def __init__(self, data, client):
        self.client = client
        super().__init__(data / "history.json", lambda: client.user, self.send)

    def send(self, entry):
        # I snapshot the account token before sending; network latency must not
        # hold the account lock used by catalog and playback requests.
        with self.client.lock:
            if self.client.user != entry["user"]:
                raise Failure("The listening event belongs to another account.")
            token, _ = self.client.token()
        headers = {"app-name": "Chilpa", "app-version": "0.1.11", "browser-name": "", "browser-version": "",
                   "client-id": self.client.config.client_id_pkce, "consent-category": "NECESSARY",
                   "os-name": platform.system(), "requested-sent-timestamp": int(time.time() * 1000), "authorization": token}
        key = "SendMessageBatchRequestEntry.1"
        form = {key + ".Id": entry["id"], key + ".MessageBody": json.dumps(event_body(entry), separators=(",", ":")),
                key + ".MessageAttribute.1.Name": "Name", key + ".MessageAttribute.1.Value.DataType": "String",
                key + ".MessageAttribute.1.Value.StringValue": "playback_session", key + ".MessageAttribute.2.Name": "Headers",
                key + ".MessageAttribute.2.Value.DataType": "String", key + ".MessageAttribute.2.Value.StringValue": json.dumps(headers)}
        response = self.client.checked(self.client.send("POST", ENDPOINT, data=form,
            headers={"Authorization": "Bearer " + token, "Content-Type": "application/x-www-form-urlencoded"}, allow_redirects=False))
        if response.status_code != 200 or len(response.content) > 65536:
            raise Failure("The listening endpoint returned an unexpected response.")
        try:
            root = ElementTree.fromstring(response.content)
        except ElementTree.ParseError:
            raise Failure("The listening acknowledgement could not be read.") from None
        # HTTP 200 may contain per-entry errors. I remove only this event's ACK.
        return any(node.findtext("{*}Id") == entry["id"] for node in root.iter() if node.tag.rsplit("}", 1)[-1] == "SendMessageBatchResultEntry")
