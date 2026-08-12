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
use crate::moodle::entrypoints;
use crate::moodle::scan::{Scan, categorise_all};

/// Runs the whole rewrite process against `root` (see `REWRITE_SPEC.md`): applies the embedded
/// patches, scans the patched codebase, rewrites every eligible path reference in place, re-scans
/// to capture the result, and — if `output_dir` is given — writes the before/after path scans out
/// as CSV. The "before" CSV doubles as step 3's audit trail: it carries a `rewritten_to` column,
/// and is written row by row as step 3 decides each reference, rather than assembled in memory
/// and written out afterwards.
pub fn run(root: &Path, output_dir: Option<&Path>) -> ExitCode {
    if !root.is_dir() {
        eprintln!("error: {} is not a directory", root.display());
        return ExitCode::FAILURE;
    }

    // Step 1: apply the embedded patches.
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

    // Step 2: scan the (now patched) codebase.
    let before_scan = match Scan::run(root) {
        Ok(scan) => scan,
        Err(err) => {
            eprintln!("error: failed to scan {}: {err}", root.display());
            return ExitCode::FAILURE;
        }
    };
    let dirroot = moodle::dirroot_prefix(root).trim_end_matches('/');
    let before_references = categorise_all(&before_scan, dirroot);
    // The unrestricted entrypoint scan: every bootstrap file, CLI script and page, not just
    // bootstrap files — see REWRITE_SPEC.md, step 2.
    let entry_point_files: HashSet<String> =
        entrypoints::classify(&before_scan.files, &before_scan.notation, false)
            .into_iter()
            .map(|classification| classification.file)
            .collect();

    // Step 3: rewrite the eligible path references, on disk — writing the "before"/audit CSV as
    // we go, if requested, so it never needs to be held in memory as a whole.
    let before_path = output_dir.map(|dir| dir.join("before.csv"));
    let mut audit = match before_path.as_deref() {
        Some(path) => match output::AuditWriter::create(path) {
            Ok(writer) => Some(writer),
            Err(err) => {
                eprintln!("error: failed to write {}: {err}", path.display());
                return ExitCode::FAILURE;
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
                return ExitCode::FAILURE;
            }
        };
    if let (Some(audit), Some(before_path)) = (audit, &before_path) {
        if let Err(err) = audit.finish() {
            eprintln!("error: failed to write {}: {err}", before_path.display());
            return ExitCode::FAILURE;
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
            return ExitCode::FAILURE;
        }
    };
    let after_references = categorise_all(&after_scan, dirroot);

    // Step 5: write the "after" path scan out as CSV, if requested ("before" was already written
    // during step 3, above).
    if let Some(output_dir) = output_dir {
        let after_path = output_dir.join("after.csv");
        if let Err(err) = output::write_csv(&after_path, &after_references, &entry_point_files) {
            eprintln!("error: failed to write {}: {err}", after_path.display());
            return ExitCode::FAILURE;
        }
        eprintln!("Wrote {}", after_path.display());
    }

    ExitCode::SUCCESS
}
