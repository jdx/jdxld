{
  inputs = {
    nixpkgs.url = "https://nixos.org/channels/nixos-unstable/nixexprs.tar.xz";
    crane.url = "github:ipetkov/crane";
  };

  outputs =
    {
      self,
      nixpkgs,
      crane,
    }:
    let
      # Generate an output for each flake-exposed system. Flakes suck.
      forAllSystems = nixpkgs.lib.genAttrs nixpkgs.lib.systems.flakeExposed;

      # Make an attribute-set that instances Nixpkgs with our overlay for each
      # system
      common = forAllSystems (system: {
        pkgs = import nixpkgs {
          inherit system;
          overlays = [
            (import self)
          ];
        };
      });
    in
    {
      formatter = forAllSystems (system: common.${system}.pkgs.nixfmt-tree);

      # Route all uses through here so we are
      # testing it the way most users will use the derivation
      # Which is `import jdxld`
      overlays.default = import self;

      # Output jdxld as a stand-alone package.
      packages = forAllSystems (system: {
        default = common.${system}.pkgs.jdxld;
      });

      # Tests to ensure jdxld continues working on Nixos
      # We run unit tests, and some smoke tests that are in Nixpkgs.
      checks = forAllSystems (
        system:
        let
          inherit (common.${system}) pkgs;
        in
        {
          # Use the crane-cached build artifacts to speed up building the unit tests.
          jdxld = pkgs.jdxld-unwrapped.overrideAttrs (old: {
            stdenv = p: p.stdenvNoCC;

            doCheck = true;
            doInstallCheck = false;
            # Skip the build phase and don't install anything
            # because it ends up building libjdxld twice. Once for the buildPhase,
            # once for the checkPhase.
            dontBuild = true;
            installPhase = "touch $out";
          });
        }
      );

      # devShell for developing jdxld
      devShells = forAllSystems (system: {
        default = common.${system}.pkgs.callPackage ./nix/shell.nix { };
      });
    };
}
