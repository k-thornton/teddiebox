"""Read Toniebox SD content: tonie.py <CONTENT dir> [out dir to extract .ogg]
UID to SD path:            tonie.py path <UID>   (e.g. E0:04:03:50:1D:1F:F1:5D)"""
import hashlib, pathlib, sys
from datetime import datetime, timezone


def varint(b, i):
    n = s = 0
    while True:
        n |= (b[i] & 0x7F) << s; s += 7; i += 1
        if b[i - 1] < 0x80: return n, i


def header(raw):
    # 4-byte big-endian proto length, proto, padded to 0x1000; audio follows
    ln = int.from_bytes(raw[:4], "big")
    p, i, h = raw[4:4 + ln], 0, {"chapters": []}
    while i < len(p):
        key, i = varint(p, i)
        f, wt = key >> 3, key & 7
        if wt == 0:
            v, i = varint(p, i)
            h[{2: "length", 3: "audio_id"}.get(f, f)] = v
        else:  # length-delimited
            n, i = varint(p, i); v = p[i:i + n]; i += n
            if f == 1: h["sha1"] = v.hex()
            elif f == 4:
                j = 0
                while j < len(v):
                    c, j = varint(v, j); h["chapters"].append(c)
    return h


def uid_path(uid):
    b = bytes.fromhex(uid.replace(":", "").replace(" ", ""))
    # E0 = ISO 15693. Stock firmware also needs 04 03 (SLIX-L); teddiebox
    # plays any ISO 15693 tag, so only a backwards UID is refused here.
    assert len(b) == 8 and b[0] == 0xE0, f"not an ISO 15693 UID (read backwards?): {uid}"
    if b[:3] != bytes.fromhex("e00403"):
        print("note: not SLIX-L, so only teddiebox will play it", file=sys.stderr)
    r = b[::-1].hex()
    return f"CONTENT/{r[:8]}/{r[8:]}"


def main(content, out=None):
    for f in sorted(pathlib.Path(content).glob("*/*")):
        if f.name.startswith("._") or not f.is_file(): continue
        raw = f.read_bytes()
        h, audio = header(raw), raw[0x1000:]
        uid = bytes.fromhex(f.parent.name + f.name)[::-1].hex().upper()
        ok = hashlib.sha1(audio).hexdigest() == h.get("sha1")
        ts = datetime.fromtimestamp(h.get("audio_id", 0), timezone.utc).date()
        print(f"{f.parent.name}/{f.name}  uid={uid}  audio_id={h.get('audio_id'):#x} ({ts})  "
              f"{len(audio)/1e6:.1f}MB  chapters={len(h['chapters'])}  sha1={'ok' if ok else 'BAD'}")
        if out:
            d = pathlib.Path(out); d.mkdir(parents=True, exist_ok=True)
            (d / f"{f.parent.name}_{f.name}.ogg").write_bytes(audio)


if __name__ == "__main__":
    if sys.argv[1] == "path":
        print(uid_path(sys.argv[2]))
    else:
        main(*sys.argv[1:])
