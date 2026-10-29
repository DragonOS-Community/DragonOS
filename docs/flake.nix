{
  description = "Reproducible VitePress documentation environment";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-25.11";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs =
    {
      self,
      nixpkgs,
      flake-utils,
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = nixpkgs.legacyPackages.${system};
        nodejs = pkgs.nodejs_22;
        port = 8000;
      in
      {
        apps.default = flake-utils.lib.mkApp {
          drv = pkgs.writeShellApplication {
            name = "vitepress-dev";
            runtimeInputs = [ nodejs ];
            text = ''
              npm install
              npm run docs:dev -- --host 0.0.0.0 --port ${toString port}
            '';
          };
        };

        apps.release = flake-utils.lib.mkApp {
          drv = pkgs.writeShellApplication {
            name = "preview-docs";
            runtimeInputs = [ nodejs ];
            text = ''
              npm install
              npm run docs:build
              npm run docs:preview -- --host 0.0.0.0 --port ${toString port}
            '';
          };
        };

        devShells.default = pkgs.mkShell {
          packages = [
            nodejs
            pkgs.git
          ];
          shellHook = ''
            echo "VitePress docs environment"
            echo "  npm run docs:dev      local preview"
            echo "  npm run docs:build    static build (includes frozen versions)"
          '';
        };
      }
    );
}
