{
  lib,
  buildGoModule,
  fetchgit,
  target ? "x86_64",
}:

let
  version = "0.1.0";
  # Pinned mirror of gitee.com/OpenCloudOS/ocm-agent. The `dragonos` branch
  # carries two portability fixes on top of the pristine upstream mirror.
  # Keep this revision in sync with user/apps/ocm-agent/upstream.
  upstream = fetchgit {
    url = "https://github.com/sparkzky/ocm-agent.git";
    rev = "d08b700e20243cee892ef67b975f89500f352c4b";
    hash = "sha256-84ybp+2mb1S3Z7pQVVqnn2HPOlTYWq7hYwB2Ty/WVdo=";
  };
in
buildGoModule {
  pname = "ocm-agent";
  inherit version;

  src = upstream;

  # CGO is required by github.com/mattn/go-sqlite3 (ongoing large-data store).
  # stdenv comes from pkgsStatic, so the resulting binary is fully static and
  # does not depend on the guest's glibc.
  env.CGO_ENABLED = 1;

  vendorHash = "sha256-ULRo/v3tflw9EPtY5QvlZnA/ipZcwLmdxrIFTetF1Vw=";

  subPackages = [ "cmd/ocm-agent" ];

  ldflags = [
    "-s"
    "-w"
    "-X"
    "gitee.com/OpenCloudOS/ocmanager/tms/pkg/version.Version=${version}"
  ];

  # Upstream tests exercise host-side behaviour (systemd, /proc fixtures,
  # network gateways) that is not available inside the Nix sandbox.
  doCheck = false;

  postInstall = ''
    mkdir -p $out/opt/ocm-agent/data $out/opt/ocm-agent/plugin-conf
    install -m755 $out/bin/ocm-agent $out/opt/ocm-agent/ocm-agent
    rm -f $out/bin/ocm-agent
    install -m644 config.yaml.example $out/opt/ocm-agent/config.yaml.example
  '';

  meta = {
    description = "OpenCloudOS OCManager host agent";
    homepage = "https://gitee.com/OpenCloudOS/ocm-agent";
    license = lib.licenses.gpl2Only;
    platforms = [ "x86_64-linux" ];
    mainProgram = "ocm-agent";
  };
}
