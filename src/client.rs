//! Account-scoped REST requests; errors never contain tokens or signed URLs.

use crate::json::{at, text, truthy};
use crate::state::{read_or, save, Failure, Result};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const API: &str = "https://api.tidal.com/v1/";
pub const V2: &str = "https://api.tidal.com/v2/";
pub const TOKEN: &str = "https://auth.tidal.com/v1/oauth2/token";

/// The two public application identities: the browser (PKCE) one, which can
/// stream every tier, and the device-code one, which the provider limits.
pub const PKCE_CLIENT_ID: &str = "6BDSRdpK9hqEBTgU";
pub const PKCE_CLIENT_SECRET: &str = "xeuPmY7nbpZ9IIbLAcQ93shka1VNheUAqN6IcszjTG8=";
pub const DEVICE_CLIENT_ID: &str = "fX2JxdmntZWK0ixT";
pub const DEVICE_CLIENT_SECRET: &str = "1Nn9AfDAjxrgJFJbKNWLeAyKGVGmINuXPPLHVXAvxAg=";

/// The address a request goes to. With the `mock-endpoints` feature and
/// `CHILPA_TIDAL_MOCK` set, every HTTPS request goes to that base instead, with
/// the original host as the first path segment.
pub fn endpoint(address: &str) -> String {
    #[cfg(feature = "mock-endpoints")]
    if let (Ok(base), Some(rest)) = (std::env::var("CHILPA_TIDAL_MOCK"), address.strip_prefix("https://")) {
        return format!("{}/{rest}", base.trim_end_matches('/'));
    }
    address.to_string()
}

pub fn now() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

/// What came back: the status, the headers the plugin reads, and the body.
pub struct Reply {
    pub status: u16,
    pub etag: String,
    pub retry_after: String,
    pub location: String,
    pub body: Vec<u8>,
}

impl Reply {
    pub fn json(&self) -> Result<Value> {
        if self.body.is_empty() {
            return Ok(json!({}));
        }
        serde_json::from_slice(&self.body).map_err(|_| Failure::new("Tidal returned an unreadable response."))
    }
}

/// The wait a limited response asks for, as the host reads it from an error's data.
pub fn pause(reply: &Reply) -> Option<Value> {
    let seconds: i64 = reply.retry_after.trim().parse().ok()?;
    (seconds > 0).then(|| json!({"retry_after_ms": seconds.min(3600) * 1000}))
}

pub struct Session {
    pub account: Value,
    pub generation: u64,
}

pub struct Client {
    path: PathBuf,
    session: Mutex<Session>,
    http: reqwest::blocking::Client,
}

pub enum Body {
    Empty,
    Form(Vec<(String, String)>),
}

impl Client {
    pub fn new(data: &Path) -> Client {
        let path = data.join("account.json");
        let account = read_or(&path, json!({}));
        let account = if account.is_object() { account } else { json!({}) };
        Client { path, session: Mutex::new(Session { account, generation: 0 }), http: http_client() }
    }

    pub fn http(&self) -> &reqwest::blocking::Client {
        &self.http
    }

    pub fn session(&self) -> MutexGuard<'_, Session> {
        self.session.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn generation(&self) -> u64 {
        self.session().generation
    }

    pub fn user(&self) -> String {
        text(at(&self.session().account, "user_id"))
    }

    pub fn name(&self) -> String {
        let session = self.session();
        let name = text(at(&session.account, "name"));
        if name.is_empty() { text(at(&session.account, "user_id")) } else { name }
    }

    /// Whether the saved session came from the device-code sign-in.
    pub fn device_session(&self) -> bool {
        at(&self.session().account, "client").as_str() == Some("device")
    }

    pub fn logout(&self) {
        let mut session = self.session();
        session.generation += 1;
        session.account = json!({});
        let _ = std::fs::remove_file(&self.path);
    }

    pub fn send(&self, method: &str, url: &str, query: &[(String, String)], headers: &[(&str, String)], body: Body,
                timeout: Option<Duration>) -> Result<Reply> {
        let method = reqwest::Method::from_bytes(method.as_bytes()).map_err(|_| Failure::new("Invalid request."))?;
        let mut request = self.http.request(method, endpoint(url)).timeout(timeout.unwrap_or(Duration::from_secs(30)));
        if !query.is_empty() {
            request = request.query(query);
        }
        for (name, value) in headers {
            request = request.header(*name, value);
        }
        if let Body::Form(fields) = body {
            request = request.form(&fields);
        }
        let response = request.send().map_err(|error| {
            let failure = Failure::new("Tidal could not be reached. Try again.");
            if error.is_timeout() { failure.outcome("timeout") } else { failure }
        })?;
        let header = |name: &str| response.headers().get(name).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
        let (status, etag, retry_after, location) = (response.status().as_u16(), header("etag"), header("retry-after"), header("location"));
        let body = response.bytes().map_err(|error| {
            let failure = Failure::new("Tidal could not be reached. Try again.");
            if error.is_timeout() { failure.outcome("timeout") } else { failure }
        })?;
        Ok(Reply { status, etag, retry_after, location, body: body.to_vec() })
    }

    pub fn checked(reply: Reply) -> Result<Reply> {
        let status = reply.status;
        if status >= 400 {
            let message = match status {
                401 => "Sign in to Tidal again.",
                403 => "This account cannot access this content.",
                404 => "This item is no longer available.",
                409 | 412 => "The playlist changed. Refresh it before editing.",
                429 => "Tidal is limiting requests. Try again shortly.",
                _ => "Tidal could not complete the request.",
            };
            let mut failure = Failure::new(message).code(if status == 401 { -32001 } else { -32000 });
            failure.status = Some(status);
            if status == 429 {
                failure.data = pause(&reply);
            }
            return Err(failure);
        }
        Ok(reply)
    }

    /// Saves a token answer once the session it opens is confirmed, unless a
    /// sign-out or a newer sign-in happened since `expected_generation`.
    pub fn accept(&self, token: &Value, expected_generation: u64, device: bool) -> Result<()> {
        let access = at(token, "access_token").as_str().filter(|value| !value.is_empty())
            .ok_or_else(|| Failure::new("The sign-in response did not contain an access token."))?;
        let info = Self::checked(self.send("GET", &format!("{API}sessions"), &[],
            &[("Authorization", format!("Bearer {access}"))], Body::Empty, None)?)?.json()?;
        let pick = |values: [&Value; 3]| values.into_iter().find(|value| truthy(value)).cloned().unwrap_or(Value::Null);
        let user = pick([at(&info, "userId"), at(token, "user_id"), at(at(token, "user"), "userId")]);
        let country = pick([at(&info, "countryCode"), at(at(token, "user"), "countryCode"), &Value::Null]);
        if !truthy(&user) || !truthy(&country) {
            return Err(Failure::new("Tidal did not return the account and country information."));
        }
        let mut session = self.session();
        if session.generation != expected_generation {
            return Err(Failure::new("This sign-in attempt was cancelled."));
        }
        let previous = session.account.clone();
        let refresh = match at(token, "refresh_token") {
            Value::String(value) => value.clone(),
            _ => text(at(&previous, "refresh_token")),
        };
        let expires_in = at(token, "expires_in").as_f64().unwrap_or(3600.0).trunc();
        let username = at(at(token, "user"), "username");
        let mut account = json!({"access_token": access, "refresh_token": refresh,
            "expires": now() + expires_in, "user_id": text(&user), "country": country,
            "name": if truthy(username) { username.clone() } else { Value::String(text(&user)) }});
        if device {
            account["client"] = json!("device");
        }
        save(&self.path, &account).map_err(|_| Failure::new("The account could not be saved."))?;
        session.account = account;
        Ok(())
    }

    fn refresh(&self, session: &mut MutexGuard<'_, Session>) -> Result<()> {
        let device = at(&session.account, "client").as_str() == Some("device");
        let (id, secret) = if device { (DEVICE_CLIENT_ID, DEVICE_CLIENT_SECRET) } else { (PKCE_CLIENT_ID, PKCE_CLIENT_SECRET) };
        let form = vec![("grant_type".into(), "refresh_token".into()),
            ("refresh_token".into(), text(at(&session.account, "refresh_token"))),
            ("client_id".into(), id.into()), ("client_secret".into(), secret.into())];
        let reply = Self::checked(self.send("POST", TOKEN, &[], &[], Body::Form(form), None)?)?.json()?;
        let generation = session.generation;
        // `accept` takes the lock itself; the refreshed token is saved with the
        // generation read under this lock, so a sign-out in between cancels it.
        let access = at(&reply, "access_token").as_str().filter(|value| !value.is_empty())
            .ok_or_else(|| Failure::new("The sign-in response did not contain an access token."))?.to_string();
        let info = Self::checked(self.send("GET", &format!("{API}sessions"), &[],
            &[("Authorization", format!("Bearer {access}"))], Body::Empty, None)?)?.json()?;
        let user = [at(&info, "userId"), at(&reply, "user_id"), at(at(&reply, "user"), "userId")].into_iter()
            .find(|value| truthy(value)).cloned().unwrap_or(Value::Null);
        let country = [at(&info, "countryCode"), at(at(&reply, "user"), "countryCode")].into_iter()
            .find(|value| truthy(value)).cloned().unwrap_or(Value::Null);
        if !truthy(&user) || !truthy(&country) {
            return Err(Failure::new("Tidal did not return the account and country information."));
        }
        if session.generation != generation {
            return Err(Failure::new("This sign-in attempt was cancelled."));
        }
        let previous = session.account.clone();
        let refresh = match at(&reply, "refresh_token") {
            Value::String(value) => value.clone(),
            _ => text(at(&previous, "refresh_token")),
        };
        let username = at(at(&reply, "user"), "username");
        let mut account = json!({"access_token": access, "refresh_token": refresh,
            "expires": now() + at(&reply, "expires_in").as_f64().unwrap_or(3600.0).trunc(), "user_id": text(&user),
            "country": country, "name": if truthy(username) { username.clone() } else { Value::String(text(&user)) }});
        if device {
            account["client"] = json!("device");
        }
        save(&self.path, &account).map_err(|_| Failure::new("The account could not be saved."))?;
        session.account = account;
        Ok(())
    }

    /// A current access token and the account's country, refreshing it when
    /// it expires within 90 seconds.
    pub fn token(&self) -> Result<(String, String)> {
        let mut session = self.session();
        if !truthy(at(&session.account, "access_token")) {
            return Err(Failure::new("Sign in to Tidal first.").code(-32001).outcome("no token"));
        }
        if at(&session.account, "expires").as_f64().unwrap_or(0.0) < now() + 90.0 {
            self.refresh(&mut session)?;
        }
        Ok((text(at(&session.account, "access_token")), text(at(&session.account, "country"))))
    }

    /// The token for optional metadata: it never refreshes, never waits past
    /// the deadline, and must belong to `user`.
    pub fn token_until(&self, deadline: Instant, user: &str) -> Result<(String, String)> {
        let session = loop {
            if Instant::now() >= deadline {
                return Err(Failure::new("The optional request ran out of time.").outcome("timeout"));
            }
            match self.session.try_lock() {
                Ok(session) => break session,
                Err(std::sync::TryLockError::Poisoned(poisoned)) => break poisoned.into_inner(),
                Err(std::sync::TryLockError::WouldBlock) => std::thread::sleep(Duration::from_millis(5)),
            }
        };
        if !truthy(at(&session.account, "access_token")) || text(at(&session.account, "user_id")) != user
            || at(&session.account, "expires").as_f64().unwrap_or(0.0) <= now() {
            return Err(Failure::new("The optional request needs a current account.").outcome("no token"));
        }
        Ok((text(at(&session.account, "access_token")), text(at(&session.account, "country"))))
    }

    /// A catalog request. `params` is an object; null values are left out.
    pub fn request(&self, path: &str, params: &Value, method: &str, data: Option<&Value>, etag: Option<&str>,
                   v2: bool, deadline: Option<(Instant, &str)>) -> Result<(Value, String)> {
        let base = url::Url::parse(if v2 { V2 } else { API }).expect("static base");
        let relative = if path.starts_with("/v1/") || path.starts_with("/v2/") { path.to_string() } else { path.trim_start_matches('/').to_string() };
        let url = base.join(&relative).map_err(|_| Failure::invalid("Invalid catalog link."))?;
        if url.scheme() != "https" || !matches!(url.host_str(), Some("api.tidal.com" | "listen.tidal.com")) {
            return Err(Failure::invalid("Invalid catalog link."));
        }
        let (token, country) = match deadline {
            Some((until, user)) => self.token_until(until, user)?,
            None => self.token()?,
        };
        let mut headers = vec![("Authorization", format!("Bearer {token}")), ("x-tidal-client-version", "2025.7.16".to_string())];
        if let Some(tag) = etag.filter(|tag| !tag.is_empty()) {
            headers.push(("If-None-Match", tag.to_string()));
        }
        let mut query = vec![("countryCode".to_string(), country)];
        query.extend(pairs(params));
        let timeout = match deadline {
            Some((until, _)) => {
                let remaining = until.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err(Failure::new("The optional request ran out of time.").outcome("timeout"));
                }
                Some(remaining)
            }
            None => None,
        };
        let body = data.map(|fields| Body::Form(pairs(fields))).unwrap_or(Body::Empty);
        let reply = Self::checked(self.send(method, url.as_str(), &query, &headers, body, timeout)?)?;
        if (300..400).contains(&reply.status) {
            return Err(Failure::new("Tidal returned an unexpected redirect."));
        }
        let etag = reply.etag.clone();
        Ok((reply.json()?, etag))
    }

    pub fn get(&self, path: &str, params: &Value) -> Result<Value> {
        Ok(self.request(path, params, "GET", None, None, false, None)?.0)
    }

    pub fn get_v2(&self, path: &str, params: &Value) -> Result<Value> {
        Ok(self.request(path, params, "GET", None, None, true, None)?.0)
    }

    pub fn get_until(&self, path: &str, params: &Value, deadline: Instant, user: &str) -> Result<Value> {
        Ok(self.request(path, params, "GET", None, None, false, Some((deadline, user)))?.0)
    }

    /// Every item of a paginated listing, a hundred per request.
    pub fn all(&self, path: &str) -> Result<Vec<Value>> {
        let mut rows = Vec::new();
        let mut offset = 0;
        while offset < 100000 {
            let page = self.get(path, &json!({"limit": 100, "offset": offset}))?;
            let batch = crate::json::list_at(&page, "items");
            rows.extend(batch.iter().cloned());
            let total = crate::json::int_at(&page, "totalNumberOfItems", 100001);
            if batch.len() < 100 || rows.len() as i64 >= total {
                return Ok(rows);
            }
            offset += 100;
        }
        Err(Failure::new("This collection is too large to load at once."))
    }
}

/// Query or form pairs from an object, without null values.
pub fn pairs(params: &Value) -> Vec<(String, String)> {
    params.as_object().map(|fields| fields.iter().filter(|(_, value)| !value.is_null())
        .map(|(key, value)| (key.clone(), match value {
            Value::String(text) => text.clone(),
            Value::Bool(flag) => if *flag { "True".into() } else { "False".into() },
            other => other.to_string(),
        })).collect()).unwrap_or_default()
}

pub fn http_client() -> reqwest::blocking::Client {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    reqwest::blocking::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(8))
        .timeout(Duration::from_secs(30))
        .build()
        .expect("the HTTP client starts")
}
