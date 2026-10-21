//! Sign-in and playback preferences, as the plugin's settings page.
//!
//! The browser sign-in is PKCE with the Android application identity, the one
//! that can stream every tier. Its return address is a page of tidal.com the
//! plugin cannot receive, so the page asks for that address, and says so before
//! the browser opens. The device-code sign-in needs nothing copied, and the
//! page labels it with the AAC limit the provider applies to it.

use crate::client::{Body, Client, DEVICE_CLIENT_ID, DEVICE_CLIENT_SECRET, PKCE_CLIENT_ID, TOKEN};
use crate::json::{at, str_at, text};
use crate::reporting::token_urlsafe;
use crate::state::{read_or, save, Failure, Result};
use base64::Engine;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const REDIRECT: &str = "https://tidal.com/android/login/auth";
pub const AUTHORIZE: &str = "https://login.tidal.com/authorize";
pub const DEVICE_AUTHORIZATION: &str = "https://auth.tidal.com/v1/oauth2/device_authorization";
const BROWSER_LIFETIME: Duration = Duration::from_secs(30 * 60);
/// The page renders its Android flow, whose last step is the return address,
/// for an Android web view.
pub const ANDROID_WEB_VIEW: &str = "Mozilla/5.0 (Linux; Android 12; wv) AppleWebKit/537.36 (KHTML, like Gecko) Version/4.0 Chrome/91.0.4472.114 Safari/537.36";
const QUALITIES: [(&str, &str); 3] = [("HI_RES_LOSSLESS", "Max · Hi-Res FLAC"), ("LOSSLESS", "High · FLAC"), ("HIGH", "AAC 320 kbps")];

enum Pending {
    Browser { url: String, verifier: String, unique_key: String, state: String, expires: Instant, generation: u64 },
    Device { device_code: String, user_code: String, link: String, interval: u64, expires: Instant, generation: u64 },
}

struct Wizard {
    pending: Option<Pending>,
    address: String,
    revision: u64,
}

pub struct Account {
    client: Arc<Client>,
    data: PathBuf,
    wizard: Mutex<Wizard>,
    preferences: Mutex<Value>,
}

fn say(locale: &str, english: &str, spanish: &str) -> String {
    if locale.starts_with("es") { spanish.to_string() } else { english.to_string() }
}

fn browser(generation: u64, locale: &str) -> Pending {
    let random: Vec<u8> = (0..32).map(|_| rand::random::<u8>()).collect();
    let verifier = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(random);
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let unique_key = format!("{:02x}", rand::random::<u64>());
    let state = token_urlsafe(32);
    let language = locale.chars().take(2).collect::<String>().to_uppercase();
    let query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("response_type", "code").append_pair("redirect_uri", REDIRECT).append_pair("client_id", PKCE_CLIENT_ID)
        .append_pair("lang", if language.is_empty() { "EN" } else { &language }).append_pair("appMode", "android")
        .append_pair("client_unique_key", &unique_key).append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256").append_pair("restrict_signup", "true").append_pair("state", &state).finish();
    Pending::Browser { url: format!("{AUTHORIZE}?{query}"), verifier, unique_key, state, expires: Instant::now() + BROWSER_LIFETIME, generation }
}

/// What the person pasted: a return address, the same without its scheme, or
/// the bare code.
fn parse_return(input: &str, expected_state: &str, locale: &str) -> Result<String> {
    let cleaned: String = input.trim().trim_matches(|c: char| c == '"' || c == '\'' || c == '<' || c == '>').split_whitespace().collect();
    if cleaned.is_empty() {
        return Err(Failure::invalid(say(locale, "Paste the address of the page Tidal shows after you approve access.",
            "Pega la dirección de la página que Tidal muestra después de que autorizas el acceso.")));
    }
    let looks_like_code = !cleaned.contains(['/', '?', '=', '&', ':', '.']) && cleaned.len() >= 8;
    if looks_like_code {
        return Ok(cleaned);
    }
    let address = if cleaned.contains("://") { cleaned.clone() } else { format!("https://{cleaned}") };
    let parsed = url::Url::parse(&address).map_err(|_| Failure::invalid(say(locale,
        "That is not a web address. Copy the whole address from the browser's address bar.",
        "Eso no es una dirección web. Copia la dirección completa de la barra del navegador.")))?;
    let host = parsed.host_str().unwrap_or("");
    if host == "login.tidal.com" || host == "link.tidal.com" {
        return Err(Failure::invalid(say(locale,
            "That is the address of the sign-in page itself. Sign in and approve access first, then copy the address of the page Tidal shows afterwards.",
            "Esa es la dirección de la página para iniciar sesión. Primero inicia sesión y autoriza el acceso; después copia la dirección de la página que Tidal muestra al final.")));
    }
    if host != "tidal.com" && !host.ends_with(".tidal.com") {
        return Err(Failure::invalid(say(locale,
            "That address is not from Tidal. Copy the address of the page Tidal shows after you approve access.",
            "Esa dirección no es de Tidal. Copia la dirección de la página que Tidal muestra después de que autorizas el acceso.")));
    }
    let pairs: Vec<(String, String)> = parsed.query_pairs().into_owned().collect();
    // Some deployed clients omit state; the per-attempt PKCE verifier still binds the code.
    if let Some((_, state)) = pairs.iter().find(|(name, _)| name == "state") {
        if state != expected_state {
            return Err(Failure::invalid(say(locale,
                "This address belongs to an earlier sign-in. Select Open Tidal sign-in again and copy the address of the new page.",
                "Esta dirección es de un intento anterior. Vuelve a seleccionar Abrir Tidal y copia la dirección de la página nueva.")));
        }
    }
    let codes: Vec<&String> = pairs.iter().filter(|(name, _)| name == "code").map(|(_, value)| value).collect();
    match codes.as_slice() {
        [code] if !code.is_empty() => Ok((*code).clone()),
        _ => Err(Failure::invalid(say(locale,
            "This address has no sign-in code yet. Approve access in Tidal first, then copy the address of the page it shows at the end.",
            "Esta dirección todavía no trae el código de acceso. Primero autoriza el acceso en Tidal; después copia la dirección de la página que muestra al final."))),
    }
}

impl Account {
    pub fn new(client: Arc<Client>, data: &Path) -> Account {
        let mut preferences = read_or(&data.join("preferences.json"), json!({"quality": "HI_RES_LOSSLESS"}));
        if !preferences.is_object() || !QUALITIES.iter().any(|(key, _)| at(&preferences, "quality") == key) {
            preferences = json!({"quality": "HI_RES_LOSSLESS"});
        }
        Account { client, data: data.to_path_buf(), wizard: Mutex::new(Wizard { pending: None, address: String::new(), revision: 1 }),
            preferences: Mutex::new(preferences) }
    }

    pub fn quality(&self) -> String {
        text(at(&self.preferences.lock().unwrap(), "quality"))
    }

    pub fn status(&self) -> Value {
        json!({"signed_in": !self.client.user().is_empty(), "name": text(at(&self.client.session().account, "name"))})
    }

    /// The page as it is now. Opening it again starts a fresh revision.
    pub fn open(&self, locale: &str) -> Value {
        let mut wizard = self.wizard.lock().unwrap();
        wizard.revision += 1;
        self.render(&mut wizard, locale, None)
    }

    pub fn reset(&self) {
        let mut wizard = self.wizard.lock().unwrap();
        wizard.pending = None;
        wizard.address.clear();
    }

    fn render(&self, wizard: &mut Wizard, locale: &str, notice: Option<String>) -> Value {
        let (mut fields, mut actions, mut links, mut content, mut values) = (Vec::new(), Vec::new(), Vec::new(), Vec::new(), json!({}));
        let mut notices: Vec<String> = notice.into_iter().collect();
        let paragraph = |content: &mut Vec<Value>, english: &str, spanish: &str| content.push(json!({"text": say(locale, english, spanish)}));
        let user = self.client.user();
        let (identity, label, phase, done, description);
        if !user.is_empty() {
            wizard.pending = None;
            (identity, phase, done) = ("done", "ready", 2);
            label = say(locale, "Ready: Tidal is connected", "Listo: Tidal está conectado");
            description = format!("{}{}", say(locale, "Signed in as ", "Conectado como "), self.client.name());
            if self.client.device_session() {
                paragraph(&mut content, "This account signed in with a code, and Tidal limits that kind of sign-in to AAC 320 kbps. For FLAC and Max, sign out and sign in again through the browser.",
                    "Esta cuenta inició sesión con un código, y Tidal limita ese tipo de acceso a AAC 320 kbps. Para FLAC y Max, cierra sesión y vuelve a entrar desde el navegador.");
            }
            paragraph(&mut content, "Playback quality. The actual resolution comes from each stream.",
                "Calidad de reproducción. La resolución real la dice cada stream.");
            let chosen = self.quality();
            for (quality, name) in QUALITIES {
                actions.push(json!({"key": format!("quality_{quality}"), "label": format!("{}{name}", if chosen == quality { "• " } else { "" }), "fields": []}));
            }
            actions.push(json!({"key": "sign_out", "label": say(locale, "Sign out", "Cerrar sesión"), "fields": []}));
        } else {
            let expired = match &wizard.pending {
                Some(Pending::Browser { expires, .. }) | Some(Pending::Device { expires, .. }) => Instant::now() >= *expires,
                None => false,
            };
            if expired {
                let device = matches!(wizard.pending, Some(Pending::Device { .. }));
                wizard.pending = None;
                wizard.address.clear();
                notices.push(if device {
                    say(locale, "The code expired. This is the browser sign-in again; you can ask for a new code below.",
                        "El código caducó. Esta es otra vez la entrada desde el navegador; abajo puedes pedir un código nuevo.")
                } else {
                    say(locale, "The previous sign-in page expired. Open the new one below.", "La página de acceso anterior caducó. Abre la nueva desde aquí.")
                });
            }
            if wizard.pending.is_none() {
                wizard.pending = Some(browser(self.client.generation(), locale));
            }
            match wizard.pending.as_ref().unwrap() {
                Pending::Browser { url, .. } => {
                    (identity, phase, done) = ("browser", "pending", 0);
                    label = say(locale, "Step 1 of 2: authorize your account", "Paso 1 de 2: autoriza tu cuenta");
                    description = say(locale, "A Tidal subscription is required for full playback. Your password stays in the browser.",
                        "La reproducción completa requiere una suscripción a Tidal. Tu contraseña se queda en el navegador.");
                    paragraph(&mut content, "1. Select Open Tidal sign-in, sign in and approve access. If Chilpa opens Tidal in a window of its own, the sign-in finishes there and nothing else is needed.",
                        "1. Selecciona Abrir Tidal, inicia sesión y autoriza el acceso. Si Chilpa abre Tidal en una ventana propia, el acceso termina ahí y no hace falta nada más.");
                    paragraph(&mut content, "2. If it opens in your browser, Tidal ends on a page that may say Oops or stay blank. That is expected: the address of that page carries the sign-in code.",
                        "2. Si se abre en tu navegador, Tidal termina en una página que puede decir Oops o quedarse en blanco. Así debe ser: la dirección de esa página trae el código de acceso.");
                    paragraph(&mut content, "3. Copy the whole address of that page from the browser's address bar (click it, then Ctrl+A and Ctrl+C), paste it below and select Finish sign-in.",
                        "3. Copia la dirección completa de esa página desde la barra del navegador (haz clic en ella, luego Ctrl+A y Ctrl+C), pégala abajo y selecciona Terminar.");
                    // A host that can open the page in its own window stops at the
                    // return address and submits it as the finish action; any other
                    // host opens the browser and the person pastes the address.
                    links.push(json!({"label": say(locale, "Open Tidal sign-in", "Abrir Tidal"), "url": url, "primary": true,
                        "capture": {"until": REDIRECT, "action": "finish", "field": "redirect_url", "user_agent": ANDROID_WEB_VIEW}}));
                    fields.push(json!({"key": "redirect_url", "label": say(locale, "Address of the page Tidal shows at the end", "Dirección de la página que Tidal muestra al final"),
                        "required": true, "description": "", "control": {"type": "text", "max_length": 8192}}));
                    values["redirect_url"] = json!(wizard.address);
                    actions.push(json!({"key": "finish", "label": say(locale, "Finish sign-in", "Terminar"), "fields": ["redirect_url"]}));
                    actions.push(json!({"key": "restart", "label": say(locale, "New sign-in page", "Nueva página de acceso"), "fields": []}));
                    actions.push(json!({"key": "device", "label": say(locale, "Sign in with a code instead (AAC only)", "Entrar con un código (solo AAC)"), "fields": []}));
                }
                Pending::Device { user_code, link, .. } => {
                    (identity, phase, done) = ("device", "pending", 0);
                    label = say(locale, "Step 1 of 2: authorize with a code", "Paso 1 de 2: autoriza con un código");
                    description = say(locale, "Tidal limits this sign-in to AAC 320 kbps. For FLAC and Max, use the browser sign-in.",
                        "Tidal limita este acceso a AAC 320 kbps. Para FLAC y Max, usa la entrada desde el navegador.");
                    paragraph(&mut content, &format!("1. Select Open link.tidal.com. The code {user_code} is already filled in; if Tidal asks for it, type it."),
                        &format!("1. Selecciona Abrir link.tidal.com. El código {user_code} ya va incluido; si Tidal te lo pide, escríbelo."));
                    paragraph(&mut content, "2. Sign in and approve access.", "2. Inicia sesión y autoriza el acceso.");
                    paragraph(&mut content, "3. Come back here and select Check sign-in.", "3. Regresa aquí y selecciona Comprobar acceso.");
                    links.push(json!({"label": say(locale, "Open link.tidal.com", "Abrir link.tidal.com"), "url": link, "primary": true}));
                    actions.push(json!({"key": "poll", "label": say(locale, "Check sign-in", "Comprobar acceso"), "fields": []}));
                    actions.push(json!({"key": "restart", "label": say(locale, "Cancel", "Cancelar"), "fields": []}));
                }
            }
        }
        json!({"revision": wizard.revision, "wizard": {"step": done + 1, "total": 2, "id": identity, "waiting": identity == "device"},
            "progress": {"done": done, "total": 2}, "status": {"phase": phase, "label": label, "description": description},
            "form": {"fields": fields, "actions": actions, "unsupported": []}, "values": values,
            "content": content, "links": links, "notices": notices})
    }

    pub fn submit(&self, params: &Value, locale: &str) -> Value {
        let mut wizard = self.wizard.lock().unwrap();
        if at(params, "revision") != &json!(wizard.revision) {
            let notice = say(locale, "Settings changed. Refresh before trying again.", "La configuración cambió. Actualiza antes de volver a intentarlo.");
            return self.render(&mut wizard, locale, Some(notice));
        }
        wizard.revision += 1;
        let key = match params.get("action") { Some(action) => text(action), None => str_at(params, "key").to_string() };
        match self.apply(&mut wizard, &key, params, locale) {
            Ok(()) => self.render(&mut wizard, locale, None),
            Err(error) => self.render(&mut wizard, locale, Some(error.message)),
        }
    }

    fn apply(&self, wizard: &mut Wizard, key: &str, params: &Value, locale: &str) -> Result<()> {
        match key {
            "finish" | "sign_in_return" => {
                wizard.address = text(at(at(params, "values"), "redirect_url"));
                self.finish_browser(wizard, locale)
            }
            "restart" | "sign_in" | "cancel" | "back" => {
                wizard.pending = Some(browser(self.client.generation(), locale));
                wizard.address.clear();
                Ok(())
            }
            "device" => {
                wizard.address.clear();
                wizard.pending = Some(self.start_device(locale)?);
                Ok(())
            }
            "poll" => self.poll_device(wizard, locale),
            "sign_out" => {
                wizard.pending = None;
                wizard.address.clear();
                self.client.logout();
                Ok(())
            }
            key if key.starts_with("quality_") => {
                let quality = &key["quality_".len()..];
                if !QUALITIES.iter().any(|(known, _)| *known == quality) {
                    return Err(Failure::invalid("Invalid quality."));
                }
                let mut preferences = self.preferences.lock().unwrap();
                preferences["quality"] = json!(quality);
                save(&self.data.join("preferences.json"), &preferences).map_err(|_| Failure::new("The preference could not be saved."))
            }
            _ => Err(Failure::invalid("Unknown settings action.")),
        }
    }

    fn finish_browser(&self, wizard: &mut Wizard, locale: &str) -> Result<()> {
        let Some(Pending::Browser { verifier, unique_key, state, expires, generation, .. }) = &wizard.pending else {
            wizard.pending = Some(browser(self.client.generation(), locale));
            return Err(Failure::new(say(locale, "Open Tidal sign-in first.", "Primero abre Tidal para iniciar sesión.")));
        };
        if Instant::now() >= *expires {
            wizard.pending = Some(browser(self.client.generation(), locale));
            wizard.address.clear();
            return Err(Failure::new(say(locale, "This sign-in page expired. Open the new one and try again.",
                "Esta página de acceso caducó. Abre la nueva e inténtalo otra vez.")));
        }
        let code = parse_return(&wizard.address, state, locale)?;
        let form = vec![("code".into(), code), ("client_id".into(), PKCE_CLIENT_ID.into()), ("grant_type".into(), "authorization_code".into()),
            ("redirect_uri".into(), REDIRECT.into()), ("scope".into(), "r_usr+w_usr+w_sub".into()), ("code_verifier".into(), verifier.clone()),
            ("client_unique_key".into(), unique_key.clone())];
        let generation = *generation;
        let reply = self.client.send("POST", TOKEN, &[], &[], Body::Form(form), None)?;
        if (400..500).contains(&reply.status) && reply.status != 429 {
            // A code works once; the next attempt needs a new page and verifier.
            wizard.pending = Some(browser(self.client.generation(), locale));
            wizard.address.clear();
            return Err(Failure::new(say(locale,
                "Tidal did not accept this code. A code works once and only for a few minutes: select Open Tidal sign-in again and copy the address of the new page.",
                "Tidal no aceptó este código. Cada código sirve una sola vez y solo unos minutos: vuelve a seleccionar Abrir Tidal y copia la dirección de la página nueva.")));
        }
        let token = Client::checked(reply)?.json()?;
        self.client.accept(&token, generation, false)?;
        wizard.pending = None;
        wizard.address.clear();
        Ok(())
    }

    fn start_device(&self, locale: &str) -> Result<Pending> {
        let form = vec![("client_id".into(), DEVICE_CLIENT_ID.into()), ("scope".into(), "r_usr w_usr w_sub".into())];
        let answer = Client::checked(self.client.send("POST", DEVICE_AUTHORIZATION, &[], &[], Body::Form(form), None)?)?.json()?;
        let (device_code, user_code) = (text(at(&answer, "deviceCode")), text(at(&answer, "userCode")));
        if device_code.is_empty() || user_code.is_empty() {
            return Err(Failure::new(say(locale, "Tidal did not return a sign-in code. Try again.", "Tidal no devolvió un código de acceso. Inténtalo otra vez.")));
        }
        let complete = text(at(&answer, "verificationUriComplete"));
        let base = if complete.is_empty() { format!("link.tidal.com/{user_code}") } else { complete };
        let link = if base.starts_with("https://") { base } else { format!("https://{}", base.trim_start_matches("http://")) };
        let link = crate::streaming::stream_url(&link).map_err(|_| Failure::new("Tidal returned an unexpected sign-in address."))?;
        let lifetime = at(&answer, "expiresIn").as_u64().unwrap_or(300).clamp(30, 3600);
        let interval = at(&answer, "interval").as_u64().unwrap_or(2).clamp(1, 30);
        Ok(Pending::Device { device_code, user_code, link, interval, expires: Instant::now() + Duration::from_secs(lifetime), generation: self.client.generation() })
    }

    /// Asks whether the code was approved, for up to twenty seconds.
    fn poll_device(&self, wizard: &mut Wizard, locale: &str) -> Result<()> {
        let Some(Pending::Device { device_code, interval, expires, generation, .. }) = &wizard.pending else {
            return Err(Failure::new(say(locale, "Ask for a code first.", "Primero pide un código.")));
        };
        let (device_code, mut interval, expires, generation) = (device_code.clone(), *interval, *expires, *generation);
        let until = (Instant::now() + Duration::from_secs(20)).min(expires);
        loop {
            let form = vec![("client_id".into(), DEVICE_CLIENT_ID.into()), ("client_secret".into(), DEVICE_CLIENT_SECRET.into()),
                ("device_code".into(), device_code.clone()), ("grant_type".into(), "urn:ietf:params:oauth:grant-type:device_code".into()),
                ("scope".into(), "r_usr w_usr w_sub".into())];
            let reply = self.client.send("POST", TOKEN, &[], &[], Body::Form(form), None)?;
            if reply.status == 200 {
                let token = reply.json()?;
                self.client.accept(&token, generation, true)?;
                wizard.pending = None;
                return Ok(());
            }
            let error = text(at(&reply.json().unwrap_or(Value::Null), "error"));
            match error.as_str() {
                "authorization_pending" => {}
                "slow_down" => interval += 2,
                "expired_token" => {
                    wizard.pending = Some(browser(self.client.generation(), locale));
                    return Err(Failure::new(say(locale, "The code expired. Ask for a new one, or use the browser sign-in.",
                        "El código caducó. Pide uno nuevo o entra desde el navegador.")));
                }
                _ => return Client::checked(reply).map(|_| ()).and(Err(Failure::new(say(locale,
                    "Tidal did not accept the sign-in. Try again.", "Tidal no aceptó el acceso. Inténtalo otra vez.")))),
            }
            let next = Instant::now() + Duration::from_secs(interval);
            if next >= until {
                let remaining = expires.saturating_duration_since(Instant::now()).as_secs() / 60;
                return Err(Failure::new(if locale.starts_with("es") {
                    format!("Tidal todavía no confirma el acceso. Autorízalo en link.tidal.com y vuelve a seleccionar Comprobar acceso (el código sigue vigente unos {} min).", remaining.max(1))
                } else {
                    format!("Tidal has not confirmed the sign-in yet. Approve it on link.tidal.com, then select Check sign-in again (the code stays valid for about {} min).", remaining.max(1))
                }));
            }
            std::thread::sleep(Duration::from_secs(interval));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_return_address_yields_its_code_with_or_without_the_scheme() {
        let state = "abc";
        assert_eq!(parse_return("https://tidal.com/android/login/auth?code=XYZ123&state=abc", state, "en").unwrap(), "XYZ123");
        assert_eq!(parse_return(" tidal.com/android/login/auth?code=XYZ123 \n", state, "en").unwrap(), "XYZ123");
        assert_eq!(parse_return("XYZ123ABCDEF", state, "en").unwrap(), "XYZ123ABCDEF");
    }

    #[test]
    fn the_sign_in_page_or_another_attempt_is_explained() {
        let early = parse_return("https://login.tidal.com/authorize?client_id=x", "abc", "en").unwrap_err();
        assert!(early.message.contains("sign-in page itself"));
        let other = parse_return("https://tidal.com/android/login/auth?code=1&state=zzz", "abc", "en").unwrap_err();
        assert!(other.message.contains("earlier sign-in"));
        let empty = parse_return("https://tidal.com/android/login/auth?state=abc", "abc", "es").unwrap_err();
        assert!(empty.message.contains("todavía no trae el código"));
        assert!(parse_return("https://example.com/?code=1", "abc", "en").is_err());
    }

    #[test]
    fn the_browser_page_asks_for_s256_with_the_android_identity() {
        let Pending::Browser { url, verifier, .. } = browser(0, "es-MX") else { panic!() };
        let parsed = url::Url::parse(&url).unwrap();
        let pairs: std::collections::HashMap<String, String> = parsed.query_pairs().into_owned().collect();
        assert_eq!(pairs["client_id"], PKCE_CLIENT_ID);
        assert_eq!(pairs["redirect_uri"], REDIRECT);
        assert_eq!(pairs["lang"], "ES");
        let expected = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        assert_eq!(pairs["code_challenge"], expected);
        assert_eq!(verifier.len(), 43);
    }

    #[test]
    fn the_signed_out_page_shows_the_link_the_field_and_the_steps() {
        let folder = tempfile::tempdir().unwrap();
        let client = Arc::new(Client::new(folder.path()));
        let account = Account::new(client, folder.path());
        let page = account.open("en");
        assert_eq!(page["links"][0]["primary"], true);
        assert_eq!(page["links"][0]["capture"], json!({"until": REDIRECT, "action": "finish", "field": "redirect_url", "user_agent": ANDROID_WEB_VIEW}));
        assert!(page["links"][0]["url"].as_str().unwrap().starts_with(AUTHORIZE));
        assert_eq!(page["form"]["fields"][0]["key"], "redirect_url");
        assert_eq!(page["form"]["fields"][0]["control"]["type"], "text");
        assert_eq!(page["content"].as_array().unwrap().len(), 3);
        let stale = account.submit(&json!({"revision": 1, "action": "finish", "values": {"redirect_url": "x"}}), "en");
        assert!(stale["notices"][0].as_str().unwrap().contains("Refresh"));
        let revision = stale["revision"].clone();
        let early = account.submit(&json!({"revision": revision, "action": "finish", "values": {"redirect_url": "https://login.tidal.com/authorize"}}), "en");
        assert!(early["notices"][0].as_str().unwrap().contains("sign-in page itself"));
        assert_eq!(early["values"]["redirect_url"], "https://login.tidal.com/authorize");
    }
}
