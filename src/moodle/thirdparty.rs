//! Third-party library locations declared by 'thirdpartylibs.xml' files, so they can be excluded
//! from analysis. This is the same convention Moodle's own coding-standard tooling uses to skip
//! vendored code that isn't subject to Moodle's own coding guidelines.

use std::collections::HashSet;
use std::path::Path;

use super::components::ComponentDiscovery;

/// The repository root's own 'lib' directory (holding lib/components.json, lib/plugins.json,
/// lib/setup.php) sits above dirroot and so is not itself a component, but — like
/// 'lib/components.json' — is a fixed, known location that may bundle its own third-party
/// libraries.
const REPO_ROOT_LIB: &str = "lib";

/// Finds every 'thirdpartylibs.xml' declared by a known component directory (plus the repo
/// root's own 'lib' directory), and returns the set of declared library locations, relative to
/// `root` (e.g. 'public/lib/phpspreadsheet/phpspreadsheet').
///
/// A 'thirdpartylibs.xml' can only ever appear at one of these roots, so this checks exactly
/// those locations rather than walking the whole tree looking for the filename.
pub fn find_thirdparty_locations(root: &Path, discovery: &ComponentDiscovery) -> HashSet<String> {
    let mut locations = HashSet::new();

    let mut component_dirs: Vec<&str> = discovery.components.values().map(String::as_str).filter(|dir| !dir.is_empty()).collect();
    component_dirs.push(REPO_ROOT_LIB);

    for component_dir in component_dirs {
        read_locations(root, component_dir, &mut locations);
    }

    locations
}

fn read_locations(root: &Path, component_dir: &str, locations: &mut HashSet<String>) {
    let manifest_path = root.join(component_dir).join("thirdpartylibs.xml");
    // Most components simply don't have one.
    let Ok(content) = std::fs::read_to_string(&manifest_path) else {
        return;
    };
    let Ok(doc) = roxmltree::Document::parse(&content) else {
        eprintln!("warning: failed to parse {}", manifest_path.display());
        return;
    };

    for location in doc.descendants().filter(|node| node.has_tag_name("location")).filter_map(|node| node.text()) {
        let location = location.trim();
        if location.is_empty() {
            continue;
        }
        locations.insert(format!("{component_dir}/{location}"));
    }
}

/// Whether `path` (relative to the same root as `locations`) is a declared third-party library
/// location, or lives inside one.
pub fn is_thirdparty(locations: &HashSet<String>, path: &str) -> bool {
    locations.iter().any(|location| path == location || path.starts_with(&format!("{location}/")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_and_nested_paths_are_thirdparty() {
        let locations: HashSet<String> = ["public/lib/phpspreadsheet/phpspreadsheet".to_string()].into_iter().collect();
        assert!(is_thirdparty(&locations, "public/lib/phpspreadsheet/phpspreadsheet"));
        assert!(is_thirdparty(&locations, "public/lib/phpspreadsheet/phpspreadsheet/src/Writer/Xlsx/Rels.php"));
    }

    #[test]
    fn unrelated_and_sibling_paths_are_not_thirdparty() {
        let locations: HashSet<String> = ["public/lib/phpspreadsheet/phpspreadsheet".to_string()].into_iter().collect();
        assert!(!is_thirdparty(&locations, "public/lib/moodlelib.php"));
        // A sibling directory sharing the same prefix must not be matched by a bare `starts_with`.
        assert!(!is_thirdparty(&locations, "public/lib/phpspreadsheet/phpspreadsheet2/lib.php"));
    }
}
