{ pkgs }:

# 通过 `-L` 优先使用 Debian 打包的 QEMU 固件。
# 不能直接使用 nixpkgs qemu 自带的 share/qemu：两者的 pvh.bin（PVH 启动 ROM）不同，
# 使用 nixpkgs 版本（qemu 10.1.2）时内核在 PVH 启动后 panic：
#   acpi_init(): ACPI not provided by bootloader
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
