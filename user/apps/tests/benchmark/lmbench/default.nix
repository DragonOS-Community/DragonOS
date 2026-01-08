{
  lib,
  pkgs,
  installDir ? "/opt/lmbench",
}:

let
  lmbenchBin = pkgs.fetchurl {
    url = "https://mirrors.dragonos.org.cn/pub/third_party/lmbench/lmbench-ubuntu2404-202511301014-36c2fb2d084343e098b5e343576d4fc0.tar.xz";
    sha256 = "sha256-/iP7oKc2n0CXpA0OgVQX86quNfbtjXBZ83RBl1MEBhE=";
  };

  testScript = pkgs.stdenv.mkDerivation {
    pname = "lmbench-test-script";
    version = "3.0-a9";

    src = lib.sourceByRegex ./. [
      "^test_cases"
      "^.*\.sh$"
    ];

    installPhase = ''
      mkdir -p $out/${installDir}

      install -m755 *.sh $out/${installDir}/
      cp -r test_cases $out/${installDir}/
      chmod +x $out/${installDir}/test_cases/*.sh
    '';
  };

  lmbench = pkgs.stdenv.mkDerivation {
    pname = "lmbench";
    version = "3.0-a9";

    src = lmbenchBin;

    nativeBuildInputs = [ pkgs.autoPatchelfHook ];

    buildInputs = [
      pkgs.stdenv.cc.cc.lib
      pkgs.glibc
      pkgs.bzip2
    ];

    sourceRoot = ".";

    installPhase = ''
      # Keep Ubuntu package's original directory structure, but flatten sysroot
      mkdir -p $out

      # If there's a sysroot directory, flatten it
      if [ -d sysroot ]; then
        cp -r sysroot/* $out/
      else
        cp -r * $out/
      fi

      # Make all binaries executable
      find $out -type f -executable -exec chmod +x {} \;

      # Remove broken symlinks (mainly documentation files from sysroot)
      find $out -xtype l -delete
    '';

    # autoPatchelfHook will automatically run after installPhase
    # to patch all ELF binaries with correct library paths
  };
in
pkgs.symlinkJoin {
  name = "lmbench-with-tests";
  paths = [
    lmbench
    testScript
  ];
}
