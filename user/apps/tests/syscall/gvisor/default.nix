{ lib, pkgs, fenix, system, installDir, version ? "20251218" }:

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
    url = "https://cnb.cool/DragonOS-Community/test-suites/-/releases/download/release_${version}/gvisor-syscalls-tests.tar.xz";
    sha256 = "sha256-JVCjDtqF9iNw6B4pXGP39gZRs6rEqtLsrroihraPqQE=";
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

  # 2. Extract and patch test binaries
  # This derivation handles downloading, extracting, and patching the test binaries.
  # Separated from scripts so that whitelist/blocklist changes don't trigger repatching.
  testBinaries = pkgs.stdenv.mkDerivation {
    pname = "gvisor-tests-binaries";
    version = "0.1.0";

    src = testsArchive;

    nativeBuildInputs = [ pkgs.autoPatchelfHook ];

    buildInputs = [ pkgs.stdenv.cc.cc.lib ];

    sourceRoot = ".";

    installPhase = ''
      mkdir -p $out/${installDir}/tests
      cp -r * $out/${installDir}/tests/

      runHook preInstall
      find $out/${installDir}/tests -type f -name '*_test' -exec chmod 755 {} \; || true
      runHook postInstall
    '';
  };

  # 3. Prepare test scripts and configuration files
  # This derivation contains frequently modified files (whitelist, blocklists, scripts).
  # Changes here won't trigger binary repatching.
  testScripts = pkgs.stdenv.mkDerivation {
    pname = "gvisor-tests-scripts";
    version = "0.1.0";

    src = lib.sourceByRegex ./. [
      "^whitelist\.txt$"
      "^blocklists"
      "^blocklists/.*"
      "^run_tests\.sh$"
    ];

    installPhase = ''
      mkdir -p $out/${installDir}

      install -m644 whitelist.txt $out/${installDir}/
      cp -r blocklists $out/${installDir}/
      install -m755 run_tests.sh $out/${installDir}/
    '';
  };

in pkgs.symlinkJoin {
  name = "gvisor-tests";
  paths = [ runner testBinaries testScripts ];
  meta = with lib; {
    description = "gVisor syscall test runner and scripts";
    platforms = platforms.linux;
    license = licenses.mit;
  };
}
