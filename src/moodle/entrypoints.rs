//! Classifies every file in a Moodle codebase's require/include graph by its relationship to
//! `core\component`'s own source file (component.php) — the file every other rewrite this project
//! produces ultimately depends on, and which cannot itself be found via `core\component`'s own
//! lookup, being the very thing that lookup is made of. Works purely from the require/include
//! graph a codebase-wide [`crate::path_finder::find_paths`] scan produces, no filesystem access of
//! its own.
//!
//! [`pre_component_extents`] answers exactly one question per file — how much of it runs before
//! `core\component` exists to be loaded, if any — as a [`PreComponentExtent`]:
//!
//! - No entry at all: nothing in the file ever runs before `core\component` is available.
//! - [`PreComponentExtent::UpToLine`]: the file has a real require/include chain, however many
//!   hops, into component.php — up to and including that chain's own earliest line *in this
//!   file*, `core\component` is not yet available; past it, it is. That last line is itself still
//!   pre-component: the statement that hands off into the rest of the chain has its own target
//!   resolved and loaded before anything *it* leads to has run. component.php itself never gets an
//!   entry here (nothing points *from* it toward itself) — see [`classify`], which reports it
//!   directly.
//! - [`PreComponentExtent::WholeFile`]: the file never reaches the boundary on its own, but is
//!   required, at or before its own referrer's boundary line, by a file that has one of the above
//!   — or by another `WholeFile` file, discovered the same way, with no line limit of its own. A
//!   file like public/lib/phpminimumversionlib.php, required by both public/install.php and
//!   public/admin/index.php before either of *them* reaches their own boundary, never requires
//!   anything reaching component.php itself, so it has no `UpToLine` of its own — but it runs
//!   exactly as pre-component as the file that reaches it, so the whole thing counts.
//!
//! [`classify`] reports every file with a non-empty extent (plus component.php itself,
//! unconditionally) as a [`BootstrapKind`] — the set of files a Composer plugin must place back at
//! their original location, since none of these shapes can be rewritten to use `core\component` to
//! find itself. The kind only ever records one further fact: whether the file lives under a `cli/`
//! directory ([`BootstrapKind::Cli`] vs. [`BootstrapKind::Other`]), or whether it reaches the
//! boundary at all rather than merely being loaded before some other file's own boundary line
//! ([`BootstrapKind::BootstrapDependency`]). An earlier version of this tool tried to also tell
//! ordinary pages (e.g. `mod/assign/view.php`) apart from internal framework plumbing that reaches
//! the boundary the same way (e.g. `lib/setup.php`) — there is no way to do that from the require
//! graph alone, since both are indistinguishably "a file whose own path to the boundary starts by
//! requiring config.php", so that distinction isn't made here.
//!
//! ## The synthetic config.php node
//!
//! A real Moodle checkout's config.php (the file a user or the installer creates at the repository
//! root) always begins by requiring `lib/setup.php`, which leads on to component.php — but that
//! specific line can never be scanned, because config.php itself doesn't exist in a bare checkout.
//! [`config_locations`] names the node standing in for it; `add_synthetic_config_edge` adds the
//! one edge that node needs (as if it required `lib/setup.php`) so a backward search seeded at
//! component.php alone walks straight through it to every page and CLI script that requires
//! config.php, rather than the search having to start from config.php too and simply assume the
//! two are equivalent.
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
//!
//! ## Files the graph can never reach
//!
//! A handful of files need exactly the same fixed placement as everything [`classify`] finds, but
//! can never be discovered by scanning the require/include graph, because nothing in the codebase
//! actually requires/includes them into that graph at all — the only thing that reaches them is a
//! subprocess spawn (e.g. `admin/tool/phpunit/cli/init.php` shells out to its own sibling
//! `util.php` with `exec()`/`passthru()`, once per invocation, rather than requiring it), which
//! [`crate::path_finder::find_paths`] cannot see by design: it only ever records require/include
//! constructs, not arbitrary function calls that merely happen to take a path-shaped string.
//! Turning that spawn into a real require isn't a fix either — `util.php` is written to run as its
//! own standalone process each time, with its own `$argv`, and requiring it repeatedly into one
//! process would redefine its own constants on the second call. There is no way to derive this
//! from the graph, so it isn't derived: `UNDETECTED_ENTRY_POINTS`, parsed by
//! `parse_undetected_entry_points`, is a short, hand-maintained list of such files. Each one is
//! folded into [`pre_component_extents`] as a [`PreComponentExtent::WholeFile`] — none of a
//! hand-seeded file's own code ever has `core\component` available either, for exactly the same
//! reason it had to be hand-seeded — and into [`classify`]'s output the same way component.php's
//! own unconditional entry is, except with its `BootstrapKind` taken from its own path rather than
//! from that `WholeFile` extent (see `undetected_entry_point_files`'s own comment for why).

use std::collections::{HashMap, HashSet};
use std::fmt;

use crate::path_finder::{PathNotation, PathResult};

/// One require/include statement: `target`, the file it requires/includes, at `line`, together
/// with `scope_end_line` — copied straight from [`PathResult::scope_end_line`] — the last line
/// this statement can be trusted to still apply to if it sits inside a conditional branch, or
/// `None` if it's unconditional (see [`BoundaryEdge`], which is what this becomes once a
/// statement like this is confirmed to actually reach the boundary).
struct Edge {
    target: String,
    line: u32,
    scope_end_line: Option<u32>,
}

/// The require/include graph in the direction it's actually executed: each file maps to the
/// files it requires/includes.
type ForwardEdges = HashMap<String, Vec<Edge>>;

/// The require/include graph inverted: each file maps to the files that require/include it.
type ReverseEdges = HashMap<String, Vec<String>>;

/// The four constructs that actually execute their target at the point they appear — the only
/// ones that can anchor a bootstrap or entry-point chain. A path merely mentioned elsewhere (an
/// existence check, an error-message string, a test mocking a request path, an array of patterns
/// to detect in a stack trace) is not evidence that the file actually loads it.
const REQUIRE_LIKE: [&str; 4] = ["require", "require_once", "include", "include_once"];

/// Whether `parent` — [`crate::path_finder::PathResult::parent`], the name of whatever construct
/// a path reference sits inside, `None` if it isn't inside any recognised one — is one of
/// `REQUIRE_LIKE`. Exposed for [`crate::moodle::categorise`], which uses it to verify a design
/// assumption behind its `pre-component`/`pre-component-literal` categories: both assume PHP's
/// require/include-specific same-directory fallback (see `REWRITE_SPEC.md`'s
/// "pre-component-literal rewriting"), which does not apply to any other construct — a path-shaped
/// argument to, say, `file_exists()` is resolved against the current working directory, not the
/// containing file's own directory, so treating it the same way would be wrong regardless of where
/// it sits.
pub fn is_require_like(parent: Option<&str>) -> bool {
    parent.is_some_and(|parent| REQUIRE_LIKE.contains(&parent))
}

/// Why a file appears in a [`FileClassification`] at all — see the module doc comment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootstrapKind {
    /// Reaches the boundary directly (has a [`PreComponentExtent::UpToLine`] of its own, or — for
    /// component.php itself — is the boundary), and lives under a directory named 'cli' (the
    /// heuristic stand-in for actually checking `define('CLI_SCRIPT', true)`).
    Cli,
    /// Reaches the boundary directly, same as [`Self::Cli`], but not under a 'cli' directory.
    Other,
    /// Never reaches the boundary itself — pulled in only because it's required, at or before its
    /// own referrer's boundary line, by a file that does (see [`PreComponentExtent::WholeFile`]).
    BootstrapDependency,
}

impl fmt::Display for BootstrapKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Cli => "cli",
            Self::Other => "other",
            Self::BootstrapDependency => "bootstrap-dependency",
        })
    }
}

/// One require/include statement, in some file, confirmed to actually reach component.php or
/// config.php (directly, or by requiring another file with a boundary of its own) — together with
/// how far past its own line it can be trusted to mean `core\component` is now available.
///
/// A file can have more than one of these: an early-exit guard clause (`if
/// (file_exists($configfile)) { require($configfile); ...; exit; }`) and the real, unconditional
/// bootstrap sequence later in the same file both count, and neither's reach can be inferred from
/// the other — see the module doc comment and [`PathResult::scope_end_line`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BoundaryEdge {
    /// The edge's own line — still pre-component itself (the require's own line hands off into
    /// the chain, but hasn't received the result of it yet).
    line: u32,
    /// The last line this edge can be trusted to apply to. `None` for an edge that sits at its
    /// file's own top level: nothing conditional stands between it and the rest of the file, so it
    /// reaches all the way to the end. `Some(line)` for an edge inside an `if`/`elseif`/`else`
    /// body: a later line *outside* that body has no guarantee the branch carrying this edge ever
    /// ran, so its reach stops at that body's own closing line, not the rest of the file.
    reach_end: Option<u32>,
}

impl BoundaryEdge {
    /// A [`BoundaryEdge`] with no conditional to scope it to — reaches to the end of the file.
    pub fn unbounded(line: u32) -> Self {
        Self {
            line,
            reach_end: None,
        }
    }

    /// The edge's own line.
    pub fn line(&self) -> u32 {
        self.line
    }

    /// Whether `line` is past this edge's own line, and still within its reach.
    fn covers(self, line: u32) -> bool {
        line > self.line && self.reach_end.is_none_or(|end| line <= end)
    }
}

/// How much of a file's own code runs before `core\component` exists — see the module doc comment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreComponentExtent {
    /// One or more [`BoundaryEdge`]s, each scoped to whatever conditional (if any) it sits inside
    /// — a line is pre-component unless *some* edge's own reach covers it. Almost always exactly
    /// one, unbounded, edge (an ordinary file with a single straight-line path to the boundary);
    /// more than one only when a file has more than one real require chain toward the boundary
    /// that don't dominate each other (see this type's own doc comment).
    UpToLine(Vec<BoundaryEdge>),
    /// The entire file. It never reaches the boundary on its own — see the module doc comment's
    /// `WholeFile` case — so there is no line past which it stops being pre-component; every
    /// reference in it is, regardless of where it sits.
    WholeFile,
}

impl PreComponentExtent {
    /// Whether `line` falls inside this extent.
    pub fn covers(&self, line: u32) -> bool {
        match self {
            Self::UpToLine(edges) => !edges.iter().any(|edge| edge.covers(line)),
            Self::WholeFile => true,
        }
    }
}

impl fmt::Display for PreComponentExtent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UpToLine(edges) => {
                let lines: Vec<String> = edges.iter().map(|edge| edge.line.to_string()).collect();
                write!(f, "{}", lines.join("+"))
            }
            Self::WholeFile => f.write_str("whole-file"),
        }
    }
}

/// One file's classification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileClassification {
    /// Repository-root-relative path, no leading slash (matches [`PathResult::real_path`]).
    pub file: String,
    pub kind: BootstrapKind,
    /// This file's own [`PreComponentExtent`] — `None` only for component.php itself (nothing
    /// points from it toward itself, so it has no chain of its own to report). Every other file
    /// reported here has one, including a file listed in `UNDETECTED_ENTRY_POINTS`: it gets
    /// [`PreComponentExtent::WholeFile`], since none of its own code ever has `core\component`
    /// available either — that's exactly why it had to be hand-seeded in the first place.
    pub extent: Option<PreComponentExtent>,
}

/// The literal path standing in for a real Moodle checkout's local config.php — the file a user or
/// the installer creates at the repository root, which never exists in a bare checkout, so it is
/// never discovered by scanning it. Used both as one of [`config_locations`]'s two entries and as
/// [`add_synthetic_config_edge`]'s own anchor into the graph.
const SYNTHETIC_CONFIG_PHP: &str = "config.php";

/// The repository-root-relative paths a real Moodle checkout's config.php can live at (the
/// `public/config.php` forwarding copy on a post-5.1 layout, and the synthetic root one — see
/// `SYNTHETIC_CONFIG_PHP`) — both fixed, known anchor points that have to be told to this tool
/// rather than discovered, the same way component.php's own location is. Used by
/// [`crate::moodle::categorise`] to recognise a reference that targets config.php itself.
pub fn config_locations(notation: &PathNotation) -> HashSet<String> {
    HashSet::from([
        dirroot_relative(notation, "config.php"),
        SYNTHETIC_CONFIG_PHP.to_string(),
    ])
}

/// Adds the one edge [`SYNTHETIC_CONFIG_PHP`] needs to participate in the backward search from
/// component.php — see the module doc comment's "synthetic config.php node" section. Added
/// directly into the reverse-edge map, as if config.php required `lib/setup.php`, rather than into
/// the forward map: only the backward search ever needs it, since every other computation in this
/// module looks only at real files, and the synthetic node is never one of those.
fn add_synthetic_config_edge(reverse: &mut ReverseEdges) {
    reverse
        .entry("lib/setup.php".to_string())
        .or_default()
        .push(SYNTHETIC_CONFIG_PHP.to_string());
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

/// For every real file in `files`, its own [`PreComponentExtent`] — see the module doc comment.
/// Standalone (as opposed to going through [`classify`]) because [`crate::moodle::categorise`]
/// needs this, and only this, for every file in the codebase, entry point or not.
pub fn pre_component_extents(
    files: &[(String, Vec<PathResult>)],
    notation: &PathNotation,
) -> HashMap<String, PreComponentExtent> {
    let forward = build_forward_edges(files);
    let mut reverse = build_reverse_edges(&forward);
    add_synthetic_config_edge(&mut reverse);
    let real_files: HashSet<&str> = files.iter().map(|(file, _)| file.as_str()).collect();
    pre_component_extents_from_graph(notation, &forward, &reverse, &real_files)
}

fn pre_component_extents_from_graph(
    notation: &PathNotation,
    forward: &ForwardEdges,
    reverse: &ReverseEdges,
    real_files: &HashSet<&str>,
) -> HashMap<String, PreComponentExtent> {
    let component_php = dirroot_relative(notation, "lib/classes/component.php");
    let reaches_boundary = reverse_closure(std::iter::once(component_php), reverse);

    let mut extents: HashMap<String, PreComponentExtent> = HashMap::new();
    let mut seeds: Vec<(String, u32)> = Vec::new();

    for file in &reaches_boundary {
        if !real_files.contains(file.as_str()) {
            continue;
        }
        // Empty here means `file` is itself one of `boundary_roots` rather than something
        // reaching them (in practice, only component.php: nothing points from it toward itself,
        // and a config.php location with no edges of its own simply has no extent to report).
        let edges = boundary_edges(file, &reaches_boundary, forward);
        if edges.is_empty() {
            continue;
        }
        // The seed line stays the single earliest edge, same as before this type could hold more
        // than one: `pre_component_dependencies` below only asks "was this file required at or
        // before some line", and the earliest edge is always at least as generous an answer to
        // that as any later one — a leaf dependency of this file's own bootstrap work is pulled in
        // on the way to *an* edge, and the earliest one is reached first regardless of how many
        // there are.
        let seed_line = edges
            .iter()
            .map(|edge| edge.line)
            .min()
            .expect("just checked non-empty");
        extents.insert(file.clone(), PreComponentExtent::UpToLine(edges));
        seeds.push((file.clone(), seed_line));
    }

    for file in pre_component_dependencies(seeds, forward, &reaches_boundary) {
        if real_files.contains(file.as_str()) {
            extents.insert(file, PreComponentExtent::WholeFile);
        }
    }

    // See the module doc comment's "Files the graph can never reach" section: none of these
    // files' own code ever runs with `core\component` available — that's exactly why the search
    // above could never find a chain for them — so, same as any other file this function only
    // discovers by being pulled in before someone else's own boundary line, the whole file counts.
    // `.entry().or_insert` leaves a file that already gained a real extent above untouched: a real,
    // graph-discovered chain is always more specific than this hand-maintained fallback.
    for file in undetected_entry_point_files(notation, real_files) {
        extents.entry(file).or_insert(PreComponentExtent::WholeFile);
    }

    extents
}

/// The real, dirroot-relative paths of every file named in `UNDETECTED_ENTRY_POINTS` that actually
/// exists in this codebase — see the module doc comment's "Files the graph can never reach"
/// section. Shared by [`pre_component_extents_from_graph`] (which must treat every one of them as
/// [`PreComponentExtent::WholeFile`], since none of their own code ever has `core\component`
/// available — the same reasoning [`crate::moodle::categorise`] relies on for an ordinary bootstrap
/// file) and [`classify`] (which additionally needs to know which files came from this list, to
/// give them the entry-point kind their own path implies rather than [`classify_kind`]'s ordinary
/// extent-based rule, which would otherwise call every one of them
/// [`BootstrapKind::BootstrapDependency`] — misleading for a file meant to be invoked directly,
/// rather than merely pulled in as some other file's dependency).
///
/// The `WholeFile` treatment is only actually correct for `admin/tool/phpunit/cli/init.php` — every
/// entry `UNDETECTED_ENTRY_POINTS` has today. A future addition that *does* have some real access
/// to `core\component`/`$CFG` (unlikely, but not something this function can verify) would need
/// different handling than what's here; nothing currently checks that assumption before applying it
/// uniformly to the whole list.
fn undetected_entry_point_files(
    notation: &PathNotation,
    real_files: &HashSet<&str>,
) -> HashSet<String> {
    parse_undetected_entry_points(UNDETECTED_ENTRY_POINTS, notation)
        .into_iter()
        .filter(|file| real_files.contains(file.as_str()))
        .collect()
}

/// Classifies every file in `files` — the codebase-wide output of [`crate::path_finder::find_paths`],
/// as `(file, that file's results)` pairs — as bootstrap-relevant or not. See the module doc
/// comment for how [`BootstrapKind`] and [`PreComponentExtent`] relate.
pub fn classify(
    files: &[(String, Vec<PathResult>)],
    notation: &PathNotation,
) -> Vec<FileClassification> {
    let forward = build_forward_edges(files);
    let mut reverse = build_reverse_edges(&forward);
    add_synthetic_config_edge(&mut reverse);
    let real_files: HashSet<&str> = files.iter().map(|(file, _)| file.as_str()).collect();

    let extents = pre_component_extents_from_graph(notation, &forward, &reverse, &real_files);
    // See `undetected_entry_point_files`'s own comment for why `classify_kind` alone isn't enough
    // for these: it can't tell a hand-seeded file's `WholeFile` extent apart from an ordinary
    // bootstrap dependency's.
    let undetected = undetected_entry_point_files(notation, &real_files);

    let mut classifications: Vec<FileClassification> = extents
        .into_iter()
        .map(|(file, extent)| {
            let kind = if undetected.contains(&file) {
                if is_cli_path(&file) {
                    BootstrapKind::Cli
                } else {
                    BootstrapKind::Other
                }
            } else {
                classify_kind(&file, &extent)
            };
            FileClassification {
                kind,
                file,
                extent: Some(extent),
            }
        })
        .collect();

    // component.php never has an extent of its own (see `pre_component_extents_from_graph`'s own
    // comment) but must always be reported regardless — it's the fixed destination every
    // pre-component require ultimately addresses by hardcoded path, not a file that reaches some
    // *other* boundary the way everything above does.
    let component_php = dirroot_relative(notation, "lib/classes/component.php");
    if real_files.contains(component_php.as_str()) {
        classifications.push(FileClassification {
            file: component_php,
            kind: BootstrapKind::Other,
            extent: None,
        });
    }

    classifications.sort_by(|a, b| a.file.cmp(&b.file));
    classifications
}

/// Which [`BootstrapKind`] a file with `extent` falls into. A [`PreComponentExtent::WholeFile`]
/// means the file never reaches the boundary itself — only pulled in before some other file's own
/// boundary line runs — so it's always [`BootstrapKind::BootstrapDependency`], regardless of its
/// own path. Otherwise it reaches the boundary directly, and the only further fact worth recording
/// is whether it lives under a 'cli' directory (see the module doc comment for why nothing finer
/// than that is derived here).
fn classify_kind(file: &str, extent: &PreComponentExtent) -> BootstrapKind {
    match extent {
        PreComponentExtent::WholeFile => BootstrapKind::BootstrapDependency,
        PreComponentExtent::UpToLine(_) if is_cli_path(file) => BootstrapKind::Cli,
        PreComponentExtent::UpToLine(_) => BootstrapKind::Other,
    }
}

/// Every file required, at or before its own referrer's boundary line, by a file in `seeds` — the
/// leaf dependencies a file with an [`PreComponentExtent::UpToLine`] of its own reaches on its way
/// to that boundary, which never lead to the boundary themselves and so are invisible to
/// [`reverse_closure`], but run exactly as pre-component as the file that reaches them. Once a file
/// joins this set on that basis, everything *it* requires is pre-component too, with no further
/// boundary check: nothing in it was ever on a path toward the boundary to begin with, so all of it
/// runs before that boundary too, regardless of where within the file a reference sits.
/// `already_known` — every file already reaching the boundary directly — is never re-discovered
/// here, even if reachable this way too: it already has a more specific `UpToLine` extent.
fn pre_component_dependencies(
    seeds: Vec<(String, u32)>,
    forward: &ForwardEdges,
    already_known: &HashSet<String>,
) -> HashSet<String> {
    let mut discovered: HashSet<String> = HashSet::new();
    let mut queue: Vec<(String, Option<u32>)> = seeds
        .into_iter()
        .map(|(file, line)| (file, Some(line)))
        .collect();
    while let Some((file, boundary)) = queue.pop() {
        for edge in forward.get(&file).into_iter().flatten() {
            if boundary.is_some_and(|boundary| edge.line > boundary) {
                continue;
            }
            if already_known.contains(&edge.target) || !discovered.insert(edge.target.clone()) {
                continue;
            }
            queue.push((edge.target.clone(), None));
        }
    }
    discovered
}

/// A repository-root-relative path (no leading slash) to `suffix` inside dirroot, computed
/// directly from `notation`'s dirroot segment rather than through glyph notation — glyph ('@'/'#')
/// is purely a human-facing rendering and must not be parsed back for program logic.
fn dirroot_relative(notation: &PathNotation, suffix: &str) -> String {
    format!("{}/{suffix}", notation.dirroot_segment())
        .trim_start_matches('/')
        .to_string()
}

fn build_forward_edges(files: &[(String, Vec<PathResult>)]) -> ForwardEdges {
    let mut forward: ForwardEdges = HashMap::new();
    for (file, results) in files {
        for result in results {
            if is_require_like(result.parent.as_deref()) {
                forward.entry(file.clone()).or_default().push(Edge {
                    target: result.real_path.clone(),
                    line: result.line,
                    scope_end_line: result.scope_end_line,
                });
            }
        }
    }
    forward
}

fn build_reverse_edges(forward: &ForwardEdges) -> ReverseEdges {
    let mut reverse: ReverseEdges = HashMap::new();
    for (source, edges) in forward {
        for Edge { target, .. } in edges {
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

/// Every one of `file`'s own require/include statements that points at something already in
/// `closure` — i.e. every place in this file a chain toward the boundary begins — as
/// [`BoundaryEdge`]s. Empty when `file` is itself a member of `closure` with no real edge of its
/// own into the rest of it (component.php, or a config.php location with nothing before its own
/// boundary-reaching line).
fn boundary_edges(
    file: &str,
    closure: &HashSet<String>,
    forward: &ForwardEdges,
) -> Vec<BoundaryEdge> {
    forward
        .get(file)
        .into_iter()
        .flatten()
        .filter(|edge| closure.contains(&edge.target))
        .map(|edge| BoundaryEdge {
            line: edge.line,
            reach_end: edge.scope_end_line,
        })
        .collect()
}

/// The heuristic stand-in for checking `define('CLI_SCRIPT', true)`: whether any directory
/// component of `file` is named 'cli'.
fn is_cli_path(file: &str) -> bool {
    file.split('/').any(|segment| segment == "cli")
}

/// Hand-maintained list of files the require/include graph can never discover on its own — see the
/// module doc comment's "Files the graph can never reach" section. Embedded from
/// `undetected_entry_points.txt` at the repository root at compile time, rather than read from
/// disk at run time, so this tool has no dependency on its own working directory to find it.
const UNDETECTED_ENTRY_POINTS: &str = include_str!("../../undetected_entry_points.txt");

/// Parses `source` — [`UNDETECTED_ENTRY_POINTS`]'s own format: one dirroot-relative path per line,
/// blank lines and lines starting with '#' ignored — into full, [`dirroot_relative`] paths for
/// `notation`'s layout. Takes `source` as a parameter, rather than reading the constant directly,
/// purely so tests can exercise the parsing rules against a fixture string without depending on
/// this project's own current list.
fn parse_undetected_entry_points(source: &str, notation: &PathNotation) -> Vec<String> {
    source
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(|line| dirroot_relative(notation, line))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(real_path: &str, parent: Option<&str>, line: u32) -> PathResult {
        scoped_result(real_path, parent, line, None)
    }

    /// Like [`result`], but for a require/include that sits inside an `if`/`elseif`/`else` body of
    /// its own — `scope_end_line` is that body's own closing line, exactly as
    /// [`crate::path_finder::PathResult::scope_end_line`] would record it.
    fn scoped_result(
        real_path: &str,
        parent: Option<&str>,
        line: u32,
        scope_end_line: Option<u32>,
    ) -> PathResult {
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
            scope_end_line,
        }
    }

    /// The extent an ordinary file with a single, unconditional require chain toward the boundary
    /// gets — every existing test fixture below is this shape unless it says otherwise.
    fn up_to_line(line: u32) -> PreComponentExtent {
        PreComponentExtent::UpToLine(vec![BoundaryEdge::unbounded(line)])
    }

    fn notation() -> PathNotation {
        PathNotation::new("public/")
    }

    /// The real chain a checkout's own local config.php begins by requiring:
    /// `public/config.php`'s real edge to the (never-scanned) root config.php,
    /// [`add_synthetic_config_edge`]'s synthetic edge onward from there, and the two further real
    /// hops from `lib/setup.php` through to component.php. Any fixture that wants a file requiring
    /// `public/config.php` to actually reach the boundary needs this appended — unlike the
    /// shortcut this project used to take (treating "requires config.php" as flatly equivalent to
    /// "reaches component.php"), [`classify`] now has to be shown the real chain to prove it.
    fn config_chain_files() -> Vec<(String, Vec<PathResult>)> {
        vec![
            (
                "public/config.php".to_string(),
                vec![result("config.php", Some("require_once"), 31)],
            ),
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
        ]
    }

    #[test]
    fn direct_component_require_is_classified_with_its_line() {
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
        let classifications = classify(&files, &notation());
        assert_eq!(
            classifications,
            vec![
                FileClassification {
                    file: "public/lib/classes/component.php".to_string(),
                    kind: BootstrapKind::Other,
                    extent: None,
                },
                FileClassification {
                    file: "public/lib/setup.php".to_string(),
                    kind: BootstrapKind::Other,
                    extent: Some(up_to_line(442)),
                },
            ]
        );
    }

    /// The end-to-end proof that [`add_synthetic_config_edge`] does its job: every real hop in a
    /// checkout's actual chain from `public/config.php` through to component.php gets its own
    /// extent, at its own line, discovered by a single backward search seeded at component.php
    /// alone — with no separate, unconditional "reaches config.php" shortcut needed to find any of
    /// them.
    #[test]
    fn synthetic_config_edge_connects_the_real_chain_all_the_way_through() {
        let classifications = classify(&config_chain_files(), &notation());
        assert_eq!(
            classifications,
            vec![
                FileClassification {
                    file: "lib/setup.php".to_string(),
                    kind: BootstrapKind::Other,
                    extent: Some(up_to_line(29)),
                },
                FileClassification {
                    file: "public/config.php".to_string(),
                    kind: BootstrapKind::Other,
                    extent: Some(up_to_line(31)),
                },
                FileClassification {
                    file: "public/lib/classes/component.php".to_string(),
                    kind: BootstrapKind::Other,
                    extent: None,
                },
                FileClassification {
                    file: "public/lib/setup.php".to_string(),
                    kind: BootstrapKind::Other,
                    extent: Some(up_to_line(442)),
                },
            ]
        );
    }

    /// On a pre-5.1 layout, config.php's own dirroot and public/config.php's dirroot coincide (no
    /// 'public/' split) — but [`add_synthetic_config_edge`]'s edge is keyed on the same bare
    /// literal regardless of layout, so a page's own require of the bare `config.php` still chains
    /// through to component.php correctly, with no `public/` prefix anywhere in it.
    #[test]
    fn pre_5_1_layout_page_reaches_the_boundary_via_bare_config_php() {
        let files = vec![
            (
                "mod/assign/view.php".to_string(),
                vec![result("config.php", Some("require"), 5)],
            ),
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
        let classifications = classify(&files, &PathNotation::new(""));
        assert_eq!(
            classifications
                .iter()
                .find(|c| c.file == "mod/assign/view.php")
                .unwrap(),
            &FileClassification {
                file: "mod/assign/view.php".to_string(),
                kind: BootstrapKind::Other,
                extent: Some(up_to_line(5)),
            }
        );
    }

    /// A file whose require sits inside a function/method (only executed if that function is
    /// called, e.g. `\core\router\util::load_full_moodle()`) is still classified the same way —
    /// this tool does not distinguish top-level code from function bodies. What matters is that
    /// this does not propagate: a caller that only ever references the class (the autoloaded,
    /// ordinary way — no require/include of the file at all, hence no PathResult, hence no edge) is
    /// completely unaffected, exactly as if the nested require did not exist.
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
        let classifications = classify(&files, &notation());
        assert_eq!(
            classifications
                .iter()
                .find(|c| c.file == "public/lib/classes/router/util.php")
                .unwrap()
                .kind,
            BootstrapKind::Other
        );
        assert!(classifications.iter().all(
            |c| c.file != "public/lib/classes/router/middleware/error_handling_middleware.php"
        ));
    }

    /// public/lib/phpunit/bootstrap.php: two requires before its own config.php require, neither
    /// of which reaches component.php on its own (they're self-contained PHPUnit helper
    /// libraries). Its own extent still correctly covers up to its own config.php line (86) — the
    /// point where its own chain toward component.php begins in this file.
    #[test]
    fn a_file_with_requires_before_its_own_config_line_gets_the_right_extent() {
        let mut files = config_chain_files();
        files.push((
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
        ));
        let classifications = classify(&files, &notation());
        assert_eq!(
            classifications
                .iter()
                .find(|c| c.file == "public/lib/phpunit/bootstrap.php")
                .unwrap(),
            &FileClassification {
                file: "public/lib/phpunit/bootstrap.php".to_string(),
                kind: BootstrapKind::Other,
                extent: Some(up_to_line(86)),
            }
        );
    }

    /// Two unconditional edges both qualify here (both are recorded — see [`BoundaryEdge`]'s own
    /// doc comment for why this type can hold more than one), but since both are unbounded, the
    /// earlier one already covers everything the later one would: coverage is identical to having
    /// only the earliest edge, at every line either edge could plausibly matter for.
    #[test]
    fn a_later_unconditional_edge_does_not_shrink_what_an_earlier_one_already_covers() {
        let mut files = config_chain_files();
        files.push((
            "public/theme/font.php".to_string(),
            vec![
                result("public/config.php", Some("require"), 32),
                result("public/lib/setup.php", Some("require"), 138),
            ],
        ));
        let classifications = classify(&files, &notation());
        let font_php = classifications
            .iter()
            .find(|c| c.file == "public/theme/font.php")
            .unwrap();
        assert_eq!(font_php.kind, BootstrapKind::Other);
        let extent = font_php.extent.as_ref().expect("has an extent");
        for line in [1, 32, 33, 100, 138, 139] {
            assert_eq!(
                extent.covers(line),
                line <= 32,
                "line {line} should be pre-component only up to and including line 32"
            );
        }
    }

    #[test]
    fn transitive_chain_reports_each_files_own_line() {
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
        let classifications = classify(&files, &notation());
        let lib_setup = classifications
            .iter()
            .find(|c| c.file == "lib/setup.php")
            .unwrap();
        assert_eq!(
            lib_setup,
            &FileClassification {
                file: "lib/setup.php".to_string(),
                kind: BootstrapKind::Other,
                extent: Some(up_to_line(29)),
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
        let classifications = classify(&files, &notation());
        assert!(
            classifications
                .iter()
                .all(|c| c.file != "public/mod/forum/lib.php")
        );
    }

    #[test]
    fn a_file_reaching_public_config_gets_an_extent() {
        let mut files = config_chain_files();
        files.push((
            "public/course/view.php".to_string(),
            vec![result("public/config.php", Some("require"), 12)],
        ));
        let classifications = classify(&files, &notation());
        assert_eq!(
            classifications
                .iter()
                .find(|c| c.file == "public/course/view.php")
                .unwrap(),
            &FileClassification {
                file: "public/course/view.php".to_string(),
                kind: BootstrapKind::Other,
                extent: Some(up_to_line(12)),
            },
        );
    }

    #[test]
    fn cli_directory_is_classified_as_cli() {
        let mut files = config_chain_files();
        files.push((
            "admin/cli/cron.php".to_string(),
            vec![result("public/config.php", Some("require"), 12)],
        ));
        let classifications = classify(&files, &notation());
        assert_eq!(
            classifications
                .iter()
                .find(|c| c.file == "admin/cli/cron.php")
                .unwrap()
                .kind,
            BootstrapKind::Cli
        );
    }

    #[test]
    fn a_file_requiring_another_config_reaching_file_is_also_classified() {
        let mut files = config_chain_files();
        files.extend(vec![
            (
                "lib/ajax/service-nologin.php".to_string(),
                vec![result("public/lib/ajax/service.php", Some("require"), 5)],
            ),
            (
                "public/lib/ajax/service.php".to_string(),
                vec![result("public/config.php", Some("require"), 20)],
            ),
        ]);
        let classifications = classify(&files, &notation());
        assert!(classifications
            .iter()
            .any(|c| c.file == "lib/ajax/service-nologin.php" && c.kind == BootstrapKind::Other));
    }

    #[test]
    fn non_real_files_never_appear_in_output() {
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
        let classifications = classify(&files, &notation());
        // Root config.php and root lib/setup.php are never scanned files (only their `public/`
        // counterparts are, in this fixture), so they must never be reported even though the
        // synthetic edge always mentions them internally.
        assert!(
            classifications
                .iter()
                .all(|c| c.file != "config.php" && c.file != "lib/setup.php")
        );
    }

    /// The real bug this project shipped: `public/install.php` requires a leaf file
    /// (`phpminimumversionlib.php`, which requires nothing itself) before its own line requiring
    /// component.php. That leaf never reaches component.php on its own, so the reverse closure from
    /// component.php alone would never find it — it has to be pulled in because `install.php`
    /// reaches it *before* `install.php` itself reaches its own boundary.
    #[test]
    fn a_leaf_required_before_a_files_own_boundary_line_gets_the_whole_file_extent() {
        let files = vec![
            (
                "public/install.php".to_string(),
                vec![
                    result(
                        "public/lib/phpminimumversionlib.php",
                        Some("require_once"),
                        70,
                    ),
                    result("public/lib/classes/component.php", Some("require_once"), 95),
                ],
            ),
            ("public/lib/phpminimumversionlib.php".to_string(), vec![]),
            ("public/lib/classes/component.php".to_string(), vec![]),
        ];
        let classifications = classify(&files, &notation());
        assert_eq!(
            classifications
                .iter()
                .find(|c| c.file == "public/lib/phpminimumversionlib.php")
                .unwrap(),
            &FileClassification {
                file: "public/lib/phpminimumversionlib.php".to_string(),
                kind: BootstrapKind::BootstrapDependency,
                extent: Some(PreComponentExtent::WholeFile),
            }
        );
    }

    /// The second real instance found while diagnosing the bug above: `public/admin/index.php`
    /// requires the same leaf file before *its own* config.php line, reached only through the real
    /// chain rather than a direct edge to component.php — the leaf must be pulled in exactly the
    /// same way regardless of which route the referring file takes to the boundary.
    #[test]
    fn a_leaf_required_before_a_files_own_config_line_gets_the_whole_file_extent() {
        let mut files = config_chain_files();
        files.extend(vec![
            (
                "public/admin/index.php".to_string(),
                vec![
                    result(
                        "public/lib/phpminimumversionlib.php",
                        Some("require_once"),
                        32,
                    ),
                    result("public/config.php", Some("require"), 87),
                ],
            ),
            ("public/lib/phpminimumversionlib.php".to_string(), vec![]),
        ]);
        let classifications = classify(&files, &notation());
        assert_eq!(
            classifications
                .iter()
                .find(|c| c.file == "public/lib/phpminimumversionlib.php")
                .unwrap()
                .extent,
            Some(PreComponentExtent::WholeFile)
        );
    }

    /// The third real instance: `phpunit/bootstrap.php` requires `bootstraplib.php` before its own
    /// config.php line, and `bootstraplib.php` in turn requires `testing/lib.php` — a second hop,
    /// with no boundary line of its own, since the whole of `bootstraplib.php` runs pre-component
    /// once it's pulled in at all. Both must be discovered, not just the first hop.
    #[test]
    fn a_dependencys_own_dependency_is_discovered_transitively() {
        let mut files = config_chain_files();
        files.extend(vec![
            (
                "public/lib/phpunit/bootstrap.php".to_string(),
                vec![
                    result(
                        "public/lib/phpunit/bootstraplib.php",
                        Some("require_once"),
                        51,
                    ),
                    result("public/config.php", Some("require"), 86),
                ],
            ),
            (
                "public/lib/phpunit/bootstraplib.php".to_string(),
                vec![result(
                    "public/lib/testing/lib.php",
                    Some("require_once"),
                    28,
                )],
            ),
            ("public/lib/testing/lib.php".to_string(), vec![]),
        ]);
        let classifications = classify(&files, &notation());
        assert_eq!(
            classifications
                .iter()
                .find(|c| c.file == "public/lib/phpunit/bootstraplib.php")
                .unwrap()
                .extent,
            Some(PreComponentExtent::WholeFile)
        );
        assert_eq!(
            classifications
                .iter()
                .find(|c| c.file == "public/lib/testing/lib.php")
                .unwrap()
                .extent,
            Some(PreComponentExtent::WholeFile)
        );
    }

    /// A require that comes *after* a file's own boundary line is not pre-component work and must
    /// not be pulled in.
    #[test]
    fn a_require_after_the_files_own_boundary_is_not_pulled_in() {
        let files = vec![
            (
                "public/lib/setup.php".to_string(),
                vec![
                    result("public/lib/classes/component.php", Some("require_once"), 50),
                    result("public/lib/setuplib_late.php", Some("require_once"), 60),
                ],
            ),
            ("public/lib/classes/component.php".to_string(), vec![]),
            ("public/lib/setuplib_late.php".to_string(), vec![]),
        ];
        let classifications = classify(&files, &notation());
        assert!(
            classifications
                .iter()
                .all(|c| c.file != "public/lib/setuplib_late.php")
        );
    }

    /// The same leaf pulled in by two different files (mirroring `phpminimumversionlib.php`, which
    /// both `install.php` and `admin/index.php` require independently in the real codebase) must be
    /// reported exactly once.
    #[test]
    fn a_leaf_required_by_two_different_files_is_reported_once() {
        let mut files = config_chain_files();
        files.extend(vec![
            (
                "public/install.php".to_string(),
                vec![
                    result(
                        "public/lib/phpminimumversionlib.php",
                        Some("require_once"),
                        70,
                    ),
                    result("public/lib/classes/component.php", Some("require_once"), 95),
                ],
            ),
            (
                "public/admin/index.php".to_string(),
                vec![
                    result(
                        "public/lib/phpminimumversionlib.php",
                        Some("require_once"),
                        32,
                    ),
                    result("public/config.php", Some("require"), 87),
                ],
            ),
            ("public/lib/phpminimumversionlib.php".to_string(), vec![]),
        ]);
        let classifications = classify(&files, &notation());
        assert_eq!(
            classifications
                .iter()
                .filter(|c| c.file == "public/lib/phpminimumversionlib.php")
                .count(),
            1
        );
    }

    /// An ordinary file with no pre-component work of its own (the common case — most pages just
    /// require config.php and nothing else beforehand) still gets an extent (up to its own
    /// config.php line), but discovers no whole-file dependencies from it, since there's nothing
    /// before that line to pull in.
    #[test]
    fn a_file_with_nothing_before_its_config_line_discovers_no_dependencies() {
        let mut files = config_chain_files();
        files.push((
            "public/course/view.php".to_string(),
            vec![result("public/config.php", Some("require"), 12)],
        ));
        let classifications = classify(&files, &notation());
        assert_eq!(
            classifications
                .iter()
                .find(|c| c.file == "public/course/view.php")
                .unwrap(),
            &FileClassification {
                file: "public/course/view.php".to_string(),
                kind: BootstrapKind::Other,
                extent: Some(up_to_line(12)),
            }
        );
        assert!(
            classifications
                .iter()
                .all(|c| c.extent != Some(PreComponentExtent::WholeFile))
        );
    }

    #[test]
    fn parse_undetected_entry_points_skips_blanks_and_comments() {
        let source = "\n\
            # comment\n\
              admin/tool/phpunit/cli/init.php  \n\
            \n\
            # another comment\n\
            some/other/file.php\n";
        let parsed = parse_undetected_entry_points(source, &notation());
        assert_eq!(
            parsed,
            vec![
                "public/admin/tool/phpunit/cli/init.php".to_string(),
                "public/some/other/file.php".to_string(),
            ]
        );
    }

    /// `admin/tool/phpunit/cli/init.php` is the real, motivating case in
    /// `undetected_entry_points.txt` — see the module doc comment's "Files the graph can never
    /// reach" section. It has no require/include of its own reaching the boundary (it only shells
    /// out to `util.php`), so without the hand-seeded list it would be invisible to `classify`
    /// entirely; this proves [`classify`] actually consults [`UNDETECTED_ENTRY_POINTS`], not just
    /// [`parse_undetected_entry_points`] in isolation.
    #[test]
    fn a_file_the_graph_cannot_reach_is_still_classified_via_the_hand_seeded_list() {
        let files = vec![("public/admin/tool/phpunit/cli/init.php".to_string(), vec![])];
        let classifications = classify(&files, &notation());
        assert_eq!(
            classifications,
            vec![FileClassification {
                file: "public/admin/tool/phpunit/cli/init.php".to_string(),
                kind: BootstrapKind::Cli,
                extent: Some(PreComponentExtent::WholeFile),
            }]
        );
    }

    /// The bug this whole mechanism exists to prevent: without a `WholeFile` extent of its own, a
    /// hand-seeded file's ordinary `__DIR__`-relative requires would fall through to
    /// [`crate::moodle::categorise`]'s static-reference rules, and — because [`classify`] also
    /// (correctly) reports the file as an entry point — get forced to a `\core\component::get_path()`
    /// call, even though `$CFG` is never actually loaded anywhere in the file. [`pre_component_extents`] is the
    /// standalone computation the rewrite step's categoriser actually consults, so this has to be
    /// proven against it directly, not just against [`classify`]'s own output.
    #[test]
    fn pre_component_extents_marks_a_hand_seeded_file_as_whole_file() {
        let files = vec![(
            "public/admin/tool/phpunit/cli/init.php".to_string(),
            vec![result("public/lib/clilib.php", Some("require_once"), 41)],
        )];
        let extents = pre_component_extents(&files, &notation());
        assert_eq!(
            extents.get("public/admin/tool/phpunit/cli/init.php"),
            Some(&PreComponentExtent::WholeFile)
        );
    }

    /// A hand-seeded path that isn't actually present in this codebase (e.g. a Moodle version
    /// predating it) must never be reported — same `real_files` guard every other seed in
    /// [`classify`] gets.
    #[test]
    fn a_hand_seeded_path_absent_from_the_codebase_is_not_reported() {
        let classifications = classify(&[], &notation());
        assert!(classifications.is_empty());
    }

    /// If a hand-seeded file ever gains a real, graph-discoverable chain of its own (e.g. a future
    /// Moodle version changes `init.php` to `require` something that reaches the boundary), that
    /// real extent must win over the hand-seeded `WholeFile` fallback — never silently overwritten
    /// by the unconditional seeding loop running after it.
    #[test]
    fn a_hand_seeded_path_with_a_real_chain_of_its_own_keeps_its_real_extent() {
        let mut files = config_chain_files();
        files.push((
            "public/admin/tool/phpunit/cli/init.php".to_string(),
            vec![result("public/config.php", Some("require"), 40)],
        ));
        let classifications = classify(&files, &notation());
        assert_eq!(
            classifications
                .iter()
                .find(|c| c.file == "public/admin/tool/phpunit/cli/init.php")
                .unwrap(),
            &FileClassification {
                file: "public/admin/tool/phpunit/cli/init.php".to_string(),
                kind: BootstrapKind::Cli,
                extent: Some(up_to_line(40)),
            }
        );
    }
}
