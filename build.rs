//! Builds the web assets the server serves out of `static/`.
//!
//! The bundle is a build artifact the binary reads off disk at runtime, so
//! producing it here keeps `cargo run` from serving whatever a previous
//! `pnpm build` happened to leave behind.

use std::path::Path;
use std::process::Command;
use std::time::SystemTime;

/// Set to serve whatever is already in `static/`, for builds without a node
/// toolchain to hand.
const SKIP: &str = "LEDECKY_SKIP_ASSETS";

const SOURCES: &[&str] = &["web/src", "web/scripts", "package.json", "pnpm-lock.yaml"];
const OUTPUTS: &[&str] = &["static/app.js", "static/app.css", "static/icons"];

fn main() {
    for path in SOURCES.iter().chain(OUTPUTS) {
        println!("cargo:rerun-if-changed={path}");
    }
    println!("cargo:rerun-if-env-changed={SKIP}");

    // NB: naming the outputs means this script runs on every build, because it
    // rewrites them itself. That is the point: cargo's freshness tracking covers
    // inputs only, so nothing else would notice `static/` — a directory git
    // ignores — being deleted. Running is cheap; `stale` is what guards the work.
    if std::env::var_os(SKIP).is_some() || !stale() {
        return;
    }

    if !Path::new("node_modules").exists() {
        panic!("node_modules is missing — run `pnpm install`, or set {SKIP}=1 to serve static/ as it stands");
    }

    // Captured rather than inherited: cargo only shows a build script's output
    // when it fails, and esbuild puts the reason on stderr.
    let output = Command::new("pnpm")
        .arg("build")
        .output()
        .unwrap_or_else(|err| {
            panic!("could not run `pnpm build`: {err} — set {SKIP}=1 to serve static/ as it stands")
        });

    if !output.status.success() {
        panic!(
            "`pnpm build` failed ({})\n{}{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
}

/// Whether the bundle is missing, or older than something it is built from.
fn stale() -> bool {
    let mut built = None;
    for path in OUTPUTS {
        let Some(at) = modified(Path::new(path)) else {
            return true;
        };
        built = Some(built.map_or(at, |built: SystemTime| built.min(at)));
    }

    let edited = SOURCES.iter().filter_map(|p| modified(Path::new(p))).max();
    match (edited, built) {
        (Some(edited), Some(built)) => edited > built,
        // No sources to compare against is not a state worth guessing about.
        _ => true,
    }
}

/// The most recent mtime at or under `path`, or `None` if it is not there.
fn modified(path: &Path) -> Option<SystemTime> {
    let meta = path.metadata().ok()?;
    if !meta.is_dir() {
        return meta.modified().ok();
    }

    std::fs::read_dir(path)
        .ok()?
        .filter_map(|entry| modified(&entry.ok()?.path()))
        .max()
        .or_else(|| meta.modified().ok())
}
