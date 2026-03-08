# Mado home-manager module — GPU-rendered terminal emulator
#
# Namespace: blackmatter.components.mado.*
#
# Generates YAML config from typed Nix options, loaded by shikumi at runtime.
# Supports hot-reload via symlink-aware file watching.
#
# Module factory: receives { hmHelpers } from flake.nix, returns HM module.
{ hmHelpers }:
{
  lib,
  config,
  pkgs,
  ...
}:
with lib;
let
  cfg = config.blackmatter.components.mado;

  settingsAttr =
    let
      filterNulls = filterAttrs (_: v: v != null);
    in
    filterNulls {
      font_family = cfg.fontFamily;
      font_size = cfg.fontSize;
      window = filterNulls {
        width = cfg.window.width;
        height = cfg.window.height;
        padding = cfg.window.padding;
      };
      shell = filterNulls {
        command = cfg.shell.command;
      };
      cursor = filterNulls {
        style = cfg.cursor.style;
        blink = cfg.cursor.blink;
        blink_rate_ms = cfg.cursor.blinkRateMs;
      };
      behavior = filterNulls {
        scrollback_lines = cfg.behavior.scrollbackLines;
        copy_on_select = cfg.behavior.copyOnSelect;
      };
    };

  settingsYaml = pkgs.writeText "mado.yaml" (
    builtins.toJSON settingsAttr
  );
in
{
  options.blackmatter.components.mado = {
    enable = mkEnableOption "mado GPU terminal emulator";

    package = mkOption {
      type = types.package;
      default = pkgs.mado or (throw "mado package not found — add mado overlay");
      description = "The mado package to use.";
    };

    fontFamily = mkOption {
      type = types.nullOr types.str;
      default = null;
      description = "Font family for terminal text.";
    };

    fontSize = mkOption {
      type = types.nullOr types.float;
      default = null;
      description = "Font size in pixels.";
    };

    window = {
      width = mkOption {
        type = types.nullOr types.int;
        default = null;
      };
      height = mkOption {
        type = types.nullOr types.int;
        default = null;
      };
      padding = mkOption {
        type = types.nullOr types.int;
        default = null;
      };
    };

    shell = {
      command = mkOption {
        type = types.nullOr types.str;
        default = null;
        description = "Shell command to run (default: user's SHELL).";
      };
    };

    cursor = {
      style = mkOption {
        type = types.nullOr (types.enum [ "block" "bar" "underline" ]);
        default = null;
      };
      blink = mkOption {
        type = types.nullOr types.bool;
        default = null;
      };
      blinkRateMs = mkOption {
        type = types.nullOr types.int;
        default = null;
      };
    };

    behavior = {
      scrollbackLines = mkOption {
        type = types.nullOr types.int;
        default = null;
      };
      copyOnSelect = mkOption {
        type = types.nullOr types.bool;
        default = null;
      };
    };
  };

  config = mkIf cfg.enable {
    home.packages = [ cfg.package ];

    xdg.configFile."mado/mado.yaml" = {
      source = settingsYaml;
    };
  };
}
