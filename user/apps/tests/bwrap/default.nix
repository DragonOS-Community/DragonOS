{
  lib,
  pkgs,
  installDir,
  version ? "20260525",
}:

let
  testsArchive = pkgs.fetchurl {
    url = "https://cnb.cool/DragonOS-Community/test-suites/-/releases/download/release_${version}/bwrap-tests.tar.xz";
    sha256 = "sha256-lN8OpxA7xMC4nVBIK82+NmUtjr6jc3IBM95g7GgFDOI=";
  };

in
pkgs.stdenv.mkDerivation {
  pname = "bwrap-tests";
  version = "0.1.0";

  src = lib.sourceByRegex ./. [
    "^whitelist\.txt$"
    "^run_tests\.sh$"
  ];

  nativeBuildInputs = [ pkgs.autoPatchelfHook ];

  buildInputs = [ pkgs.stdenv.cc.cc.lib ];

  # Don't let Nix patchShebangs in the test scripts
  dontPatchShebangs = true;

  installPhase = ''
    mkdir -p $out/${installDir}

    install -m644 whitelist.txt $out/${installDir}/
    install -m755 run_tests.sh $out/${installDir}/

    mkdir -p $out/${installDir}/tests
    tar -xf ${testsArchive} -C $out/${installDir}/tests --strip-components=1

    find $out/${installDir}/tests -type f -name '*_test' -exec chmod 755 {} \;

    # Install bwrap binary to /bin so libtest.sh can find it via PATH
    mkdir -p $out/bin
    install -m755 $out/${installDir}/tests/bwrap-static $out/bin/bwrap
  '';

  meta = with lib; {
    description = "bubblewrap functional test suite for DragonOS";
    platforms = platforms.linux;
  };
}
