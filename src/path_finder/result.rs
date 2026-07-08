/// One internal-path reference found in a Moodle file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathResult {
    /// The resolved path, in glyph notation (e.g. '@/lib/setup.php').
    pub path: String,
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
}
