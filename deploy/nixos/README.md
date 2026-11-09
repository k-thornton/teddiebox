# Running the server side on NixOS

Two NixOS modules for the machine that serves the box, at home or hosted:

- `teddycloud.nix`: teddyCloud as a podman container managed by systemd,
  ports 80, 443 and 8443, data under `/var/lib/teddycloud`.
- `tonietube.nix`: the playlist service from `tools/tonietube/`, same
  arrangement, data under `/var/lib/tonietube`.

Import both from the machine's `configuration.nix`:

```nix
imports = [ ./teddycloud.nix ./tonietube.nix ];
```

then `sudo nixos-rebuild switch`. The tonietube image is built on the
machine itself; `tools/tonietube/README.md` has the commands. Everything
else about the machine (hardware, users, network) is its own business and
not part of this project.

Container data is state, not configuration, and is not here:
`/var/lib/teddycloud` holds the certificates, content and library;
`/var/lib/tonietube` holds `cards.toml` and `state.json`.
