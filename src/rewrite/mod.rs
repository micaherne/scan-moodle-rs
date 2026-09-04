//! Rewrite-Moodle feature, gated behind the `rewrite` cargo feature.
//!
//! See `REWRITE_SPEC.md` at the repository root for the process this implements.

mod apply;
mod decide;
mod output;
mod patches;
mod splice;

use std::collections::HashSet;
use std::path::Path;
use std::process::ExitCode;

use crate::moodle;
use crate::moodle::components::ComponentDiscovery;
use crate::moodle::entrypoints::{self, FileClassification};
use crate::moodle::scan::{Scan, categorise_all};

/// What running the whole rewrite process (see `REWRITE_SPEC.md`) produced, for a caller that
/// wants to build on the result rather than just report it — see [`execute`].
pub struct RewriteOutcome {
    pub discovered: ComponentDiscovery,
    pub rewritten: usize,
    /// Every bootstrap-relevant file in `root` (`Cli`, `Other` and `BootstrapDependency` alike),
    /// classified *before* step 3's rewrite ran — see [`execute`] for why that ordering, rather
    /// than the rewritten codebase, is what a caller wanting accurate classifications should use.
    pub entry_points: Vec<FileClassification>,
}

/// Runs the whole rewrite process against `root` (see `REWRITE_SPEC.md`): applies the embedded
/// patches, scans the patched codebase, rewrites every eligible path reference in place, re-scans
/// to capture the result, and — if `output_dir` is given — writes the before/after path scans out
/// as CSV. The "before" CSV doubles as step 3's audit trail: it carries a `rewritten_to` column,
/// and is written row by row as step 3 decides each reference, rather than assembled in memory
/// and written out afterwards. `None` on failure (an error has already been printed).
///
/// Entry-point/bootstrap classification happens on the *patched-but-not-yet-rewritten* scan, in
/// step 2, before step 3 turns other files' requires into `\core\component::component_path(...)`
/// calls that the same scanner used for classification cannot see. A require this project can
/// actually trace is either a literal/`$CFG->dirroot`-rooted path (recognised whether or not it's
/// been patched) or was already unrecognisable before patching too (the embedded patches that
/// touch a require/include at all only ever replace a *dynamically*-computed plugin path, e.g.
/// `$CFG->dirroot . '/mod/' . $modname . '/lib.php'`, which the scanner could never resolve to a
/// specific file to begin with) — so classifying here, rather than after step 3, loses nothing.
pub fn execute(root: &Path, output_dir: Option<&Path>) -> Option<RewriteOutcome> {
    if !root.is_dir() {
        eprintln!("error: {} is not a directory", root.display());
        return None;
    }

    // Step 1: apply the embedded patches.
    let patches = match patches::all() {
        Ok(patches) => patches,
        Err(err) => {
            eprintln!("error: failed to read embedded patches archive: {err}");
            return None;
        }
    };
    if let Err(err) = apply::apply_all(root, &patches) {
        eprintln!("error: {err}");
        return None;
    }
    eprintln!("Applied {} patches to {}", patches.len(), root.display());

    // Step 2: scan the (now patched) codebase.
    let before_scan = match Scan::run(root) {
        Ok(scan) => scan,
        Err(err) => {
            eprintln!("error: failed to scan {}: {err}", root.display());
            return None;
        }
    };
    let dirroot = moodle::dirroot_prefix(root).trim_end_matches('/');
    let before_references = categorise_all(&before_scan, dirroot);
    // Every bootstrap-relevant file, `Cli`/`Other`/`BootstrapDependency` alike — see
    // REWRITE_SPEC.md, step 2.
    let entry_points = entrypoints::classify(&before_scan.files, &before_scan.notation);
    let entry_point_files: HashSet<String> = entry_points
        .iter()
        .map(|classification| classification.file.clone())
        .collect();

    // Step 3: rewrite the eligible path references, on disk — writing the "before"/audit CSV as
    // we go, if requested, so it never needs to be held in memory as a whole.
    let before_path = output_dir.map(|dir| dir.join("before.csv"));
    let mut audit = match before_path.as_deref() {
        Some(path) => match output::AuditWriter::create(path) {
            Ok(writer) => Some(writer),
            Err(err) => {
                eprintln!("error: failed to write {}: {err}", path.display());
                return None;
            }
        },
        None => None,
    };
    let rewritten =
        match splice::apply(root, &before_references, &entry_point_files, audit.as_mut()) {
            Ok(rewritten) => rewritten,
            Err(err) => {
                eprintln!(
                    "error: failed to rewrite files under {}: {err}",
                    root.display()
                );
                return None;
            }
        };
    if let (Some(audit), Some(before_path)) = (audit, &before_path) {
        if let Err(err) = audit.finish() {
            eprintln!("error: failed to write {}: {err}", before_path.display());
            return None;
        }
        eprintln!("Wrote {}", before_path.display());
    }
    eprintln!(
        "Rewrote {rewritten} path reference{}",
        if rewritten == 1 { "" } else { "s" }
    );

    // Step 4: re-scan the (now rewritten) codebase to capture the "after" state.
    let after_scan = match Scan::run(root) {
        Ok(scan) => scan,
        Err(err) => {
            eprintln!("error: failed to re-scan {}: {err}", root.display());
            return None;
        }
    };
    let after_references = categorise_all(&after_scan, dirroot);

    // Step 5: write the "after" path scan out as CSV, if requested ("before" was already written
    // during step 3, above).
    if let Some(output_dir) = output_dir {
        let after_path = output_dir.join("after.csv");
        if let Err(err) = output::write_csv(&after_path, &after_references, &entry_point_files) {
            eprintln!("error: failed to write {}: {err}", after_path.display());
            return None;
        }
        eprintln!("Wrote {}", after_path.display());
    }

    Some(RewriteOutcome {
        discovered: before_scan.discovered,
        rewritten,
        entry_points,
    })
}

/// The `rewrite-moodle` CLI command: runs [`execute`] and reduces its result to an [`ExitCode`].
pub fn run(root: &Path, output_dir: Option<&Path>) -> ExitCode {
    match execute(root, output_dir) {
        Some(_) => ExitCode::SUCCESS,
        None => ExitCode::FAILURE,
    }
}
