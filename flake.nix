{
  description = "Mado (窓) — GPU-rendered terminal emulator";

  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs?ref=nixos-25.11";
    crate2nix.url = "github:nix-community/crate2nix";
    flake-utils.url = "github:numtide/flake-utils";
    substrate = {
      url = "github:pleme-io/substrate";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = {
    self,
    nixpkgs,
    crate2nix,
    flake-utils,
    substrate,
  }:
    (import "${substrate}/lib/rust-tool-release-flake.nix" {
      inherit nixpkgs crate2nix flake-utils;
    }) {
      toolName = "mado";
      src = self;
      repo = "pleme-io/mado";

      # rmcp 0.15 (and its macros crate) read `env!("CARGO_CRATE_NAME")`
      # at compile time. crate2nix's default build step doesn't set
      # that env var, so the crate fails with "environment variable
      # not defined at compile time". substrate exposes
      # `crateOverrides` as the canonical per-crate build-attrs hook
      # — we thread the CARGO_CRATE_NAME env through here so rmcp's
      # macro expansions resolve. No ad-hoc flake rewrite; same
      # pattern any fleet crate can use for the same issue.
      crateOverrides = {
        rmcp = attrs: {
          CARGO_CRATE_NAME = "rmcp";
        };
        rmcp-macros = attrs: {
          CARGO_CRATE_NAME = "rmcp_macros";
        };
        kaname = attrs: {
          CARGO_CRATE_NAME = "kaname";
        };
      };
    }
    // {
      homeManagerModules.default = import ./module {
        hmHelpers = import "${substrate}/lib/hm-service-helpers.nix" { lib = nixpkgs.lib; };
      };
    };
}
