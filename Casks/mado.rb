# Homebrew cask for Mado (窓) — the GPU terminal emulator.
#
# This is the EASIEST macOS install path:  brew install --cask pleme-io/tap/mado
#
# It tracks the AUTOBUMP rolling release (`macos-arm64-latest`) so a fresh `brew
# upgrade` always pulls the newest merged build. mado is ad-hoc signed (not
# notarized), so the cask clears the quarantine flag on install (the `xattr`
# postflight) — no right-click→Open dance for cask users.
#
# ── Using this cask ─────────────────────────────────────────────────────────
# Until a dedicated `pleme-io/homebrew-tap` repo exists, install straight from
# this repo's raw cask file:
#
#   brew install --cask \
#     https://raw.githubusercontent.com/pleme-io/mado/main/Casks/mado.rb
#
# Once a tap repo is published (copy this file to `pleme-io/homebrew-tap/Casks/`):
#
#   brew tap pleme-io/tap https://github.com/pleme-io/homebrew-tap
#   brew install --cask pleme-io/tap/mado
#
# ── Pinning to a version (instead of the rolling latest) ────────────────────
# Point `url` at a `v<X.Y.Z>` semver release asset and set `version` to X.Y.Z.
# The rolling form below is the frictionless default.
cask "mado" do
  # The rolling AUTOBUMP DMG. `version :latest` + `sha256 :no_check` because the
  # `macos-arm64-latest` prerelease moves on every merge to main.
  version :latest
  sha256 :no_check

  url "https://github.com/pleme-io/mado/releases/download/macos-arm64-latest/Mado-latest-arm64.dmg",
      verified: "github.com/pleme-io/mado/"
  name "Mado"
  desc "GPU-rendered terminal emulator (窓)"
  homepage "https://github.com/pleme-io/mado"

  # Apple Silicon only for now — the autobump leg builds an arm64 DMG. An
  # x86_64 cask variant lands when the Intel autobump leg is wired.
  depends_on arch: :arm64

  app "Mado.app"

  # Ad-hoc signed, not notarized: clear quarantine so first launch just works.
  postflight do
    system_command "/usr/bin/xattr",
                   args: ["-dr", "com.apple.quarantine", "#{appdir}/Mado.app"],
                   sudo: false
  end

  zap trash: [
    "~/.config/mado",
    "~/Library/Saved Application State/io.pleme.mado.savedState",
  ]
end
