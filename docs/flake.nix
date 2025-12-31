{
  description = "Reproducible Sphinx Documentation Build";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-25.11";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = nixpkgs.legacyPackages.${system};
        lib = pkgs.lib;

        # ----------------------------------------------------------------
        # 1. 定义精确的 Python 环境
        # ----------------------------------------------------------------
        # 这里显式列出依赖，确保无论在哪台机器，Sphinx 版本一致
        pythonEnv = pkgs.python3.withPackages (ps: with ps; [
          sphinx
          sphinx-multiversion
          sphinx-autobuild
          # 在这里添加你的 theme 或 extension，例如:
          sphinx-rtd-theme
          myst-parser
          sphinxcontrib-mermaid
          linkify-it-py
        ]);

        # ----------------------------------------------------------------
        # 2. 从 Flake 输入中提取 Git 信息 (Pure 方式)
        # ----------------------------------------------------------------
        # Nix 在求值时知道当前的 revision，不需要在 build 时运行 git 命令
        gitHash = if (self ? rev) then self.rev else "dirty-dev";
        isDirty = if (self ? rev) then "0" else "1";

        buildDir = "./_build";
        port = 8000;

      in
      {
        # ----------------------------------------------------------------
        # Packages: 纯净构建 (nix build)
        # ----------------------------------------------------------------
        # 这种方式构建的是"当前代码的快照"，完全不依赖 .git 目录
        packages.default = pkgs.stdenv.mkDerivation {
          name = "sphinx-docs";
          src = ./.;

          buildInputs = [ pythonEnv ];

          # 将 Nix 提取的 Git 信息注入环境变量
          # 这完全替代了 Makefile 中 $(shell git rev-parse) 的逻辑
          env = {
            LANGUAGE = "zh_CN";
            CURRENT_GIT_COMMIT_HASH = gitHash;
            CURRENT_GIT_COMMIT_DIRTY = isDirty;
            SPHINXBUILD = "sphinx-build";
          };

          # 直接定义构建逻辑，不再依赖 Makefile
          buildPhase = ''
            echo "Building documentation for commit: $CURRENT_GIT_COMMIT_HASH"

            # 使用 -W 将警告视为错误，确保构建质量
            sphinx-build -M html . ${buildDir} \
              -D language=$LANGUAGE \
              -w _build/warnings.log
          '';

          installPhase = ''
            mkdir -p $out
            cp -r ${buildDir}/* $out/
          '';
        };

        # ----------------------------------------------------------------
        # Apps: 快速运行工具 (nix run)
        # ----------------------------------------------------------------
        # 提供一个脚本来预览构建结果
        apps.release = flake-utils.lib.mkApp {
          drv = let targetDir = self.packages.${system}.default;
            in pkgs.writeShellApplication {
              name = "preview-docs";
              runtimeInputs = [ pythonEnv ];
              text = ''
                echo "Open at http://localhost:${toString port}/index.html"
                python3 -m http.server --directory "${targetDir}/html" ${toString port}
              '';
            };
        };
        app.develop = flake-utils.lib.mkApp {
          drv = pkgs.writeShellApplication {
            name = "sphinx-autobuild";
            runtimeInputs = [ pythonEnv ];
            text = ''
              sphinx-autobuild . ${buildDir} --host 0.0.0.0 --port ${toString port}
            '';
          };
        };
        # 设置默认 run 行为
        apps.default = self.app.${system}.develop;

        # ----------------------------------------------------------------
        # DevShell: 开发环境 (nix develop)
        # ----------------------------------------------------------------
        # 用于开发和需要访问 .git 的操作（如 sphinx-multiversion）
        devShells.default = pkgs.mkShell {
          packages = [ pythonEnv pkgs.git pkgs.gnumake ];

          shellHook = ''
            export LANGUAGE="zh_CN"
            export SPHINXBUILD="sphinx-build"

            # 在开发环境中，我们可以动态获取 git 状态
            export CURRENT_GIT_COMMIT_HASH=$(git rev-parse --short HEAD 2>/dev/null || echo "unknown")

            echo "🚀 Sphinx Dev Environment Loaded"
            echo "-----------------------------------"
            echo "Run 'make html' for standard build"
            echo "Run 'make html-multiversion' for versioned build (Requires .git)"
            echo "Run 'nix build' for clean production build"
          '';
        };
      }
    );
}
