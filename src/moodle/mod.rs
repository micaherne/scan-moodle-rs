//! Moodle-domain concepts layered on top of the generic path-finding tools.

use std::path::Path;

pub mod components;

/// The path from the repository root to $CFG->dirroot, with a trailing slash (e.g. 'public/'),
/// or '' if the two coincide.
///
/// Since Moodle 5.1, $CFG->dirroot lives in a 'public/' subdirectory of the repository root;
/// earlier layouts have dirroot and the repository root coincide.
pub fn dirroot_prefix(root: &Path) -> &'static str {
    if root.join("public").join("lib").join("setup.php").is_file() { "public/" } else { "" }
}
