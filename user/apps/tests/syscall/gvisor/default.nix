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

  testsArchive = pkgs.fetchurl {
    url = "https://cnb.cool/DragonOS-Community/test-suites/-/releases/download/release_20250626/gvisor-syscalls-tests.tar.xz";
    sha256 = "sha256-GSZ0N3oUOerb0lXU4LZ0z4ybD/xZdy7TtfstEoffcsk=";
  };

  # 1. Build the Rust runner separately
  # This ensures that changes to test scripts or data don't trigger a Rust rebuild.
  runner = rustPlatform.buildRustPackage {
    pname = "gvisor-test-runner-bin";
    version = "0.1.0";

    src = ./runner;
    cargoLock = {
      lockFile = ./runner/Cargo.lock;
    };

    # Move the binary to the expected install directory structure
    postInstall = ''
      mkdir -p $out/${installDir}
      if [ -f "$out/bin/runner" ]; then
        mv "$out/bin/runner" "$out/${installDir}/gvisor-test-runner"
        # Clean up empty bin directory if it exists, to avoid clutter in symlinkJoin
        rmdir "$out/bin" || true
      fi
    '';
  };

  # 2. Prepare the test data, scripts, and patched binaries
  # This derivation handles downloading, extracting, and patching the tests.
  tests = pkgs.stdenv.mkDerivation {
    pname = "gvisor-tests-data";
    version = "0.1.0";

    # Use sourceByRegex to only depend on relevant files.
    # This prevents rebuilds when files in ./runner change.
    src = lib.sourceByRegex ./. [
      "^whitelist\.txt$"
      "^blocklists"
      "^blocklists/.*"
      "^run_tests\.sh$"
    ];

    nativeBuildInputs = [ pkgs.autoPatchelfHook ];

    buildInputs = [ pkgs.stdenv.cc.cc.lib ];

    installPhase = ''
      mkdir -p $out/${installDir}

      install -m644 whitelist.txt $out/${installDir}/
      cp -r blocklists $out/${installDir}/
      install -m755 run_tests.sh $out/${installDir}/

      # Bundle tests archive for offline systems
      mkdir -p $out/${installDir}/tests
      tar -xf ${testsArchive} -C $out/${installDir}/tests --strip-components=1

      runHook preInstall
      find $out/${installDir}/tests -type f -name '*_test' -exec install -m755 {} $out/${installDir}/tests \; || true
      runHook postInstall
    '';
  };

in pkgs.symlinkJoin {
  name = "gvisor-tests";
  paths = [ runner tests ];
  meta = with lib; {
    description = "gVisor syscall test runner and scripts";
    platforms = platforms.linux;
    license = licenses.mit;
  };
}
