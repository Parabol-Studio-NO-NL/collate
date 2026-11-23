"""Validate the updater's private HTTPS endpoint configuration."""

import re
import sys
from urllib.parse import urlsplit


def private_update_base(value):
    if not value or re.search(r"[\x00-\x20\x7f\\]", value):
        raise ValueError("Set CODESIGN_PRIVATE_UPDATE_BASE_URL to a private HTTPS endpoint.")
    url = urlsplit(value.rstrip("/"))
    host = (url.hostname or "").rstrip(".")
    deny = ("github.com", "github.io", "githubusercontent.com", "getartcraft.com")
    if (
        url.scheme != "https"
        or not host
        or url.port == 0
        or any(host == domain or host.endswith(f".{domain}") for domain in deny)
        or url.username
        or url.password
        or url.query
        or url.fragment
        or not url.path.endswith("/updates")
        or not re.fullmatch(r"(?:/[a-zA-Z0-9_-]+)+", url.path)
    ):
        raise ValueError("Use a private HTTPS URL ending in /updates, without credentials.")
    return url.geturl()


if __name__ == "__main__":
    try:
        print(private_update_base(sys.argv[1]))
    except ValueError as error:
        sys.exit(str(error))
