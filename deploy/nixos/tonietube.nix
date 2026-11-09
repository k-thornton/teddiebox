{ ... }:
# tonietube beside teddyCloud: a root podman container managed by systemd,
# so it starts at boot like teddycloud does. Import from configuration.nix.
#
# Image: built locally as root from ~kyle/tonietube/src:
#   sudo podman build -t localhost/tonietube:latest -f Containerfile .
# then `sudo systemctl restart podman-tonietube`. Config and state live in
# /var/lib/tonietube (cards.toml, teddycloud-ca.pem, state.json), owned by
# kyle so cards can be added without sudo.
{
  virtualisation.oci-containers.containers.tonietube = {
    image = "localhost/tonietube:latest";
    volumes = [ "/var/lib/tonietube:/data" ];
    environment = {
      PYTHONUNBUFFERED = "1";
      TZ = "America/Los_Angeles";
    };
    # Downloads and the TAF being built live in /tmp inside the container.
    extraOptions = [ "--tmpfs=/tmp:size=2G" ];
  };

  systemd.tmpfiles.rules = [ "d /var/lib/tonietube 0755 kyle users -" ];
}
