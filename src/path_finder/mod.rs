//! A port of moodle-splittable's `PathFindingVisitor`: finds paths in a Moodle file that refer
//! to another file in the codebase and converts them to a standard, root-relative notation.

mod finder;
mod notation;
mod result;

pub use finder::find_paths;
pub use notation::PathNotation;
pub use result::PathResult;
