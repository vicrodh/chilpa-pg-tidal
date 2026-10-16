"""Private atomic JSON state and bounded concurrent metadata caching."""
import json
import os
import tempfile
import threading
import time
from collections import OrderedDict
from pathlib import Path


class Failure(Exception):
    def __init__(self, message, code=-32000, status=None, data=None):
        super().__init__(message)
        self.code, self.status, self.data = code, status, data


def save(path, value):
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    fd, name = tempfile.mkstemp(dir=path.parent, prefix=".state-")
    try:
        with os.fdopen(fd, "w") as stream:
            json.dump(value, stream, ensure_ascii=False, allow_nan=False)
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(name, path)
    finally:
        if os.path.exists(name):
            os.unlink(name)


def read(path, default):
    try:
        with open(path) as stream:
            return json.load(stream)
    except FileNotFoundError:
        return default


class Cache:
    def __init__(self):
        self.lock = threading.RLock()
        self.entries = OrderedDict()
        self.flights = {}
        self.epoch = 0

    def clear(self):
        with self.lock:
            self.entries.clear()
            self.epoch += 1

    def get(self, key, fetch, ttl=300):
        with self.lock:
            found = self.entries.get(key)
            if found and found[0] > time.monotonic():
                return found[1]
            flight = self.flights.setdefault(key, threading.Lock())
        with flight:
            with self.lock:
                found = self.entries.get(key)
                if found and found[0] > time.monotonic():
                    return found[1]
                epoch = self.epoch
            try:
                result = fetch()
                with self.lock:
                    if epoch == self.epoch:
                        self.entries[key] = (time.monotonic() + ttl, result)
                        self.entries.move_to_end(key)
                        while len(self.entries) > 256:
                            self.entries.popitem(last=False)
                return result
            finally:
                with self.lock:
                    if self.flights.get(key) is flight:
                        del self.flights[key]
