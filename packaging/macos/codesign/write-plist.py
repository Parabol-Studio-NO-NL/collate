"""Derive the fork's macOS bundle metadata from the upstream document associations."""

import base64
import os
import plistlib
import re
import sys
from pathlib import Path
from urllib.parse import urlsplit


def write_plist(template, destination):
    version = os.environ["CODESIGN_VERSION"]
    build = os.environ["CODESIGN_BUILD_NUMBER"]
    channel = os.environ["CODESIGN_UPDATE_CHANNEL"]
    feed = os.environ["CODESIGN_UPDATE_FEED"]
    public_key = os.environ["CODESIGN_SPARKLE_PUBLIC_KEY"]

    numeric = version.split("-", 1)[0]
    if not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+(?:-(?:dev|beta\.[1-9][0-9]*))?", version):
        raise ValueError("Use X.Y.Z, X.Y.Z-beta.N, or X.Y.Z-dev for the release version.")
    if not re.fullmatch(r"[1-9][0-9]{0,8}", build):
        raise ValueError("Use a positive numeric build number up to nine digits.")
    if channel not in {"stable", "beta"} or ("-beta." in version) != (channel == "beta"):
        raise ValueError("The version must agree with the stable or beta update channel.")
    url = urlsplit(feed)
    if (
        url.scheme != "https"
        or not url.hostname
        or url.username
        or url.password
        or url.query
        or url.fragment
        or not url.path.endswith(f"/{channel}/appcast.xml")
        or url.hostname == "storytold.github.io"
        or "/storytold/" in url.path.lower()
        or re.search(r"[\x00-\x20\x7f]", feed)
    ):
        raise ValueError("Use a fork-owned HTTPS appcast URL with the matching channel path.")
    if len(base64.b64decode(public_key, validate=True)) != 32:
        raise ValueError("The Sparkle public key must encode 32 bytes in base64.")

    info = plistlib.loads(Path(template).read_bytes())
    for key in ("DesignCraftVersion", "DesignCraftBuildCommit"):
        info.pop(key, None)
    for document in info.get("CFBundleDocumentTypes", []):
        if document.get("CFBundleTypeIconFile") == "DesignCraft":
            document["CFBundleTypeIconFile"] = "Collate"
        if document.get("LSHandlerRank") == "Owner":
            document["LSHandlerRank"] = "Alternate"
    for document in info.pop("UTExportedTypeDeclarations", []):
        if document.get("UTTypeIconFile") == "DesignCraft":
            document["UTTypeIconFile"] = "Collate"
        info.setdefault("UTImportedTypeDeclarations", []).append(document)

    info.update(
        CFBundleDisplayName="Collate",
        CFBundleName="Collate",
        CFBundleExecutable="Collate",
        CFBundleIdentifier="com.larshoie.codesign",
        CFBundleIconFile="Collate",
        CFBundleShortVersionString=numeric,
        CFBundleVersion=build,
        LSMinimumSystemVersion="12.0",
        NSLocalNetworkUsageDescription="Connect with invited collaborators on your local network.",
        CoDesignVersion=version,
        CoDesignBuildCommit=os.environ["CODESIGN_COMMIT"],
        CoDesignUpstreamCommit=os.environ["CODESIGN_UPSTREAM_SHA"],
        CoDesignUpdateChannel=channel,
        SUFeedURL=feed,
        SUPublicEDKey=public_key,
        SUEnableAutomaticChecks=not version.endswith("-dev"),
        SUAllowsAutomaticUpdates=False,
        SUEnableSystemProfiling=False,
        SUVerifyUpdateBeforeExtraction=True,
        SURequireSignedFeed=True,
        SUSignedFeedFailureExpirationInterval=0,
    )
    with Path(destination).open("wb") as stream:
        plistlib.dump(info, stream)


if __name__ == "__main__":
    try:
        write_plist(*sys.argv[1:])
    except (KeyError, ValueError) as error:
        sys.exit(str(error))
