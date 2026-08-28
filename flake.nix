{
  description = "Profile-aware Chrome DevTools broker for coding agents";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-parts.url = "github:hercules-ci/flake-parts";
    treefmt-nix.url = "github:numtide/treefmt-nix";
  };

  outputs =
    inputs@{ flake-parts, ... }:
    flake-parts.lib.mkFlake { inherit inputs; } {
      systems = [
        "x86_64-linux"
        "aarch64-darwin"
      ];

      imports = [ inputs.treefmt-nix.flakeModule ];

      perSystem =
        { system, ... }:
        let
          pkgs = import inputs.nixpkgs {
            inherit system;
            config.allowUnfree = true;
          };
          chromeDevtoolsMcp = pkgs.callPackage ./nix/chrome-devtools-mcp.nix { };
          runtimePackages = [
            pkgs.nodejs_22
          ]
          ++ pkgs.lib.optionals pkgs.stdenv.hostPlatform.isLinux [
            pkgs.google-chrome
            pkgs.procps
          ];
        in
        {
          packages = {
            chrome-devtools-mcp = chromeDevtoolsMcp;
            default = pkgs.callPackage ./nix/package.nix {
              inherit chromeDevtoolsMcp runtimePackages;
            };
          };

          devShells.default = pkgs.mkShell {
            packages = with pkgs; [
              cargo
              clippy
              nodejs_22
              pkg-config
              rust-analyzer
              rustc
              rustfmt
            ];
          };

          treefmt.programs = {
            nixfmt.enable = true;
            rustfmt.enable = true;
          };
        };
    };
}
