{
  description = "ledecky — Claude Code kanban agent manager";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { nixpkgs, flake-utils, ... }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = nixpkgs.legacyPackages.${system};
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
        devShells.default = pkgs.mkShell {
          # NB: no rust here on purpose — cargo/rustc come from the system profile.
          packages = [
            pkgs.nodejs_22
            pkgs.pnpm
            pkgs.esbuild
            # The diff pane shells out to this; there is no fallback path.
            pkgs.delta
            # `scripts/migrate-data-dir.sh` rewrites the board database.
            pkgs.sqlite
          ];

          # Playwright's own browser download produces binaries that will not run
          # on NixOS, so take them from the store instead. The npm
          # `@playwright/test` version must match `playwright-driver`.
          PLAYWRIGHT_BROWSERS_PATH = browsers;
          PLAYWRIGHT_SKIP_BROWSER_DOWNLOAD = "1";
          PLAYWRIGHT_SKIP_VALIDATE_HOST_REQUIREMENTS = "true";
          PLAYWRIGHT_VERSION = pkgs.playwright-driver.version;
          FONTCONFIG_FILE = fonts;
        };
      });
}
