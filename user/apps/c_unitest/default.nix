{ stdenv }:

stdenv.mkDerivation {
  pname = "c-unitest";
  version = "0.1.0";

  src = ./.;

  makeFlags = [
    "ARCH=x86_64"
    "CROSS_COMPILE=${stdenv.cc.targetPrefix}"
  ];

  installPhase = ''
    mkdir -p $out/bin

    # 安装所有编译出的测试程序
    for bin in test_* dmesg ptmx_demo http_server tty_demo; do
      if [ -f "$bin" ]; then
        install -m755 "$bin" $out/bin/"$bin"
      fi
    done
  '';

  meta = {
    description = "C unit tests for DragonOS";
    platforms = [ "x86_64-linux" ];
  };
}
