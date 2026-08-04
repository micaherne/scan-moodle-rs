//! Rewrite-Moodle feature, gated behind the `rewrite` cargo feature.
//!
//! See `REWRITE_SPEC.md` at the repository root for the process this implements.

mod apply;
mod patches;

use std::path::Path;
use std::process::ExitCode;

// TEMPORARY: only performs step 1 (applying the embedded patches) and stops there, so that step
// can be tested against a real checkout before steps 2-5 (scanning and rewriting path
// references, see REWRITE_SPEC.md) exist. Remove this note once those steps are wired in here.
pub fn run(root: &Path) -> ExitCode {
    if !root.is_dir() {
        eprintln!("error: {} is not a directory", root.display());
        return ExitCode::FAILURE;
    }

    let patches = match patches::all() {
        Ok(patches) => patches,
        Err(err) => {
            eprintln!("error: failed to read embedded patches archive: {err}");
            return ExitCode::FAILURE;
        }
    };

    if let Err(err) = apply::apply_all(root, &patches) {
        eprintln!("error: {err}");
        return ExitCode::FAILURE;
    }

    eprintln!("Applied {} patches to {}", patches.len(), root.display());
    ExitCode::SUCCESS
}
