{ lib, pkgs, fenix, system, installDir }:

let
  fenixPkgs = fenix.packages.${system};
  toolchain = fenixPkgs.combine (with fenixPkgs; [
    minimal.rustc
    minimal.cargo
  ]);
  rustPlatform = pkgs.makeRustPlatform {
    cargo = toolchain;
    rustc = toolchain;
  };
  fullSrc = ./.;
  runnerSrc = ./runner;
  runnerName = "runner";
  outName = "gvisor-tests";
  # installDir = installDir;
  testsArchive = pkgs.fetchurl {
    url = "https://cnb.cool/DragonOS-Community/test-suites/-/releases/download/release_20250626/gvisor-syscalls-tests.tar.xz";
    sha256 = "sha256-GSZ0N3oUOerb0lXU4LZ0z4ybD/xZdy7TtfstEoffcsk=";
  };

in rustPlatform.buildRustPackage {
  pname = outName;
  version = "0.1.0";

  src = runnerSrc;
  cargoLock = {
    lockFile = ./runner/Cargo.lock;
  };

  postInstall = ''
    # Ensure runner binary exists and rename to gvisor-test-runner as per Makefile install
    mkdir -p "$out/${installDir}"
    if [ -x "$out/bin/${runnerName}" ]; then
      mv "$out/bin/${runnerName}" "$out/${installDir}/gvisor-test-runner"
    fi

    # Only package files used by install target: whitelist, blocklists, run_tests.sh
    mkdir -p $out/${installDir}
    install -m644 ${fullSrc}/whitelist.txt $out/${installDir}/
    cp -r ${fullSrc}/blocklists $out/${installDir}/
    install -m755 ${fullSrc}/run_tests.sh $out/${installDir}/

    # Bundle tests archive for offline systems
    mkdir -p $out/${installDir}/tests
    tar -xf ${testsArchive} -C $out/${installDir}/tests --strip-components=1
    # Ensure test binaries are executable
    find $out/${installDir}/tests -type f -name '*_test' -exec chmod +x {} + || true
  '';

  meta = with lib; {
    description = "gVisor syscall test runner and scripts";
    platforms = platforms.linux;
    license = licenses.mit;
  };
}
