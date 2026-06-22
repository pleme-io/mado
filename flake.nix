{
  description = "Mado (窓) — GPU-rendered terminal emulator";

  inputs = {
    # Substrate bundles every dep the rust build kit needs (nixpkgs,
    # crate2nix, fenix, devenv, flake-utils, gen). Consumers stop
    # redeclaring those.
    substrate.url = "github:pleme-io/substrate";

    # ishou is mado-specific variance — the fleet typography source.
    # Mado's HM module reads `ishou.packages.${system}.fleet-fonts`
    # to resolve canonical font_family / font_italic defaults and
    # install the underlying nixpkgs font packages. Aligned to
    # substrate's nixpkgs pin to avoid the two-nixpkgs diamond.
    ishou = {
      url = "github:pleme-io/ishou";
      inputs.nixpkgs.follows = "substrate/nixpkgs";
    };
  };

  # `src = ./.` is required (not `self`) because substrate reads
  # Cargo.build-spec.json at eval time and `self` triggers an
  # outputs-attrset cycle.
  outputs = { substrate, ishou, ... }:
  let
    # The canonical substrate Rust-tool surface (binary + HM/NixOS/Darwin
    # module trio + the six operator verbs). Unchanged.
    base = substrate.rust.tool {
    src = ./.;
    repo = "pleme-io/mado";

    # rmcp 0.15 (and its macros crate) read `env!("CARGO_CRATE_NAME")`
    # at compile time. crate2nix's default build step doesn't set
    # that env var, so the crate fails with "environment variable
    # not defined at compile time". Same pattern any fleet crate
    # can use for the same issue.
    crateOverrides = {
      rmcp = attrs: { CARGO_CRATE_NAME = "rmcp"; };
      rmcp-macros = attrs: { CARGO_CRATE_NAME = "rmcp_macros"; };
      kaname = attrs: { CARGO_CRATE_NAME = "kaname"; };
    };

    module = {
      description = "Mado (窓) — GPU-rendered terminal emulator";
      hmNamespace = "blackmatter.components";
      withShikumiConfig = true;

      # Desktop GUI-app install (substrate mkModuleTrio appBundle).
      # A consumer who sets `blackmatter.components.mado.installApp = true`
      # gets a real Mado.app in ~/Applications (macOS — Spotlight /
      # Launchpad / Dock) or a Mado .desktop entry + icon under
      # ~/.local/share (Linux — application menu). The .app is built by
      # substrate's mkDarwinAppBundle (SVG → .icns); we do NOT duplicate
      # any bundle/icon logic here.
      appBundle = {
        appName  = "Mado";
        bundleId = "io.pleme.mado";
        iconSvg  = ./assets/mado-icon.svg;
        desktopCategories = "System;TerminalEmulator;";
      };

      shikumiTypedGroups = {
        window = {
          width   = { type = "int"; default = 1280; description = "Window width in pixels."; };
          height  = { type = "int"; default = 800;  description = "Window height in pixels."; };
          padding = { type = "int"; default = 8;    description = "Window padding in pixels."; };
        };

        shell = {
          command = {
            type        = "nullOrStr";
            # Fleet directive (mado → frostmourne stack, 2026-05-21):
            # frostmourne is the official mado default shell. Ships
            # skim + atuin + Ctrl-R history picker + 100M scrollback.
            # Operators who want $SHELL fallback set this to null.
            default     = "frostmourne";
            description = "Shell command to run (default: frostmourne; set null for $SHELL fallback).";
          };
        };

        cursor = {
          style = {
            type        = "enum";
            values      = [ "block" "bar" "underline" ];
            default     = "block";
            description = "Cursor style.";
          };
          blink         = { type = "bool"; default = true; description = "Whether the cursor blinks."; };
          blink_rate_ms = { type = "int";  default = 500;  description = "Cursor blink rate in milliseconds."; };
        };

        behavior = {
          # Deliberately tighter than the Rust prescribed default
          # (usize::MAX): the fleet-rendered YAML bounds scrollback
          # RAM; operators override per-host.
          scrollback_lines = { type = "int";  default = 10000; description = "Number of scrollback lines retained."; };
          # copy_on_select mirrors the Rust-side
          # default_copy_on_select() = true — the muscle-memory
          # contract (operator directive 2026-06-11): a highlight
          # goes straight to the clipboard, no extra chord.
          copy_on_select   = { type = "bool"; default = true; description = "Copy selection to clipboard automatically."; };
        };

        appearance = {
          background = { type = "str";   default = "#2e3440"; description = "Background color (hex)."; };
          foreground = { type = "str";   default = "#eceff4"; description = "Foreground color (hex)."; };
          opacity    = { type = "float"; default = 1.0;       description = "Window opacity (0.0-1.0)."; };
        };

        # Tear-multiplexer integration — pleme.terminal aggregator
        # writes through this group. Lands at
        # `blackmatter.components.mado.tear.*` in Nix and
        # `tear: { … }` in ~/.config/mado/mado.yaml.
        tear = {
          mode = {
            type    = "enum";
            values  = [ "auto" "always" "never" "attach" ];
            default = "auto";
            description = "Tear attachment policy.";
          };
          socket = {
            type = "nullOrStr";
            default = null;
            description = "Daemon socket path. null = default_socket_path().";
          };
          auto_spawn = {
            type = "bool";
            default = true;
            description = "Spawn `tear daemon` on demand when no daemon answers.";
          };
          spawn_wait_ms = {
            type = "int";
            default = 2000;
            description = "Milliseconds to wait for the auto-spawned daemon to bind.";
          };
          session_switching = {
            type = "bool";
            default = true;
            description = "Runtime single-pane re-attach: the Ctrl-S session switcher and the switch_session MCP tool actually switch the displayed pane. Defaults on; set false for the legacy one-shot binding.";
          };
        };

        # Performance / pacing — null fields defer to garasu::adaptive.
        performance = {
          vsync = { type = "bool"; default = true; description = "Enable vsync for smoother rendering."; };
          target_fps = {
            type = "nullOrInt";
            default = null;
            description = "Explicit fps target. null = adaptive detected refresh.";
          };
          fps_cap = {
            type = "nullOrInt";
            default = null;
            description = "Upper bound on the adaptive recommendation.";
          };
          battery_fps_cap = {
            type = "nullOrInt";
            default = null;
            description = "Upper bound when on battery.";
          };
        };
      };

      # Top-level font_family / font_size live outside any typed
      # group in the legacy module — keep them as bespoke options
      # so the YAML round-trips byte-identical. Function-form
      # extraHmOptions receives `lib` from substrate so consumers
      # don't need to declare nixpkgs as a flake input.
      extraHmOptions = lib: {
        fontFamily = lib.mkOption {
          type = lib.types.nullOr lib.types.str;
          default = null;
          description = "Primary monospace family. null = ishou::fleet-fonts.primary.name.";
        };
        fontItalic = lib.mkOption {
          type = lib.types.nullOr lib.types.str;
          default = null;
          description = "Italic-face family. null = ishou::fleet-fonts.italic.name.";
        };
        fontSize = lib.mkOption {
          type = lib.types.nullOr lib.types.float;
          default = null;
          description = "Font size in pixels.";
        };
        extraSettings = lib.mkOption {
          type = lib.types.attrs;
          default = { };
          description = "Additional raw settings merged on top of the typed YAML.";
        };
      };

      # Merge font fields into the YAML payload + install canonical
      # fleet font packages via home.packages. Defaults sourced from
      # ishou::fleet-fonts; installing the nixpkgs font packages is
      # load-bearing (glyphon's cosmic-text fontdb falls back to a
      # different metric otherwise, the 2026-05-13 'gap between
      # every character' bug).
      extraHmConfigFn = { cfg, pkgs, lib, ... }:
        let
          fleetFonts = import "${ishou.packages.${pkgs.stdenv.hostPlatform.system}.fleet-fonts}"
            { inherit pkgs; };
          resolvedFontFamily = if cfg.fontFamily != null
            then cfg.fontFamily
            else fleetFonts.primary.name;
          resolvedFontItalic = if cfg.fontItalic != null
            then cfg.fontItalic
            else fleetFonts.italic.name;
          fontExtras = {
            font_family = resolvedFontFamily;
            font_italic = resolvedFontItalic;
          } // (if cfg.fontSize != null then { font_size = cfg.fontSize; } else { });
          extras = fontExtras // cfg.extraSettings;
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

    # ── release-macos: build + sign + publish the Mado.app DMG. ───────────
    # A maintainer cuts a local DMG (or pushes a GitHub Release) with
    # `nix run .#release-macos`. The whole pipeline is the typed Rust
    # `mado-release` binary (in the isolated `release/` workspace, which
    # consumes the SHARED `mac-app-release` primitive); this flake app is
    # the sanctioned 3-line glue that builds + execs it. NO bundle/icon
    # logic lives here — it is all in `mac-app-release`.
    #
    # nixpkgs + flake-utils come from substrate's own inputs (the consumer
    # never re-declares them — same closure-dedup rule as the rest of the
    # flake). The release tool runs `cargo build --release --bin mado`
    # itself (mado's GPU build needs the host SDK), so the shim only needs
    # cargo/git/gh on PATH plus the Xcode CLT (codesign/hdiutil/iconutil).
    nixpkgs = substrate.inputs.nixpkgs;
    flake-utils = substrate.inputs.flake-utils;

    releaseOutputs = flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs { inherit system; };
        releaseMacos = pkgs.writeShellScriptBin "release-macos" ''
          set -euo pipefail
          export PATH="${pkgs.lib.makeBinPath [ pkgs.git pkgs.gh pkgs.coreutils ]}:$PATH"
          start="''${MADO_DIR:-$PWD}"
          here="$(git -C "$start" rev-parse --show-toplevel 2>/dev/null || echo "$start")"
          export MADO_DIR="$here"
          # The release tooling lives in its own workspace under release/ so it
          # never enters mado's substrate-built root crate.
          cargo build --release --manifest-path "$here/release/Cargo.toml" --bin mado-release
          exec "$here/release/target/release/mado-release" "$@"
        '';
      in {
        packages.release-macos = releaseMacos;
        apps.release-macos = {
          type = "app";
          program = "${releaseMacos}/bin/release-macos";
        };
      });
  in
    # Deep-merge the release outputs onto the substrate surface. recursiveUpdate
    # merges per-system `apps`/`packages` without clobbering substrate's.
    nixpkgs.lib.recursiveUpdate base releaseOutputs;
}
