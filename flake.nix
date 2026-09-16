{
  description = "kanban2 — Claude Code kanban agent manager";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { nixpkgs, flake-utils, ... }:
    flake-utils.lib.eachDefaultSystem (system:
      let pkgs = nixpkgs.legacyPackages.${system};
      in {
        devShells.default = pkgs.mkShell {
          # NB: no rust here on purpose — cargo/rustc come from the system profile.
          packages = [
            pkgs.nodejs_22
            pkgs.pnpm
            pkgs.esbuild
          ];
        };
      });
}
