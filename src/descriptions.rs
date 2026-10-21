//! Provider biography links become native references and safe styled text.

use regex::Regex;
use std::sync::LazyLock;

static WIMP_LINK: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"\[wimpLink\s+(artist|album|track|playlist)Id="([\w-]+)"\]"#).unwrap());
static NATIVE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"/(artist|album|playlist|track)/([\w-]+)(?:/|$)").unwrap());
static ALLOWED: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(?:artist|album|track|playlist):[\w-]+$|^https?://").unwrap());

pub fn native_link(link: &str) -> String {
    let parsed = url::Url::parse(link).ok();
    let host = parsed.as_ref().and_then(|url| url.host_str()).unwrap_or("");
    let path = match &parsed {
        Some(url) => url.path().to_string(),
        None => link.split(['?', '#']).next().unwrap_or("").to_string(),
    };
    if ["tidal.com", "www.tidal.com", "listen.tidal.com"].contains(&host) || (parsed.is_none() && link.starts_with('/')) {
        if let Some(found) = NATIVE.captures(&path) {
            let (kind, key) = (&found[1], &found[2]);
            return if kind == "track" { format!("https://tidal.com/browse/track/{key}") } else { format!("{kind}:{key}") };
        }
    }
    if let Some(key) = link.strip_prefix("track:") {
        return format!("https://tidal.com/browse/track/{key}");
    }
    link.to_string()
}

fn escape(text: &str, quote: bool) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' if quote => out.push_str("&quot;"),
            '\'' if quote => out.push_str("&#x27;"),
            ch => out.push(ch),
        }
    }
    out
}

/// Character references in text and attribute values, as an HTML parser reads them.
fn unescape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        let end = rest[1..].find(|c: char| !(c.is_ascii_alphanumeric() || c == '#')).map(|i| i + 1).unwrap_or(rest.len());
        let name = &rest[1..end];
        let decoded = if let Some(number) = name.strip_prefix('#') {
            let value = if let Some(hex) = number.strip_prefix(['x', 'X']) { u32::from_str_radix(hex, 16).ok() } else { number.parse().ok() };
            value.and_then(char::from_u32).map(String::from)
        } else {
            match name {
                "amp" => Some("&".into()), "lt" => Some("<".into()), "gt" => Some(">".into()),
                "quot" => Some("\"".into()), "apos" => Some("'".into()), "nbsp" => Some("\u{a0}".into()),
                "hellip" => Some("…".into()), "mdash" => Some("—".into()), "ndash" => Some("–".into()),
                "rsquo" => Some("’".into()), "lsquo" => Some("‘".into()), "rdquo" => Some("”".into()), "ldquo" => Some("“".into()),
                _ => None,
            }
        };
        match decoded {
            Some(value) if end > 1 => {
                out.push_str(&value);
                rest = &rest[end..];
                if rest.starts_with(';') {
                    rest = &rest[1..];
                }
            }
            _ => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

fn attribute(tag: &str, wanted: &str) -> Option<String> {
    let mut rest = tag;
    while !rest.is_empty() {
        rest = rest.trim_start();
        let name_end = rest.find(|c: char| c.is_whitespace() || c == '=' || c == '/' ).unwrap_or(rest.len());
        let name = rest[..name_end].to_lowercase();
        rest = rest[name_end..].trim_start();
        let value = if let Some(after) = rest.strip_prefix('=') {
            let after = after.trim_start();
            if let Some(quote) = after.chars().next().filter(|c| *c == '"' || *c == '\'') {
                let close = after[1..].find(quote).map(|i| i + 1).unwrap_or(after.len());
                let value = after[1..close].to_string();
                rest = after.get(close + 1..).unwrap_or("");
                value
            } else {
                let close = after.find(char::is_whitespace).unwrap_or(after.len());
                let value = after[..close].to_string();
                rest = &after[close..];
                value
            }
        } else {
            if name.is_empty() {
                rest = rest.get(1..).unwrap_or("");
            }
            String::new()
        };
        if name == wanted {
            return Some(unescape(&value));
        }
    }
    None
}

/// Safe styled text: a few formatting tags, native links, everything else escaped.
pub fn description(text: &str) -> String {
    let text = WIMP_LINK.replace_all(text, |found: &regex::Captures| format!(r#"<a href="{}:{}">"#, &found[1], &found[2]));
    let text = text.replace("[/wimpLink]", "</a>");
    let mut parts = String::new();
    let mut rest = text.as_str();
    let data = |chunk: &str, parts: &mut String| parts.push_str(&escape(&unescape(chunk), true).replace('\n', "<br>"));
    while let Some(open) = rest.find('<') {
        let after = &rest[open + 1..];
        let is_tag = after.starts_with(|c: char| c.is_ascii_alphabetic() || c == '/' || c == '!');
        let close = after.find('>');
        if !is_tag || close.is_none() {
            data(&rest[..open + 1], &mut parts);
            rest = after;
            continue;
        }
        data(&rest[..open], &mut parts);
        let inner = &after[..close.unwrap()];
        rest = &after[close.unwrap() + 1..];
        if inner.starts_with('!') {
            continue;
        }
        if let Some(name) = inner.strip_prefix('/') {
            let name = name.trim().to_lowercase();
            if ["b", "strong", "i", "em", "p", "ul", "ol", "li", "a"].contains(&name.as_str()) {
                parts.push_str(&format!("</{name}>"));
            }
            continue;
        }
        let name_end = inner.find(|c: char| c.is_whitespace() || c == '/').unwrap_or(inner.len());
        let name = inner[..name_end].to_lowercase();
        if ["b", "strong", "i", "em", "p", "br", "ul", "ol", "li"].contains(&name.as_str()) {
            parts.push_str(&format!("<{name}>"));
        } else if name == "a" {
            let link = native_link(&attribute(&inner[name_end..], "href").unwrap_or_default());
            if ALLOWED.is_match(&link) {
                parts.push_str(&format!(r#"<a href="{}">"#, escape(&link, true)));
            } else {
                parts.push_str("<a>");
            }
        }
    }
    data(rest, &mut parts);
    parts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_links_become_native_references() {
        let text = r#"Played with [wimpLink artistId="12"]Someone[/wimpLink] & <b>others</b><script>x</script>"#;
        assert_eq!(description(text), r#"Played with <a href="artist:12">Someone</a> &amp; <b>others</b>x"#);
    }

    #[test]
    fn a_web_link_to_a_track_stays_a_web_link() {
        assert_eq!(native_link("https://tidal.com/browse/album/5"), "album:5");
        assert_eq!(native_link("https://tidal.com/browse/track/9"), "https://tidal.com/browse/track/9");
        assert_eq!(native_link("javascript:alert(1)"), "javascript:alert(1)");
    }

    #[test]
    fn new_lines_and_references_render_as_markup() {
        assert_eq!(description("a\nb &amp; c"), "a<br>b &amp; c");
    }
}
