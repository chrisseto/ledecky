{ lib
, rustPlatform
, pnpm
, pnpmConfigHook
, fetchPnpmDeps
, nodejs_22
, esbuild
, makeWrapper
, coreutils
, delta
, git
}:

let
  pname = "ledecky";
  version = (lib.importTOML ../Cargo.toml).package.version;

  # NB: an explicit fileset — `static/`, `node_modules/`, `target/` and
  # `.direnv/` are all untracked and would otherwise land in the hash.
  src = lib.fileset.toSource {
    root = ../.;
    fileset = lib.fileset.unions (map (p: ../. + "/${p}") [
      "Cargo.toml"
      "Cargo.lock"
      "Rocket.toml"
      "build.rs"
      "migrations"
      "package.json"
      "pnpm-lock.yaml"
      "src"
      "templates"
      "web"
    ]);
  };

  # The toolchain `build.rs` reaches for. Named so the dev shell can take the
  # same one rather than keeping its own list in step with this.
  assetInputs = [ nodejs_22 pnpm esbuild ];

  # What the server shells out to, with no fallback for any of them: `git`
  # drives the worktrees and snapshots, `delta` renders every diff, and `kill`
  # retires an orphaned agent. The wrapper bakes these in, and the dev shell
  # needs them for the same reason `cargo run` does.
  #
  # These go *ahead* of whatever PATH the server inherits. `review::ansi` reads
  # delta's output as a vocabulary — sentinel backgrounds and the `Nord`
  # palette, which `palette_is_complete` asserts on — so the diff pane is only
  # correct against the delta this was built with, not whichever one a session
  # happens to carry.
  runtimeInputs = [ git delta coreutils ];

  # Everything the bundle reads is a dependency, so the only thing `--prod`
  # leaves out is `@playwright/test` — which the end-to-end suite needs and this
  # does not.
  pnpmInstallFlags = [ "--prod" ];
in
rustPlatform.buildRustPackage {
  inherit pname version src pnpmInstallFlags;

  # Vendoring from the lockfile leaves no second hash to keep in step.
  cargoLock.lockFile = ../Cargo.lock;

  # Regenerate whenever `pnpm-lock.yaml` changes: build, and paste back the hash
  # the mismatch prints.
  pnpmDeps = fetchPnpmDeps {
    inherit pname version src pnpmInstallFlags;
    fetcherVersion = 4;
    hash = "sha256-AgCEqQxVclyHvKFmZyWPNFYmCaLfGnHTQT99FS72XU0=";
  };

  # `pnpmConfigHook` lands `node_modules`, and `build.rs` bundles out of it into
  # `static/` — which it must, before rustc runs, because `assets::ASSETS` is an
  # `include_dir!` over that directory.
  #
  # NB: esbuild rides in `assetInputs` rather than `package.json` — the dev
  # shell has always supplied it, and so must this.
  nativeBuildInputs = assetInputs ++ [ pnpmConfigHook makeWrapper ];

  # `cargo test` shells out to git and writes scratch repositories; it belongs
  # in the dev shell rather than in the sandbox.
  doCheck = false;

  # `static/icons` and `templates/` are read relative to the working directory
  # at runtime, as is `Rocket.toml`, so the wrapper supplies one. Only the icons
  # go with them: the rest of the bundle is already embedded, and a second copy
  # on disk would be one nothing reads.
  postInstall = ''
    mkdir -p $out/share/ledecky/static
    cp -r static/icons $out/share/ledecky/static/icons
    cp -r templates $out/share/ledecky/templates
    cp Rocket.toml $out/share/ledecky/Rocket.toml

    wrapProgram $out/bin/ledecky \
      --chdir $out/share/ledecky \
      --prefix PATH : ${lib.makeBinPath runtimeInputs}
  '';

  passthru = { inherit assetInputs runtimeInputs; };

  meta = {
    description = "A local kanban board for Claude Code agents";
    homepage = "https://github.com/chrisseto/ledecky";
    mainProgram = "ledecky";
    platforms = lib.platforms.unix;
  };
}
