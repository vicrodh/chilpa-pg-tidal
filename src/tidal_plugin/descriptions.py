"""Provider biography links become native references and safe styled text."""
import html
import re
from urllib.parse import urlsplit
from html.parser import HTMLParser


def native_link(link):
    parsed = urlsplit(link)
    if parsed.hostname in {"tidal.com", "www.tidal.com", "listen.tidal.com"} or (not parsed.scheme and link.startswith("/")):
        found = re.search(r"/(artist|album|playlist|track)/([\w-]+)(?:/|$)", parsed.path)
        if found:
            kind, key = found.groups()
            return f"https://tidal.com/browse/track/{key}" if kind == "track" else f"{kind}:{key}"
    if link.startswith("track:"):
        return "https://tidal.com/browse/track/" + link[6:]
    return link


class Styled(HTMLParser):
    def __init__(self):
        super().__init__(convert_charrefs=True)
        self.parts = []

    def handle_starttag(self, tag, attrs):
        if tag in {"b", "strong", "i", "em", "p", "br", "ul", "ol", "li"}:
            self.parts.append("<" + tag + ">")
        elif tag == "a":
            link = native_link(dict(attrs).get("href", ""))
            if re.match(r"^(?:artist|album|track|playlist):[\w-]+$|^https?://", link):
                self.parts.append('<a href="' + html.escape(link, quote=True) + '">')
            else:
                self.parts.append("<a>")

    def handle_endtag(self, tag):
        if tag in {"b", "strong", "i", "em", "p", "ul", "ol", "li", "a"}:
            self.parts.append("</" + tag + ">")

    def handle_data(self, data):
        self.parts.append(html.escape(data).replace("\n", "<br>"))


def description(text):
    text = re.sub(r'\[wimpLink\s+(artist|album|track|playlist)Id="([\w-]+)"\]',
                  lambda match: '<a href="' + match[1] + ':' + match[2] + '">', str(text or ""))
    text = text.replace("[/wimpLink]", "</a>")
    parser = Styled()
    parser.feed(text)
    return "".join(parser.parts)
