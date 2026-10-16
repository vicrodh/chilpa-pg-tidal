"""I retain measured listening and acknowledge it before attempting network delivery."""
from copy import deepcopy
import json
import secrets
import threading
import time
import uuid
from .state import Failure, save


class EventQueue:
    def __init__(self, path, account, deliver):
        self.path, self.account, self.deliver = path, account, deliver
        self.lock = threading.RLock()
        self.delivery = threading.Lock()
        self.stop = threading.Event()
        self.thread = None
        self.error = None
        self.blocked = False
        try:
            self.state = json.loads(path.read_text())
        except FileNotFoundError:
            self.state = {}
        except (OSError, ValueError):
            self.state, self.blocked = {}, True
            self.error = "The listening journal needs recovery."
        if not isinstance(self.state, dict) or any(not isinstance(self.state.get(k, default), type(default))
                for k, default in [("tickets", {}), ("plays", {}), ("history", []), ("outbox", [])]):
            self.state, self.blocked = {}, True
            self.error = "The listening journal needs recovery."
        for name, default in [("tickets", {}), ("plays", {}), ("history", []), ("outbox", [])]:
            self.state.setdefault(name, default)

    def start(self):
        self.thread = threading.Thread(target=self.run, daemon=True)
        self.thread.start()

    def close(self):
        self.stop.set()
        if self.thread:
            self.thread.join(timeout=35)

    def run(self):
        while not self.stop.wait(2):
            self.flush()

    def clear(self):
        with self.delivery, self.lock:
            self.state = {"tickets": {}, "plays": {}, "history": [], "outbox": []}
            if self.path.exists():
                save(self.path, self.state)
            self.error, self.blocked = None, False

    def ticket(self, track, extra=None):
        with self.lock:
            if self.blocked:
                raise Failure("The listening journal needs recovery.")
            key = secrets.token_urlsafe(24)
            state = deepcopy(self.state)
            state["tickets"][key] = {"user": self.account(), "track": track, "extra": extra or {}}
            for old in list(state["tickets"])[:-512]:
                del state["tickets"][old]
            save(self.path, state)
            self.state = state
            return key

    def event(self, params):
        with self.lock:
            if self.blocked:
                raise Failure("The listening journal needs recovery.")
            key, kind = params.get("playback_id"), params.get("kind")
            if not isinstance(key, str) or not key or len(key) > 128 or kind not in {"started", "progress", "ended"}:
                raise Failure("Invalid playback event.", -32602)
            state = deepcopy(self.state)
            user, plays = self.account(), state["plays"]
            if not user:
                return {"accepted": False, "discard": True}
            if kind == "started":
                ticket = state["tickets"].get(params.get("resolution_id"))
                if not ticket or ticket["user"] != user:
                    return {"accepted": False, "discard": True}
                if key in plays:
                    return {"accepted": plays[key]["user"] == user, "discard": plays[key]["user"] != user}
                plays[key] = {**ticket, "sequence": 0, "played_ms": 0, "ended": False, "spans": [],
                              "position": params.get("position_ms"), "timestamp": int(params.get("started_at", time.time()) * 1000)}
                state["history"] = [ticket["track"]] + [t for t in state["history"] if t["ref"] != ticket["track"]["ref"]][:99]
                for old in list(plays)[:-512]:
                    if plays[old]["ended"]:
                        del plays[old]
                if len(plays) > 1024:
                    raise Failure("The listening journal is full.")
            else:
                play = plays.get(key)
                if not play or play["user"] != user:
                    return {"accepted": False, "discard": True}
                sequence, played = params.get("sequence"), params.get("played_ms")
                if type(sequence) is not int or sequence <= 0 or type(played) is not int or played < 0:
                    raise Failure("Invalid playback checkpoint.", -32602)
                if play["ended"] or sequence <= play["sequence"]:
                    return {"accepted": True}
                if played < play["played_ms"]:
                    raise Failure("Playback time cannot decrease.", -32602)
                self.checkpoint(play, params)
                play.update(sequence=sequence, played_ms=played, ended=kind == "ended")
                # Older hosts have no media cursor. I keep their local history,
                # but never invent positions or backfill historical listens.
                if kind == "ended" and play.get("spans"):
                    if len(state["outbox"]) >= 1024:
                        raise Failure("The listening delivery queue is full.")
                    state["outbox"].append({"id": str(uuid.uuid4()), "user": user, "play": deepcopy(play),
                                            "reason": params.get("reason"), "retry_at": 0, "attempts": 0})
            save(self.path, state)
            self.state = state
            return {"accepted": True}

    @staticmethod
    def checkpoint(play, params):
        position, stamp = params.get("position_ms"), params.get("observed_at_ms")
        delta = params["played_ms"] - play["played_ms"]
        if type(position) is not int or position < 0 or type(stamp) is not int or stamp <= 0:
            return
        spans = play.setdefault("spans", [])
        if delta > 0:
            # A checkpoint carries consumed time, never the seek distance or
            # elapsed wall time while paused. I retain only measured intervals.
            length = min(delta, position)
            if length:
                span = {"start": position - length, "end": position, "at": stamp - length, "until": stamp}
                if spans and spans[-1]["end"] == span["start"] and abs(spans[-1]["until"] - span["at"]) <= 250:
                    spans[-1].update(end=position, until=spans[-1]["at"] + spans[-1]["end"] - spans[-1]["start"] + length)
                else:
                    spans.append(span)
                if len(spans) > 96:
                    raise Failure("Too many listening intervals in one playback.")
        play.update(position=position, timestamp=stamp)

    def flush(self):
        if not self.delivery.acquire(blocking=False):
            return
        try:
            with self.lock:
                if self.blocked or not self.state["outbox"]:
                    return
                entry = deepcopy(self.state["outbox"][0])
                if entry["retry_at"] > time.time():
                    return
            try:
                accepted = self.deliver(entry) if entry["user"] == self.account() else "discard"
                if not accepted:
                    raise Failure("The listening event was not acknowledged.")
            except Exception as error:
                delay = getattr(error, "data", None) or {}
                wait = delay.get("retry_after_ms", 0) / 1000 or delay.get("retry_after", 0)
                with self.lock:
                    self.error = "Listening delivery failed; the event remains queued."
                    entry["attempts"] += 1
                    entry["retry_at"] = time.time() + max(min(float(wait), 3600), min(300, 5 * 2 ** min(entry["attempts"], 6)))
                    state = deepcopy(self.state)
                    state["outbox"][0] = entry
                    save(self.path, state)
                    self.state = state
                return
            with self.lock:
                state = deepcopy(self.state)
                state["outbox"].pop(0)
                counter = "discarded" if accepted == "discard" else "delivered"
                state[counter] = state.get(counter, 0) + 1
                save(self.path, state)
                self.state, self.error = state, None
        except (OSError, ValueError):
            self.error = "The listening journal could not be saved."
        finally:
            self.delivery.release()

    def status(self):
        with self.lock:
            return {"upstream_supported": True, "pending": len(self.state["outbox"]), "local_history": True,
                    "delivered": self.state.get("delivered", 0), "last_error": self.error,
                    "requires_playback_positions": True}
