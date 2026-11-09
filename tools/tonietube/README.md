# tonietube

YouTube playlists on Toniebox cards, through TeddyCloud. No Google account
involved: yt-dlp lists and downloads anonymously, so playlists must be
**public or unlisted** (not private). Single videos work too.

`cards.toml` maps a card UID to a URL (`cards.example.toml` shows the
format; the real file lives beside the service's state and is edited by the
web page, so it is not in the repository):

```toml
[cards]
"E0:04:01:53:2A:8B:77:38" = "https://youtube.com/playlist?list=PLQBVGreLWMIQ"
```

The web page at `http://<server>:8080/` is the normal way to assign cards. It lists every card TeddyCloud has seen,
takes a playlist or video URL for each, shows the title, chapter count and
last render, and has a "render again" button. It edits the same `cards.toml`
and wakes the service, so an assignment renders within a couple of minutes.
`web_port = 0` turns the page off.

`plugin/` is a TeddyCloud plugin that puts the page in TeddyCloud's Tonies
menu as "Playlists"; it opens the page in a new tab. Install it by copying
the folder to `/var/lib/teddycloud/plugins/tonietube/` on the server.

Each pass (every `interval_minutes`, and the file is re-read each time), every
card's playlist is listed. When its set of videos differs from the last render,
the audio is downloaded, packed into one TAF with one chapter per video
(TonieToolbox, 96 kbps Opus), uploaded to TeddyCloud's library as
`lib://tonietube/<ruid>.taf` and assigned to the card with `nocloud`. The box
picks it up on the next placement. `state.json` beside the config records what
was rendered.

A render of a 7-track playlist takes about 90 s, nearly all of it yt-dlp
pausing between downloads as YouTube asks. A brand-new card therefore plays
nothing on its first tap; add the card to `cards.toml` first and give it a
couple of minutes.

## Running locally

```sh
python3 -m venv .venv && .venv/bin/pip install -r requirements.txt
brew install ffmpeg opus-tools deno       # deno: yt-dlp needs a JS runtime
PATH=$PWD/.venv/bin:$PATH python3 tonietube.py --once -v   # one pass
```

`--force` re-renders every card even if unchanged.

## On the server (NixOS, root podman container like teddycloud)

`deploy/nixos/tonietube.nix` declares the container; it is imported from
`/etc/nixos/configuration.nix` and starts at boot as `podman-tonietube`.
Config and state live in `/var/lib/tonietube` on the server (`cards.toml`,
`teddycloud-ca.pem`, `state.json`), owned by kyle so cards can be added
without sudo. The file is re-read every pass, so edits need no restart.

Ship a code change:

```sh
rsync -a --delete --exclude .venv --exclude state.json tools/tonietube/ kyle@192.168.50.188:~/tonietube/src/
ssh -t kyle@192.168.50.188 'cd ~/tonietube/src && sudo podman build -q -t localhost/tonietube:latest -f Containerfile . && sudo systemctl restart podman-tonietube'
```

Logs: `journalctl -u podman-tonietube -f`. The container upgrades yt-dlp on
every start (YouTube breaks it a few times a year), so a restart is the first
fix when downloads fail. A fresh `state.json` makes the service re-render every
card once, which gives each a new audio id and so one more download on the box;
copy the old `state.json` along when moving the service.

## TeddyCloud API used

- `GET  /api/getTagIndex`, `GET /content/json/get/<ruid>` (note: no `/api` prefix on `content/json`)
- `POST /api/dirCreate?special=library` (body: path; 500 if it exists)
- `POST /api/fileUpload?path=<dir>&special=library` (multipart `file`)
- `GET  /api/fileIndexV2?path=<dir>&special=library` (reports `tafHeader.valid`)
- `POST /content/json/set/<ruid>` (form: `source`, `nocloud`, `live`, `tonie_model`)

`ruid` is the UID with its bytes reversed, lowercase: `E0:04:01:53:2A:8B:77:38` → `38778b2a530104e0`.
The web UI's certificate is signed by TeddyCloud's own CA (`teddycloud-ca.pem`),
CN `TeddyCloud Server` with no SANs, so the client pins the CA and skips the
hostname check.
