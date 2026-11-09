{ ... }:
let
  dataDir = "/var/lib/teddycloud";
  # The container's data folders, from teddyCloud's docker-compose.yaml.
  folders = {
    certs = "/teddycloud/certs";
    config = "/teddycloud/config";
    content = "/teddycloud/data/content";
    library = "/teddycloud/data/library";
    custom_img = "/teddycloud/data/www/custom_img";
    firmware = "/teddycloud/data/firmware";
    cache = "/teddycloud/data/cache";
    plugins = "/teddycloud/data/www/plugins";
  };
in
{
  # teddyCloud for the Toniebox. LAN only: it has no authentication, and
  # anyone who can reach 8443 can upload firmware the box will run.
  virtualisation.oci-containers.backend = "podman";
  virtualisation.oci-containers.containers.teddycloud = {
    # Pinned so a rebuild never upgrades it; bump deliberately.
    image = "ghcr.io/toniebox-reverse-engineering/teddycloud:tc_v0.7.1";
    ports = [
      "443:443" # the box connects here; must stay 443
      "8443:8443" # web interface and API (HTTPS)
      "80:80" # web interface (HTTP)
    ];
    volumes = builtins.attrValues (builtins.mapAttrs (name: path: "${dataDir}/${name}:${path}") folders);
  };

  systemd.tmpfiles.rules = map (name: "d ${dataDir}/${name} 0750 root root -") (builtins.attrNames folders);

  networking.firewall.allowedTCPPorts = [ 80 443 8443 ];
}
