//! The `extract-packages` command, only available when the `rewrite` feature is enabled.
//!
//! Takes a *vanilla* Moodle codebase, runs the whole rewrite process on it in place (see
//! [`crate::rewrite::execute`]), then copies the result into a directory of self-contained
//! Composer packages, one per component plus a `moodle-root` catch-all, each with its own git
//! history and a `composer.json` carrying the metadata a future loader would need.

mod composer_json;
mod copy;
mod git;

use std::path::Path;
use std::process::ExitCode;

use rayon::prelude::*;

use crate::moodle::resolver::{ComponentResolver, ROOT_COMPONENT};
use crate::rewrite;

/// Runs the whole `extract-packages` process. `root` is mutated in place by the rewrite step
/// (exactly as running `rewrite-moodle` on it directly would) before anything is copied out of it.
pub fn run(root: &Path, dest: &Path, clean: bool, output_dir: Option<&Path>) -> ExitCode {
    // Writing into the codebase being walked would feed the copies back into the walk, so the run
    // would never finish. Compare canonical paths, since either side may be relative or symlinked.
    if let (Ok(canonical_root), Ok(canonical_dest)) = (root.canonicalize(), dest.canonicalize())
        && canonical_dest.starts_with(&canonical_root)
    {
        eprintln!(
            "error: destination {} is inside the codebase being extracted from",
            dest.display()
        );
        return ExitCode::FAILURE;
    }

    if clean
        && dest.exists()
        && let Err(err) = std::fs::remove_dir_all(dest)
    {
        eprintln!("error: failed to clean {}: {err}", dest.display());
        return ExitCode::FAILURE;
    }
    if let Err(err) = std::fs::create_dir_all(dest) {
        eprintln!("error: failed to create {}: {err}", dest.display());
        return ExitCode::FAILURE;
    }

    let Some(outcome) = rewrite::execute(root, output_dir) else {
        return ExitCode::FAILURE;
    };
    let resolver = ComponentResolver::new(&outcome.discovered);

    eprintln!("Copying {} into {}...", root.display(), dest.display());
    let copied = copy::copy_all(root, dest, &resolver);
    eprintln!(
        "Copied {} files ({:.1} MiB) into {} component directories in {:.2?} ({:.2?} walking)",
        copied.copied,
        copied.bytes as f64 / (1024.0 * 1024.0),
        copied.components_used.len(),
        copied.elapsed,
        copied.walked,
    );
    if !copied.unresolved.is_empty() {
        eprintln!(
            "{} files did not resolve to a component and went to moodle-root:",
            copied.unresolved.len()
        );
        for path in copied.unresolved.iter().take(10) {
            eprintln!("  {path}");
        }
        if copied.unresolved.len() > 10 {
            eprintln!("  ... and {} more", copied.unresolved.len() - 10);
        }
    }
    if copied.failed > 0 {
        eprintln!("Failed to copy {} files", copied.failed);
        return ExitCode::FAILURE;
    }

    let mut entry_points_by_component =
        composer_json::group_by_component(&outcome.entry_points, &resolver);

    let mut write_failures = Vec::new();
    for component in &copied.components_used {
        let package_dir = dest.join(copy::component_dir_name(component));
        let result = if component == ROOT_COMPONENT {
            composer_json::write_root(&package_dir)
        } else {
            let entry_points = entry_points_by_component.remove(component);
            composer_json::write(&package_dir, component, entry_points)
        };
        if let Err(err) = result {
            write_failures.push((component.clone(), err.to_string()));
        }
    }
    for (component, err) in &write_failures {
        eprintln!("error: failed to write composer.json for {component}: {err}");
    }
    if !write_failures.is_empty() {
        return ExitCode::FAILURE;
    }

    eprintln!(
        "Wrote composer.json for {} packages",
        copied.components_used.len()
    );

    eprintln!("Committing each package to git...");
    let git_failures: Vec<(String, String)> = copied
        .components_used
        .par_iter()
        .filter_map(|component| {
            let package_dir = dest.join(copy::component_dir_name(component));
            git::init_and_commit(&package_dir)
                .err()
                .map(|err| (component.clone(), err))
        })
        .collect();
    for (component, err) in &git_failures {
        eprintln!("error: failed to commit {component}'s package: {err}");
    }
    if !git_failures.is_empty() {
        return ExitCode::FAILURE;
    }

    eprintln!(
        "Committed {} packages to their own 'main' branch",
        copied.components_used.len()
    );

    ExitCode::SUCCESS
}
