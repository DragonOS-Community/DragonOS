{ pkgs }:

pkgs.stdenv.mkDerivation {
  pname = "qemu-system-data";
  version = "10.1.3+ds-1";

  src = pkgs.fetchurl {
    url = "http://snapshot.debian.org/archive/debian/20251216T024428Z/pool/main/q/qemu/qemu-system-data_10.1.3+ds-1_all.deb";
    sha256 = "sha256-T5vxRINJx8FNvuSfucXcbLp3pkOwRViPuBUPHiXk41s";
  };

  nativeBuildInputs = with pkgs; [ dpkg ];

  unpackPhase = ''
    dpkg-deb -x $src .
  '';

  installPhase = ''
    mkdir -p $out
    cp -r usr/share/qemu/* $out/
  '';

  meta = with pkgs.lib; {
    description = "QEMU firmware files from Debian package";
    platforms = platforms.all;
  };
}
