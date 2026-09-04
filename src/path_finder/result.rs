use std::fmt;

/// One internal-path reference found in a Moodle file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathResult {
    /// The resolved path, in glyph notation (e.g. '@/lib/setup.php') — a rendering of
    /// `real_path`, see [`crate::path_finder::PathNotation::to_glyph`].
    pub path: String,
    /// The resolved path, relative to the repository root, with no leading slash (e.g.
    /// 'public/lib/setup.php').
    pub real_path: String,
    /// The expression that anchors this path, roughly in order of how confidently it identifies
    /// an actual file reference.
    pub kind: PathKind,
    /// 1-indexed source line of the matched expression.
    pub line: u32,
    /// The exact source text of the matched expression.
    pub code: String,
    /// The name of the enclosing include/require, or the function/method/static call this
    /// expression was passed to as an argument, if any (e.g. "require_once", "dirname",
    /// "Foo::bar").
    pub parent: Option<String>,
    /// Byte offset of the start of the matched expression (inclusive).
    pub start_pos: Option<usize>,
    /// Byte offset of the end of the matched expression (exclusive).
    pub end_pos: Option<usize>,
    /// The literal that immediately follows the path root in a concatenation, if it is a
    /// recognised separator ('/' or 'DIRECTORY_SEPARATOR'); empty otherwise.
    pub separator: String,
    /// The faithful PHP expression for the part of a dirroot-rooted path after $CFG->dirroot,
    /// ready to pass to core\component::from_mono_path(). Empty for paths not rooted at
    /// $CFG->dirroot.
    pub mono_path_expr: String,
    /// The last line still inside this reference's own innermost enclosing `if`/`elseif`/`else`
    /// body, if it sits inside one — `None` if it sits at its file's own top level. Only
    /// `if`/`elseif`/`else` bodies are tracked (not loops, `switch`, `try`, or function/method
    /// bodies — see [`crate::path_finder::find_paths`]'s walker for why): this project's own boundary
    /// computation ([`crate::moodle::entrypoints`]) is the only consumer, and needs to know that a
    /// require/include reachable only on one conditional branch cannot be assumed to have run for
    /// code outside that branch, the same way a require unconditionally at the top of the file
    /// can be.
    pub scope_end_line: Option<u32>,
}

/// The kind of expression that anchors a [`PathResult`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathKind {
    /// Rooted at $CFG->dirroot.
    Dirroot,
    /// Rooted at $CFG->libdir.
    Libdir,
    /// Rooted at $CFG->root, the repository root.
    Root,
    /// Rooted at __DIR__, or a dirname(__FILE__)/dirname(__DIR__) chain.
    Dir,
    /// Rooted at __FILE__.
    File,
    /// A bare string literal used as the sole value of a require/include construct — no
    /// concatenation or other wrapping expression — and so treated as definitely a path.
    RequireLiteral,
    /// A bare variable used as the sole value of a require/include construct, resolved by tracing
    /// back to the nearest earlier same-file assignment to that same variable whose own
    /// right-hand side is itself one of the other recognised shapes above — one hop only, no
    /// further tracing through whatever that assignment's own value came from. Never eligible for
    /// this project's rewrite step (see `crate::rewrite::decide::is_eligible`): the source text at
    /// the require site is the variable, not the path, so there is nothing there that could be
    /// safely replaced with a `component_path()`/`__DIR__`-relative expression.
    TracedVariable,
}

impl fmt::Display for PathKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Dirroot => "dirroot",
            Self::Libdir => "libdir",
            Self::Root => "root",
            Self::Dir => "dir",
            Self::File => "file",
            Self::RequireLiteral => "require-literal",
            Self::TracedVariable => "traced-variable",
        })
    }
}
