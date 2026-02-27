{
  description = "tend — workspace repository manager";

  nixConfig = {
    allow-import-from-derivation = true;
  };

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    crate2nix.url = "github:nix-community/crate2nix";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = {
    self,
    nixpkgs,
    crate2nix,
    flake-utils,
  }:
    flake-utils.lib.eachDefaultSystem (system: let
      pkgs = import nixpkgs {inherit system;};
      cargoNix = import ./Cargo.nix {inherit pkgs;};
      tendBinary = cargoNix.rootCrate.build;
    in {
      packages = {
        default = tendBinary;
        tend = tendBinary;
      };

      apps = {
        default = {
          type = "app";
          program = "${tendBinary}/bin/tend";
        };

        regenerate-cargo-nix = {
          type = "app";
          program = toString (pkgs.writeShellScript "regenerate-cargo-nix" ''
            echo "Regenerating Cargo.nix..."
            ${crate2nix.packages.${system}.default}/bin/crate2nix generate
            echo "Cargo.nix regenerated."
            echo "Don't forget to commit: git add Cargo.nix"
          '');
        };
      };

      devShells.default = pkgs.mkShell {
        buildInputs = with pkgs; [
          cargo
          rustc
          rust-analyzer
          clippy
          rustfmt
          crate2nix.packages.${system}.default
          pkg-config
          openssl
        ];
      };
    });
}
