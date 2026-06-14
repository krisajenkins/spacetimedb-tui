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
      {
        packages = { };

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
