{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-25.11";
    crane.url = "github:ipetkov/crane";
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
      crane,
      flake-utils,
      rust-overlay,
      ...
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [ (import rust-overlay) ];
        };

        rustToolchain = pkgs.rust-bin.stable.latest.default;

        craneLib = (crane.mkLib pkgs).overrideToolchain rustToolchain;

        cargoVendorDir = craneLib.vendorCargoDeps {
          src = craneLib.cleanCargoSource ./.;
        };

        gridshift = craneLib.buildPackage {
          src = craneLib.cleanCargoSource ./.;
          inherit cargoVendorDir;
          nativeBuildInputs = [ pkgs.protobuf ];
        };
      in
      {
        packages.default = gridshift;

        checks.gridshift = pkgs.testers.runNixOSTest (import ./test.nix { inherit gridshift; });
      }
    );
}
