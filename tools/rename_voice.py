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

USED = list(LINES)


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
