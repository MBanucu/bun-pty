{
  description = "Cross-compiling Rust for Windows on NixOS";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    crane.url = "github:ipetkov/crane";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { nixpkgs, crane, flake-utils, rust-overlay, ... }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        crossPkgs = import nixpkgs {
          inherit system;
          overlays = [ (import rust-overlay) ];
          crossSystem = {
            config = "x86_64-w64-mingw32";
          };
        };

        craneLib = (crane.mkLib crossPkgs).overrideToolchain (
          p: p.rust-bin.stable.latest.default.override {
            targets = [ "x86_64-pc-windows-gnu" ];
          }
        );

        # Your crate; adjust src if needed
        myCrate = craneLib.buildPackage {
          src = craneLib.cleanCargoSource ../rust-pty;
          strictDeps = true;
          buildInputs = [ crossPkgs.windows.pthreads ];
          CARGO_BUILD_TARGET = "x86_64-pc-windows-gnu";
          CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER = "${crossPkgs.stdenv.cc.targetPrefix}cc";
        };
      in
      {
        packages = {
          default = myCrate;
        };

        devShells.default = crossPkgs.mkShell {
          buildInputs = [ crossPkgs.windows.pthreads ];
          nativeBuildInputs = [ craneLib.cargo ];
          CARGO_BUILD_TARGET = "x86_64-pc-windows-gnu";
          CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER = "${crossPkgs.stdenv.cc.targetPrefix}cc";
        };
      }
    );
}