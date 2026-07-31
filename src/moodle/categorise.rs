//! Categorises every path reference found anywhere in a scanned codebase by how it relates to
//! Moodle's own bootstrap sequence and its component system.
//!
//! The point of this is to help judge which references could plausibly be rewritten to use
//! `core\component` to locate their own files, and which categorically cannot: a reference
//! categorised `PreComponent`, for instance, is real file-loading work done before
//! `core\component` could exist to be loaded, no matter what the reference itself resolves to.

use std::collections::HashSet;
use std::fmt;

use crate::moodle::resolver::Resolution;
use crate::path_finder::PathResult;

/// One path reference's category, most-specific rule first: a reference can only be `Config` if
/// it targets config.php outright, and can only be `PreComponent` if it runs before the
/// containing file's own boundary line — either check settles the category regardless of what
/// the reference's target itself resolves to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathCategory {
    /// Targets config.php itself (root or `public/`) — see
    /// [`crate::moodle::entrypoints::config_locations`].
    Config,
    /// Sits in a bootstrap file (see [`crate::moodle::entrypoints::classify`] with
    /// `bootstrap_only: true`), before that file's own boundary line — real file-loading work
    /// done with no possible access to `core\component` yet, regardless of what it leads to.
    PreComponent,
    /// Resolved to a component synthesised from a dynamic plugin-name segment (e.g.
    /// `mod_{$modname}`) rather than a literal one. Checked ahead of [`Self::Directory`]: a
    /// dynamic plugin name is the defining feature of a reference like
    /// `$CFG->dirroot . '/enrol/' . $plugin` whether or not it goes on to name a file inside that
    /// plugin, and it is the part that decides how the reference could be rewritten.
    DynamicComponent,
    /// Resolved to a bare directory rather than a specific file (e.g. `$CFG->dirroot` alone, or
    /// `$CFG->dirroot . '/mod'`, a plugin type's own root) — the reference is about locating a
    /// directory, not loading a library. Only literal directories reach this:
    /// [`Self::DynamicComponent`] claims the dynamic ones first.
    Directory,
    /// Resolved to a literal file in the same component as the file containing the reference.
    StaticSameComponent,
    /// Resolved to a literal file in a different component than the one containing the reference.
    StaticDifferentComponent,
    /// Did not resolve, but has the shape of a real path with one or more runtime-only segments
    /// (e.g. `$CFG->dirroot . $includefile`) — unknowable statically, but not malformed.
    VariableOnly,
    /// Nothing above applies — did not resolve, and is not explained by [`Self::VariableOnly`]
    /// either (e.g. a path that escapes the repository root entirely).
    Uncategorised,
}

impl fmt::Display for PathCategory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Config => "config",
            Self::PreComponent => "pre-component",
            Self::DynamicComponent => "dynamic-component",
            Self::Directory => "directory",
            Self::StaticSameComponent => "static-same-component",
            Self::StaticDifferentComponent => "static-different-component",
            Self::VariableOnly => "variable-only",
            Self::Uncategorised => "uncategorised",
        })
    }
}

/// Categorises `result`, one path reference from anywhere in the scanned codebase.
///
/// `config_locations` is [`crate::moodle::entrypoints::config_locations`]. `file_boundary_line`
/// is the line for the file containing `result`, taken from
/// [`crate::moodle::entrypoints::classify`] called with `bootstrap_only: true` — `None` if that
/// file isn't in that list at all (true of most files: ordinary application code with no real,
/// traceable require chain into component.php). `source_component` is the component owning the
/// containing file itself; `target` is the containing file's own resolution of
/// `result.real_path`, both via [`crate::moodle::resolver::ComponentResolver`].
pub fn categorise(
    result: &PathResult,
    config_locations: &HashSet<String>,
    file_boundary_line: Option<u32>,
    source_component: Option<&str>,
    target: Option<&Resolution>,
) -> PathCategory {
    if config_locations.contains(&result.real_path) {
        return PathCategory::Config;
    }
    if file_boundary_line.is_some_and(|boundary_line| result.line < boundary_line) {
        return PathCategory::PreComponent;
    }
    match target {
        Some(resolution) if resolution.component.contains('{') => PathCategory::DynamicComponent,
        Some(_) if is_bare_directory(&result.real_path) => PathCategory::Directory,
        Some(resolution) if source_component == Some(resolution.component.as_str()) => PathCategory::StaticSameComponent,
        Some(_) => PathCategory::StaticDifferentComponent,
        None if is_variable_shaped(&result.real_path) => PathCategory::VariableOnly,
        None => PathCategory::Uncategorised,
    }
}

/// Whether `real_path` names a directory rather than a file, going purely on its own shape — a
/// trailing separator (with or without anything before it, e.g. `$CFG->dirroot . '/'`), or,
/// lacking that, a last segment with no `.` in it (a directory name never has one; this is
/// deliberately a naming-convention assumption, not a lookup against which directories the
/// resolver happens to already know about — a plugin-type root like 'public/mod' is exactly as
/// unlisted there as any other directory nobody has looked inside of yet).
fn is_bare_directory(real_path: &str) -> bool {
    match real_path.rsplit('/').next() {
        None | Some("") => true,
        Some(last_segment) => !last_segment.contains('.'),
    }
}

/// Whether `real_path` looks like a real path that simply has one or more runtime-only segments,
/// rather than something the resolver was right to refuse outright. The scanner renders anything
/// it could not evaluate as a `{...}` marker (see
/// [`crate::moodle::resolver::ComponentResolver::resolve`]), so its presence is a reliable signal
/// of "a variable stands here" once the shapes that mean something else entirely are ruled out: a
/// `..` segment (the path escapes the repository root, so the rest of it describes somewhere
/// else) and a backslash (a Windows separator or a namespace, either way not a `/`-delimited path
/// this scheme can reason about).
fn is_variable_shaped(real_path: &str) -> bool {
    real_path.contains('{') && !real_path.contains('\\') && !real_path.split('/').any(|segment| segment == "..")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::path_finder::PathKind;

    fn result(real_path: &str, line: u32) -> PathResult {
        PathResult {
            path: String::new(),
            real_path: real_path.to_string(),
            kind: PathKind::Dirroot,
            line,
            code: String::new(),
            parent: None,
            start_pos: None,
            end_pos: None,
            separator: String::new(),
            mono_path_expr: String::new(),
        }
    }

    fn resolution(component: &str, path_in_component: &str) -> Resolution {
        Resolution { component: component.to_string(), path_in_component: path_in_component.to_string() }
    }

    #[test]
    fn targeting_config_php_is_config_regardless_of_anything_else() {
        let locations = HashSet::from(["public/config.php".to_string()]);
        let category = categorise(&result("public/config.php", 999), &locations, Some(1), Some("core"), None);
        assert_eq!(category, PathCategory::Config);
    }

    #[test]
    fn a_line_before_the_files_own_boundary_line_is_pre_component() {
        let locations = HashSet::new();
        let target = resolution("core", "/setup.php");
        let category = categorise(&result("public/lib/setup.php", 30), &locations, Some(122), Some("tool_behat"), Some(&target));
        assert_eq!(category, PathCategory::PreComponent);
    }

    #[test]
    fn a_line_after_the_files_own_boundary_line_is_not_pre_component() {
        let locations = HashSet::new();
        let target = resolution("core", "/setup.php");
        let category = categorise(&result("public/lib/setup.php", 200), &locations, Some(122), Some("tool_behat"), Some(&target));
        assert_eq!(category, PathCategory::StaticDifferentComponent);
    }

    #[test]
    fn a_bare_directory_with_a_dynamic_component_is_still_directory_not_dynamic_component() {
        let locations = HashSet::new();
        let target = resolution("mod_{$modname}", "");
        let category = categorise(&result("public/mod/{$modname}", 10), &locations, None, Some("root"), Some(&target));
        assert_eq!(category, PathCategory::Directory);
    }

    #[test]
    fn a_dynamic_plugin_name_component_with_a_real_path_is_dynamic_component() {
        let locations = HashSet::new();
        let target = resolution("mod_{$modname}", "/lib.php");
        let category = categorise(&result("public/mod/{$modname}/lib.php", 10), &locations, None, Some("root"), Some(&target));
        assert_eq!(category, PathCategory::DynamicComponent);
    }

    #[test]
    fn a_bare_directory_is_directory_wrangling_even_in_the_same_component() {
        let locations = HashSet::new();
        let target = resolution("core", "");
        let category = categorise(&result("public/lib", 10), &locations, None, Some("core"), Some(&target));
        assert_eq!(category, PathCategory::Directory);
    }

    #[test]
    fn a_trailing_slash_bare_directory_is_also_directory() {
        let locations = HashSet::new();
        let target = resolution("core", "/");
        let category = categorise(&result("public/lib/", 10), &locations, None, Some("core"), Some(&target));
        assert_eq!(category, PathCategory::Directory);
    }

    #[test]
    fn a_literal_file_in_the_same_component_is_static_same_component() {
        let locations = HashSet::new();
        let target = resolution("core", "/setup.php");
        let category = categorise(&result("public/lib/setup.php", 10), &locations, None, Some("core"), Some(&target));
        assert_eq!(category, PathCategory::StaticSameComponent);
    }

    #[test]
    fn a_literal_file_in_a_different_component_is_static_different_component() {
        let locations = HashSet::new();
        let target = resolution("core", "/classes/component.php");
        let category = categorise(&result("public/lib/classes/component.php", 10), &locations, None, Some("root"), Some(&target));
        assert_eq!(category, PathCategory::StaticDifferentComponent);
    }

    #[test]
    fn an_unresolved_path_shaped_by_a_variable_is_variable_only() {
        let locations = HashSet::new();
        let category = categorise(&result("public{$includefile}", 10), &locations, None, Some("core"), None);
        assert_eq!(category, PathCategory::VariableOnly);
    }

    #[test]
    fn an_unresolved_path_escaping_the_repository_root_is_uncategorised_not_variable_only() {
        let locations = HashSet::new();
        let category = categorise(&result("../moodledata", 10), &locations, None, Some("root"), None);
        assert_eq!(category, PathCategory::Uncategorised);
    }

    #[test]
    fn an_unresolved_path_with_no_dynamic_marker_at_all_is_uncategorised() {
        let locations = HashSet::new();
        let category = categorise(&result("some/weird/path", 10), &locations, None, Some("root"), None);
        assert_eq!(category, PathCategory::Uncategorised);
    }

    /// The bug this case exists to catch: bare dirroot itself resolves to the root
    /// pseudo-component with a non-empty `path_in_component` (see resolver.rs's own tests on
    /// this), so bareness has to come from `real_path`'s own shape, not from `target`'s.
    #[test]
    fn dirroot_itself_is_directory_from_its_trailing_slash_not_path_in_component() {
        let locations = HashSet::new();
        let target = resolution("root", "/public/");
        let category = categorise(&result("public/", 10), &locations, None, Some("tool_xmldb"), Some(&target));
        assert_eq!(category, PathCategory::Directory);
    }

    /// A plugin type root (e.g. `$CFG->dirroot . '/mod'`) has the exact same root-pseudo-component
    /// shape as dirroot itself, and must categorise the same way — its last segment ('mod') has
    /// no '.' in it, same as any other bare directory name.
    #[test]
    fn a_plugin_type_root_is_directory_from_its_dotless_last_segment() {
        let locations = HashSet::new();
        let target = resolution("root", "/public/mod");
        let category = categorise(&result("public/mod", 10), &locations, None, Some("root"), Some(&target));
        assert_eq!(category, PathCategory::Directory);
    }

    /// An ordinary root-owned file has the exact same root-pseudo-component shape as a bare
    /// directory under root does — its last segment having a '.' in it is what tells them apart.
    #[test]
    fn a_root_owned_file_is_not_directory_despite_the_root_pseudo_component() {
        let locations = HashSet::new();
        let target = resolution("root", "/public/backup/backup.class.php");
        let category = categorise(&result("public/backup/backup.class.php", 10), &locations, None, Some("tool_xmldb"), Some(&target));
        assert_eq!(category, PathCategory::StaticDifferentComponent);
    }
}
