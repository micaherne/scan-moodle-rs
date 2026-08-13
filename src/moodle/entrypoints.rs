//! Classifies files as Moodle "entry points" (pages and CLI scripts) or "bootstrap" files, purely
//! from the require/include graph a codebase-wide [`crate::path_finder::find_paths`] scan
//! produces — no filesystem access of its own.
//!
//! An entry point is any file that requires config.php (the real file a user or the installer
//! creates at the repository root, or its `public/config.php` forwarding copy on a post-5.1
//! layout) directly, or that requires another entry point instead of config.php itself (e.g.
//! `lib/ajax/service-nologin.php`).
//!
//! A bootstrap file is one that could never be rewritten to use `core\component` to locate its
//! own files, because it necessarily runs before component.php exists to be loaded:
//! component.php itself, anything that requires it, anything that requires *that*, and so on.
//!
//! config.php never exists in a bare checkout, so two hops of that chain can never be discovered
//! by scanning it and must be injected by hand (see `add_synthetic_config_chain`); by default
//! those hops are included, which transitively pulls every entry point into the bootstrap set too
//! (every page reaches component.php via config.php) — `bootstrap_only` omits them, and reports
//! bootstrap files only, leaving just the files that reach component.php without going through
//! config.php at all (e.g. install.php). Outside `bootstrap_only`, a file that is also an entry
//! point is reported as such rather than as bootstrap: knowing it's a page is more useful than
//! knowing it eventually reaches component.php — *unless* it does real file-loading work of its
//! own before its own config.php line (see `bootstrap_boundary_for_pre_config_work`), such as
//! public/lib/phpunit/bootstrap.php's two requires before its config.php require: those requires
//! run with no possible access to core\component yet, regardless of what they themselves happen to
//! lead to, which earns "bootstrap" outright, and unlike the general precedence rule, that is not
//! affected by `bootstrap_only` — it comes entirely from real edges, no synthetic chain involved.
//! The reported boundary line is still the file's own config.php line, not the earlier require
//! that triggered the rule — that earlier line only proves the rule applies, it is not where
//! access to core\component actually begins.
//!
//! A require/include edge is recorded regardless of whether it sits at the top level of the file
//! or nested inside a function or method body (e.g. `\core\router\util::load_full_moodle()`,
//! called only from deep inside the router's error-handling middleware, manually requires
//! lib/setup.php as a fallback when a route's own config.php bootstrap was aborted early) — this
//! tool does not trace which functions get called, by design, so it cannot tell "requires this
//! unconditionally on load" from "contains code that would require this if some function were
//! called". This is safe in the one way that matters: a require/include edge can only propagate
//! a false "bootstrap" label onto some *other*, unrelated file if that file directly requires the
//! one containing the nested statement — and a file whose only route to `\core\router\util` (or
//! any other ordinary, autoloaded class) is a `use` statement or a `Foo::bar()` call is invisible
//! to this graph entirely, exactly like any other class reference: those aren't require/include
//! constructs, so `find_paths` never emits a path for them, and no edge is ever created. A false
//! propagation would need something to `require`/`include` the file directly instead of relying on
//! autoloading — which, for an ordinary class file, is not a pattern this codebase uses.

use std::collections::{HashMap, HashSet};
use std::fmt;

use crate::path_finder::{PathNotation, PathResult};

/// The require/include graph in the direction it's actually executed: each file maps to the
/// files it requires/includes, with the line of that require/include (`None` for the synthetic
/// config.php hops added by [`add_synthetic_config_chain`], which correspond to no real line).
type ForwardEdges = HashMap<String, Vec<(String, Option<u32>)>>;

/// The require/include graph inverted: each file maps to the files that require/include it.
type ReverseEdges = HashMap<String, Vec<String>>;

/// The four constructs that actually execute their target at the point they appear — the only
/// ones that can anchor a bootstrap or entry-point chain. A path merely mentioned elsewhere (an
/// existence check, an error-message string, a test mocking a request path, an array of patterns
/// to detect in a stack trace) is not evidence that the file actually loads it.
const REQUIRE_LIKE: [&str; 4] = ["require", "require_once", "include", "include_once"];

/// The kind of file a [`FileClassification`] describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntrypointKind {
    /// Reaches component.php (directly or transitively) without itself being an entry point.
    Bootstrap,
    /// An entry point living under a directory named 'cli' (the heuristic stand-in for actually
    /// checking `define('CLI_SCRIPT', true)`).
    Cli,
    /// An entry point everywhere else.
    Page,
}

impl fmt::Display for EntrypointKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Bootstrap => "bootstrap",
            Self::Cli => "cli",
            Self::Page => "page",
        })
    }
}

/// One file's classification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileClassification {
    /// Repository-root-relative path, no leading slash (matches [`PathResult::real_path`]).
    pub file: String,
    pub kind: EntrypointKind,
    /// The line, within `file`, of the require/include that anchors it into the bootstrap chain.
    /// `None` for entry points (not required), and for a bootstrap file reachable only through a
    /// synthetic hop of the config.php chain, which carries no real line.
    pub line: Option<u32>,
}

/// The repository-root-relative paths a real Moodle checkout's config.php can live at (the
/// `public/config.php` forwarding copy on a post-5.1 layout, and the root `config.php` a user or
/// the installer creates) — neither exists in a bare checkout, so neither is ever discovered by
/// scanning it. Used both to seed the entry-point search in [`classify`] and, standalone, by
/// [`crate::moodle::categorise`] to recognise a reference that targets config.php itself.
pub fn config_locations(notation: &PathNotation) -> HashSet<String> {
    HashSet::from([
        dirroot_relative(notation, "config.php"),
        "config.php".to_string(),
    ])
}

/// The repository-root-relative paths of `core\component`'s own source file and its unit test —
/// Moodle's original, monolithic implementation of component/path resolution, which the patches
/// applied in step 1 of the rewrite process add `component_path()`/`from_mono_path()` to. Every
/// rewrite this project produces ultimately calls into that code at run time, so it must never be
/// rewritten itself. Used standalone by [`crate::moodle::categorise`] to recognise a reference that
/// appears in either file, regardless of what it targets.
pub fn component_locations(notation: &PathNotation) -> HashSet<String> {
    HashSet::from([
        dirroot_relative(notation, "lib/classes/component.php"),
        dirroot_relative(notation, "lib/tests/component_test.php"),
    ])
}

/// Classifies every file in `files` — the codebase-wide output of [`crate::path_finder::find_paths`],
/// as `(file, that file's results)` pairs — as an entry point or a bootstrap file.
pub fn classify(
    files: &[(String, Vec<PathResult>)],
    notation: &PathNotation,
    bootstrap_only: bool,
) -> Vec<FileClassification> {
    let mut forward = build_forward_edges(files);

    let component_php = dirroot_relative(notation, "lib/classes/component.php");
    let config_php_public = dirroot_relative(notation, "config.php");
    let config_php_root = "config.php".to_string();
    let setup_php_root = "lib/setup.php".to_string();

    if !bootstrap_only {
        add_synthetic_config_chain(
            &mut forward,
            &config_php_public,
            &config_php_root,
            &setup_php_root,
        );
    }

    let reverse = build_reverse_edges(&forward);

    // config.php itself is not an entry point — it is what entry points require — so the seeds
    // are excluded from the reported set; only files reverse-reachable *from* them (i.e. that
    // actually require one of them, directly or transitively) count.
    let config_seeds = config_locations(notation);
    let entrypoints: HashSet<String> = reverse_closure(config_seeds.iter().cloned(), &reverse)
        .into_iter()
        .filter(|file| !config_seeds.contains(file))
        .collect();
    let bootstrap = reverse_closure([component_php], &reverse);

    let real_files: HashSet<&str> = files.iter().map(|(file, _)| file.as_str()).collect();

    let mut classifications: Vec<FileClassification> = Vec::new();

    for file in &entrypoints {
        if !real_files.contains(file.as_str()) {
            continue;
        }
        // An entry point that also does real file-loading work *before* its own config.php line
        // (e.g. public/lib/phpunit/bootstrap.php's two requires before its config.php require) did
        // that work with no possible access to core\component, whatever those requires themselves
        // go on to lead to — that earns "bootstrap" regardless of the file also being an entry
        // point, and unlike the general entry-point/bootstrap precedence below, it is not
        // suppressed by `bootstrap_only`: it comes entirely from real edges.
        if let Some(line) = bootstrap_boundary_for_pre_config_work(file, &forward, &config_seeds) {
            classifications.push(FileClassification {
                file: file.clone(),
                kind: EntrypointKind::Bootstrap,
                line: Some(line),
            });
        } else if !bootstrap_only {
            classifications.push(FileClassification {
                file: file.clone(),
                kind: if is_cli_path(file) {
                    EntrypointKind::Cli
                } else {
                    EntrypointKind::Page
                },
                line: None,
            });
        }
    }

    for file in &bootstrap {
        // A file that is itself an entry point is more usefully reported as a page or CLI script
        // than as "bootstrap" — see the module doc comment. `entrypoints` is computed the same way
        // regardless of `bootstrap_only` (it never depended on the synthetic config.php chain to
        // begin with), so this exclusion applies unconditionally — `bootstrap_only` only decides
        // whether entry-point rows are *also* emitted, above, not whether they count here.
        if entrypoints.contains(file) || !real_files.contains(file.as_str()) {
            continue;
        }
        classifications.push(FileClassification {
            file: file.clone(),
            kind: EntrypointKind::Bootstrap,
            line: anchor_line(file, &bootstrap, &forward),
        });
    }

    classifications.sort_by(|a, b| a.file.cmp(&b.file));
    classifications
}

/// A repository-root-relative path (no leading slash) to `suffix` inside dirroot, computed
/// directly from `notation`'s dirroot segment rather than through glyph notation — glyph ('@'/'#')
/// is purely a human-facing rendering and must not be parsed back for program logic.
fn dirroot_relative(notation: &PathNotation, suffix: &str) -> String {
    format!("{}/{suffix}", notation.dirroot_segment())
        .trim_start_matches('/')
        .to_string()
}

/// Neither hop below is discoverable by scanning: config.php never exists in a bare checkout (a
/// user or the installer creates it), and the deferred `require_once($configfile)` in
/// public/config.php resolves its target through a variable, which the path finder cannot trace
/// back to a recognised root (it does record the preceding `$configfile = __DIR__ .
/// '/../config.php'` assignment, but not tagged as a require, since it isn't one — see
/// [`build_forward_edges`]). Both hops are fixed facts about Moodle's own bootstrap sequence
/// rather than anything read from a specific file, so neither carries a line number.
fn add_synthetic_config_chain(
    forward: &mut ForwardEdges,
    config_php_public: &str,
    config_php_root: &str,
    setup_php_root: &str,
) {
    // On a pre-5.1 layout (no public/ split) these coincide, and the edge below would be a
    // meaningless self-loop.
    if config_php_public != config_php_root {
        forward
            .entry(config_php_public.to_string())
            .or_default()
            .push((config_php_root.to_string(), None));
    }
    forward
        .entry(config_php_root.to_string())
        .or_default()
        .push((setup_php_root.to_string(), None));
}

fn build_forward_edges(files: &[(String, Vec<PathResult>)]) -> ForwardEdges {
    let mut forward: ForwardEdges = HashMap::new();
    for (file, results) in files {
        for result in results {
            if result
                .parent
                .as_deref()
                .is_some_and(|parent| REQUIRE_LIKE.contains(&parent))
            {
                forward
                    .entry(file.clone())
                    .or_default()
                    .push((result.real_path.clone(), Some(result.line)));
            }
        }
    }
    forward
}

fn build_reverse_edges(forward: &ForwardEdges) -> ReverseEdges {
    let mut reverse: ReverseEdges = HashMap::new();
    for (source, edges) in forward {
        for (target, _) in edges {
            reverse
                .entry(target.clone())
                .or_default()
                .push(source.clone());
        }
    }
    reverse
}

/// Every file that reaches one of `seeds` by requiring/including it, directly or transitively.
fn reverse_closure(
    seeds: impl IntoIterator<Item = String>,
    reverse: &ReverseEdges,
) -> HashSet<String> {
    let mut closure: HashSet<String> = seeds.into_iter().collect();
    let mut queue: Vec<String> = closure.iter().cloned().collect();
    while let Some(target) = queue.pop() {
        for source in reverse.get(&target).into_iter().flatten() {
            if closure.insert(source.clone()) {
                queue.push(source.clone());
            }
        }
    }
    closure
}

/// `file`'s own line requiring config.php — the point after which it gains access to
/// core\component, transitively, the same as any other bootstrap file's boundary — but only when
/// at least one of `file`'s *other* require/include edges comes before that line, i.e. `file` does
/// real file-loading work of its own before config.php (and so before core\component) could
/// possibly be available to it yet, regardless of what that earlier require itself goes on to lead
/// to. That earlier line is only evidence that this rule applies at all; it is not itself the
/// boundary, which is always `file`'s own config.php line. `None` if `file` has no edge into
/// `config_seeds` at all (not an entry point), or if nothing precedes it (no such earlier work).
fn bootstrap_boundary_for_pre_config_work(
    file: &str,
    forward: &ForwardEdges,
    config_seeds: &HashSet<String>,
) -> Option<u32> {
    let edges = forward.get(file)?;
    let config_line = edges
        .iter()
        .filter(|(target, _)| config_seeds.contains(target))
        .filter_map(|(_, line)| *line)
        .min()?;
    edges
        .iter()
        .filter_map(|(_, line)| *line)
        .any(|line| line < config_line)
        .then_some(config_line)
}

/// The smallest line, among `file`'s own require/include statements, that points at something
/// already in `closure` — i.e. where in this file the chain toward the bootstrap seed begins.
/// `None` when every such edge is a synthetic config.php hop (no real line) or `file` is the seed
/// itself (component.php does not require itself).
fn anchor_line(file: &str, closure: &HashSet<String>, forward: &ForwardEdges) -> Option<u32> {
    forward
        .get(file)?
        .iter()
        .filter(|(target, _)| closure.contains(target))
        .filter_map(|(_, line)| *line)
        .min()
}

/// The heuristic stand-in for checking `define('CLI_SCRIPT', true)`: whether any directory
/// component of `file` is named 'cli'.
fn is_cli_path(file: &str) -> bool {
    file.split('/').any(|segment| segment == "cli")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(real_path: &str, parent: Option<&str>, line: u32) -> PathResult {
        PathResult {
            path: String::new(),
            real_path: real_path.to_string(),
            kind: crate::path_finder::PathKind::Dirroot,
            line,
            code: String::new(),
            parent: parent.map(str::to_string),
            start_pos: None,
            end_pos: None,
            separator: String::new(),
            mono_path_expr: String::new(),
        }
    }

    fn notation() -> PathNotation {
        PathNotation::new("public/")
    }

    #[test]
    fn direct_component_require_is_bootstrap_with_its_line() {
        let files = vec![
            (
                "public/lib/setup.php".to_string(),
                vec![result(
                    "public/lib/classes/component.php",
                    Some("require_once"),
                    442,
                )],
            ),
            ("public/lib/classes/component.php".to_string(), vec![]),
        ];
        let classifications = classify(&files, &notation(), true);
        assert_eq!(
            classifications,
            vec![
                FileClassification {
                    file: "public/lib/classes/component.php".to_string(),
                    kind: EntrypointKind::Bootstrap,
                    line: None,
                },
                FileClassification {
                    file: "public/lib/setup.php".to_string(),
                    kind: EntrypointKind::Bootstrap,
                    line: Some(442)
                },
            ]
        );
    }

    /// A file whose require sits inside a function/method (only executed if that function is
    /// called, e.g. `\core\router\util::load_full_moodle()`) is still classified as bootstrap
    /// itself — this tool does not distinguish top-level code from function bodies. What matters
    /// is that this does not propagate: a caller that only ever references the class (the
    /// autoloaded, ordinary way — no require/include of the file at all, hence no PathResult, hence
    /// no edge) is completely unaffected, exactly as if the nested require did not exist.
    #[test]
    fn a_require_nested_in_a_method_does_not_leak_to_a_caller_that_only_uses_the_class() {
        let files = vec![
            (
                "public/lib/classes/router/util.php".to_string(),
                vec![result("public/lib/setup.php", Some("require"), 361)],
            ),
            (
                "public/lib/setup.php".to_string(),
                vec![result(
                    "public/lib/classes/component.php",
                    Some("require_once"),
                    442,
                )],
            ),
            ("public/lib/classes/component.php".to_string(), vec![]),
            // The caller: no PathResult at all, since `\core\router\util::load_full_moodle();` is a
            // plain method call, not a require/include — find_paths would never emit one for it.
            (
                "public/lib/classes/router/middleware/error_handling_middleware.php".to_string(),
                vec![],
            ),
        ];
        let classifications = classify(&files, &notation(), true);
        assert_eq!(
            classifications
                .iter()
                .find(|c| c.file == "public/lib/classes/router/util.php")
                .unwrap()
                .kind,
            EntrypointKind::Bootstrap
        );
        assert!(classifications.iter().all(
            |c| c.file != "public/lib/classes/router/middleware/error_handling_middleware.php"
        ));
    }

    /// public/lib/phpunit/bootstrap.php: two requires before its own config.php require. Neither
    /// target reaches component.php on its own (they're self-contained PHPUnit helper libraries),
    /// so this is only detectable from *when* they happen relative to config.php, not *what they
    /// lead to* — having at least one of them earns "bootstrap", overriding the entry-point kind
    /// this file would otherwise get purely from requiring config.php. The reported boundary is
    /// still the file's own config.php line (86), not either of the two requires that triggered
    /// the rule — access to core\component only actually begins once config.php has run.
    #[test]
    fn entrypoint_with_a_require_before_its_own_config_line_is_bootstrap() {
        let files = vec![(
            "public/lib/phpunit/bootstrap.php".to_string(),
            vec![
                result(
                    "public/lib/phpunit/bootstraplib.php",
                    Some("require_once"),
                    51,
                ),
                result("public/lib/testing/lib.php", Some("require_once"), 52),
                result("public/config.php", Some("require"), 86),
            ],
        )];
        let classifications = classify(&files, &notation(), false);
        assert_eq!(
            classifications,
            vec![FileClassification {
                file: "public/lib/phpunit/bootstrap.php".to_string(),
                kind: EntrypointKind::Bootstrap,
                line: Some(86),
            }]
        );
    }

    /// The same file under `bootstrap_only`: this rule comes entirely from real edges (no synthetic
    /// config.php chain involved), so it must still apply and the row must still appear, unlike an
    /// ordinary entry point (which is entirely suppressed under `bootstrap_only`).
    #[test]
    fn entrypoint_with_a_require_before_its_own_config_line_still_shows_under_bootstrap_only() {
        let files = vec![(
            "public/lib/phpunit/bootstrap.php".to_string(),
            vec![
                result(
                    "public/lib/phpunit/bootstraplib.php",
                    Some("require_once"),
                    51,
                ),
                result("public/config.php", Some("require"), 86),
            ],
        )];
        let classifications = classify(&files, &notation(), true);
        assert_eq!(
            classifications,
            vec![FileClassification {
                file: "public/lib/phpunit/bootstrap.php".to_string(),
                kind: EntrypointKind::Bootstrap,
                line: Some(86),
            }]
        );
    }

    /// A require that comes *after* config.php (e.g. the theme asset servers, which require
    /// config.php first and only fall back to a direct lib/setup.php require if that aborted
    /// early) must not trigger the rule — only requires genuinely preceding config.php count.
    #[test]
    fn a_require_after_config_php_does_not_trigger_the_rule() {
        let files = vec![(
            "public/theme/font.php".to_string(),
            vec![
                result("public/config.php", Some("require"), 32),
                result("public/lib/setup.php", Some("require"), 138),
            ],
        )];
        let classifications = classify(&files, &notation(), false);
        assert_eq!(
            classifications,
            vec![FileClassification {
                file: "public/theme/font.php".to_string(),
                kind: EntrypointKind::Page,
                line: None
            }]
        );
    }

    #[test]
    fn transitive_bootstrap_chain_reports_each_files_own_line() {
        let files = vec![
            (
                "lib/setup.php".to_string(),
                vec![result("public/lib/setup.php", Some("require_once"), 29)],
            ),
            (
                "public/lib/setup.php".to_string(),
                vec![result(
                    "public/lib/classes/component.php",
                    Some("require_once"),
                    442,
                )],
            ),
            ("public/lib/classes/component.php".to_string(), vec![]),
        ];
        let classifications = classify(&files, &notation(), true);
        let lib_setup = classifications
            .iter()
            .find(|c| c.file == "lib/setup.php")
            .unwrap();
        assert_eq!(
            lib_setup,
            &FileClassification {
                file: "lib/setup.php".to_string(),
                kind: EntrypointKind::Bootstrap,
                line: Some(29)
            }
        );
    }

    /// A path merely mentioned in passing (an existence check, an error message, an array of
    /// patterns to detect in a stack trace, a test mocking a request path) is not evidence a file
    /// actually loads it — probing this against a real Moodle checkout found exactly this kind of
    /// false positive (e.g. a `$dangerouscode` detection array in setuplib.php, and a PHPUnit test
    /// mocking `$_SERVER['SCRIPT_FILENAME']`) once the parent constraint was relaxed, with no
    /// genuine bootstrap file gained in exchange — every real chain already has a directly-tagged
    /// require/include edge.
    #[test]
    fn require_passed_as_a_function_argument_is_not_an_edge() {
        let files = vec![
            (
                "public/mod/forum/lib.php".to_string(),
                vec![result(
                    "public/lib/classes/component.php",
                    Some("file_exists"),
                    1,
                )],
            ),
            ("public/lib/classes/component.php".to_string(), vec![]),
        ];
        let classifications = classify(&files, &notation(), true);
        assert!(
            classifications
                .iter()
                .all(|c| c.file != "public/mod/forum/lib.php")
        );
    }

    #[test]
    fn page_requiring_public_config_is_an_entrypoint() {
        let files = vec![
            (
                "public/course/view.php".to_string(),
                vec![result("public/config.php", Some("require"), 12)],
            ),
            ("public/config.php".to_string(), vec![]),
        ];
        let classifications = classify(&files, &notation(), false);
        assert_eq!(
            classifications
                .iter()
                .find(|c| c.file == "public/course/view.php")
                .unwrap(),
            &FileClassification {
                file: "public/course/view.php".to_string(),
                kind: EntrypointKind::Page,
                line: None
            },
        );
    }

    #[test]
    fn cli_directory_is_classified_as_cli() {
        let files = vec![(
            "admin/cli/cron.php".to_string(),
            vec![result("public/config.php", Some("require"), 12)],
        )];
        let classifications = classify(&files, &notation(), false);
        assert_eq!(classifications[0].kind, EntrypointKind::Cli);
    }

    #[test]
    fn file_requiring_an_entrypoint_is_also_an_entrypoint() {
        let files = vec![
            (
                "lib/ajax/service-nologin.php".to_string(),
                vec![result("public/lib/ajax/service.php", Some("require"), 5)],
            ),
            (
                "public/lib/ajax/service.php".to_string(),
                vec![result("public/config.php", Some("require"), 20)],
            ),
        ];
        let classifications = classify(&files, &notation(), false);
        assert!(classifications.iter().any(|c| c.file == "lib/ajax/service-nologin.php" && c.kind == EntrypointKind::Page));
    }

    /// A page that requires config.php directly, but *also* has a genuine, direct require of
    /// something already in the bootstrap set (as real Moodle pages routinely do once config.php
    /// has already finished loading everything — e.g. the theme asset servers, which abort
    /// config.php's own bootstrap early and then require lib/setup.php directly themselves), must
    /// still be excluded from the bootstrap report under `bootstrap_only` — the flag only omits
    /// entry-point *rows*, it does not disable entry-point *precedence*. Regression test for a bug
    /// where the precedence check was mistakenly gated on `!bootstrap_only`.
    #[test]
    fn bootstrap_only_still_excludes_entrypoints_that_also_reach_component_directly() {
        let files = vec![
            (
                "public/some/page.php".to_string(),
                vec![
                    result("public/config.php", Some("require_once"), 3),
                    result("public/lib/setup.php", Some("require_once"), 10),
                ],
            ),
            (
                "public/lib/setup.php".to_string(),
                vec![result(
                    "public/lib/classes/component.php",
                    Some("require_once"),
                    442,
                )],
            ),
            ("public/lib/classes/component.php".to_string(), vec![]),
        ];
        let classifications = classify(&files, &notation(), true);
        assert!(
            classifications
                .iter()
                .all(|c| c.file != "public/some/page.php")
        );
    }

    /// Without `bootstrap_only`, the synthetic config.php chain would make every entry point
    /// transitively reach component.php too — but entry-point classification takes precedence, so
    /// it must never show up labelled "bootstrap".
    #[test]
    fn entrypoint_precedence_hides_it_from_the_default_bootstrap_sweep() {
        let files = vec![
            (
                "public/course/view.php".to_string(),
                vec![result("public/config.php", Some("require"), 12)],
            ),
            (
                "public/lib/setup.php".to_string(),
                vec![result(
                    "public/lib/classes/component.php",
                    Some("require_once"),
                    442,
                )],
            ),
            ("public/lib/classes/component.php".to_string(), vec![]),
        ];
        let classifications = classify(&files, &notation(), false);
        let view_php = classifications
            .iter()
            .find(|c| c.file == "public/course/view.php")
            .unwrap();
        assert_eq!(view_php.kind, EntrypointKind::Page);
    }

    /// The synthetic public/config.php -> config.php -> lib/setup.php chain only takes effect
    /// without `bootstrap_only`: a file reaching component.php *only* through it (not via any
    /// real, directly-discovered edge) is bootstrap by default, and not bootstrap at all under
    /// `--bootstrap-only`.
    #[test]
    fn bootstrap_only_excludes_files_reachable_only_via_the_synthetic_config_chain() {
        let files = vec![
            ("public/config.php".to_string(), vec![]),
            (
                "lib/setup.php".to_string(),
                vec![result("public/lib/setup.php", Some("require_once"), 29)],
            ),
            (
                "public/lib/setup.php".to_string(),
                vec![result(
                    "public/lib/classes/component.php",
                    Some("require_once"),
                    442,
                )],
            ),
            ("public/lib/classes/component.php".to_string(), vec![]),
        ];

        let default_classifications = classify(&files, &notation(), false);
        assert_eq!(
            default_classifications
                .iter()
                .find(|c| c.file == "public/config.php")
                .unwrap()
                .kind,
            EntrypointKind::Bootstrap
        );

        let bootstrap_only_classifications = classify(&files, &notation(), true);
        assert!(
            bootstrap_only_classifications
                .iter()
                .all(|c| c.file != "public/config.php")
        );
    }

    #[test]
    fn non_existent_synthetic_nodes_never_appear_in_output() {
        let files = vec![
            ("public/config.php".to_string(), vec![]),
            (
                "public/lib/setup.php".to_string(),
                vec![result(
                    "public/lib/classes/component.php",
                    Some("require_once"),
                    442,
                )],
            ),
            ("public/lib/classes/component.php".to_string(), vec![]),
        ];
        let classifications = classify(&files, &notation(), false);
        // Root config.php and root lib/setup.php are never scanned files, so they must never be
        // reported even though they are graph nodes internally.
        assert!(
            classifications
                .iter()
                .all(|c| c.file != "config.php" && c.file != "lib/setup.php")
        );
    }

    /// On a pre-5.1 layout, config.php's own dirroot and public/config.php's dirroot coincide (no
    /// 'public/' split), so the two entry-point seeds — and the two synthetic-chain endpoints —
    /// are the same string; this must not produce a self-loop or otherwise confuse the graph.
    #[test]
    fn pre_5_1_layout_collapses_the_two_config_php_seeds() {
        let files = vec![
            ("config.php".to_string(), vec![]),
            (
                "lib/setup.php".to_string(),
                vec![result(
                    "lib/classes/component.php",
                    Some("require_once"),
                    100,
                )],
            ),
            ("lib/classes/component.php".to_string(), vec![]),
        ];
        let classifications = classify(&files, &PathNotation::new(""), false);
        assert!(
            classifications
                .iter()
                .any(|c| c.file == "lib/setup.php" && c.kind == EntrypointKind::Bootstrap)
        );
    }
}
