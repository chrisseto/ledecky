{
  description = "ledecky — Claude Code kanban agent manager";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, flake-utils, ... }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs { inherit system; config.allowUnfree = true; };
        ledecky = pkgs.callPackage ./nix/package.nix { };
        # Chromium only: webkit currently fails to build in unstable, and firefox
        # is a large download this suite never opens.
        browsers = pkgs.playwright-driver.browsers.override {
          withFirefox = false;
          withWebkit = false;
        };
        # Without this the container has no monospace font at all, so `monospace`
        # resolves to a proportional face and the terminal renders with visible
        # gaps between glyphs — enough to make a screenshot review misleading.
        fonts = pkgs.makeFontsConf {
          fontDirectories = [ pkgs.dejavu_fonts pkgs.liberation_ttf pkgs.jetbrains-mono ];
        };
      in {
        packages.default = ledecky;

        devShells.default = pkgs.mkShell {
          # What the package builds and runs with comes from the package, so the
          # two cannot drift; the rest is what development adds on top.
          #
          # NB: no rust here on purpose — cargo/rustc come from the system profile.
          packages = ledecky.assetInputs ++ ledecky.runtimeInputs ++ [
            pkgs.claude-code
            pkgs.perl # Also used by agents
            pkgs.python3 # Used by agents
            pkgs.sccache # Shared cache for rust
            pkgs.sqlite # For debugging, if need be.
          ];

          RUSTC_WRAPPER="sccache";
          # Playwright's own browser download produces binaries that will not run
          # on NixOS, so take them from the store instead. The npm
          # `@playwright/test` version must match `playwright-driver`.
          PLAYWRIGHT_BROWSERS_PATH = browsers;
          PLAYWRIGHT_SKIP_BROWSER_DOWNLOAD = "1";
          PLAYWRIGHT_SKIP_VALIDATE_HOST_REQUIREMENTS = "true";
          PLAYWRIGHT_VERSION = pkgs.playwright-driver.version;
          FONTCONFIG_FILE = fonts;
        };
      })
    # System-independent, so outside `eachDefaultSystem`.
    // {
      overlays.default = final: _prev: {
        ledecky = final.callPackage ./nix/package.nix { };
      };

      homeManagerModules.default = import ./nix/hm-module.nix { inherit self; };
    };
}
