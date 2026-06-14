{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/26.05";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs {
          inherit system;
          config.allowUnfree = true;
          overlays = [
          ];
        };
      in
      rec {
        packages = rec {
          spacetimedb-tui = pkgs.rustPlatform.buildRustPackage {
            pname = "spacetimedb-tui";
            version = "0.1.0";
            src = ./.;

            cargoLock = {
              lockFile = ./Cargo.lock;
            };

            nativeBuildInputs = [
              pkgs.pkg-config
            ];

            buildInputs = [
              pkgs.openssl
            ];

            meta = with pkgs.lib; {
              description = "A terminal user interface for SpacetimeDB";
              homepage = "https://github.com/clockworklabs/spacetimedb-tui";
              license = licenses.mit;
              maintainers = [ ];
              mainProgram = "spacetimedb-tui";
            };
          };

          default = spacetimedb-tui;
        };

        apps.default = {
          type = "app";
          meta = packages.spacetimedb-tui.meta;
          program = "${self.packages.${system}.spacetimedb-tui}/bin/spacetimedb-tui";
        };

        checks = {
          build = self.packages.${system}.spacetimedb-tui;
        };

        formatter = pkgs.nixpkgs-fmt;

        devShells.default = with pkgs;
          mkShell {
            buildInputs = [
              # Rust
              cargo-generate
              cargo-watch
              rustup
              rust-analyzer
            ];
          };
      });
}
