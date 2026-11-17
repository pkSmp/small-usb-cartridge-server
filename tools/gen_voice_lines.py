"""Generate the cartridge voice catalogue locally with XTTS v2.

Only the lines the server can actually queue are kept.  Synthesis runs entirely
on the authoring machine; the server never runs TTS and never keeps a clip.
The finished WAVs are written under ``usb/sounds/`` and are copied to the USB
stick.  Output is 16-bit stereo 44.1 kHz WAV, trimmed and levelled for ALSA.

Requirements (use a Python 3.12 virtual environment):
  pip install coqui-tts torch torchaudio
  ffmpeg on PATH

The reference voice is a public-domain LibriVox/LibriTTS-R male recording at
``_tts/ref/en-man.wav`` by default; override it with XTTS_REFERENCE.
Generate into the USB staging tree with ``--usb-dir PATH``.
"""

import argparse
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile

# Every key the server can queue.  Unused catalogue entries are deliberately
# absent, so old clips are removed instead of being regenerated.
LINES = {
    # boot report and units
    "BOOT": "Boot OK.",
    "UNIT-DRIVE": "drive",
    "UNIT-DRIVES": "drives",
    "UNIT-GB-STORAGE": "gigabytes storage",
    "UNIT-GB-RAM": "gigabytes RAM",
    "ALL-FIT": "All sites in RAM.",
    "NOT-ALL-FIT": "Some sites on disk.",
    "UNIT-RAM-SITES": "gigabytes of sites in RAM",
    "UNIT-RAM-SITE": "gigabyte of sites in RAM",
    "UNIT-MB-RAM-SITES": "megabytes of sites in RAM",
    "UNIT-MB-RAM-SITE": "megabyte of sites in RAM",
    "UNIT-OUT-OF": "out of",
    "UNIT-GB": "gigabytes",
    "UNIT-STORAGE-SITES": "gigabytes of sites on disk",
    "UNIT-STORAGE-SITE": "gigabyte of sites on disk",
    "UNIT-MB-STORAGE-SITES": "megabytes of sites on disk",
    "UNIT-MB-STORAGE-SITE": "megabyte of sites on disk",

    # import milestones and errors
    "CONFIG-OK": "Settings OK.",
    "COPYING": "Copy in progress.",
    "COPY-DONE": "Copy done.",
    "PUBLISH": "Publishing.",
    "PUBLISH-OK": "Sites live.",
    "COMPLETE": "Done.",
    "ERROR": "Cartridge refused.",
    "ERROR-DEVICE": "Not a cartridge.",
    "ERROR-CONFIG": "Bad settings.",
    "ERROR-COPY": "Copy failed. Old sites live.",
    "ERROR-EMPTY": "No sites.",
    "ERROR-STORAGE": "Storage full.",
}

DIGIT_TEXT = ("zero", "one", "two", "three", "four",
              "five", "six", "seven", "eight", "nine")
for _n, _word in enumerate(DIGIT_TEXT):
    LINES["DIGIT-%d" % _n] = _word


def slug(text):
    return re.sub(r"[^a-z0-9]+", "_", text.lower()).strip("_")


def filename(key, text):
    if key.startswith("DIGIT-"):
        return key[-1] + ".wav"
    return slug(text) + ".wav"


def reference_wav():
    here = os.path.dirname(os.path.abspath(__file__))
    default = os.path.abspath(os.path.join(here, "..", "..", "_tts", "ref", "en-man.wav"))
    return os.environ.get("XTTS_REFERENCE", default)


def make_engine():
    os.environ.setdefault("COQUI_TOS_AGREED", "1")
    try:
        from TTS.api import TTS
    except Exception as exc:
        raise SystemExit("XTTS is not installed in this Python: %s" % exc)
    if not os.path.isfile(reference_wav()):
        raise SystemExit("reference voice is missing: %s" % reference_wav())
    return TTS("tts_models/multilingual/multi-dataset/xtts_v2").to("cpu")


# Remove trailing silence, even the level, and add a short lead-in so the sound
# card never clips the first syllable.  The voice itself is left untouched.
AF = (
    "silenceremove=start_periods=1:start_silence=0.05:start_threshold=-48dB,"
    "areverse,"
    "silenceremove=start_periods=1:start_silence=0.12:start_threshold=-48dB,"
    "areverse,"
    "loudnorm=I=-16:TP=-1.5:LRA=11,"
    "adelay=40|40"
)


def make_engine_file(tts, key, dest):
    with tempfile.TemporaryDirectory(prefix="cartridge-tts-") as tmp:
        raw = os.path.join(tmp, "raw.wav")
        final = os.path.join(tmp, "final.wav")
        tts.tts_to_file(
            text=LINES[key],
            speaker_wav=[reference_wav()],
            language="en",
            file_path=raw,
            split_sentences=False,
        )
        subprocess.run(
            ["ffmpeg", "-y", "-loglevel", "error", "-i", raw,
             "-ar", "44100", "-ac", "2", "-sample_fmt", "s16",
             "-af", AF, final],
            check=True,
        )
        shutil.copyfile(final, dest)
        return os.path.getsize(dest)


def prune(sounds, expected):
    removed = 0
    for name in os.listdir(sounds):
        if not name.lower().endswith(".wav"):
            continue
        if name not in expected:
            os.remove(os.path.join(sounds, name))
            removed += 1
    return removed


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--usb-dir", default="usb")
    parser.add_argument("--force", action="store_true")
    parser.add_argument("--prune-only", action="store_true")
    args = parser.parse_args()

    usb = os.path.abspath(args.usb_dir)
    sounds = os.path.join(usb, "sounds")
    os.makedirs(sounds, exist_ok=True)

    expected = {filename(key, text): key for key, text in LINES.items()}
    removed = prune(sounds, expected)

    # Also clean the old separate voice directory if this tree still has one.
    old_voice = os.path.join(usb, "voice")
    if os.path.isdir(old_voice):
        for name in os.listdir(old_voice):
            if name.lower().endswith(".wav"):
                os.remove(os.path.join(old_voice, name))
        try:
            os.rmdir(old_voice)
        except OSError:
            pass

    if args.prune_only:
        print("removed %d unused clip(s)" % removed)
        return 0

    tts = make_engine()
    made = 0
    total = 0
    for key in LINES:
        name = filename(key, LINES[key])
        dest = os.path.join(sounds, name)
        if not args.force and os.path.isfile(dest) and os.path.getsize(dest) > 44:
            total += os.path.getsize(dest)
            continue
        size = make_engine_file(tts, key, dest)
        total += size
        made += 1
        print("%-24s %7d B  %s" % (key, size, LINES[key]))

    config = {"sounds": {key: filename(key, text) for key, text in LINES.items()}}
    with open(os.path.join(usb, "config.json"), "w", encoding="utf-8") as out:
        json.dump(config, out, indent=2)
        out.write("\n")

    print("removed %d unused, generated %d, total %d bytes, %d lines" %
          (removed, made, total, len(LINES)))
    return 0


if __name__ == "__main__":
    sys.exit(main())
