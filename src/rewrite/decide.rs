//! Step 3 of the rewrite process (see `REWRITE_SPEC.md`): decides what a single eligible path
//! reference should be rewritten to, if anything.
//!
//! This module only decides — it does not touch any files. See [`super::splice`] for where a
//! decision is turned into an actual edit on disk.

use std::collections::HashSet;

use crate::moodle::categorise::PathCategory;
use crate::moodle::resolver::{ROOT_COMPONENT, Resolution};
use crate::moodle::scan::CategorisedReference;
use crate::path_finder::PathKind;

/// Whether `category` is one step 3 ever rewrites at all — [`PathCategory::Config`],
/// [`PathCategory::StaticSameComponent`], [`PathCategory::StaticDifferentComponent`], or
/// [`PathCategory::VariableOnly`]. Every other category is left untouched unconditionally (see
/// `REWRITE_SPEC.md`); [`decide`] is only meaningful when this is true. Note that for `Config` and
/// `VariableOnly` specifically, eligibility here is not a guarantee [`decide`] will actually
/// produce a replacement — see their own doc comments.
pub fn is_eligible(category: PathCategory) -> bool {
    matches!(
        category,
        PathCategory::Config
            | PathCategory::StaticSameComponent
            | PathCategory::StaticDifferentComponent
            | PathCategory::VariableOnly
    )
}

/// Decides what `reference` should be rewritten to. Returns `Option` because that is the natural
/// shape for "what should this become" — every eligible category other than [`PathCategory::Config`]
/// and [`PathCategory::VariableOnly`] always produces a replacement in practice. For `Config`,
/// `None` means the reference is already `__DIR__`-relative ([`crate::path_finder::PathResult::kind`]
/// is [`PathKind::Dir`]) — nothing to do. For `VariableOnly`, `None` means the reference isn't
/// actually rooted at `$CFG->dirroot` (an empty [`crate::path_finder::PathResult::mono_path_expr`])
/// — in the codebase this project targets that never happens outside
/// `public/lib/classes/component.php`/`public/lib/tests/component_test.php`, which are excluded
/// from this category entirely (see [`PathCategory::Component`]), but nothing stops it happening
/// elsewhere (e.g. a reference rooted at `$CFG->libdir` instead), so it isn't treated as
/// unreachable.
///
/// Only meaningful for a reference already categorised as [`PathCategory::Config`],
/// [`PathCategory::StaticSameComponent`], [`PathCategory::StaticDifferentComponent`] or
/// [`PathCategory::VariableOnly`] (see [`is_eligible`]) — every other category is left untouched by
/// step 3 unconditionally, before this function is ever consulted (see `REWRITE_SPEC.md`).
///
/// `entry_point_files` is every file the entrypoint scan classified as a bootstrap file, a CLI
/// script or a page (all three count as "an entry point" here) — the *unrestricted* scan, not
/// `find-entrypoints`' `--bootstrap-only` one (see `crate::moodle::entrypoints::classify`, called
/// with `bootstrap_only: false`). Unused for `Config`/`VariableOnly`: neither replacement depends
/// on whether the source file is an entry point — see their own doc comments for why. Checked on
/// both sides of a `StaticSameComponent`/`StaticDifferentComponent` reference: whether the
/// *source* file is an entry point affects how a non-entry-point target gets addressed (see the
/// inline comment above the `match` below); whether the *target* file is an entry point overrides
/// that entirely, regardless of the source — see the inline comment right before that `match`.
pub fn decide(
    reference: &CategorisedReference,
    entry_point_files: &HashSet<String>,
) -> Option<String> {
    debug_assert!(
        is_eligible(reference.category),
        "decide is only meaningful for a reference the path scan categorised as config, static-same/different-component or variable-only"
    );

    if reference.category == PathCategory::VariableOnly {
        return variable_only_expression(reference);
    }
    if reference.category == PathCategory::Config {
        return config_expression(reference);
    }

    let target = reference
        .target
        .as_ref()
        .expect("static-same/different-component always resolves a target");
    let target_file = &reference.result.real_path;
    let source_is_entry_point = entry_point_files.contains(&reference.file);

    // A page, CLI script or bootstrap file is a special case on the *target* side too, separately
    // from the source-side handling below: whatever component the entry-point file nominally
    // belongs to, its file still gets copied into that component's own package like any other file
    // in it — but a not-yet-written Composer plugin *also* places a copy of every entry point back
    // at its original path, directly under the project root, because that is the one physical
    // location a page/CLI script/bootstrap file must sit at to actually work (a web server or `php`
    // invocation needs it at a fixed, predictable path; a bootstrap file specifically must be
    // loadable before `core\component` exists to resolve anything through). So an entry-point
    // target ends up with two copies on disk: the "real" one the plugin places at the project root,
    // and an inert extra one sitting inside its component's package purely because the one-pass
    // copy that builds packages doesn't distinguish entry points from ordinary files. A reference
    // rewritten to reach into the package copy (`__DIR__`-relative or `component_path()`) would
    // silently load that inert copy instead of the real one — and if the real copy of the same file
    // is *also* loaded somewhere else in the same request (as it usually will be, since it's an
    // entry point), PHP fatals on redeclaring the same classes/functions twice. So a reference to an
    // entry-point target is always rewritten the same way a reference to a `root`-owned target is,
    // straight to `$CFG->root` — the plugin-placed copy at the project root is the only one that's
    // ever safe to point at. This overrides everything below, including same-component handling:
    // two files being nominally in the same component does not mean they end up in the same place
    // once one of them is an entry point.
    if target.component != ROOT_COMPONENT && entry_point_files.contains(target_file) {
        return Some(root_expression(target_file));
    }

    // An entry point never gets a `__DIR__`-relative path, full stop — not just into its own
    // component's code, but anywhere, including root. Nothing about an entry point's own eventual
    // location is settled: today it's a script sitting directly in the deployed layout, but that's
    // exactly the arrangement this project might replace later (e.g. a routing shim in its place,
    // with the real file packaged alongside the rest of its component). A `component_path()`/
    // `$CFG->root` call keeps working under whatever that ends up being; a hard-coded `__DIR__`
    // climb only keeps working for as long as the entry point stays exactly where it is today, and
    // would all have to be re-rewritten the moment it doesn't.
    match reference.category {
        PathCategory::StaticSameComponent
            if target.component == ROOT_COMPONENT && source_is_entry_point =>
        {
            Some(root_expression(target_file))
        }
        PathCategory::StaticSameComponent if target.component == ROOT_COMPONENT => {
            Some(dir_relative_expression(&reference.file, target_file))
        }
        PathCategory::StaticSameComponent if source_is_entry_point => {
            Some(component_path_expression(target))
        }
        PathCategory::StaticSameComponent => {
            Some(dir_relative_expression(&reference.file, target_file))
        }
        PathCategory::StaticDifferentComponent if target.component == ROOT_COMPONENT => {
            Some(root_expression(target_file))
        }
        PathCategory::StaticDifferentComponent => Some(component_path_expression(target)),
        _ => unreachable!("guarded by the debug_assert above"),
    }
}

/// `\core\component::from_mono_path(<mono_path_expr>)` for `reference` — see `REWRITE_SPEC.md`'s
/// "variable-only rewriting" section. `mono_path_expr` is used verbatim, exactly as
/// [`crate::path_finder::finder`] extracted it from the source: it is already the original
/// reference's own text for everything after `$CFG->dirroot`, with the concatenation operator (or
/// enclosing `{...}` for an interpolated string) removed and nothing else touched — no leading `/`
/// added or trimmed, since `from_mono_path()` does no normalisation of its own and simply
/// concatenates it onto dirroot exactly as the original code did. `None` if `reference` isn't
/// actually rooted at `$CFG->dirroot` (`mono_path_expr` is empty) — this tool has no rewrite for
/// that.
fn variable_only_expression(reference: &CategorisedReference) -> Option<String> {
    let mono_path_expr = &reference.result.mono_path_expr;
    if mono_path_expr.is_empty() {
        return None;
    }
    Some(format!(
        "\\core\\component::from_mono_path({mono_path_expr})"
    ))
}

/// The `__DIR__`-relative replacement for a reference to config.php itself — see
/// `REWRITE_SPEC.md`'s "config.php rewriting" section. Unlike every other rewrite in this module,
/// this applies identically whether or not `reference`'s file is an entry point: `component_path()`
/// and `$CFG->root` are never an option for config.php regardless (see `REWRITE_SPEC.md`), so
/// there's no more-robust alternative being given up by using `__DIR__` here the way there would be
/// for, say, a same-component reference in an entry point — and the bare relative literal a
/// reference to config.php almost always starts as (e.g. `'../config.php'`) is already exactly as
/// tied to the referencing file's current location as a `__DIR__`-relative expression is, so
/// rewriting it introduces no new fragility either. `None` if `reference` is already
/// `__DIR__`-relative (`PathResult::kind == PathKind::Dir`) — nothing to do.
fn config_expression(reference: &CategorisedReference) -> Option<String> {
    if reference.result.kind == PathKind::Dir {
        return None;
    }
    Some(dir_relative_expression(
        &reference.file,
        &reference.result.real_path,
    ))
}

/// Whether `reference` still relies on `$CFG->dirroot`/`$CFG->libdir` in some form step 3 hasn't
/// eliminated — which is a broader question than "did step 3's own rules rewrite it", since most
/// categories step 3 leaves alone are leftover work this tool doesn't implement *yet*, not work
/// that's genuinely finished:
///
/// - [`PathCategory::Component`] and [`PathCategory::IncludePathRelative`]: always `false`, for two
///   different reasons. `public/lib/classes/component.php` and its unit test are Moodle's original,
///   monolithic component/path resolution — the very code every other rewrite this step produces
///   ends up calling into — so nothing in either file is ever rewritten, on principle, not because
///   this tool doesn't yet know how. `IncludePathRelative` isn't a `$CFG->dirroot`/`libdir`
///   dependency at all — it's resolved by PHP's own include-path search — so there is nothing for
///   this check to be flagging in the first place, unlike every other category on this list.
/// - [`PathCategory::Config`]: `true` when the current source text isn't already what [`decide`]
///   says it should be — the same "does the current text already match" check as the static
///   categories below, since `Config` is now actually rewritten (to `__DIR__`-relative) rather than
///   just reported on; see [`decide`]'s own doc comment for why `component_path()`/`$CFG->root`
///   still never apply here.
/// - [`PathCategory::PreComponent`]: also always `false`, but for a reason distinct from
///   `Component`/`IncludePathRelative` above, and one that matters more: this is real file-loading
///   work done before `core\component` exists, so — like `Config` — it can never be rewritten to a
///   `component_path()`/`$CFG->root` call by this tool. Unlike `Config`, though, it also isn't
///   rewritten to a `__DIR__`-relative expression, even though one could in principle be written by
///   hand. That's a deliberate scope decision, not an oversight: the bootstrap sequence
///   `PreComponent` lives in cannot be split out under this project's whole approach, on principle —
///   the mechanism this project relies on for splitting is `core\component` itself acting as the
///   oracle for where every component lives, and pre-component code by definition runs before that
///   oracle exists, so nothing here is ever a *candidate* for splitting in the first place. Whatever
///   form the reference is written in today — a bare literal, `$CFG->dirroot`, `$CFG->root`, or
///   already `__DIR__`-relative — is therefore equally permanent, and Moodle works correctly in a
///   split-up layout regardless of which one it is. That is a categorically different situation from
///   every other not-yet-implemented category below, where the reference genuinely does need
///   resolving somehow before a split layout would work — conflating the two by both reading `true`
///   is exactly what this variant exists to avoid.
/// - [`PathCategory::StaticSameComponent`]/[`PathCategory::StaticDifferentComponent`]: `true` when
///   the current source text isn't already what [`decide`] says it should be — normally because
///   the reference didn't exist yet when step 3 ran (e.g. introduced by an applied patch), rather
///   than because step 3 missed it.
/// - [`PathCategory::VariableOnly`]: `true` unconditionally when
///   [`crate::path_finder::PathResult::mono_path_expr`] is empty — the reference isn't actually
///   rooted at `$CFG->dirroot` (e.g. `$CFG->libdir` instead), so [`decide`] has nothing to rewrite
///   it with; same "not implemented, not the same as done" reasoning as the categories below, not
///   folded into the shared eligible-category check because unlike those, `decide` returning `None`
///   here does not mean nothing was outstanding. Otherwise, the same "does the current text already
///   match what `decide` says" check as the static categories above.
/// - Every other category: `true` unconditionally. Unlike `PreComponent` above, these genuinely do
///   need to be resolved somehow before Moodle works correctly in a split-up layout. This tool has
///   no rule for any of them yet — most will end up needing a manual patch rather than an automated
///   rewrite — but "not implemented here" isn't the same claim as "doesn't still depend on
///   `$CFG->dirroot`/`libdir`", and the two must not be conflated.
///
/// The `bool` this returns collapses several different reasons for "not done" into one signal —
/// "the tool will rewrite this, it just hasn't yet" and "the tool has no rule for this at all"
/// both read as `true`; "already as good as it gets" and "actually rewritten" both read as
/// `false`. A tri-state status (e.g. done/to-do/error) would let those be told apart in the audit
/// CSV's `rewritten_to` column — deliberately not built, since nothing in the codebase this
/// project currently targets needs it (e.g. no `VariableOnly` reference is rooted at anything
/// other than `$CFG->dirroot` outside the two files `Component` already excludes).
pub fn still_needs_rewrite(
    reference: &CategorisedReference,
    entry_point_files: &HashSet<String>,
) -> bool {
    match reference.category {
        PathCategory::Component
        | PathCategory::IncludePathRelative
        | PathCategory::PreComponent => false,
        PathCategory::Config
        | PathCategory::StaticSameComponent
        | PathCategory::StaticDifferentComponent => decide(reference, entry_point_files)
            .is_some_and(|replacement| replacement != reference.result.code),
        PathCategory::VariableOnly => {
            reference.result.mono_path_expr.is_empty()
                || decide(reference, entry_point_files)
                    .is_some_and(|replacement| replacement != reference.result.code)
        }
        _ => true,
    }
}

/// `\core\component::component_path($component, $path)` for `target` — see `REWRITE_SPEC.md`'s
/// "component_path() arguments" section for the three-way empty/single-slash/other rule this
/// implements for the second argument.
fn component_path_expression(target: &Resolution) -> String {
    let path_argument = match target.path_in_component.as_str() {
        "" => String::new(),
        "/" => "/".to_string(),
        other => other.strip_prefix('/').unwrap_or(other).to_string(),
    };
    format!(
        "\\core\\component::component_path({}, {})",
        quote_php_string(&target.component),
        quote_php_string(&path_argument)
    )
}

/// `$CFG->root . '<path>'` for `target_file`, a plain repository-root-relative path (no leading
/// slash, the same shape [`crate::path_finder::PathResult::real_path`] always has) — used verbatim,
/// with a leading slash spliced on, since `$CFG->root` already points at the repository root. Used
/// for two different reasons a target can be pinned to this one fixed, permanent location rather
/// than wherever its owning component's package ends up: it belongs to no component at all (the
/// `root` pseudo-component), or it's an entry point (see `decide`'s own doc comment) — see
/// `REWRITE_SPEC.md`.
fn root_expression(target_file: &str) -> String {
    format!(
        "$CFG->root . {}",
        quote_php_string(&format!("/{target_file}"))
    )
}

/// The `__DIR__`-relative replacement expression for a reference in `source_file` targeting
/// `target_path` — see `REWRITE_SPEC.md`'s "__DIR__-relative path mechanics" section.
fn dir_relative_expression(source_file: &str, target_path: &str) -> String {
    let source_dir = parent_dir(source_file);
    let (levels_up, suffix) = climb_and_descend(source_dir, target_path);
    let base = match levels_up {
        0 => "__DIR__".to_string(),
        1 => "dirname(__DIR__)".to_string(),
        levels => format!("dirname(__DIR__, {levels})"),
    };
    match suffix {
        Some(literal) => format!("{base} . {}", quote_php_string(&literal)),
        None => base,
    }
}

/// `file`'s own directory, repository-relative with no leading or trailing slash — the empty
/// string for a file living at the repository root.
fn parent_dir(file: &str) -> &str {
    match file.rfind('/') {
        Some(index) => &file[..index],
        None => "",
    }
}

/// How far `source_dir` has to climb to reach the deepest ancestor directory it shares with
/// `target_path`, and the literal suffix (leading `/`, with any trailing separator on
/// `target_path` preserved) needed to descend from there back down to the target. The suffix is
/// `None` when the climb lands exactly on the target, with nothing left to append.
fn climb_and_descend(source_dir: &str, target_path: &str) -> (usize, Option<String>) {
    let source_segments: Vec<&str> = source_dir
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect();
    let trailing_separator = target_path.ends_with('/');
    let target_segments: Vec<&str> = target_path
        .trim_end_matches('/')
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect();

    let common = source_segments
        .iter()
        .zip(&target_segments)
        .take_while(|(source, target)| source == target)
        .count();
    let levels_up = source_segments.len() - common;
    let remainder = &target_segments[common..];

    let suffix = if remainder.is_empty() {
        trailing_separator.then(|| "/".to_string())
    } else {
        Some(format!(
            "/{}{}",
            remainder.join("/"),
            if trailing_separator { "/" } else { "" }
        ))
    };
    (levels_up, suffix)
}

/// A single-quoted PHP string literal for `value`, escaping `'` and `\`.
fn quote_php_string(value: &str) -> String {
    let mut quoted = String::with_capacity(value.len() + 2);
    quoted.push('\'');
    for c in value.chars() {
        if c == '\'' || c == '\\' {
            quoted.push('\\');
        }
        quoted.push(c);
    }
    quoted.push('\'');
    quoted
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reference(
        file: &str,
        real_path: &str,
        kind: PathKind,
        category: PathCategory,
        target: Option<(&str, &str)>,
    ) -> CategorisedReference {
        use crate::path_finder::PathResult;

        CategorisedReference {
            file: file.to_string(),
            result: PathResult {
                path: String::new(),
                real_path: real_path.to_string(),
                kind,
                line: 1,
                code: String::new(),
                parent: None,
                start_pos: Some(0),
                end_pos: Some(0),
                separator: String::new(),
                mono_path_expr: String::new(),
            },
            source_component: None,
            target: target.map(|(component, path_in_component)| Resolution {
                component: component.to_string(),
                path_in_component: path_in_component.to_string(),
            }),
            category,
        }
    }

    fn entry_points(files: &[&str]) -> HashSet<String> {
        files.iter().map(|f| f.to_string()).collect()
    }

    #[test]
    fn same_component_rewrites_to_a_dir_relative_path() {
        let reference = reference(
            "public/mod/quiz/report/overview/report.php",
            "public/mod/quiz/lib.php",
            PathKind::Dirroot,
            PathCategory::StaticSameComponent,
            Some(("mod_quiz", "/lib.php")),
        );
        assert_eq!(
            decide(&reference, &entry_points(&[])),
            Some("dirname(__DIR__, 2) . '/lib.php'".to_string())
        );
    }

    #[test]
    fn same_component_landing_in_the_files_own_directory_is_bare_dir() {
        let reference = reference(
            "public/mod/quiz/lib.php",
            "public/mod/quiz/locallib.php",
            PathKind::Dirroot,
            PathCategory::StaticSameComponent,
            Some(("mod_quiz", "/locallib.php")),
        );
        assert_eq!(
            decide(&reference, &entry_points(&[])),
            Some("__DIR__ . '/locallib.php'".to_string())
        );
    }

    #[test]
    fn same_component_landing_exactly_one_level_up_has_no_count_argument() {
        let reference = reference(
            "public/mod/quiz/classes/lib.php",
            "public/mod/quiz/locallib.php",
            PathKind::Dirroot,
            PathCategory::StaticSameComponent,
            Some(("mod_quiz", "/locallib.php")),
        );
        assert_eq!(
            decide(&reference, &entry_points(&[])),
            Some("dirname(__DIR__) . '/locallib.php'".to_string())
        );
    }

    /// Root-owned content is never split out into its own package, so two root-owned files' paths
    /// relative to each other are permanent — that makes a `__DIR__`-relative rewrite just as safe
    /// here as it is between two files in the same real component, and there's no reason to leave
    /// the `$CFG->dirroot`/`libdir` dependency in place just because "root" isn't a real component
    /// that `component_path()` could name.
    #[test]
    fn same_component_root_placeholder_rewrites_to_a_dir_relative_path() {
        let reference = reference(
            "public/backup/util.php",
            "public/backup/backup.class.php",
            PathKind::Dirroot,
            PathCategory::StaticSameComponent,
            Some(("root", "/public/backup/backup.class.php")),
        );
        assert_eq!(
            decide(&reference, &entry_points(&[])),
            Some("__DIR__ . '/backup.class.php'".to_string())
        );
    }

    #[test]
    fn different_component_rewrites_to_component_path() {
        let reference = reference(
            "public/mod/forum/lib.php",
            "public/mod/assign/view.php",
            PathKind::Dirroot,
            PathCategory::StaticDifferentComponent,
            Some(("mod_assign", "/view.php")),
        );
        assert_eq!(
            decide(&reference, &entry_points(&[])),
            Some("\\core\\component::component_path('mod_assign', 'view.php')".to_string())
        );
    }

    #[test]
    fn different_component_bare_directory_passes_an_empty_path_argument() {
        let reference = reference(
            "public/mod/forum/lib.php",
            "public/mod/assign",
            PathKind::Dirroot,
            PathCategory::StaticDifferentComponent,
            Some(("mod_assign", "")),
        );
        assert_eq!(
            decide(&reference, &entry_points(&[])),
            Some("\\core\\component::component_path('mod_assign', '')".to_string())
        );
    }

    #[test]
    fn different_component_trailing_slash_directory_passes_a_single_slash_path_argument() {
        let reference = reference(
            "public/mod/forum/lib.php",
            "public/mod/assign/",
            PathKind::Dirroot,
            PathCategory::StaticDifferentComponent,
            Some(("mod_assign", "/")),
        );
        assert_eq!(
            decide(&reference, &entry_points(&[])),
            Some("\\core\\component::component_path('mod_assign', '/')".to_string())
        );
    }

    #[test]
    fn different_component_trailing_slash_on_a_file_is_preserved_in_the_path_argument() {
        let reference = reference(
            "public/mod/forum/lib.php",
            "public/mod/assign/db/",
            PathKind::Dirroot,
            PathCategory::StaticDifferentComponent,
            Some(("mod_assign", "/db/")),
        );
        assert_eq!(
            decide(&reference, &entry_points(&[])),
            Some("\\core\\component::component_path('mod_assign', 'db/')".to_string())
        );
    }

    #[test]
    fn different_component_root_target_rewrites_to_cfg_root() {
        let reference = reference(
            "public/admin/tool/xmldb/index.php",
            "public/backup/backup.class.php",
            PathKind::Dirroot,
            PathCategory::StaticDifferentComponent,
            Some(("root", "/public/backup/backup.class.php")),
        );
        assert_eq!(
            decide(&reference, &entry_points(&[])),
            Some("$CFG->root . '/public/backup/backup.class.php'".to_string())
        );
    }

    #[test]
    fn entry_point_source_via_dirroot_to_root_owned_target_rewrites_to_cfg_root() {
        let reference = reference(
            "public/admin/tool/xmldb/index.php",
            "public/backup/backup.class.php",
            PathKind::Dirroot,
            PathCategory::StaticDifferentComponent,
            Some(("root", "/public/backup/backup.class.php")),
        );
        let entry_points = entry_points(&["public/admin/tool/xmldb/index.php"]);
        assert_eq!(
            decide(&reference, &entry_points),
            Some("$CFG->root . '/public/backup/backup.class.php'".to_string())
        );
    }

    #[test]
    fn entry_point_source_already_dir_relative_to_root_owned_target_is_still_forced_to_cfg_root() {
        let reference = reference(
            "public/admin/tool/xmldb/index.php",
            "public/backup/backup.class.php",
            PathKind::Dir,
            PathCategory::StaticDifferentComponent,
            Some(("root", "/public/backup/backup.class.php")),
        );
        let entry_points = entry_points(&["public/admin/tool/xmldb/index.php"]);
        assert_eq!(
            decide(&reference, &entry_points),
            Some("$CFG->root . '/public/backup/backup.class.php'".to_string())
        );
    }

    #[test]
    fn entry_point_source_to_another_entry_point_via_dirroot_rewrites_to_cfg_root() {
        let reference = reference(
            "public/admin/tool/xmldb/index.php",
            "public/course/view.php",
            PathKind::Dirroot,
            PathCategory::StaticDifferentComponent,
            Some(("root", "/public/course/view.php")),
        );
        let entry_points = entry_points(&[
            "public/admin/tool/xmldb/index.php",
            "public/course/view.php",
        ]);
        assert_eq!(
            decide(&reference, &entry_points),
            Some("$CFG->root . '/public/course/view.php'".to_string())
        );
    }

    /// This is the "same component" category, not "different component" — `backup.php` is itself
    /// root-owned, same as its target — but an entry point never gets `__DIR__`-relative addressing
    /// regardless of category: it still rewrites to `$CFG->root`, exactly as it would if the entry
    /// point instead belonged to a real component (see the "different component, root" case above).
    #[test]
    fn entry_point_source_to_root_owned_target_rewrites_to_cfg_root() {
        let reference = reference(
            "public/backup/backup.php",
            "public/backup/moodle2/backup_plan_builder.class.php",
            PathKind::Dirroot,
            PathCategory::StaticSameComponent,
            Some((
                "root",
                "/public/backup/moodle2/backup_plan_builder.class.php",
            )),
        );
        let entry_points = entry_points(&["public/backup/backup.php"]);
        assert_eq!(
            decide(&reference, &entry_points),
            Some("$CFG->root . '/public/backup/moodle2/backup_plan_builder.class.php'".to_string())
        );
    }

    #[test]
    fn entry_point_source_to_ordinary_component_code_rewrites_to_component_path_even_in_its_own_component()
     {
        let reference = reference(
            "public/mod/forum/view.php",
            "public/mod/forum/lib.php",
            PathKind::Dirroot,
            PathCategory::StaticSameComponent,
            Some(("mod_forum", "/lib.php")),
        );
        let entry_points = entry_points(&["public/mod/forum/view.php"]);
        assert_eq!(
            decide(&reference, &entry_points),
            Some("\\core\\component::component_path('mod_forum', 'lib.php')".to_string())
        );
    }

    /// A target file that happens to itself be a page, CLI script or bootstrap file has exactly one
    /// real copy once split up — the one a (not yet written) Composer plugin places back at its
    /// original path — regardless of which component the reference resolves it into for packaging
    /// purposes. So it's addressed the same way a `root`-owned target is, `$CFG->root`, never
    /// `component_path()`, even though this is an ordinary different-component reference in every
    /// other respect.
    #[test]
    fn entry_point_target_in_a_different_component_rewrites_to_cfg_root_not_component_path() {
        let reference = reference(
            "public/admin/tool/phpunit/cli/util.php",
            "public/lib/setup.php",
            PathKind::Dirroot,
            PathCategory::StaticDifferentComponent,
            Some(("core", "/setup.php")),
        );
        assert_eq!(
            decide(&reference, &entry_points(&["public/lib/setup.php"])),
            Some("$CFG->root . '/public/lib/setup.php'".to_string())
        );
    }

    /// The tricky case: unlike two `root`-owned files (which travel together, so a same-component
    /// reference between them stays `__DIR__`-relative), a source and an entry-point target being
    /// nominally in the *same* real component does not mean they end up in the same place — the
    /// source's file still gets packaged, but the target's real copy is pinned to the project root
    /// by the plugin. `$CFG->root` is still correct here even though the source is not itself an
    /// entry point.
    #[test]
    fn entry_point_target_in_the_same_component_rewrites_to_cfg_root_not_dir_relative() {
        let reference = reference(
            "public/lib/classes/deep/nested/path/x.php",
            "public/lib/setup.php",
            PathKind::Dirroot,
            PathCategory::StaticSameComponent,
            Some(("core", "/setup.php")),
        );
        assert_eq!(
            decide(&reference, &entry_points(&["public/lib/setup.php"])),
            Some("$CFG->root . '/public/lib/setup.php'".to_string())
        );
    }

    /// The entry-point-target rule takes priority regardless of whether the source is *also* an
    /// entry point — both routes were already going to land on `$CFG->root`, but this confirms they
    /// don't interact to produce anything unexpected (e.g. a doubled path).
    #[test]
    fn entry_point_target_rule_applies_even_when_source_is_also_an_entry_point() {
        let reference = reference(
            "public/admin/tool/phpunit/cli/util.php",
            "public/lib/phpunit/bootstrap.php",
            PathKind::Dirroot,
            PathCategory::StaticDifferentComponent,
            Some(("core", "/phpunit/bootstrap.php")),
        );
        let entry_points = entry_points(&[
            "public/admin/tool/phpunit/cli/util.php",
            "public/lib/phpunit/bootstrap.php",
        ]);
        assert_eq!(
            decide(&reference, &entry_points),
            Some("$CFG->root . '/public/lib/phpunit/bootstrap.php'".to_string())
        );
    }

    /// A target that is both `root`-owned *and* an entry point (e.g. a root-level CLI script) is
    /// already handled correctly by the existing `root`-owned-target logic — the new entry-point
    /// check only ever applies to a target resolving to a *real* component, so it stays out of the
    /// way here rather than layering a second, redundant rule on top.
    #[test]
    fn root_owned_entry_point_target_is_handled_by_the_existing_root_logic() {
        let reference = reference(
            "public/course/view.php",
            "public/r.php",
            PathKind::Dirroot,
            PathCategory::StaticDifferentComponent,
            Some(("root", "/public/r.php")),
        );
        assert_eq!(
            decide(&reference, &entry_points(&["public/r.php"])),
            Some("$CFG->root . '/public/r.php'".to_string())
        );
    }

    #[test]
    fn dirname_covers_arbitrarily_deep_nesting() {
        let deep_reference = reference(
            "public/lib/classes/deep/nested/path/x.php",
            "public/lib/setup.php",
            PathKind::Dirroot,
            PathCategory::StaticSameComponent,
            Some(("core", "/setup.php")),
        );
        assert_eq!(
            decide(&deep_reference, &entry_points(&[])),
            Some("dirname(__DIR__, 4) . '/setup.php'".to_string())
        );
    }

    /// A bare-literal reference to config.php rewrites to a `__DIR__`-relative expression — the
    /// same mechanism as a same-component reference, just reached via `Config`'s own branch in
    /// `decide` rather than the same-component one. This is the exact real-world shape of
    /// `public/group/assign.php`'s own `require_once('../config.php')`.
    #[test]
    fn config_reference_rewrites_to_a_dir_relative_path() {
        let reference = reference(
            "public/group/assign.php",
            "public/config.php",
            PathKind::RequireLiteral,
            PathCategory::Config,
            None,
        );
        assert_eq!(
            decide(&reference, &entry_points(&[])),
            Some("dirname(__DIR__) . '/config.php'".to_string())
        );
    }

    /// Unlike every other rule in `decide`, a config.php reference gets the same replacement
    /// whether or not the referencing file is an entry point — there is no `component_path()`/
    /// `$CFG->root` alternative being given up either way, so entry-point status has nothing to
    /// change here.
    #[test]
    fn config_reference_rewrite_is_unaffected_by_entry_point_status() {
        let reference = reference(
            "public/group/assign.php",
            "public/config.php",
            PathKind::RequireLiteral,
            PathCategory::Config,
            None,
        );
        let entry_points = entry_points(&["public/group/assign.php"]);
        assert_eq!(
            decide(&reference, &entry_points),
            Some("dirname(__DIR__) . '/config.php'".to_string())
        );
    }

    /// A config.php reference that's already `__DIR__`-relative has nothing left to decide.
    #[test]
    fn config_reference_already_dir_relative_has_nothing_to_decide() {
        let reference = reference(
            "public/group/assign.php",
            "public/config.php",
            PathKind::Dir,
            PathCategory::Config,
            None,
        );
        assert_eq!(decide(&reference, &entry_points(&[])), None);
    }

    /// `reference`, but as `VariableOnly` with `mono_path_expr` set to `mono_path_expr` — the field
    /// `decide` and `still_needs_rewrite` actually key off for this category; `target` is always
    /// `None`, the same as every real `VariableOnly` reference (it didn't resolve to one).
    fn variable_only_reference(file: &str, mono_path_expr: &str) -> CategorisedReference {
        let mut reference = reference(
            file,
            "public{$includefile}",
            PathKind::Dirroot,
            PathCategory::VariableOnly,
            None,
        );
        reference.result.mono_path_expr = mono_path_expr.to_string();
        reference
    }

    #[test]
    fn variable_only_rewrites_to_from_mono_path() {
        let reference = variable_only_reference("public/mod/quiz/view.php", "$includefile");
        assert_eq!(
            decide(&reference, &entry_points(&[])),
            Some("\\core\\component::from_mono_path($includefile)".to_string())
        );
    }

    /// The exact source text after `$CFG->dirroot` is used verbatim, whatever shape it is — a
    /// leading `/` baked into a literal piece of the concatenation is neither added nor trimmed.
    #[test]
    fn variable_only_preserves_the_exact_remainder_including_its_own_leading_separator() {
        let reference =
            variable_only_reference("public/mod/quiz/view.php", "'/mod/' . $type . '/lib.php'");
        assert_eq!(
            decide(&reference, &entry_points(&[])),
            Some("\\core\\component::from_mono_path('/mod/' . $type . '/lib.php')".to_string())
        );
    }

    /// A `variable-only` reference not actually rooted at `$CFG->dirroot` (e.g. `$CFG->libdir`
    /// instead) has an empty `mono_path_expr` — `from_mono_path()` has no equivalent for it, so
    /// there is nothing to decide.
    #[test]
    fn variable_only_with_no_mono_path_expr_has_nothing_to_decide() {
        let reference = variable_only_reference("public/mod/quiz/view.php", "");
        assert_eq!(decide(&reference, &entry_points(&[])), None);
    }

    /// `reference`, but with `code` set to the exact text already sitting at the reference's
    /// location — the thing [`still_needs_rewrite`] compares [`decide`]'s answer against.
    fn reference_with_code(
        file: &str,
        real_path: &str,
        category: PathCategory,
        target: Option<(&str, &str)>,
        code: &str,
    ) -> CategorisedReference {
        let mut reference = reference(file, real_path, PathKind::Dirroot, category, target);
        reference.result.code = code.to_string();
        reference
    }

    /// A category step 3 has no rule for yet (anything other than `Component`, `Config`,
    /// `PreComponent`, `IncludePathRelative`, `StaticSameComponent`, `StaticDifferentComponent` or
    /// `VariableOnly`) always still needs rewriting — not implemented here isn't the same claim as
    /// no longer depending on `$CFG->dirroot`/`$CFG->libdir`, and this reference plainly still does.
    #[test]
    fn category_with_no_rule_yet_always_still_needs_rewriting() {
        let reference = reference_with_code(
            "public/mod/quiz/view.php",
            "public/mod/quiz",
            PathCategory::PluginTypeRoot,
            None,
            "$CFG->dirroot . '/mod'",
        );
        assert!(still_needs_rewrite(&reference, &entry_points(&[])));
    }

    /// `Config` is the one category left untouched by step 3 that still has a real "done" state:
    /// the reference to config.php itself can never be rewritten to use `component_path()`, but it
    /// can already be `__DIR__`-relative rather than a bare literal or `$CFG->dirroot`/`root` — see
    /// `config_reference_on_a_bare_literal_still_needs_rewriting` below for the other state.
    #[test]
    fn config_reference_already_dir_relative_does_not_still_need_rewriting() {
        let reference = reference(
            "public/mod/quiz/view.php",
            "public/config.php",
            PathKind::Dir,
            PathCategory::Config,
            None,
        );
        assert!(!still_needs_rewrite(&reference, &entry_points(&[])));
    }

    #[test]
    fn config_reference_on_a_bare_literal_still_needs_rewriting() {
        let reference = reference(
            "public/mod/quiz/view.php",
            "public/config.php",
            PathKind::RequireLiteral,
            PathCategory::Config,
            None,
        );
        assert!(still_needs_rewrite(&reference, &entry_points(&[])));
    }

    #[test]
    fn config_reference_on_dirroot_still_needs_rewriting() {
        let reference = reference(
            "public/mod/quiz/view.php",
            "public/config.php",
            PathKind::Dirroot,
            PathCategory::Config,
            None,
        );
        assert!(still_needs_rewrite(&reference, &entry_points(&[])));
    }

    /// `PreComponent` never needs rewriting, regardless of its current form — the bootstrap
    /// sequence it lives in can't be split out under this project's approach at all (there's no
    /// `core\component` oracle available yet to split it *through*), so whatever the reference is
    /// written as today is already permanent, unlike the categories this tool hasn't implemented a
    /// rule for yet. Unlike `Config`, this holds even for a bare literal or a `$CFG->dirroot`/`root`
    /// reference, not just an already-`__DIR__`-relative one.
    #[test]
    fn pre_component_reference_already_dir_relative_does_not_still_need_rewriting() {
        let reference = reference(
            "public/lib/setup.php",
            "public/lib/component.php",
            PathKind::Dir,
            PathCategory::PreComponent,
            None,
        );
        assert!(!still_needs_rewrite(&reference, &entry_points(&[])));
    }

    #[test]
    fn pre_component_reference_on_a_bare_literal_does_not_still_need_rewriting() {
        let reference = reference(
            "public/lib/setup.php",
            "public/lib/component.php",
            PathKind::RequireLiteral,
            PathCategory::PreComponent,
            None,
        );
        assert!(!still_needs_rewrite(&reference, &entry_points(&[])));
    }

    #[test]
    fn pre_component_reference_on_dirroot_does_not_still_need_rewriting() {
        let reference = reference(
            "public/lib/setup.php",
            "public/lib/component.php",
            PathKind::Dirroot,
            PathCategory::PreComponent,
            None,
        );
        assert!(!still_needs_rewrite(&reference, &entry_points(&[])));
    }

    #[test]
    fn eligible_reference_already_matching_decides_output_does_not_still_need_rewriting() {
        let reference = reference_with_code(
            "public/mod/quiz/report/overview/report.php",
            "public/mod/quiz/lib.php",
            PathCategory::StaticSameComponent,
            Some(("mod_quiz", "/lib.php")),
            "dirname(__DIR__, 2) . '/lib.php'",
        );
        assert!(!still_needs_rewrite(&reference, &entry_points(&[])));
    }

    #[test]
    fn eligible_reference_not_yet_matching_decides_output_still_needs_rewriting() {
        let reference = reference_with_code(
            "public/mod/quiz/report/overview/report.php",
            "public/mod/quiz/lib.php",
            PathCategory::StaticSameComponent,
            Some(("mod_quiz", "/lib.php")),
            "$CFG->dirroot . '/mod/quiz/lib.php'",
        );
        assert!(still_needs_rewrite(&reference, &entry_points(&[])));
    }

    /// Same-component pointing at the "root" placeholder still needs rewriting for as long as it's
    /// sitting on `$CFG->dirroot` — see `same_component_root_placeholder_rewrites_to_a_dir_relative_path`
    /// above — the same as any other eligible reference that hasn't caught up with `decide` yet.
    #[test]
    fn eligible_root_placeholder_reference_still_on_dirroot_still_needs_rewriting() {
        let reference = reference_with_code(
            "public/backup/util.php",
            "public/backup/backup.class.php",
            PathCategory::StaticSameComponent,
            Some(("root", "/public/backup/backup.class.php")),
            "$CFG->dirroot . '/backup/backup.class.php'",
        );
        assert!(still_needs_rewrite(&reference, &entry_points(&[])));
    }

    #[test]
    fn eligible_root_placeholder_reference_already_dir_relative_does_not_still_need_rewriting() {
        let reference = reference_with_code(
            "public/backup/util.php",
            "public/backup/backup.class.php",
            PathCategory::StaticSameComponent,
            Some(("root", "/public/backup/backup.class.php")),
            "__DIR__ . '/backup.class.php'",
        );
        assert!(!still_needs_rewrite(&reference, &entry_points(&[])));
    }

    #[test]
    fn variable_only_reference_already_matching_decides_output_does_not_still_need_rewriting() {
        let mut reference = variable_only_reference("public/mod/quiz/view.php", "$includefile");
        reference.result.code = "\\core\\component::from_mono_path($includefile)".to_string();
        assert!(!still_needs_rewrite(&reference, &entry_points(&[])));
    }

    #[test]
    fn variable_only_reference_not_yet_matching_decides_output_still_needs_rewriting() {
        let mut reference = variable_only_reference("public/mod/quiz/view.php", "$includefile");
        reference.result.code = "$CFG->dirroot . $includefile".to_string();
        assert!(still_needs_rewrite(&reference, &entry_points(&[])));
    }

    /// A `variable-only` reference rooted at something other than `$CFG->dirroot` (so
    /// `mono_path_expr` is empty and `decide` has no rewrite for it) always still needs rewriting —
    /// the same "not implemented, not the same as done" fallback every other not-yet-handled
    /// category gets, not silently marked done just because `decide` happens to return `None`.
    #[test]
    fn variable_only_reference_with_no_mono_path_expr_always_still_needs_rewriting() {
        let reference = variable_only_reference("public/mod/quiz/view.php", "");
        assert!(still_needs_rewrite(&reference, &entry_points(&[])));
    }

    /// `Component` is always done, regardless of what the current code looks like — the code in
    /// `core\component`'s own source file and its unit test must never be flagged as outstanding
    /// work, since this tool will never touch it.
    #[test]
    fn component_reference_never_needs_rewriting() {
        let reference = reference_with_code(
            "public/lib/classes/component.php",
            "public/lib/classes/component.php",
            PathCategory::Component,
            None,
            "$CFG->dirroot . $path",
        );
        assert!(!still_needs_rewrite(&reference, &entry_points(&[])));
    }

    /// `IncludePathRelative` never needs rewriting, but for a different reason than `Component`:
    /// there is no `$CFG->dirroot`/`libdir` dependency here at all to flag as outstanding — the
    /// bare literal is resolved by PHP's own include-path search, not by anything this project
    /// rewrites.
    #[test]
    fn include_path_relative_reference_never_needs_rewriting() {
        let reference = reference_with_code(
            "public/lib/form/checkbox.php",
            "public/lib/form/HTML/QuickForm/checkbox.php",
            PathCategory::IncludePathRelative,
            None,
            "'HTML/QuickForm/checkbox.php'",
        );
        assert!(!still_needs_rewrite(&reference, &entry_points(&[])));
    }
}
