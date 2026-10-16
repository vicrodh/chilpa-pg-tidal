"""Bounded concurrent JSON-RPC worker. Stdout belongs exclusively to the host."""
import argparse
import concurrent.futures
import json
import logging
import os
import sys
import threading
from tidal_plugin.service import Service
from tidal_plugin.state import Failure


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--data-dir", default=os.environ.get("CHILPA_PLUGIN_DATA_DIR", "/data"))
    args = parser.parse_args()
    # Upstream libraries can log HTTP bodies on failures. I suppress them.
    logging.disable(logging.CRITICAL)
    service = Service(args.data_dir)
    output = threading.Lock()
    slots = threading.Semaphore(32)

    def call(message):
        try:
            try:
                result = service.handle(message.get("method", ""), message.get("params") or {})
                reply = {"result": result}
            except Failure as error:
                reply = {"error": {"code": error.code, "message": str(error)}}
                detail = dict(error.data or {})
                if error.status:
                    detail["status"] = error.status
                if detail:
                    reply["error"]["data"] = detail
            except Exception:
                reply = {"error": {"code": -32000, "message": "The plugin could not complete this request. Refresh and try again."}}
            if "id" in message:
                with output:
                    print(json.dumps({"jsonrpc": "2.0", "id": message["id"], **reply}, ensure_ascii=False, allow_nan=False), flush=True)
        finally:
            slots.release()

    try:
        with concurrent.futures.ThreadPoolExecutor(max_workers=8) as pool:
            for line in sys.stdin:
                if len(line) > 4 * 1024 * 1024:
                    continue
                try:
                    message = json.loads(line)
                    if not isinstance(message, dict):
                        continue
                except ValueError:
                    continue
                slots.acquire()
                if message.get("method") == "playback.event":
                    call(message)
                else:
                    pool.submit(call, message)
    finally:
        service.close()


if __name__ == "__main__":
    main()
