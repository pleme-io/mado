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
    # ishou owns the fleet typography. The mado HM module reads
    # `ishou.packages.${system}.fleet-fonts` to (a) default
    # font_family / font_italic / font_size to the canonical
    # MonoFonts::pleme() values and (b) install the underlying
    # nixpkgs font packages via home.packages so glyphon's cosmic-
    # text fontdb can actually resolve them at runtime. Without the
    # install, glyphon falls back to whichever face the system has
    # (FiraCode on the current fleet) and mado's cell_width
    # measurement diverges from the actually-rendered ASCII advance
    # — the operator-visible 'gap between every character' bug
    # diagnosed via MCP snapshot_grid on 2026-05-13.
    ishou = {
      url = "github:pleme-io/ishou";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = {
    self,
    nixpkgs,
    crate2nix,
    flake-utils,
    substrate,
    ishou,
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

      # Migration to substrate module-trio + shikumiTypedGroups.
      # See kekkai (2fc3c84) for the canonical template. Mado uses
      # nullable fields for every option (filterNulls in legacy
      # module). With typed groups, defaults always serialize — this
      # is identical to hikki's pattern and matches what shikumi
      # expects (defaults from Nix override shikumi's compile-time
      # defaults; users override via Nix).
      module = {
        description = "Mado (窓) — GPU-rendered terminal emulator";
        hmNamespace = "blackmatter.components";

        # Shikumi YAML config at ~/.config/mado/mado.yaml.
        withShikumiConfig = true;

        shikumiTypedGroups = {
          window = {
            width   = { type = "int"; default = 1280; description = "Window width in pixels."; };
            height  = { type = "int"; default = 800;  description = "Window height in pixels."; };
            padding = { type = "int"; default = 8;    description = "Window padding in pixels."; };
          };

          shell = {
            command = {
              type        = "nullOrStr";
              default     = null;
              description = "Shell command to run (default: user's SHELL).";
            };
          };

          cursor = {
            style = {
              type        = nixpkgs.lib.types.enum [ "block" "bar" "underline" ];
              default     = "block";
              description = "Cursor style.";
            };
            blink         = { type = "bool"; default = true; description = "Whether the cursor blinks."; };
            blink_rate_ms = { type = "int";  default = 500;  description = "Cursor blink rate in milliseconds."; };
          };

          behavior = {
            scrollback_lines = { type = "int";  default = 10000; description = "Number of scrollback lines retained."; };
            copy_on_select   = { type = "bool"; default = false; description = "Copy selection to clipboard automatically."; };
          };

          appearance = {
            background = { type = "str";   default = "#2e3440"; description = "Background color (hex)."; };
            foreground = { type = "str";   default = "#eceff4"; description = "Foreground color (hex)."; };
            opacity    = { type = "float"; default = 1.0;       description = "Window opacity (0.0-1.0)."; };
          };

          # ── Performance / pacing — adaptive runtime-posture knobs ──
          #
          # Every field except `vsync` is `nullOrInt`. `null` (default)
          # means: defer to garasu::adaptive — the detected display
          # refresh rate becomes the smart default. Pin to a specific
          # number to override detection on this node.
          #
          # See pleme-io/garasu/src/adaptive.rs for the recommender
          # rule table and pleme-io/mado/src/config.rs for the typed
          # precedence chain: hardcoded fallback (60) ← detected ←
          # this YAML ← named profile ← CLI flag.
          performance = {
            vsync = { type = "bool"; default = true; description = "Enable vsync for smoother rendering."; };
            target_fps = {
              type = "nullOrInt";
              default = null;
              description = "Explicit fps target. null = defer to garasu::adaptive detected refresh (per-display).";
            };
            fps_cap = {
              type = "nullOrInt";
              default = null;
              description = "Upper bound on the adaptive recommendation. null = no ceiling.";
            };
            battery_fps_cap = {
              type = "nullOrInt";
              default = null;
              description = "Upper bound when on battery (laptops). null = same as fps_cap. Inert until M1 battery detection lands.";
            };
          };
        };

        # Top-level font_family / font_size live outside any typed group
        # in the legacy module — keep them as bespoke options so the YAML
        # round-trips byte-identical to the hand-rolled module's shape
        # (font_family / font_size at the top level, not under a group).
        extraHmOptions = {
          fontFamily = nixpkgs.lib.mkOption {
            type = nixpkgs.lib.types.nullOr nixpkgs.lib.types.str;
            default = null;
            description = ''
              Primary monospace family. null = inherit the fleet
              canonical name from `ishou::fleet-fonts.primary.name`
              (currently 'JetBrainsMono Nerd Font' per
              ishou-tokens::MonoFonts::pleme()).
            '';
          };
          fontItalic = nixpkgs.lib.mkOption {
            type = nixpkgs.lib.types.nullOr nixpkgs.lib.types.str;
            default = null;
            description = ''
              Italic-face family. null = inherit
              `ishou::fleet-fonts.italic.name` (currently 'Iosevka',
              calligraphic style intent per
              ishou-tokens::MonoFonts::pleme()).
            '';
          };
          fontSize = nixpkgs.lib.mkOption {
            type = nixpkgs.lib.types.nullOr nixpkgs.lib.types.float;
            default = null;
            description = "Font size in pixels.";
          };
          extraSettings = nixpkgs.lib.mkOption {
            type = nixpkgs.lib.types.attrs;
            default = { };
            description = "Additional raw settings merged on top of the typed YAML.";
          };
        };

        # Merge font_family / font_italic / font_size / extraSettings
        # into the YAML payload at the top level, AND install the
        # canonical fleet font packages via home.packages.
        #
        # Defaults are sourced from `ishou::fleet-fonts` — the typed
        # fleet typography. Operators can override per-host via
        # `services.mado.fontFamily = "...";` but the canonical
        # answer lives in pleme-io/ishou/crates/ishou-tokens/src/typography.rs.
        # Installing the underlying nixpkgs font packages is
        # load-bearing: without it, glyphon's cosmic-text fontdb falls
        # back to a different metric than mado's cell_width
        # measurement (the 2026-05-13 'gap between every character'
        # rendering bug).
        extraHmConfigFn = { cfg, pkgs, lib, ... }:
          let
            fleetFonts = import "${ishou.packages.${pkgs.stdenv.hostPlatform.system}.fleet-fonts}"
              { inherit pkgs; };
            # Resolved values: explicit option override beats fleet
            # default. nullable fields → either operator's value or
            # the canonical fleet name from ishou.
            resolvedFontFamily = if cfg.fontFamily != null
              then cfg.fontFamily
              else fleetFonts.primary.name;
            resolvedFontItalic = if cfg.fontItalic != null
              then cfg.fontItalic
              else fleetFonts.italic.name;
            fontExtras = {
              font_family = resolvedFontFamily;
              font_italic = resolvedFontItalic;
            } // (if cfg.fontSize != null
              then { font_size = cfg.fontSize; }
              else { });
            extras = fontExtras // cfg.extraSettings;
            # Filter null packages — emoji is OS-shipped on macOS.
            fontPackages = lib.filter (p: p != null) [
              fleetFonts.primary.package
              fleetFonts.italic.package
              fleetFonts.bold.package
              fleetFonts.symbols.package
            ];
          in {
            services.mado.settings = extras;
            home.packages = fontPackages;
          };
      };
    };
}
