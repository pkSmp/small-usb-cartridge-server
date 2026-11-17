"""Rename every spoken clip to the sentence it says, so the filename itself is
the transcript.  Also prints the Rust catalog entries for the lines the server
uses by name.

Usage: python tools/rename_voice.py [--apply]
"""

import os
import re
import sys

sys.path.insert(0, os.path.dirname(__file__))
from gen_voice_lines import LINES  # noqa: E402

ROOT_LINES = {
    "copying.wav": "Copying the cartridge now. This will only take a moment.",
    "complete.wav": "All done. Your sites are live.",
    "error.wav": "Sorry. That cartridge was refused. Nothing has changed, and the previous sites are still running.",
}

USED = [
    "boot", "n0", "n1", "n2", "n3", "n4", "n5", "n6", "n7", "n8", "n9",
    "unit-drive", "unit-drives", "unit-gb", "unit-gb-storage", "unit-gb-ram",
    "unit-out-of", "unit-ram-sites", "unit-mb-ram-sites", "unit-storage-sites",
    "unit-mb-storage-sites", "all-fit", "not-all-fit", "usb-found",
    "config-ok", "copy-done", "publish", "publish-ok", "error-device",
    "error-config", "error-storage", "error-empty", "error-copy",
]


def slug(text):
    s = text.lower()
    s = re.sub(r"[^a-z0-9]+", "_", s).strip("_")
    return s[:200]


def main():
    apply = "--apply" in sys.argv
    seen = {}
    renames = {}
    voice = os.path.join("usb", "voice")
    for name, text in LINES.items():
        old = os.path.join(voice, name + ".wav")
        new_name = slug(text) + ".wav"
        if new_name in seen:
            print("COLLISION", name, seen[new_name], new_name)
            continue
        seen[new_name] = name
        if os.path.isfile(old):
            renames[old] = os.path.join(voice, new_name)
    for name, text in ROOT_LINES.items():
        old = os.path.join("usb", name)
        if os.path.isfile(old):
            renames[old] = os.path.join("usb", slug(text) + ".wav")
    for old, new in sorted(renames.items()):
        print(f"{os.path.basename(old):28} -> {os.path.basename(new)}")
        if apply:
            os.replace(old, new)
    print(f"{len(renames)} file(s){' renamed' if apply else ' would be renamed'}")
    print("\n// Rust catalog for the lines the server uses by key:")
    for key in USED:
        print(f'    ("{key}", "{LINES[key]}"),')


if __name__ == "__main__":
    main()
