{ pkgs }:

pkgs.stdenv.mkDerivation {
  pname = "qemu-system-data";
  version = "10.1.3+ds-1";

  src = pkgs.fetchurl {
    url = "http://ftp.cn.debian.org/debian/pool/main/q/qemu/qemu-system-data_10.1.3+ds-1_all.deb";
    sha256 = "0nz3whjiw3qmp27mhidh8fk7gfkcvk2vk7z4pr6w3is9hd2g36sg";
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
