================================================================
 CARTRIDGE USB DRIVE
================================================================
 A cartridge for the small USB cartridge server. Plug it in and
 the sites on it go live on your network. Nothing to run here.

 WHAT IS ON THIS STICK
   config.json        marks this as a cartridge (one JSON object)
   boot.png           the picture the TV shows at boot
   README.txt         this file (ignored by the server)
   3000/              the main site   -> http://<server>:3000/
   8080/              runtime page    -> http://<server>:8080/
   9090/              JSON endpoint   -> http://<server>:9090/

 WHAT HAPPENS WHEN YOU PLUG IT IN
   1  the stick is mounted read-only, never written to
   2  config.json must be a JSON object; no file means the stick is
      ignored, broken JSON means it is refused
   3  every file is hashed (SHA-256). A file that already matches
      the running release is hard-linked, not copied, so replaying
      the same stick writes zero bytes
   4  a new release is built and the live symlink is flipped; as much
      of it as fits safely is copied into RAM, the rest is linked to
      disk so every site still works
   5  boot art is rebuilt only if boot.png changed, then verified
      against the framebuffer

 FOLDERS ARE PORTS
   3000/index.html   ->  http://<server>:3000/
   8080/             ->  http://<server>:8080/
   index.html        ->  http://<server>/        (port 80)
   A top-level folder is a port only when its name is all digits
   and the number is 80 or 1024-65535. Any other folder is copied
   but has no URL of its own; put it inside a port folder instead.
   Everything else at the root belongs to the port 80 site, so a
   stray file at the root becomes a URL.

 CONFIG.JSON
   {}
   That is all it needs. Nothing inside the object is read yet.
   Names never published: config.json, boot.png, README.txt,
   readme.txt, System Volume Information, $RECYCLE.BIN, .Trashes.

 NO SOUND
   The server has no speaker, no voice and no text-to-speech, and
   reads no keys out of config.json. There is no sounds/ folder on
   this stick, because nothing would play it.

 RAM AND STORAGE (AUTOMATIC)
   Every release stays on disk, so a reboot brings the sites back.
   On publish the server measures free memory and copies as much of
   the release as fits safely into RAM: never more than half of RAM,
   with a quarter of RAM always left free. Files that do not fit stay
   on disk and are linked, so all sites keep working.

 BOOT ART
   boot.png is converted to coloured half blocks: two source rows
   per text cell, so 50x63 becomes 50 columns by 32 rows. A bigger
   picture is shrunk to the largest size that fits the screen. 1, 2,
   4 and 8 bit PNGs are read. Leave boot.png out to keep the
   current picture.

 THE SCREEN
   The TV shows the art, the sites and their addresses, and:
       login   Ctrl+Alt+F2
   Ctrl+Alt+F2 gives a text login on the machine.
   Ctrl+Alt+F1 comes back to the art.

 CHECKING ON IT (on the server)
   cartridge status            stick, staging, sites, URLs
   cartridge sites             live sites and ports as a Markdown table
   tail -f /var/log/cartridge/import.log

 SAFE HANDLING
   The server mounts this stick read-only and unmounts it when the
   import ends. If you edit files on a PC, eject properly before
   unplugging.

 TROUBLESHOOTING
   nothing changes
       the importer logged why: no config.json, broken JSON, or a
       file it refused
   page shows 404
       the folder name must be the port you are visiting, and the
       file must be inside it
   art looks wrong
       compare it with cartridge art verify

 TURNING A PLAIN STICK INTO A CARTRIDGE
   1  format FAT32 or ext4
   2  copy a config.json holding {}, one folder per port, and an
      optional boot.png
   3  plug it in; the import log says what happened

 SIZES ON THIS STICK
   The three sites and boot.png are a few kilobytes. There is no
   audio on the stick.
================================================================
