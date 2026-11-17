"""Generate the cartridge voice catalogue with edge-tts, as 16-bit stereo WAV.

The server never runs a TTS engine: these files are the whole voice.  Each file
is one short line, so the machine can queue them and play them back to back.

Usage:  python tools/gen_voice_lines.py [name ...]
        (no names = every line)
"""

import os
import subprocess
import sys

VOICE = "en-GB-SoniaNeural"
RATE = "-3%"
OUT = os.path.join("usb", "voice")

LINES = {
    # digits, spoken one at a time so any number can be built
    "n0": "0", "n1": "1", "n2": "2", "n3": "3", "n4": "4",
    "n5": "5", "n6": "6", "n7": "7", "n8": "8", "n9": "9",

    # units the digits hang off
    "unit-drive": "drive",
    "unit-drives": "drives",
    "unit-gb": "gigabytes",
    "unit-gb-storage": "gigabytes of storage",
    "unit-gb-ram": "gigabytes of RAM",
    "unit-mb": "megabytes",
    "unit-files": "files",
    "unit-seconds": "seconds",

    # boot and power
    "boot": "The computer started correctly.",
    "ready": "The cartridge server is ready.",
    "power-on": "Powering up.",
    "power-off": "Shutting down. See you next time.",
    "shutdown": "Shutting down.",
    "reboot": "Rebooting.",
    "goodbye": "Goodbye.",
    "hello": "Hello.",
    "standby": "Standby.",

    # generic status words
    "notice": "Attention.",
    "warning": "Warning.",
    "success": "Success.",
    "failure": "Failure.",
    "checking": "Checking.",
    "loading": "Loading.",
    "saving": "Saving.",
    "starting": "Starting.",
    "finished": "Finished.",
    "wait": "Please wait.",
    "almost-done": "Almost done.",
    "online": "Online.",
    "offline": "Offline.",
    "network": "The network is connected.",
    "no-network": "The network is not connected.",
    "disk-ok": "There is plenty of space.",
    "disk-full": "The disk is full.",
    "memory-ok": "Memory is fine.",
    "memory-low": "Memory is running low.",
    "temperature": "The machine is warm.",
    "fan": "The fan is running.",
    "update": "Updating.",
    "updated": "Updated.",

    # cartridge in and out
    "no-usb": "No cartridge is plugged in. The last sites are still running.",
    "usb-found": "Cartridge detected. Reading it now.",
    "usb-removed": "The cartridge was removed. The current sites are still running.",
    "usb-safe": "It is safe to remove the cartridge.",

    # checking and copying
    "config-check": "Checking the cartridge settings.",
    "config-ok": "The settings look good.",
    "config-bad": "That settings file cannot be read.",
    "missing-config": "This cartridge has no settings file.",
    "files-check": "Checking the files.",
    "sounds-ok": "The voice lines are ready.",
    "sound-missing": "A voice line is missing. I will use the built-in sound.",
    "copy-start": "Starting the copy.",
    "copy-half": "Halfway through the copy.",
    "copy-done": "The files are copied.",
    "verify": "Checking every copied file.",
    "verify-ok": "Every file checks out.",
    "linking": "Unchanged files were linked, so nothing was copied twice.",
    "new-files": "New files written.",
    "no-change": "Nothing changed.",
    "publish": "Switching the sites over.",
    "publish-ok": "The sites are live.",
    "done": "Import complete.",
    "prune": "Old releases cleaned up.",

    # boot art and the server
    "art": "Drawing the boot screen.",
    "art-ok": "The boot screen is ready.",
    "server-start": "Starting the web server.",
    "server-up": "The web server is running.",
    "ports": "The sites are listening.",
    "checking-ram": "Checking memory.",
    "checking-storage": "Checking storage.",

    # what fits where, and how much is in use
    "all-fit": "All sites fit in RAM.",
    "not-all-fit": "Not all sites fit in RAM. The rest stay on storage.",
    "sites-stored": "Every site is stored on disk for the next boot.",
    "unit-ram-sites": "gigabytes of RAM used for sites",
    "unit-mb-ram-sites": "megabytes of RAM used for sites",
    "unit-storage-sites": "gigabytes of storage used",
    "unit-mb-storage-sites": "megabytes of storage used",
    "unit-out-of": "out of",

    # verdicts and refusals
    "cartridge-ok": "The cartridge is good.",
    "cartridge-bad": "The cartridge is not usable.",
    "refused": "Cartridge refused.",
    "error-config": "That cartridge has a broken settings file.",
    "error-storage": "That cartridge does not fit in storage.",
    "error-ram": "That cartridge does not fit in memory.",
    "error-empty": "That cartridge has no sites on it.",
    "error-copy": "The copy failed. The previous sites are still running.",
    "error-device": "That is not a cartridge. I left it alone.",
    "error-art": "The boot picture could not be drawn.",
    "error-sound": "The sound card did not answer.",
}

# One ffmpeg pass: trim the silence TTS leaves, even out the level, and add a
# short lead-in so the first syllable is never clipped by the sound card.
AF = (
    "silenceremove=start_periods=1:start_silence=0.05:start_threshold=-45dB,"
    "areverse,"
    "silenceremove=start_periods=1:start_silence=0.12:start_threshold=-45dB,"
    "areverse,"
    "loudnorm=I=-16:TP=-1.5:LRA=11,"
    "adelay=40|40"
)


def make(name: str, text: str) -> int:
    os.makedirs(OUT, exist_ok=True)
    mp3 = os.path.join("_tts", name + ".mp3")
    wav = os.path.join(OUT, name + ".wav")
    os.makedirs("_tts", exist_ok=True)
    subprocess.run(
        [sys.executable, "-m", "edge_tts", "--voice", VOICE, "--rate=" + RATE,
         "--text", text, "--write-media", mp3],
        check=True, stdout=subprocess.DEVNULL,
    )
    subprocess.run(
        ["ffmpeg", "-y", "-loglevel", "error", "-i", mp3, "-ar", "44100",
         "-ac", "2", "-sample_fmt", "s16", "-af", AF, wav],
        check=True,
    )
    return os.path.getsize(wav)


def main() -> int:
    force = "--force" in sys.argv
    wanted = [a for a in sys.argv[1:] if a != "--force"] or list(LINES)
    total = 0
    for i, name in enumerate(wanted, 1):
        if name not in LINES:
            print("skip unknown", name, file=sys.stderr)
            continue
        wav = os.path.join(OUT, name + ".wav")
        if not force and os.path.isfile(wav) and os.path.getsize(wav) > 44:
            total += os.path.getsize(wav)
            continue
        size = make(name, LINES[name])
        total += size
        print(f"[{i:>3}/{len(wanted)}] {name:18} {size:>8} B  {LINES[name]}")
    print(f"{len(wanted)} line(s), {total} bytes")
    return 0


if __name__ == "__main__":
    sys.exit(main())
