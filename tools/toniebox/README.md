# toniebox

- `tonie.py <CONTENT dir> [outdir]`: lists every file on the SD card (UID, audio ID/upload date, chapters, SHA1 check). With an outdir it also extracts the audio as `.ogg`.
- `tonie.py path <UID>`: prints the SD path for a tag UID (`E0:04:03:50:1C:EC:9A:9D` → `CONTENT/9d9aec1c/500304e0`). It refuses UIDs that don't start `E0` (catches one read backwards) and notes when a tag isn't SLIX-L, since only teddiebox plays those.
- `WIRING.md`: FTDI-to-box serial wiring, download mode, and how to check each step.

## Custom SLIX-L cards (SD-card path, no TeddyCloud)

1. **Buy the right tags.** They must be NXP ICODE SLIX-L, privacy-mode capable, with a UID starting `E0 04 03`. Plain SLIX/SLIX2 tags only work on a patched box.
2. **Read the UID before the tag ever touches the box.** The box puts the tag into privacy mode, and after that ordinary readers can't see the UID. Use NXP TagInfo. NFC Tools shows the bytes reversed.
3. **Make a TAF** on macOS with `pip install TonieToolbox` (or opus2tonie.py) plus ffmpeg. TeddyBench does this too, but it's Windows-only.
4. **Put it on the card.** UID `E00403501D1FF15D` becomes `CONTENT/5DF11F1D/500304E0`. The folder is the last 4 bytes reversed, and the file name is always `500304E0`. Renaming a folder reassigns that audio to a different tag.
5. **Stop the cloud from deleting it.** Without this, the box moves unknown content to `TRASHCAN`. Turn on offline mode before powering on: hold both ears until the beep, then one ear until "offline mode activated". Power loss turns offline mode off. Blocking the box's internet in the router is a sturdier option.
6. Hold the tag a few mm above the box; it doesn't read reliably when the tag sits right on the box.
