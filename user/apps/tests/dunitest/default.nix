{
  lib,
  pkgs,
  rustPlatform,
  # another_ext4 依赖 nightly 特性，与内核使用同一套工具链
  nightlyRustPlatform,
  installDir,
}:

let
  gtest = pkgs.fetchgit {
    url = "https://git.mirrors.dragonos.org.cn/DragonOS-Community/googletest";
    rev = "v1.17.0";
    sha256 = "sha256-HIHMxAUR4bjmFLoltJeIAVSulVQ6kVuIT2Ku+lwAx/4=";
  };

  # 1. Build the Rust runner separately
  # This ensures that changes to test scripts or data don't trigger a Rust rebuild.
  runner = rustPlatform.buildRustPackage {
    pname = "dunitest-runner-bin";
    version = "0.1.0";

    src = ./runner;
    cargoLock = {
      lockFile = ./runner/Cargo.lock;
    };

    # Move the binary to the expected install directory structure
    postInstall = ''
      mkdir -p $out/${installDir}
      if [ -f "$out/bin/dunitest-runner" ]; then
        mv "$out/bin/dunitest-runner" "$out/${installDir}/dunitest-runner"
        # Clean up empty bin directory if it exists, to avoid clutter in symlinkJoin
        rmdir "$out/bin" || true
      fi
    '';
  };

  # Host helpers that seed the ext4 fixtures. The Makefile would otherwise
  # `cargo run` them from the kernel tree, which the sandbox cannot do.
  ext4HostTools = nightlyRustPlatform.buildRustPackage {
    pname = "dunitest-ext4-host-tools";
    version = "0.1.0";

    # ext4_test depends on its parent crate through `path = ".."`
    src = ../../../../kernel/crates/another_ext4;
    cargoRoot = "ext4_test";
    buildAndTestSubdir = "ext4_test";
    cargoLock = {
      lockFile = ../../../../kernel/crates/another_ext4/ext4_test/Cargo.lock;
    };
    cargoBuildFlags = [
      "--bin"
      "shared_xattr"
      "--bin"
      "orphan_recovery_fixture"
    ];
    doCheck = false;
  };

  # 2. Build the C++ test suites
  # This derivation handles compiling the gtest-based test cases.
  testSuites = pkgs.stdenv.mkDerivation {
    pname = "dunitest-suites";
    version = "0.1.0";

    # Use sourceByRegex to only depend on relevant files.
    # This prevents rebuilds when files in ./runner change.
    src = lib.sourceByRegex ./. [
      "^suites$"
      "^suites/.*"
      "^Makefile$"
      "^whitelist\\.txt$"
      "^no_skip\\.txt$"
      "^scripts$"
      "^scripts/run_tests\\.sh$"
    ];

    nativeBuildInputs = [
      pkgs.autoPatchelfHook
      pkgs.e2fsprogs
      pkgs.dosfstools
    ];

    buildInputs = [ pkgs.stdenv.cc.cc.lib ];

    buildPhase = ''
      runHook preBuild

      make -j"''${NIX_BUILD_CORES:-1}" build-suites \
        GTEST_ROOT=${gtest} \
        EXT4_HOST_TOOL_BIN_DIR=${ext4HostTools}/bin \
        CXX="$CXX" \
        CXXFLAGS="-Wall -O2 -std=c++17 -pthread"

      runHook postBuild
    '';

    installPhase = ''
      runHook preInstall

      mkdir -p $out/${installDir}
      cp -r bin $out/${installDir}/
      cp -r build/fixtures $out/${installDir}/
      install -m644 whitelist.txt $out/${installDir}/
      install -m644 no_skip.txt $out/${installDir}/
      install -m755 scripts/run_tests.sh $out/${installDir}/

      runHook postInstall
    '';
  };

in
pkgs.symlinkJoin {
  name = "dunitest";
  paths = [
    runner
    testSuites
  ];
  meta = with lib; {
    description = "DragonOS dunitest runner and test suites";
    platforms = platforms.linux;
    license = licenses.gpl2;
  };
}
