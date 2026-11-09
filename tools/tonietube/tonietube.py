#!/usr/bin/env python3
"""Keep Toniebox cards in sync with YouTube playlists through TeddyCloud.

cards.toml maps each card UID to a YouTube playlist or video URL. Every poll,
each playlist is listed anonymously with yt-dlp; when its set of videos has
changed since the last render, the audio is downloaded, packed into one TAF
(one chapter per video) with TonieToolbox, uploaded into TeddyCloud's library
and assigned to the card. Nothing here needs a Google account.

    tonietube.py [--config cards.toml] [--once] [--force]

The UI is a TeddyCloud plugin (`plugin/`): its page is kept in TeddyCloud's
library as `tonietube/ui.html`, reads this service's `tonietube/status.json`
beside it, and writes `tonietube/cards.json` with changes, which this service
picks up and applies to cards.toml. Everything goes through TeddyCloud, so
the page needs no port or certificate of its own.
"""
import argparse
import hashlib
import json
import logging
import os
import re
import shutil
import signal
import ssl
import subprocess
import sys
import tempfile
import threading
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


UID_FORMAT = re.compile(r"^[0-9A-Fa-f]{2}(:[0-9A-Fa-f]{2}){7}$")
PLAYLIST_HOSTS = {"youtube.com", "www.youtube.com", "m.youtube.com", "music.youtube.com", "youtu.be"}


def check_url(url):
    """A YouTube http(s) URL, or a ValueError. Anything else cannot be a
    playlist here, and the string ends up on a command line and in a config
    file."""
    parts = urllib.parse.urlsplit(url)
    if parts.scheme not in ("http", "https") or parts.hostname not in PLAYLIST_HOSTS:
        raise ValueError("not a YouTube URL")
    if any(ord(c) < 32 or c in "\"\\" for c in url):
        raise ValueError("URL has characters it cannot have")
    return url


def set_card(path, uid, url):
    """Add, change or (with url None) remove one card in cards.toml, keeping
    everything above the [cards] table as it is."""
    if not UID_FORMAT.match(uid):
        raise ValueError("a UID is eight hex bytes with colons")
    if url is not None:
        check_url(url)
    with open(path) as f:
        text = f.read()
    head, _, body = text.partition("[cards]\n")
    if not _:
        head, body = text.rstrip("\n") + "\n\n", ""
    cards = {}
    for line in body.splitlines():
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        key, _, value = line.partition("=")
        cards[ruid(key.strip().strip('"'))] = (key.strip().strip('"'), value.strip().strip('"'))
    rid = ruid(uid)
    if url is None:
        cards.pop(rid, None)
    else:
        cards[rid] = (cards.get(rid, (uid.upper(), None))[0], url)
    # json.dumps makes a valid TOML basic string: same quoting and escapes.
    lines = [f"{json.dumps(key)} = {json.dumps(value)}" for key, value in sorted(cards.values())]
    tmp = path + ".tmp"
    with open(tmp, "w") as f:
        f.write(head + "[cards]\n" + "\n".join(lines) + ("\n" if lines else ""))
    os.replace(tmp, path)


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
        ["yt-dlp", "-q", "--no-warnings", "--flat-playlist", "-J", "--", url],
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
            "--",
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

    def get_library(self, path):
        """A library file's bytes, or None when there is no such file."""
        try:
            return self._call(f"/library/{urllib.parse.quote(path)}", timeout=15)
        except urllib.error.HTTPError as e:
            if e.code == 404:
                return None
            raise

    def put_library(self, remote_dir, name, data):
        """Puts `data` into the library as `remote_dir/name`."""
        boundary = uuid.uuid4().hex
        body = (
            f"--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; "
            f"filename=\"{name}\"\r\nContent-Type: application/octet-stream\r\n\r\n"
        ).encode() + data + f"\r\n--{boundary}--\r\n".encode()
        q = urllib.parse.urlencode({"path": remote_dir, "special": "library"})
        self._call(
            f"/api/fileUpload?{q}",
            data=body,
            headers={"Content-Type": f"multipart/form-data; boundary={boundary}"},
        )

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


def sync_all(cfg, state, tc, force=False, only=None):
    for rid, url in cfg["cards"].items():
        if only and rid not in only:
            continue
        try:
            sync_card(cfg, state, tc, rid, url, force or (only is not None))
            if "error" in state.get(rid):
                state.set(rid, {k: v for k, v in state.get(rid).items() if k != "error"})
        except Exception as e:
            log.exception("%s: sync failed", rid)
            state.set(rid, {**state.get(rid), "url": url, "error": f"{e}"[:300]})


class Service:
    """One pass at a time, in the main thread; the page talks through files
    in TeddyCloud's library, which `watch_page` reads every few seconds."""

    POLL_SECONDS = 5

    def __init__(self, config_path):
        self.config_path = os.path.abspath(config_path)
        self.state_path = os.path.join(os.path.dirname(self.config_path), "state.json")
        self.wake = threading.Event()
        self.lock = threading.Lock()
        self.force = set()  # ruids to render again on the next pass
        self.busy = None  # the ruid being rendered right now
        self.applied_seq = None  # the page's last request we acted on

    def cfg(self):
        return load_config(self.config_path)

    def client(self, cfg=None):
        cfg = cfg or self.cfg()
        return TeddyCloud(cfg["teddycloud"], cfg.get("ca_file"))

    def run_once(self, force_all=False):
        cfg = self.cfg()
        state = State(self.state_path)
        tc = self.client(cfg)
        with self.lock:
            only = None if (force_all or not self.force) else set(self.force)
            self.force.clear()
        if only:
            sync_all(cfg, state, tc, only=only)
        sync_all(cfg, state, tc, force=force_all)
        self.publish(cfg, tc)
        return cfg

    def status(self, cfg=None):
        """What the page shows about each mapped card, by ruid."""
        cfg = cfg or self.cfg()
        state = State(self.state_path)
        cards = {}
        for rid, url in cfg["cards"].items():
            known = state.get(rid)
            cards[rid] = {
                "uid": ":".join(reversed([rid[i : i + 2] for i in range(0, 16, 2)])).upper(),
                "url": url,
                "title": known.get("title"),
                "videos": known.get("videos"),
                "bytes": known.get("bytes"),
                "rendered": known.get("rendered"),
                "error": known.get("error"),
                "busy": self.busy == rid,
            }
        return {"cards": cards, "written": time.strftime("%Y-%m-%dT%H:%M:%S")}

    def publish(self, cfg=None, tc=None):
        """Puts the page and the status beside it into TeddyCloud's library."""
        cfg = cfg or self.cfg()
        tc = tc or self.client(cfg)
        try:
            tc.mkdir(cfg["library_dir"])
            here = os.path.dirname(os.path.abspath(__file__))
            with open(os.path.join(here, "ui.html"), "rb") as f:
                tc.put_library(cfg["library_dir"], "ui.html", f.read())
            tc.put_library(cfg["library_dir"], "status.json", json.dumps(self.status(cfg)).encode())
        except Exception:
            log.exception("could not publish the page's files to TeddyCloud")

    def apply_requests(self):
        """Reads the page's `cards.json` and acts on it once per sequence number."""
        cfg = self.cfg()
        tc = self.client(cfg)
        raw = tc.get_library(f"{cfg['library_dir']}/cards.json")
        if raw is None:
            return
        req = json.loads(raw)
        seq = req.get("seq")
        if seq is None or seq == self.applied_seq:
            return
        # Each entry is checked on its own; a bad one is skipped and logged,
        # the rest still apply, and the sequence is consumed either way.
        wanted = {}
        for uid, url in (req.get("cards") or {}).items():
            if not url:
                continue
            try:
                if not UID_FORMAT.match(str(uid)):
                    raise ValueError("a UID is eight hex bytes with colons")
                wanted[ruid(uid)] = (uid, check_url(str(url)))
            except ValueError as e:
                log.warning("the page's request names %r with %r — ignored: %s", uid, url, e)
        changed = False
        for rid, (uid, url) in wanted.items():
            if cfg["cards"].get(rid) != url:
                set_card(self.config_path, uid, url)
                changed = True
                log.info("%s: the page assigned %s", rid, url)
        for rid in set(cfg["cards"]) - set(wanted):
            uid = ":".join(reversed([rid[i : i + 2] for i in range(0, 16, 2)])).upper()
            set_card(self.config_path, uid, None)
            changed = True
            log.info("%s: the page removed the mapping", rid)
        again = [r for r in (req.get("render") or []) if re.fullmatch(r"[0-9a-f]{16}", r)]
        if again:
            with self.lock:
                self.force.update(again)
            log.info("the page asked to render %s again", ", ".join(again))
        self.applied_seq = seq
        if changed or again:
            self.wake.set()
        else:
            self.publish(cfg, tc)

    def watch_page(self):
        """Runs in its own thread: the page's requests, every few seconds."""
        while True:
            try:
                self.apply_requests()
            except Exception:
                log.exception("could not read the page's request")
            time.sleep(self.POLL_SECONDS)


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

    service = Service(args.config)
    if not args.once:
        threading.Thread(target=service.watch_page, daemon=True, name="page").start()
    while True:
        # The config is re-read each pass, so edits need no restart.
        cfg = service.run_once(force_all=args.force)
        if args.once:
            break
        args.force = False
        # Woken early by the page's requests; otherwise the regular interval.
        service.wake.wait(cfg["interval_minutes"] * 60)
        service.wake.clear()


if __name__ == "__main__":
    main()
