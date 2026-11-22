A ready-to-use demo cartridge that matches the current server.

Contents, at the root of the zip so it extracts straight onto a stick:
- `3000/`, `8080/`, `9090/` - three static sites
- `boot.png` - the 50x63 boot picture
- `config.json` - `{}`; the server needs a JSON object and reads nothing inside it
- `README.txt` - the stick's own notes

There is no `sounds/` folder and no audio: the server has no speaker, no voice
and no text-to-speech, and reads no keys out of `config.json`.

Extract the zip to the root of a FAT32 or ext4 stick - the files land at the top
level, not inside a wrapper folder - and plug it in. The three sites appear on
ports 3000, 8080 and 9090.

Build the asset from this directory:

    cd demo && zip -r -X ../usb-demo-cartridge.zip .

SHA-256: `efcf823facd6293c72686aba7630bb39803881a48c1bb313613775d50d2241aa`
(9,509 bytes, 8 entries, no wrapper folder)
