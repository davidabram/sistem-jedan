{
  description = "sistem-jedan";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      rust-overlay,
    }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "aarch64-darwin"
      ];
      forAllSystems =
        f:
        nixpkgs.lib.genAttrs systems (
          system:
          f (
            import nixpkgs {
              inherit system;
              overlays = [ rust-overlay.overlays.default ];
            }
          )
        );
      toolchain = pkgs: pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
    in
    {
      packages = forAllSystems (pkgs: {
        default =
          (pkgs.makeRustPlatform {
            rustc = toolchain pkgs;
            cargo = toolchain pkgs;
          }).buildRustPackage
            {
              pname = "sistem-jedan";
              version = "0.1.0";
              src = pkgs.lib.fileset.toSource {
                root = ./.;
                fileset = pkgs.lib.fileset.unions [
                  ./Cargo.toml
                  ./Cargo.lock
                  ./src
                ];
              };
              cargoLock.lockFile = ./Cargo.lock;
              nativeCheckInputs = [ pkgs.cacert ];
              meta.mainProgram = "sistem-jedan";
            };
      });

      checks = forAllSystems (pkgs: {
        default = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
      });

      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          packages = [ (toolchain pkgs) ];
        };
      });

      formatter = forAllSystems (pkgs: pkgs.nixfmt);
    };
}
