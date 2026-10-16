"""Browser PKCE sign-in as used by hiresTI, with masked return URLs."""
import secrets
import threading
import time
from urllib.parse import urlencode, urlsplit, parse_qs
from tidalapi import Config
from .client import TOKEN
from .state import Failure, read, save


class Account:
    def __init__(self, client, data):
        self.client, self.data = client, data
        self.lock = threading.RLock()
        self.pending = None
        self.shown = False
        self.redirect = ""
        self.revision = 1
        self.preferences = read(data / "preferences.json", {"quality": "HI_RES_LOSSLESS"})

    def status(self):
        return {"signed_in": bool(self.client.user), "name": self.client.account.get("name", "")}

    def open(self, locale="en", notice=None):
        with self.lock:
            es = locale.startswith("es")
            say = lambda en, spanish: spanish if es else en
            fields, actions, links, values = [], [], [], {}
            if self.client.user:
                content = say("Signed in as ", "Conectado como ") + self.client.account.get("name", self.client.user)
                for quality, label in [("HI_RES_LOSSLESS", "Max · Hi-Res FLAC"), ("LOSSLESS", "High · FLAC"), ("HIGH", "AAC 320 kbps")]:
                    actions.append({"key": "quality_" + quality, "label": ("• " if self.preferences["quality"] == quality else "") + label, "fields": []})
                actions.append({"key": "sign_out", "label": say("Sign out", "Cerrar sesión"), "fields": []})
            elif self.pending:
                content = say("Complete sign-in in your browser. Copy the final address from the Tidal page (including an Oops page), paste it here and finish sign-in.",
                              "Completa el acceso en el navegador. Copia la dirección final de la página de Tidal (aunque muestre Oops), pégala aquí y termina el acceso.")
                fields.append({"key": "redirect_url", "label": say("Return address", "Dirección de retorno"), "required": True,
                    "description": "", "control": {"type": "text" if self.shown else "secret", "max_length": 8192}})
                values["redirect_url"] = self.redirect
                actions.extend([{"key": "visibility", "label": "👁 " + say("Hide address" if self.shown else "Show address", "Ocultar dirección" if self.shown else "Mostrar dirección"), "fields": ["redirect_url"]},
                    {"key": "finish", "label": say("Finish sign-in", "Terminar acceso"), "fields": ["redirect_url"]},
                    {"key": "cancel", "label": say("Cancel", "Cancelar"), "fields": []}])
                links.append({"label": say("Open Tidal", "Abrir Tidal"), "url": self.pending["url"]})
            else:
                content = say("Sign in to Tidal in your browser. Your password stays in the browser. A subscription is required for full playback.",
                              "Entra a Tidal en el navegador. Tu contraseña se queda en el navegador. La reproducción completa requiere una suscripción.")
                actions.append({"key": "sign_in", "label": say("Sign in", "Entrar"), "fields": []})
            return {"revision": self.revision, "form": {"fields": fields, "actions": actions, "unsupported": []},
                "values": values, "content": [{"text": content}], "links": links, "notices": [notice] if notice else []}

    def submit(self, params, locale):
        with self.lock:
            if params.get("revision") != self.revision:
                return self.open(locale, "Settings changed. Refresh before trying again.")
            self.revision += 1
            key = params.get("action", params.get("key", ""))
            try:
                if key == "sign_in":
                    config = Config()
                    state = secrets.token_urlsafe(32)
                    query = {"response_type": "code", "redirect_uri": config.pkce_uri_redirect,
                        "client_id": config.client_id_pkce, "lang": locale[:2].upper(), "appMode": "android",
                        "client_unique_key": config.client_unique_key, "code_challenge": config.code_challenge,
                        "code_challenge_method": "S256", "restrict_signup": "true", "state": state}
                    self.pending = {"config": config, "state": state, "expires": time.monotonic() + 600,
                        "generation": self.client.generation, "url": config.api_pkce_auth + "?" + urlencode(query)}
                elif key == "visibility":
                    self.redirect = str(params.get("values", {}).get("redirect_url", ""))
                    self.shown = not self.shown
                elif key == "finish":
                    self.finish(str(params.get("values", {}).get("redirect_url", "")))
                elif key in {"cancel", "sign_out"}:
                    self.pending, self.redirect, self.shown = None, "", False
                    if key == "sign_out":
                        self.client.logout()
                elif key.startswith("quality_"):
                    quality = key.removeprefix("quality_")
                    if quality not in {"HI_RES_LOSSLESS", "LOSSLESS", "HIGH"}:
                        raise Failure("Invalid quality.", -32602)
                    self.preferences["quality"] = quality
                    save(self.data / "preferences.json", self.preferences)
                else:
                    raise Failure("Unknown settings action.", -32602)
            except Failure as error:
                return self.open(locale, str(error))
            return self.open(locale)

    def finish(self, address):
        pending = self.pending
        if not pending or time.monotonic() >= pending["expires"]:
            self.pending = None
            raise Failure("Sign-in expired. Start again.")
        config = pending["config"]
        parsed = urlsplit(address.strip())
        expected = urlsplit(config.pkce_uri_redirect)
        query = parse_qs(parsed.query)
        if (parsed.scheme, parsed.netloc, parsed.path) != (expected.scheme, expected.netloc, expected.path):
            raise Failure("Paste the final Tidal return address from this sign-in attempt.")
        # Some deployed clients omit state; the per-attempt PKCE verifier still binds the code.
        if query.get("state") and query["state"] != [pending["state"]]:
            raise Failure("This return address belongs to a different sign-in attempt.")
        if len(query.get("code", [])) != 1 or not query["code"][0]:
            raise Failure("The return address does not contain an authorization code.")
        token = self.client.checked(self.client.send("POST", TOKEN, data={"code": query["code"][0],
            "client_id": config.client_id_pkce, "grant_type": "authorization_code", "redirect_uri": config.pkce_uri_redirect,
            "scope": "r_usr+w_usr+w_sub", "code_verifier": config.code_verifier, "client_unique_key": config.client_unique_key})).json()
        self.client.accept(token, pending["generation"])
        self.pending, self.redirect, self.shown = None, "", False
