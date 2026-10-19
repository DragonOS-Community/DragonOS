{
  stdenv,
  target ? "x86_64",
}:

stdenv.mkDerivation {
  pname = "ebpf-smoke";
  version = "0.1.0";

  src = ./.;

  makeFlags = [
    "ARCH=${target}"
    "CROSS_COMPILE=${stdenv.cc.targetPrefix}"
  ];

  installPhase = ''
    mkdir -p $out/bin
    install -m755 test_ebpf_smoke $out/bin/
  '';

  meta = {
    description = "eBPF capability smoke test for DragonOS (maps/prog/tracepoint/uprobe/perf output)";
    platforms = [ "x86_64-linux" ];
  };
}
