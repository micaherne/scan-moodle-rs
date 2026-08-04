//! Scans a Moodle codebase for its components and every path reference `find_paths` can locate
//! across its PHP files — the shared starting point for `find-paths`, `find-entrypoints`, and the
//! rewrite command's scans, so each reads the codebase exactly once, and categorises the result
//! the same way `find-paths --categorise` does.

use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use rayon::prelude::*;
use walkdir::WalkDir;

use super::categorise::{self, PathCategory};
use super::components::{ComponentDiscovery, discover_components};
use super::entrypoints;
use super::resolver::{ComponentResolver, Resolution};
use super::thirdparty;
use crate::path_finder::{PathNotation, PathResult, find_paths};

/// `path`, relative to `root`, as a forward-slash-separated string.
pub fn relative_unix_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .components()
        .map(|component| component.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

/// Every PHP file eligible for path-finding analysis, filtered identically for every command that
/// scans one: third-party vendored code, and anything [`super::is_excluded_from_scan`] rules out
/// (e.g. 'config-dist.php', a template that is never real code).
pub fn php_paths<'a>(
    root: &'a Path,
    thirdparty_locations: &'a HashSet<String>,
) -> impl ParallelIterator<Item = PathBuf> + 'a {
    WalkDir::new(root)
        .into_iter()
        .filter_entry(|entry| {
            !thirdparty::is_thirdparty(thirdparty_locations, &relative_unix_path(root, entry.path()))
        })
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_file())
        .map(|entry| entry.into_path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "php"))
        .filter(move |path| !super::is_excluded_from_scan(&relative_unix_path(root, path)))
        .par_bridge()
}

/// A codebase's components, and every path reference found across its PHP files.
pub struct Scan {
    pub discovered: ComponentDiscovery,
    pub notation: PathNotation,
    pub files: Vec<(String, Vec<PathResult>)>,
    /// How many PHP files could not be read (and so are missing from `files`); a warning is
    /// printed for each as it happens.
    pub failed_reads: usize,
}

impl Scan {
    /// Scans `root`'s PHP files, given its already-discovered components (see
    /// [`discover_components`]) — split out from [`Scan::run`] so a caller that already has a
    /// `ComponentDiscovery` (e.g. to report its own "not a directory"/discovery-failed errors
    /// first) doesn't pay for discovering it twice.
    pub fn from_discovery(root: &Path, discovered: ComponentDiscovery) -> Scan {
        let notation = PathNotation::from_root(root);
        let thirdparty_locations = thirdparty::find_thirdparty_locations(root, &discovered);

        let failed = AtomicUsize::new(0);
        let files: Vec<(String, Vec<PathResult>)> = php_paths(root, &thirdparty_locations)
            .filter_map(|path| {
                let relative = relative_unix_path(root, &path);
                let bytes = match std::fs::read(&path) {
                    Ok(bytes) => bytes,
                    Err(err) => {
                        eprintln!("warning: failed to read {}: {err}", path.display());
                        failed.fetch_add(1, Ordering::Relaxed);
                        return None;
                    }
                };
                let source = String::from_utf8_lossy(&bytes);
                let results = find_paths(&source, &relative, &notation);
                Some((relative, results))
            })
            .collect();

        Scan { discovered, notation, files, failed_reads: failed.load(Ordering::Relaxed) }
    }

    /// Discovers `root`'s components and scans its PHP files in one step.
    pub fn run(root: &Path) -> Result<Scan, Box<dyn Error>> {
        let discovered = discover_components(root)?;
        Ok(Scan::from_discovery(root, discovered))
    }
}

/// One path reference, resolved to its source/target components and categorised — the shape
/// `find-paths --categorise` writes out, and what the rewrite command's step 3 rewrite rules key
/// off of.
pub struct CategorisedReference {
    pub file: String,
    pub result: PathResult,
    pub source_component: Option<String>,
    pub target: Option<Resolution>,
    pub category: PathCategory,
}

/// Categorises every path reference in `scan`. `dirroot` is dirroot's own path relative to the
/// repository root, with no leading or trailing slash (see [`super::dirroot_prefix`]).
pub fn categorise_all(scan: &Scan, dirroot: &str) -> Vec<CategorisedReference> {
    let resolver = ComponentResolver::new(&scan.discovered);
    let config_locations = entrypoints::config_locations(&scan.notation);
    let boundary_lines: HashMap<String, u32> = entrypoints::classify(&scan.files, &scan.notation, true)
        .into_iter()
        .filter_map(|classification| classification.line.map(|line| (classification.file, line)))
        .collect();
    let plugin_type_roots: HashSet<String> = scan.discovered.plugin_types.values().cloned().collect();

    scan.files
        .par_iter()
        .flat_map_iter(|(file, results)| {
            let source_component = resolver.resolve(file).map(|resolution| resolution.component);
            let file_boundary_line = boundary_lines.get(file).copied();
            // Reborrowed here (rather than letting the `move` below capture the originals
            // directly) so the capture is a cheap `Copy` of a reference, not an attempt to move
            // a `HashSet` out of this `Fn` closure's environment on every one of its many calls.
            let resolver = &resolver;
            let config_locations = &config_locations;
            let plugin_type_roots = &plugin_type_roots;
            results.iter().map(move |result| {
                let target = resolver.resolve(&result.real_path);
                let category = categorise::categorise(
                    result,
                    config_locations,
                    file_boundary_line,
                    plugin_type_roots,
                    source_component.as_deref(),
                    target.as_ref(),
                    dirroot,
                );
                CategorisedReference {
                    file: file.clone(),
                    result: result.clone(),
                    source_component: source_component.clone(),
                    target,
                    category,
                }
            })
        })
        .collect()
}
