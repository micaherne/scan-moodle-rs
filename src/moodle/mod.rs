//! Moodle-domain concepts layered on top of the generic path-finding tools.

use std::path::Path;

pub mod categorise;
pub mod components;
pub mod entrypoints;
pub mod resolver;
pub mod scan;
pub mod thirdparty;

/// The path from the repository root to $CFG->dirroot, with a trailing slash (e.g. 'public/'),
/// or '' if the two coincide.
///
/// Since Moodle 5.1, $CFG->dirroot lives in a 'public/' subdirectory of the repository root;
/// earlier layouts have dirroot and the repository root coincide.
pub fn dirroot_prefix(root: &Path) -> &'static str {
    if root.join("public").join("lib").join("setup.php").is_file() {
        "public/"
    } else {
        ""
    }
}

/// Whether `relative_path` (repository-root-relative, no leading slash) must be excluded from
/// path discovery entirely, rather than merely filtered out of one particular analysis.
///
/// 'config-dist.php' is a template showing what a user's config.php should look like — it is
/// never executed and never required by anything real. Left in, its example
/// `require_once(__DIR__ . '/lib/setup.php')` line would look like a genuine reference into the
/// bootstrap chain (see [`entrypoints`]).
pub fn is_excluded_from_scan(relative_path: &str) -> bool {
    relative_path == "config-dist.php"
}
