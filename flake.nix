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

      # Color-emoji on every host. mado renders the shell prompt's
      # emoji (seki's 🦀 Rust segment, session-name presets, etc.)
      # through garasu's cosmic-text stack. On macOS the system Apple
      # Color Emoji is found by garasu's font scan; on a Linux/Nix host
      # there is no system emoji font, so the prompt's emoji would
      # resolve to .notdef (a red tofu box — the 2026-06-23 report).
      # Declaring the font as a runtime dep installs it onto the user's
      # font path (`~/.nix-profile/share/fonts`), which garasu scans —
      # so color emoji render fleet-wide. (garasu also self-heals via
      # build-time embed + runtime discovery; this is the clean,
      # zero-bloat deployment lever.)
      extraPackages = [ "noto-fonts-color-emoji" ];

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
          # Mirrors the Rust-side default_mouse_hide() = true. Hides the
          # mouse pointer while typing; mado restores it on a mouse MOVE.
          # Operators with a stationary pointer over the window (the
          # "mado makes my mouse vanish" report) set this false.
          mouse_hide_while_typing = { type = "bool"; default = true; description = "Hide the mouse pointer while typing (restored on mouse movement)."; };
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
          # runtime + auto_attach were previously authored only via
          # extraSettings (raw attrs). Two modules both defining
          # `extraSettings.tear` shallow-merge-clobbered each other,
          # so the fleet's auto_attach silently rendered ABSENT and
          # the Rust default (Off) won — cd-auto-switch dead in
          # production. Typed-group fields render unconditionally
          # into the YAML, so the defaults below ARE the fleet
          # defaults (same convention as behavior.copy_on_select).
          runtime = {
            type    = "enum";
            values  = [ "embedded" "daemon" ];
            default = "embedded";
            description = "Tear runtime: embedded = in-process tear_core (no IPC, ghostty-class latency; the default); daemon = Unix-socket tear daemon (multi-attach: ayatsuri overlay / namimado / ssh-mux sharing sessions).";
          };
          auto_attach = {
            type    = "enum";
            values  = [ "off" "auto_switch" "suggest" ];
            default = "auto_switch";
            description = "Auto-attach-on-cd (praca automation): when the displayed session cds into a different project — off = never move the pane (Rust default); auto_switch = switch to that project's session, spawning one if needed (fleet default); suggest = surface the decision, never move the pane. Requires session_switching = true.";
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

        # Floating & snapping browser surfaces (theory/BROWSER.md).
        # Lands at blackmatter.components.mado.browser.* in Nix and
        # `browser: { … }` in ~/.config/mado/mado.yaml — the typed mirror of
        # config.rs::BrowserConfig (keep the two in lockstep or a rendered
        # key fails deny_unknown_fields at load).
        browser = {
          enabled = {
            type = "bool";
            default = true;
            description = "Enable floating & snapping browser surfaces. false disables the whole subsystem.";
          };
          default_width = {
            type = "int";
            default = 900;
            description = "Default float width in logical pixels for a new surface.";
          };
          default_height = {
            type = "int";
            default = 640;
            description = "Default float height in logical pixels for a new surface.";
          };
          default_opacity = {
            type = "float";
            default = 0.98;
            description = "Default float opacity (0.0-1.0) — the browser quad's alpha over the grid.";
          };
          snap_enabled = {
            type = "bool";
            default = true;
            description = "Enable edge/corner snapping during a window drag.";
          };
          snap_band = {
            type = "float";
            default = 0.06;
            description = "Snap activation-band thickness as a fraction of the viewport dimension (clamped 0.0-0.5).";
          };
          restore_on_close = {
            type = "bool";
            default = true;
            description = "Restore last float geometry + URL when a surface is re-opened.";
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
          description = "Additional raw settings merged on top of the typed YAML. Do NOT set `suggestions` here when the typed `suggestions` option is used — extraSettings shallow-merges last and would clobber the typed render wholesale (the cross-module tear.* clobber class, 2026-07-02).";
        };

        # ── M2 typed surface for the Ctrl-S suggestion stream ──────────
        # Rich schema (list-of-submodule sources) — too structured for
        # shikumiTypedGroups' flat verbatim fields, so it rides the
        # extraHmOptions idiom (the blackmatter-fortinet precedent).
        # Every scalar is nullable and renders ONLY when set, and the
        # whole `suggestions:` key renders only when something is set —
        # consumers that don't touch this option get a byte-identical
        # YAML. `safra` is deliberately NOT typed yet (no fleet consumer
        # arms it; extraSettings still carries it raw if needed).
        suggestions = lib.mkOption {
          default = { };
          description = "Typed config for the Ctrl-S suggestion stream (the living board). Rendered under `suggestions:` in mado.yaml; per-kind `sources` entries MERGE over mado's prescribed arm-list (SuggestionsConfig::effective_sources) — a params-only entry never disarms the rest.";
          type = lib.types.submodule {
            options = {
              enabled = lib.mkOption {
                type = lib.types.nullOr lib.types.bool;
                default = null;
                description = "Master switch. null = mado's default (on). With engine hot-reload, flipping this at runtime parks/revives the engine without a restart.";
              };
              persist = lib.mkOption {
                type = lib.types.nullOr lib.types.bool;
                default = null;
                description = "Persist the board snapshot across restarts. null = mado's default.";
              };
              ttl_secs = lib.mkOption {
                type = lib.types.nullOr lib.types.int;
                default = null;
                description = "Global row TTL floor in seconds. null = mado's default.";
              };
              max_entries = lib.mkOption {
                type = lib.types.nullOr lib.types.int;
                default = null;
                description = "Hard cap on stored rows (rank-ordered GC). null = mado's default.";
              };
              persist_debounce_secs = lib.mkOption {
                type = lib.types.nullOr lib.types.int;
                default = null;
                description = "Snapshot write coalescing cadence. null = mado's default.";
              };
              default_enabled = lib.mkOption {
                type = lib.types.nullOr lib.types.bool;
                default = null;
                description = "Whether kinds absent from `sources` are armed. null = mado's default.";
              };
              sources_replace = lib.mkOption {
                type = lib.types.nullOr lib.types.bool;
                default = null;
                description = "Escape hatch: true = `sources` REPLACES the prescribed arm-list instead of merging over it.";
              };
              sources = lib.mkOption {
                default = [ ];
                description = "Per-kind overrides (credentials, cadence, params). Merged over the prescribed arm-list by kind.";
                type = lib.types.listOf (lib.types.submodule {
                  options = {
                    kind = lib.mkOption {
                      type = lib.types.str;
                      description = "Source kind slug (e.g. jira-assigned, flux-failing, github-actions-failing).";
                    };
                    enabled = lib.mkOption {
                      type = lib.types.nullOr lib.types.bool;
                      default = null;
                      description = "Arm/disarm this kind. null = omit (mado's per-kind default).";
                    };
                    interval_secs = lib.mkOption {
                      type = lib.types.nullOr lib.types.int;
                      default = null;
                      description = "Poll cadence override in seconds. null = omit.";
                    };
                    max_items = lib.mkOption {
                      type = lib.types.nullOr lib.types.int;
                      default = null;
                      description = "Row budget override for this kind. null = omit.";
                    };
                    params = lib.mkOption {
                      type = lib.types.attrs;
                      default = { };
                      description = "Kind-specific params (site, base_url, secret paths, context, repos, ...). Rendered only when non-empty.";
                    };
                  };
                });
              };
            };
          };
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
          # Typed suggestions render: nullable scalars appear only when
          # set; a source entry is {kind} plus set-only optionals; the
          # whole `suggestions` key appears only when non-empty — a
          # consumer that never touches the option renders byte-identical
          # YAML. extraSettings still merges LAST (raw wins), so setting
          # suggestions in BOTH places clobbers — documented on the option.
          scalarOpt = name: v: lib.optionalAttrs (v != null) { ${name} = v; };
          renderSource = src:
            { kind = src.kind; }
            // scalarOpt "enabled" src.enabled
            // scalarOpt "interval_secs" src.interval_secs
            // scalarOpt "max_items" src.max_items
            // lib.optionalAttrs (src.params != { }) { params = src.params; };
          suggestionsBody =
            scalarOpt "enabled" cfg.suggestions.enabled
            // scalarOpt "persist" cfg.suggestions.persist
            // scalarOpt "ttl_secs" cfg.suggestions.ttl_secs
            // scalarOpt "max_entries" cfg.suggestions.max_entries
            // scalarOpt "persist_debounce_secs" cfg.suggestions.persist_debounce_secs
            // scalarOpt "default_enabled" cfg.suggestions.default_enabled
            // scalarOpt "sources_replace" cfg.suggestions.sources_replace
            // lib.optionalAttrs (cfg.suggestions.sources != [ ]) {
              sources = map renderSource cfg.suggestions.sources;
            };
          suggestionsExtras =
            lib.optionalAttrs (suggestionsBody != { }) { suggestions = suggestionsBody; };
          extras = fontExtras // suggestionsExtras // cfg.extraSettings;
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

    # ── Linux GUI runtime wrap ────────────────────────────────────────────
    # Two problems compose on Linux, and only fixing BOTH gets a working GUI:
    #
    # 1. Static-musl target vs GUI app.
    #    substrate.rust.tool's `packages.default` on Linux is built for
    #    `x86_64-unknown-linux-musl` via pkgsStatic — a fully static-pie
    #    binary with NO INTERP segment, NO NEEDED entries and empty RUNPATH.
    #    That's fine for a deploy CLI, but wrong for a GUI app: winit/wgpu
    #    dlopen `libwayland-client.so.0`, `libxkbcommon.so.0`, `libGL.so.1`,
    #    `libvulkan.so.1` and the Xlib/XCB families at RUN time, and a
    #    static-musl process has no working dlopen path for external
    #    libraries — LD_LIBRARY_PATH is a glibc-ld.so concept the musl
    #    static loader does not honor. Substrate already exposes the glibc
    #    variant as `packages.<system>.host-tool` (linked against regular
    #    nixpkgs, not pkgsStatic); we swap it in as the Linux default.
    #
    # 2. Runtime library path.
    #    Even on glibc, the nix binary has no RPATH for wayland/GL/vulkan
    #    (they're dlopen'd, not linked), so on NixOS they aren't findable
    #    via the standard dynamic linker without help. Wrap with
    #    `LD_LIBRARY_PATH` set to a directory holding BOTH wayland AND X11
    #    loaders — winit auto-detects at runtime (prefers Wayland when
    #    `WAYLAND_DISPLAY` is set, falls back to X11), so one binary works
    #    in every Linux session type.
    #
    # Deliberately a wrapper (symlinkJoin + makeWrapper) rather than
    # rpath-stamping via autoPatchelfHook + runtimeDependencies: the
    # substrate.rust.tool crateOverrides surface does not expose per-system
    # pkgs to the consumer, so plumbing patchelf cleanly would mean
    # extending substrate. Wrapping stays a one-repo change.
    #
    # macOS/darwin: no-op — the wgpu Metal backend needs none of this.
    linuxGuiOutputs = flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs { inherit system; };
      in
        if !pkgs.stdenv.hostPlatform.isLinux then {} else
        let
          # Glibc-linked native binary (substrate's `host-tool` variant),
          # NOT `packages.default` — the latter is pkgsStatic musl and
          # cannot dlopen the GUI runtime libs at all.
          baseBin = base.packages.${system}.host-tool;
          # Wayland + X11 + GL + Vulkan + font loaders. Names track
          # substrate/lib/build/rust/eframe.nix::linuxRuntimeLibs (the
          # canonical fleet list); keep the two in step if that grows.
          guiLibs = with pkgs; [
            wayland
            libxkbcommon
            libGL
            vulkan-loader
            libx11
            libxcursor
            libxi
            libxrandr
            libxcb
            libxcb-util
            fontconfig
            freetype
            expat
          ];
          wrapped = pkgs.symlinkJoin {
            name = "mado-linux-gui";
            paths = [ baseBin ];
            nativeBuildInputs = [ pkgs.makeWrapper ];
            postBuild = ''
              for f in $out/bin/*; do
                [ -L "$f" ] || continue
                wrapProgram "$f" \
                  --prefix LD_LIBRARY_PATH : ${pkgs.lib.makeLibraryPath guiLibs}
              done
            '';
            meta = (baseBin.meta or {}) // {
              mainProgram = baseBin.meta.mainProgram or "mado";
            };
          };
        in {
          packages.default = wrapped;
          packages.mado = wrapped;
          apps.default = {
            type = "app";
            program = "${wrapped}/bin/mado";
          };
        });

    # The HM/NixOS/Darwin module trio reads `pkgs.mado` through
    # `overlays.default`; re-emit that overlay so a Linux consumer's
    # `pkgs.mado` (and therefore the HM module's `services.mado.package`
    # default) resolves to the wrapped binary.
    linuxGuiOverlay = {
      overlays.default = final: prev:
        let baseAttrs = base.overlays.default final prev;
        in baseAttrs
           // (if final.stdenv.hostPlatform.isLinux
               then { mado = linuxGuiOutputs.packages.${final.stdenv.hostPlatform.system}.default; }
               else {});
    };
  in
    # Deep-merge in order: base ← release apps ← Linux GUI wrap ← overlay
    # re-emit. recursiveUpdate merges per-system `apps`/`packages` without
    # clobbering substrate's; the overlay's outer `overlays.default` key
    # is *replaced* wholesale (correct — the new function composes the
    # base overlay by calling it).
    nixpkgs.lib.recursiveUpdate
      (nixpkgs.lib.recursiveUpdate
        (nixpkgs.lib.recursiveUpdate base releaseOutputs)
        linuxGuiOutputs)
      linuxGuiOverlay;
}
