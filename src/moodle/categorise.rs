//! Categorises every path reference found anywhere in a scanned codebase by how it relates to
//! Moodle's own bootstrap sequence and its component system.
//!
//! The point of this is to help judge which references could plausibly be rewritten to use
//! `core\component` to locate their own files, and which categorically cannot: a reference
//! categorised `PreComponent`, for instance, is real file-loading work done before
//! `core\component` could exist to be loaded, no matter what the reference itself resolves to.

use std::collections::HashSet;
use std::fmt;

use crate::moodle::resolver::{ROOT_COMPONENT, Resolution};
use crate::path_finder::PathResult;

/// One path reference's category, most-specific rule first: a reference can only be `Config` if
/// it targets config.php outright, and can only be `PreComponent` if it runs before the
/// containing file's own boundary line — either check settles the category regardless of what
/// the reference's target itself resolves to. `PluginTypeRoot` is checked next, ahead of
/// everything resolution-based, since it too is a plain string match against a fixed, known set of
/// locations. Past those, `DynamicComponent`, `DirrootWrangling` and `RootWrangling` are specific
/// shapes checked ahead of the plain same/different-component fallback that everything else
/// resolved lands in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathCategory {
    /// Targets config.php itself (root or `public/`) — see
    /// [`crate::moodle::entrypoints::config_locations`].
    Config,
    /// Sits in a bootstrap file (see [`crate::moodle::entrypoints::classify`] with
    /// `bootstrap_only: true`), before that file's own boundary line — real file-loading work
    /// done with no possible access to `core\component` yet, regardless of what it leads to.
    PreComponent,
    /// The reference is exactly a plugin type's own root directory — the directory that holds
    /// every plugin of that type (e.g. `mod/`, `theme/`), which code referencing it directly is
    /// very likely scanning to enumerate installed plugins. This is a different thing entirely
    /// from an individual plugin's own directory (e.g. `mod/forum/`), which is not this category —
    /// that already resolves to that plugin's own component and is `StaticSameComponent` or
    /// `StaticDifferentComponent` like any other reference to a known component.
    PluginTypeRoot,
    /// Resolved to a component synthesised from a dynamic plugin-name segment (e.g.
    /// `mod_{$modname}`) rather than a literal one — regardless of whether the reference goes on
    /// to name a file inside that plugin or is the bare plugin directory itself.
    DynamicComponent,
    /// The reference is exactly `$CFG->dirroot` itself, or that plus a single trailing separator
    /// appended by how it was concatenated (e.g. `$CFG->dirroot . '/'`) — nothing else
    /// concatenated on. `$CFG->dirroot . '/mod'` is not this: there is real content after dirroot
    /// in the reference (it is `PluginTypeRoot` instead, checked earlier).
    DirrootWrangling,
    /// The same idea as [`Self::DirrootWrangling`], for `$CFG->root` itself.
    RootWrangling,
    /// Resolved to a literal file or directory in the same component as the file containing the
    /// reference — including a bare directory that is a real, known component's own directory
    /// (e.g. `$CFG->libdir` alone).
    StaticSameComponent,
    /// Resolved to a literal file or directory in a different component than the one containing
    /// the reference.
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
            Self::PluginTypeRoot => "plugin-type-root",
            Self::DynamicComponent => "dynamic-component",
            Self::DirrootWrangling => "dirroot-wrangling",
            Self::RootWrangling => "root-wrangling",
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
/// traceable require chain into component.php). `plugin_type_roots` is every plugin type's own
/// root directory, repository-relative (e.g. 'public/mod', 'public/theme') — the values of
/// [`crate::moodle::components::ComponentDiscovery::plugin_types`]. `source_component` is the
/// component owning the containing file itself; `target` is the containing file's own resolution
/// of `result.real_path`, both via [`crate::moodle::resolver::ComponentResolver`]. `dirroot` is
/// dirroot's own path relative to the repository root, with no leading or trailing slash (e.g.
/// 'public', or '' pre-Moodle-5.1 where dirroot and the repository root coincide) — see
/// [`crate::moodle::dirroot_prefix`]; used only to recognise dirroot-wrangling by shape.
pub fn categorise(
    result: &PathResult,
    config_locations: &HashSet<String>,
    file_boundary_line: Option<u32>,
    plugin_type_roots: &HashSet<String>,
    source_component: Option<&str>,
    target: Option<&Resolution>,
    dirroot: &str,
) -> PathCategory {
    if config_locations.contains(&result.real_path) {
        return PathCategory::Config;
    }
    if file_boundary_line.is_some_and(|boundary_line| result.line < boundary_line) {
        return PathCategory::PreComponent;
    }
    if is_plugin_type_root(&result.real_path, plugin_type_roots) {
        return PathCategory::PluginTypeRoot;
    }
    match target {
        Some(resolution) if resolution.component.contains('{') => PathCategory::DynamicComponent,
        Some(resolution) if resolution.component == ROOT_COMPONENT && is_dirroot_wrangling(&result.real_path, dirroot) => {
            PathCategory::DirrootWrangling
        }
        Some(resolution) if resolution.component == ROOT_COMPONENT && is_root_wrangling(&result.real_path) => PathCategory::RootWrangling,
        Some(resolution) if source_component == Some(resolution.component.as_str()) => PathCategory::StaticSameComponent,
        Some(_) => PathCategory::StaticDifferentComponent,
        // The resolver refuses to resolve anything with a backslash in it — rightly, since a
        // backslash is generally a Windows separator or a namespace, not something it can
        // interpret as a `/`-delimited path. But code that is explicitly checking for a
        // Windows-style path (e.g. `strpos($filepath, $CFG->dirroot.'\\')`, see
        // `enrol/flatfile/lib.php`) legitimately means dirroot/root plus a literal trailing
        // separator, exactly like the ordinary `/` case above, just spelled with the other
        // separator — so it is recognised here specifically for wrangling, without loosening how
        // the resolver treats backslashes anywhere else.
        None if is_dirroot_wrangling(&result.real_path, dirroot) => PathCategory::DirrootWrangling,
        None if is_root_wrangling(&result.real_path) => PathCategory::RootWrangling,
        None if is_variable_shaped(&result.real_path) => PathCategory::VariableOnly,
        None => PathCategory::Uncategorised,
    }
}

/// Whether `real_path` is exactly one of `plugin_type_roots`, or one of them with a single
/// trailing '/' appended by how it was concatenated (e.g. `$CFG->dirroot . '/mod/'`) — mirroring
/// the same tolerance [`is_dirroot_wrangling`] and [`is_root_wrangling`] give their own single
/// fixed location.
fn is_plugin_type_root(real_path: &str, plugin_type_roots: &HashSet<String>) -> bool {
    plugin_type_roots.contains(real_path)
        || real_path.strip_suffix('/').is_some_and(|stripped| plugin_type_roots.contains(stripped))
}

/// Whether `real_path` is exactly `dirroot` itself, or that same location with a single trailing
/// separator appended by how it was concatenated (e.g. `$CFG->dirroot . '/'` or, for code
/// explicitly checking a Windows-style path, `$CFG->dirroot . '\\'`).
fn is_dirroot_wrangling(real_path: &str, dirroot: &str) -> bool {
    real_path == dirroot || real_path == format!("{dirroot}/") || real_path == format!("{dirroot}\\")
}

/// Whether `real_path` is exactly `$CFG->root` itself, or that same location with a trailing
/// separator. Checked as "every segment is empty" rather than against a single fixed string, so
/// it catches a trailing '/' regardless of exactly how the scanner happens to render it; a
/// trailing backslash is checked separately, since backslash is not a segment separator anywhere
/// else in this scheme (see the `None` arms in `categorise`).
fn is_root_wrangling(real_path: &str) -> bool {
    real_path.split('/').all(str::is_empty) || real_path == "\\"
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

    const DIRROOT: &str = "public";

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
        let category = categorise(&result("public/config.php", 999), &locations, Some(1), &HashSet::new(), Some("core"), None, DIRROOT);
        assert_eq!(category, PathCategory::Config);
    }

    #[test]
    fn a_line_before_the_files_own_boundary_line_is_pre_component() {
        let locations = HashSet::new();
        let target = resolution("core", "/setup.php");
        let category =
            categorise(&result("public/lib/setup.php", 30), &locations, Some(122), &HashSet::new(), Some("tool_behat"), Some(&target), DIRROOT);
        assert_eq!(category, PathCategory::PreComponent);
    }

    #[test]
    fn a_line_after_the_files_own_boundary_line_is_not_pre_component() {
        let locations = HashSet::new();
        let target = resolution("core", "/setup.php");
        let category =
            categorise(&result("public/lib/setup.php", 200), &locations, Some(122), &HashSet::new(), Some("tool_behat"), Some(&target), DIRROOT);
        assert_eq!(category, PathCategory::StaticDifferentComponent);
    }

    /// A dynamic plugin name at its own bare plugin root (nothing after it) is still
    /// `DynamicComponent` — the dynamic name is what decides how a reference like this could be
    /// rewritten, whether or not it goes on to name a file inside that plugin.
    ///
    /// This shape is genuinely ambiguous with a rarer, wrongly-classified one — see the
    /// known-inaccuracy note on `is_whole_dynamic_segment` in `resolver.rs` — but is kept as-is
    /// because it is right far more often than not, and every `DynamicComponent` result already
    /// needs human review before being acted on regardless.
    #[test]
    fn a_dynamic_plugin_name_at_its_own_bare_root_is_dynamic_component() {
        let locations = HashSet::new();
        let target = resolution("mod_{$modname}", "");
        let category = categorise(&result("public/mod/{$modname}", 10), &locations, None, &HashSet::new(), Some("root"), Some(&target), DIRROOT);
        assert_eq!(category, PathCategory::DynamicComponent);
    }

    #[test]
    fn a_dynamic_plugin_name_component_with_a_real_path_is_dynamic_component() {
        let locations = HashSet::new();
        let target = resolution("mod_{$modname}", "/lib.php");
        let category =
            categorise(&result("public/mod/{$modname}/lib.php", 10), &locations, None, &HashSet::new(), Some("root"), Some(&target), DIRROOT);
        assert_eq!(category, PathCategory::DynamicComponent);
    }

    #[test]
    fn bare_dirroot_itself_is_dirroot_wrangling() {
        let locations = HashSet::new();
        let target = resolution("root", "/public");
        let category = categorise(&result("public", 10), &locations, None, &HashSet::new(), Some("tool_xmldb"), Some(&target), DIRROOT);
        assert_eq!(category, PathCategory::DirrootWrangling);
    }

    #[test]
    fn dirroot_with_a_trailing_separator_is_also_dirroot_wrangling() {
        let locations = HashSet::new();
        let target = resolution("root", "/public/");
        let category = categorise(&result("public/", 10), &locations, None, &HashSet::new(), Some("tool_xmldb"), Some(&target), DIRROOT);
        assert_eq!(category, PathCategory::DirrootWrangling);
    }

    /// `strpos($filepath, $CFG->dirroot.'\\') === 0` (`enrol/flatfile/lib.php`) is code explicitly
    /// checking a Windows-style path, so the trailing backslash means the same thing a trailing
    /// '/' would. The resolver refuses to resolve anything containing a backslash at all — rightly
    /// so everywhere else, since a backslash usually means something other than a path separator —
    /// so this never reaches `categorise` as a resolved target; it has to be recognised straight
    /// off the unresolved `real_path` instead.
    #[test]
    fn dirroot_with_a_trailing_backslash_is_also_dirroot_wrangling() {
        let locations = HashSet::new();
        let category = categorise(&result("public\\", 10), &locations, None, &HashSet::new(), Some("enrol_flatfile"), None, DIRROOT);
        assert_eq!(category, PathCategory::DirrootWrangling);
    }

    #[test]
    fn bare_root_itself_is_root_wrangling() {
        let locations = HashSet::new();
        let target = resolution("root", "");
        let category = categorise(&result("", 10), &locations, None, &HashSet::new(), Some("tool_xmldb"), Some(&target), DIRROOT);
        assert_eq!(category, PathCategory::RootWrangling);
    }

    /// Whatever shape the scanner happens to render a trailing separator on `$CFG->root` in,
    /// root-wrangling still has to catch it — this is why it is checked as "every segment is
    /// empty", not as one specific literal string.
    #[test]
    fn root_with_a_trailing_separator_is_also_root_wrangling() {
        let locations = HashSet::new();
        let target = resolution("root", "/");
        let category = categorise(&result("/", 10), &locations, None, &HashSet::new(), Some("tool_xmldb"), Some(&target), DIRROOT);
        assert_eq!(category, PathCategory::RootWrangling);
    }

    /// The same Windows-style-path reasoning as
    /// `dirroot_with_a_trailing_backslash_is_also_dirroot_wrangling`, for `$CFG->root` instead of
    /// `$CFG->dirroot`.
    #[test]
    fn root_with_a_trailing_backslash_is_also_root_wrangling() {
        let locations = HashSet::new();
        let category = categorise(&result("\\", 10), &locations, None, &HashSet::new(), Some("tool_xmldb"), None, DIRROOT);
        assert_eq!(category, PathCategory::RootWrangling);
    }

    /// A plugin-type root (e.g. `$CFG->dirroot . '/mod'`) resolves to the same `root`
    /// pseudo-component as dirroot itself, but there is real content after dirroot in the
    /// reference, so it is not dirroot-wrangling — and now that it is a known plugin type's own
    /// root directory, it is `PluginTypeRoot`, not the ordinary same-component fallback it would
    /// otherwise land in like anything else that resolves to `root`.
    #[test]
    fn a_known_plugin_type_root_is_plugin_type_root_not_static_same_component() {
        let locations = HashSet::new();
        let plugin_type_roots = HashSet::from(["public/mod".to_string()]);
        let target = resolution("root", "/public/mod");
        let category = categorise(&result("public/mod", 10), &locations, None, &plugin_type_roots, Some("root"), Some(&target), DIRROOT);
        assert_eq!(category, PathCategory::PluginTypeRoot);
    }

    /// The same shape referenced from a plugin instead of `root` — `PluginTypeRoot` wins
    /// regardless of the source component, since it is checked before the same/different-component
    /// distinction is even considered.
    #[test]
    fn a_known_plugin_type_root_is_plugin_type_root_not_static_different_component() {
        let locations = HashSet::new();
        let plugin_type_roots = HashSet::from(["public/mod".to_string()]);
        let target = resolution("root", "/public/mod");
        let category =
            categorise(&result("public/mod", 10), &locations, None, &plugin_type_roots, Some("mod_forum"), Some(&target), DIRROOT);
        assert_eq!(category, PathCategory::PluginTypeRoot);
    }

    /// A trailing separator appended by how the reference was concatenated (e.g.
    /// `$CFG->dirroot . '/mod/'`) still means the plugin type's own root directory, the same
    /// tolerance `DirrootWrangling`/`RootWrangling` give their own fixed location.
    #[test]
    fn plugin_type_root_tolerates_a_trailing_separator() {
        let locations = HashSet::new();
        let plugin_type_roots = HashSet::from(["public/mod".to_string()]);
        let category = categorise(&result("public/mod/", 10), &locations, None, &plugin_type_roots, Some("root"), None, DIRROOT);
        assert_eq!(category, PathCategory::PluginTypeRoot);
    }

    /// `PreComponent` is still checked first: real file-loading work with no possible access to
    /// core\component yet must win even over a reference that happens to be a plugin type's own
    /// root directory.
    #[test]
    fn pre_component_still_wins_over_plugin_type_root() {
        let locations = HashSet::new();
        let plugin_type_roots = HashSet::from(["public/mod".to_string()]);
        let category =
            categorise(&result("public/mod", 10), &locations, Some(20), &plugin_type_roots, Some("root"), None, DIRROOT);
        assert_eq!(category, PathCategory::PreComponent);
    }

    /// A bare directory that resolves to a real, known component (not the `root`
    /// pseudo-component) is just that component's own directory — static-same/different-component,
    /// the same as a file within it, not a wrangling category.
    #[test]
    fn a_bare_directory_resolving_to_a_real_component_is_static_same_component() {
        let locations = HashSet::new();
        let target = resolution("core", "");
        let category = categorise(&result("public/lib", 10), &locations, None, &HashSet::new(), Some("core"), Some(&target), DIRROOT);
        assert_eq!(category, PathCategory::StaticSameComponent);
    }

    #[test]
    fn a_literal_file_in_the_same_component_is_static_same_component() {
        let locations = HashSet::new();
        let target = resolution("core", "/setup.php");
        let category = categorise(&result("public/lib/setup.php", 10), &locations, None, &HashSet::new(), Some("core"), Some(&target), DIRROOT);
        assert_eq!(category, PathCategory::StaticSameComponent);
    }

    #[test]
    fn a_literal_file_in_a_different_component_is_static_different_component() {
        let locations = HashSet::new();
        let target = resolution("core", "/classes/component.php");
        let category =
            categorise(&result("public/lib/classes/component.php", 10), &locations, None, &HashSet::new(), Some("root"), Some(&target), DIRROOT);
        assert_eq!(category, PathCategory::StaticDifferentComponent);
    }

    #[test]
    fn an_unresolved_path_shaped_by_a_variable_is_variable_only() {
        let locations = HashSet::new();
        let category = categorise(&result("public{$includefile}", 10), &locations, None, &HashSet::new(), Some("core"), None, DIRROOT);
        assert_eq!(category, PathCategory::VariableOnly);
    }

    #[test]
    fn an_unresolved_path_escaping_the_repository_root_is_uncategorised_not_variable_only() {
        let locations = HashSet::new();
        let category = categorise(&result("../moodledata", 10), &locations, None, &HashSet::new(), Some("root"), None, DIRROOT);
        assert_eq!(category, PathCategory::Uncategorised);
    }

    #[test]
    fn an_unresolved_path_with_no_dynamic_marker_at_all_is_uncategorised() {
        let locations = HashSet::new();
        let category = categorise(&result("some/weird/path", 10), &locations, None, &HashSet::new(), Some("root"), None, DIRROOT);
        assert_eq!(category, PathCategory::Uncategorised);
    }

    #[test]
    fn a_root_owned_file_is_not_dirroot_wrangling_despite_the_root_pseudo_component() {
        let locations = HashSet::new();
        let target = resolution("root", "/public/backup/backup.class.php");
        let category =
            categorise(&result("public/backup/backup.class.php", 10), &locations, None, &HashSet::new(), Some("tool_xmldb"), Some(&target), DIRROOT);
        assert_eq!(category, PathCategory::StaticDifferentComponent);
    }

    /// Pre-Moodle-5.1 layouts have no `public/` split, so dirroot and the repository root
    /// coincide: `dirroot` is `''` there, the same value `$CFG->root` itself always resolves to,
    /// and the two wrangling shapes become indistinguishable by `real_path` alone. Whichever arm
    /// is checked first wins in that case; this pins it down as dirroot-wrangling rather than
    /// leaving it to match-arm-order happenstance.
    #[test]
    fn pre_5_1_dirroot_and_root_coincide_and_resolve_to_dirroot_wrangling() {
        let locations = HashSet::new();
        let target = resolution("root", "");
        let category = categorise(&result("", 10), &locations, None, &HashSet::new(), Some("tool_xmldb"), Some(&target), "");
        assert_eq!(category, PathCategory::DirrootWrangling);
    }
}
