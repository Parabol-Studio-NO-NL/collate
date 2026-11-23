#!/usr/bin/env python3
"""Check a staged Collate appcast against the two deployed update channels."""

import argparse
import re
import xml.etree.ElementTree as ET
from pathlib import Path

SPARKLE = {"sparkle": "http://www.andymatuschak.org/xml-namespaces/sparkle"}
MAX_FEED_BYTES = 4 * 1024 * 1024


def build_numbers(path: Path, channel: str) -> list[int]:
    with path.open("rb") as feed:
        data = feed.read(MAX_FEED_BYTES + 1)
    if len(data) > MAX_FEED_BYTES:
        raise ValueError(f"{path} exceeds the 4 MiB feed limit.")

    root = ET.fromstring(data)
    channels = root.findall("channel")
    if root.tag != "rss" or len(channels) != 1:
        raise ValueError(f"{path} must contain one RSS channel.")
    items = channels[0].findall("item")
    if not items:
        raise ValueError(f"{path} contains no releases.")

    numbers = []
    expected = ["beta"] if channel == "beta" else []
    for item in items:
        tags = [node.text for node in item.findall("sparkle:channel", SPARKLE)]
        if tags != expected:
            raise ValueError(f"{path} contains a release outside the {channel} channel.")
        versions = item.findall("sparkle:version", SPARKLE)
        if len(versions) != 1 or len(versions[0]) or not re.fullmatch(r"[1-9][0-9]{0,8}", versions[0].text or ""):
            raise ValueError(f"{path} contains an invalid Sparkle build number.")
        numbers.append(int(versions[0].text))
    return numbers


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("candidate", type=Path, help="the staged single-release appcast")
    parser.add_argument("channel", choices=("stable", "beta"))
    parser.add_argument("stable", help="deployed stable appcast, or '-' for a channel that has never shipped")
    parser.add_argument("beta", help="deployed beta appcast, or '-' for a channel that has never shipped")
    args = parser.parse_args()

    try:
        candidate = build_numbers(args.candidate, args.channel)
        if len(candidate) != 1:
            raise ValueError("The staged appcast must contain one release.")
        deployed = []
        for channel in ("stable", "beta"):
            path = getattr(args, channel)
            if path != "-":
                deployed.extend(build_numbers(Path(path), channel))
        latest = max(deployed, default=0)
        if candidate[0] <= latest:
            raise ValueError(f"Build {candidate[0]} must be greater than deployed build {latest}.")
    except (OSError, ET.ParseError, ValueError) as error:
        parser.exit(1, f"Build number check failed. {error}\n")

    print(f"Build {candidate[0]} is greater than the deployed maximum {latest}.")


if __name__ == "__main__":
    main()
