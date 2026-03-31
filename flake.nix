{
  description = "Tchap crous bot";
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-25.11";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };
  outputs =
    {
      self,
      nixpkgs,
      flake-utils,
      rust-overlay,
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs {
          inherit system overlays;
        };
      in
      rec {
        packages = rec {
          tchap-bot = pkgs.rustPlatform.buildRustPackage {
            pname = "tchap-bot";
            version = "0.1.0";
            src = ./.;
            cargoLock.lockFile = ./Cargo.lock;
            nativeBuildInputs = [ pkgs.pkg-config ];
            buildInputs = [
              pkgs.openssl
              pkgs.sqlite
            ];
          };
          default = tchap-bot;
        };

        legacyPackages = packages;

        devShells.default = pkgs.mkShell {
          packages = with pkgs; [
            rust-bin.stable."1.93.1".default
            openssl
            pkg-config
            sqlite
          ];
        };
      }
    );
}
