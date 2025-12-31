{ stdenv }:

stdenv.mkDerivation {
  pname = "about";
  version = "0.1.0";

  src = ./.;

  makeFlags = [
    "ARCH=x86_64"
    "CROSS_COMPILE=${stdenv.cc.targetPrefix}"
  ];

  installPhase = ''
    mkdir -p $out/bin
    install -m755 about $out/bin/about.elf
  '';

  meta = {
    description = "About utility for DragonOS";
    platforms = [ "x86_64-linux" ];
  };
}
