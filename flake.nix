{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
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
        crossPkgs = nixpkgs.legacyPackages.${system}.pkgsCross.mingwW64;
      in
      {
        devShells.default = pkgs.mkShell {
          buildInputs = with pkgs; [
            cargo
            rustc
            rustup  # Added for managing Rust targets
            zig  # For zigbuild cross-compilation
            bun
            bashInteractive
          ];
        };

        devShells.crossWindows = crossPkgs.mkShell {
          buildInputs = with crossPkgs; [
            rustup
            bun
            bashInteractive
          ];
        };
      }
    );
}
