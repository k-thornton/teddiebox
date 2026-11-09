#!/usr/bin/env python3
"""Keep Toniebox cards in sync with YouTube playlists through TeddyCloud.

cards.toml maps each card UID to a YouTube playlist or video URL. Every poll,
each playlist is listed anonymously with yt-dlp; when its set of videos has
changed since the last render, the audio is downloaded, packed into one TAF
(one chapter per video) with TonieToolbox, uploaded into TeddyCloud's library
and assigned to the card. Nothing here needs a Google account.

    tonietube.py [--config cards.toml] [--once] [--force]
"""
import argparse
import hashlib
import json
import logging
import os
import shutil
import signal
import ssl
import subprocess
import sys
import tempfile
import time
import tomllib
import urllib.error
import urllib.parse
import urllib.request
import uuid

log = logging.getLogger("tonietube")


# --- config / state -------------------------------------------------------


def load_config(path):
    with open(path, "rb") as f:
        cfg = tomllib.load(f)
    cfg.setdefault("teddycloud", "https://localhost:8443")
    cfg.setdefault("interval_minutes", 30)
    cfg.setdefault("library_dir", "tonietube")
    cfg.setdefault("bitrate", 96)
    cfg.setdefault("cards", {})
    if cfg.get("ca_file"):  # relative to the config file
        cfg["ca_file"] = os.path.join(os.path.dirname(os.path.abspath(path)), cfg["ca_file"])
    cfg["cards"] = {ruid(uid): url for uid, url in cfg["cards"].items()}
    return cfg


def ruid(uid):
    """TeddyCloud's content id for a tag: UID bytes reversed, lowercase hex.
    E0:04:01:53:2A:8B:77:38 -> 38778b2a530104e0"""
    clean = uid.replace(":", "").replace(" ", "").lower()
    if len(clean) != 16:
        raise ValueError(f"UID {uid!r} is not 8 bytes")
    if clean.endswith("04e0"):  # already reversed
        return clean
    if not clean.startswith("e004"):
        raise ValueError(f"UID {uid!r} does not start with E0 04 (read backwards?)")
    return "".join(reversed([clean[i : i + 2] for i in range(0, 16, 2)]))


class State:
    def __init__(self, path):
        self.path = path
        try:
            with open(path) as f:
                self.data = json.load(f)
        except FileNotFoundError:
            self.data = {}

    def get(self, key):
        return self.data.get(key, {})

    def set(self, key, value):
        self.data[key] = value
        tmp = self.path + ".tmp"
        with open(tmp, "w") as f:
            json.dump(self.data, f, indent=2, sort_keys=True)
        os.replace(tmp, self.path)


# --- youtube --------------------------------------------------------------


def run(cmd, **kw):
    log.debug("$ %s", " ".join(cmd))
    return subprocess.run(cmd, check=True, text=True, **kw)


def list_videos(url):
    """(title, [video ids]) for a playlist or single-video URL, no login."""
    out = run(
        ["yt-dlp", "-q", "--no-warnings", "--flat-playlist", "-J", url],
        stdout=subprocess.PIPE,
    ).stdout
    info = json.loads(out)
    if info.get("_type") == "playlist":
        ids = [e["id"] for e in info["entries"] if e and e.get("id")]
    else:
        ids = [info["id"]]
    return info.get("title") or url, ids


def download_audio(url, dest):
    """Download every video's audio as Opus into dest, numbered in playlist order."""
    run(
        [
            "yt-dlp", "-q", "--no-warnings", "--ignore-errors",
            "-f", "bestaudio", "-x", "--audio-format", "opus",
            "-o", os.path.join(dest, "%(playlist_index|1)03d [%(id)s].%(ext)s"),
            url,
        ]
    )
    files = sorted(f for f in os.listdir(dest) if f.endswith(".opus"))
    if not files:
        raise RuntimeError("no audio downloaded")
    return files


# --- taf ------------------------------------------------------------------


def build_taf(audio_dir, out_path, bitrate):
    run(["tonietoolbox", "-q", "-fc", "-b", str(bitrate), audio_dir, out_path])
    if not os.path.isfile(out_path):
        raise RuntimeError("tonietoolbox produced no file")


# --- teddycloud -----------------------------------------------------------


class TeddyCloud:
    def __init__(self, base, ca_file=None):
        self.base = base.rstrip("/")
        # TeddyCloud's cert is CN "TeddyCloud Server" with no SANs (the box
        # insists on that), so the hostname can't match. Pin its CA instead.
        ctx = ssl.create_default_context(cafile=ca_file)
        ctx.check_hostname = False
        if not ca_file:
            log.warning("no ca_file configured: TeddyCloud's certificate is not verified")
            ctx.verify_mode = ssl.CERT_NONE
        self.opener = urllib.request.build_opener(urllib.request.HTTPSHandler(context=ctx))

    def _call(self, path, data=None, headers=None, timeout=60):
        req = urllib.request.Request(self.base + path, data=data, headers=headers or {})
        with self.opener.open(req, timeout=timeout) as r:
            return r.read()

    def get_json(self, path):
        return json.loads(self._call(path))

    def mkdir(self, name):
        try:
            self._call("/api/dirCreate?special=library", data=name.encode())
        except urllib.error.HTTPError as e:  # 500 when it already exists
            q = urllib.parse.urlencode({"path": name, "special": "library"})
            self.get_json(f"/api/fileIndexV2?{q}")  # raises if it really is missing
            log.debug("library dir %s exists (%s)", name, e)

    def upload(self, local_path, remote_dir):
        boundary = uuid.uuid4().hex
        name = os.path.basename(local_path)
        with open(local_path, "rb") as f:
            payload = f.read()
        body = (
            f"--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; "
            f"filename=\"{name}\"\r\nContent-Type: application/octet-stream\r\n\r\n"
        ).encode() + payload + f"\r\n--{boundary}--\r\n".encode()
        q = urllib.parse.urlencode({"path": remote_dir, "special": "library"})
        self._call(
            f"/api/fileUpload?{q}",
            data=body,
            headers={"Content-Type": f"multipart/form-data; boundary={boundary}"},
            timeout=600,
        )
        files = self.get_json(f"/api/fileIndexV2?{q}")["files"]
        entry = next((f for f in files if f["name"] == name), None)
        if not entry or not entry.get("tafHeader", {}).get("valid"):
            raise RuntimeError(f"upload of {name} not valid on server: {entry}")
        return entry["tafHeader"].get("trackSeconds")

    def assign(self, rid, lib_path):
        form = urllib.parse.urlencode(
            {"source": lib_path, "nocloud": "true", "live": "false", "tonie_model": ""}
        ).encode()
        out = self._call(f"/content/json/set/{rid}", data=form)
        if out.strip() != b"OK":
            raise RuntimeError(f"assign {rid}: {out!r}")


# --- sync -----------------------------------------------------------------


def sync_card(cfg, state, tc, rid, url, force):
    title, ids = list_videos(url)
    fingerprint = hashlib.sha1("\n".join(ids).encode()).hexdigest()[:16]
    prev = state.get(rid)
    if not force and prev.get("fingerprint") == fingerprint and prev.get("lib_path"):
        log.debug("%s: %s unchanged (%d videos)", rid, title, len(ids))
        return
    log.info("%s: rendering %r (%d videos)", rid, title, len(ids))

    taf_name = f"{rid}.taf"
    lib_path = f"lib://{cfg['library_dir']}/{taf_name}"
    with tempfile.TemporaryDirectory(prefix="tonietube-") as tmp:
        audio_dir = os.path.join(tmp, "audio")
        os.mkdir(audio_dir)
        files = download_audio(url, audio_dir)
        if len(files) < len(ids):
            log.warning("%s: %d of %d videos downloaded", rid, len(files), len(ids))
        taf = os.path.join(tmp, taf_name)
        build_taf(audio_dir, taf, cfg["bitrate"])
        size = os.path.getsize(taf)
        tc.mkdir(cfg["library_dir"])
        tracks = tc.upload(taf, cfg["library_dir"])
    tc.assign(rid, lib_path)
    state.set(
        rid,
        {
            "fingerprint": fingerprint,
            "title": title,
            "url": url,
            "videos": len(files),
            "lib_path": lib_path,
            "bytes": size,
            "track_seconds": tracks,
            "rendered": time.strftime("%Y-%m-%dT%H:%M:%S"),
        },
    )
    log.info("%s: %r ready, %d chapters, %.1f MB", rid, title, len(files), size / 1e6)


def sync_all(cfg, state, tc, force=False):
    for rid, url in cfg["cards"].items():
        try:
            sync_card(cfg, state, tc, rid, url, force)
        except Exception:
            log.exception("%s: sync failed", rid)


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--config", default=os.environ.get("TONIETUBE_CONFIG", "cards.toml"))
    ap.add_argument("--once", action="store_true", help="one pass, then exit")
    ap.add_argument("--force", action="store_true", help="re-render even if unchanged")
    ap.add_argument("-v", "--verbose", action="store_true")
    args = ap.parse_args()
    # As PID 1 in a container SIGTERM is ignored by default; `podman stop`
    # would wait out its timeout and SIGKILL us.
    signal.signal(signal.SIGTERM, lambda *_: sys.exit(0))
    logging.basicConfig(
        level=logging.DEBUG if args.verbose else logging.INFO,
        format="%(asctime)s %(levelname)s %(message)s",
        stream=sys.stdout,
    )
    for tool in ("yt-dlp", "tonietoolbox", "ffmpeg", "opusenc"):
        if not shutil.which(tool):
            sys.exit(f"{tool} not found on PATH")

    while True:
        cfg = load_config(args.config)  # re-read each pass so edits need no restart
        state = State(os.path.join(os.path.dirname(os.path.abspath(args.config)), "state.json"))
        tc = TeddyCloud(cfg["teddycloud"], cfg.get("ca_file"))
        sync_all(cfg, state, tc, force=args.force)
        if args.once:
            break
        args.force = False
        time.sleep(cfg["interval_minutes"] * 60)


if __name__ == "__main__":
    main()
