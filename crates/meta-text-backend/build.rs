/*!
 * build.rs
 *
 * Links the system `libtoxcore` when the `tox-protocol` feature is enabled.
 *
 * The Tox transport talks to the same C library the original `metaText`
 * (`metaText.c`) uses, through hand written FFI bindings in `src/tox.rs`. This
 * script locates the shared/static library and, when found, links it and bakes
 * the runtime search path into the binary so it starts without
 * `DYLD_LIBRARY_PATH` / `LD_LIBRARY_PATH`.
 *
 * When the library is missing the crate still builds: `src::tox` compiles to a
 * stub whose `start` method reports `ToxError::Unavailable`. That keeps
 * `cargo test --all-features` working on machines (and CI runners) without
 * libtoxcore installed.
 */

use std::path::PathBuf;

/// Names a libtoxcore artifact can have, per platform.
const LIB_NAMES: [&str; 3] = ["libtoxcore.dylib", "libtoxcore.so", "libtoxcore.a"];

fn main() {
    // Declare the custom cfg so `cargo check` does not warn about it.
    println!("cargo::rustc-check-cfg=cfg(toxcore_found)");
    println!("cargo::rerun-if-changed=build.rs");
    println!("cargo::rerun-if-env-changed=TOXCORE_LIB_DIR");

    // Only link when the feature asked for it.
    if std::env::var_os("CARGO_FEATURE_TOX_PROTOCOL").is_none() {
        return;
    }

    match find_libtoxcore() {
        Some(dir) => {
            println!("cargo::rustc-link-search=native={}", dir.display());
            println!("cargo::rustc-link-lib=toxcore");
            // Bake in the runtime path so the executable and test binaries can
            // find the dylib without environment tweaks.
            if cfg!(target_os = "macos") || cfg!(target_os = "linux") {
                println!("cargo::rustc-link-arg=-Wl,-rpath,{}", dir.display());
            }
            println!("cargo::rustc-cfg=toxcore_found");
            println!(
                "cargo::warning=tox-protocol: linking libtoxcore from {}",
                dir.display()
            );
        }
        None => {
            println!(
                "cargo::warning=tox-protocol is enabled but libtoxcore was not found. \
                 The Tox transport compiles to a stub. Install it (macOS: `brew install \
                 toxcore`, Debian/Ubuntu: `apt install libtoxcore-dev`) or point \
                 TOXCORE_LIB_DIR at the directory containing libtoxcore.(dylib|so|a)."
            );
        }
    }
}

/// Look for a libtoxcore artifact in the usual places.
///
/// When `TOXCORE_LIB_DIR` is set it is the *only* directory searched, so an
/// explicit override can also be used to force the stub build
/// (`TOXCORE_LIB_DIR=/nonexistent cargo build --features tox-protocol`).
fn find_libtoxcore() -> Option<PathBuf> {
    let candidates: Vec<PathBuf> = match std::env::var_os("TOXCORE_LIB_DIR") {
        Some(explicit) => vec![PathBuf::from(explicit)],
        None => [
            "/opt/homebrew/lib", // Apple silicon Homebrew
            "/usr/local/lib",    // Intel Homebrew / manual installs
            "/opt/local/lib",    // MacPorts
            "/usr/lib",          // distribution packages
            "/usr/lib/x86_64-linux-gnu",
            "/usr/lib/aarch64-linux-gnu",
            "/usr/lib64",
        ]
        .iter()
        .map(PathBuf::from)
        .collect(),
    };

    candidates
        .into_iter()
        .find(|dir| LIB_NAMES.iter().any(|name| dir.join(name).exists()))
}
