"""Account-scoped REST requests; errors never contain tokens or signed URLs."""
import threading
import time
from urllib.parse import urljoin, urlsplit
import requests
from tidalapi import Config
from .state import Failure, read, save

API = "https://api.tidal.com/v1/"
V2 = "https://api.tidal.com/v2/"
TOKEN = "https://auth.tidal.com/v1/oauth2/token"


def pause(response):
    """The wait a limited response asks for, as the host reads it from an error's data."""
    try:
        seconds = int(str(response.headers.get("Retry-After", "")).strip())
    except (AttributeError, ValueError):
        return None
    return {"retry_after_ms": min(seconds, 3600) * 1000} if seconds > 0 else None


class Client:
    def __init__(self, data):
        self.path = data / "account.json"
        self.lock = threading.RLock()
        self.local = threading.local()
        self.config = Config()
        self.account = read(self.path, {})
        self.generation = 0

    def http(self):
        if not hasattr(self.local, "http"):
            self.local.http = requests.Session()
            self.local.http.trust_env = False
        return self.local.http

    def send(self, method, url, **kwargs):
        try:
            return self.http().request(method, url, timeout=(8, 20), **kwargs)
        except requests.RequestException:
            raise Failure("Tidal could not be reached. Try again.") from None

    @staticmethod
    def checked(response):
        if response.status_code >= 400:
            status = response.status_code
            message = {401: "Sign in to Tidal again.", 403: "This account cannot access this content.",
                       404: "This item is no longer available.", 409: "The playlist changed. Refresh it before editing.",
                       412: "The playlist changed. Refresh it before editing.", 429: "Tidal is limiting requests. Try again shortly."}.get(status, "Tidal could not complete the request.")
            raise Failure(message, -32001 if status == 401 else -32000, status, pause(response) if status == 429 else None)
        return response

    @property
    def user(self):
        with self.lock:
            return str(self.account.get("user_id", ""))

    def logout(self):
        with self.lock:
            self.generation += 1
            self.account = {}
            self.path.unlink(missing_ok=True)

    def accept(self, token, expected_generation):
        access = token.get("access_token")
        if not access:
            raise Failure("The sign-in response did not contain an access token.")
        info = self.checked(self.send("GET", API + "sessions", headers={"Authorization": "Bearer " + access})).json()
        user = info.get("userId") or token.get("user_id") or token.get("user", {}).get("userId")
        country = info.get("countryCode") or token.get("user", {}).get("countryCode")
        if not user or not country:
            raise Failure("Tidal did not return the account and country information.")
        with self.lock:
            if self.generation != expected_generation:
                raise Failure("This sign-in attempt was cancelled.")
            previous = self.account
            account = {"access_token": access, "refresh_token": token.get("refresh_token", previous.get("refresh_token", "")),
                       "expires": time.time() + int(token.get("expires_in", 3600)), "user_id": str(user),
                       "country": country, "name": token.get("user", {}).get("username") or str(user)}
            save(self.path, account)
            self.account = account

    def token(self):
        with self.lock:
            if not self.account.get("access_token"):
                raise Failure("Sign in to Tidal first.", -32001)
            if self.account.get("expires", 0) < time.time() + 90:
                response = self.checked(self.send("POST", TOKEN, data={"grant_type": "refresh_token",
                    "refresh_token": self.account.get("refresh_token", ""),
                    "client_id": self.config.client_id_pkce, "client_secret": self.config.client_secret_pkce}))
                self.accept(response.json(), self.generation)
            return self.account["access_token"], self.account["country"]

    def request(self, path, params=None, method="GET", data=None, etag=None, v2=False):
        base = V2 if v2 else API
        url = urljoin(base, path if path.startswith(("/v1/", "/v2/")) else path.lstrip("/"))
        parsed = urlsplit(url)
        if parsed.scheme != "https" or parsed.hostname not in {"api.tidal.com", "listen.tidal.com"}:
            raise Failure("Invalid catalog link.", -32602)
        token, country = self.token()
        headers = {"Authorization": "Bearer " + token, "x-tidal-client-version": "2025.7.16"}
        if etag:
            headers["If-None-Match"] = etag
        query = {"countryCode": country, **(params or {})}
        response = self.checked(self.send(method, url, params=query, data=data, headers=headers, allow_redirects=False))
        if 300 <= response.status_code < 400:
            raise Failure("Tidal returned an unexpected redirect.")
        try:
            result = response.json() if response.content else {}
        except ValueError:
            raise Failure("Tidal returned an unreadable response.") from None
        return result, response.headers.get("ETag", "")

    def get(self, path, params=None, v2=False):
        return self.request(path, params, v2=v2)[0]

    def all(self, path, params=None):
        rows = []
        for offset in range(0, 100000, 100):
            page = self.get(path, {**(params or {}), "limit": 100, "offset": offset})
            batch = page.get("items", [])
            rows.extend(batch)
            if len(batch) < 100 or len(rows) >= page.get("totalNumberOfItems", 100001):
                return rows
        raise Failure("This collection is too large to load at once.")
