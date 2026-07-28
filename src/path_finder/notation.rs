//! The path coordinate system used throughout the analysis. Repository-root-relative paths are
//! rendered in glyph notation: '@' is $CFG->dirroot and '#' is the repository root ($CFG->root,
//! the parent of dirroot since Moodle 5.1).
//!
//! The dirroot prefix is the path from the repository root to dirroot — 'public/' since Moodle
//! 5.1, or '' for earlier layouts where the two roots coincide.

use std::path::Path;

#[derive(Debug, Clone)]
pub struct PathNotation {
    /// The dirroot path relative to the repository root, with a leading slash (e.g. '/public').
    dirroot_segment: String,
}

impl PathNotation {
    pub fn new(dirroot_prefix: &str) -> Self {
        let prefix = dirroot_prefix.trim_matches('/');
        let dirroot_segment = if prefix.is_empty() { String::new() } else { format!("/{prefix}") };
        Self { dirroot_segment }
    }

    /// Builds the notation for a codebase at `root`, auto-detecting the dirroot prefix.
    pub fn from_root(root: &Path) -> Self {
        Self::new(crate::moodle::dirroot_prefix(root))
    }

    pub fn dirroot_segment(&self) -> &str {
        &self.dirroot_segment
    }

    /// Renders a repository-root-relative path (with a leading slash, e.g. '/public/lib/x.php',
    /// '/config.php', or the bare dirroot segment '/public') in glyph notation.
    ///
    /// The dirroot string prefix becomes '@' by pure prefix substitution, so '@' stands for the
    /// same bytes as dirroot: '/public/lib/x.php' => '@/lib/x.php', '/public' => '@', and a
    /// separator-less concat '/public{$dir}' => '@{$dir}'. Paths above dirroot keep '#', e.g.
    /// '/config.php' => '#/config.php'.
    pub fn to_glyph(&self, repo_relative_path: &str) -> String {
        let rooted = format!("#{repo_relative_path}");
        let dirroot = format!("#{}", self.dirroot_segment);
        match rooted.strip_prefix(dirroot.as_str()) {
            Some(rest) => format!("@{rest}"),
            None => rooted,
        }
    }

    /// The inverse of [`Self::to_glyph`]: resolves a glyph path to a repository-root-relative
    /// path with no leading slash (the form used for filesystem 'file' values and component
    /// directories).
    ///
    /// '@' expands back to the dirroot segment, '#' to the repository root: '@/lib/x.php' =>
    /// 'public/lib/x.php', '#/lib/setup.php' => 'lib/setup.php', and the bare dirroot '@' =>
    /// 'public'. On pre-5.1 layouts (empty dirroot segment) the two coincide, so '@/lib/x.php' =>
    /// 'lib/x.php'.
    pub fn to_repo_path(&self, glyph_path: &str) -> String {
        let rooted = match glyph_path.strip_prefix('@') {
            Some(rest) => format!("{}{rest}", self.dirroot_segment),
            None => glyph_path[1..].to_string(),
        };
        rooted.trim_start_matches('/').to_string()
    }

    /// Resolves '.' and '..' segments in a path, returning it with a single leading slash. A
    /// trailing slash is preserved. This is a pure path operation, independent of the coordinate
    /// system.
    ///
    /// A '..' that climbs above the start of the path is kept as a leading '..' rather than
    /// dropped, so a path pointing outside the repository stays visibly outside it instead of
    /// being clamped onto an unrelated location inside it.
    pub fn normalise(path: &str) -> String {
        let mut stack: Vec<&str> = Vec::new();
        for part in path.split('/') {
            match part {
                "." => continue,
                ".." => {
                    if stack.last().is_some_and(|&last| last != "..") {
                        stack.pop();
                    } else {
                        stack.push("..");
                    }
                }
                "" => {
                    if !stack.is_empty() {
                        stack.push(part);
                    }
                }
                _ => stack.push(part),
            }
        }
        format!("/{}", stack.join("/"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_repo_path_dirroot_file() {
        assert_eq!(PathNotation::new("public/").to_repo_path("@/lib/setup.php"), "public/lib/setup.php");
    }

    #[test]
    fn to_repo_path_repo_root_file() {
        assert_eq!(PathNotation::new("public/").to_repo_path("#/config-dist.php"), "config-dist.php");
    }

    #[test]
    fn to_repo_path_bare_dirroot() {
        assert_eq!(PathNotation::new("public/").to_repo_path("@"), "public");
    }

    #[test]
    fn to_repo_path_nested_dirroot_file() {
        assert_eq!(PathNotation::new("public/").to_repo_path("@/mod/forum/lib.php"), "public/mod/forum/lib.php");
    }

    #[test]
    fn to_repo_path_pre_5_1_dirroot_file() {
        assert_eq!(PathNotation::new("").to_repo_path("@/lib/setup.php"), "lib/setup.php");
    }

    #[test]
    fn to_repo_path_pre_5_1_repo_root_file() {
        assert_eq!(PathNotation::new("").to_repo_path("#/config.php"), "config.php");
    }

    #[test]
    fn normalise_resolves_dot_and_dot_dot() {
        assert_eq!(PathNotation::normalise("/public/mod/forum/../../lib/setup.php"), "/public/lib/setup.php");
        assert_eq!(PathNotation::normalise("/public/./lib"), "/public/lib");
        assert_eq!(PathNotation::normalise("/public/lib/"), "/public/lib/");
    }

    /// A '..' that climbs past the start of the path is kept, so that a path leaving the
    /// repository cannot be mistaken for one inside it.
    #[test]
    fn normalise_keeps_dot_dot_above_the_root() {
        assert_eq!(PathNotation::normalise("/public/lib/../../../../etc/passwd"), "/../../etc/passwd");
        assert_eq!(PathNotation::normalise("/.."), "/..");
        assert_eq!(PathNotation::normalise("/public/.."), "/");
    }

    /// to_repo_path inverts to_glyph for repository-root-relative inputs.
    #[test]
    fn round_trip_from_glyph() {
        let cases = [("public/", "@/lib/setup.php"), ("public/", "#/config-dist.php"), ("", "@/lib/setup.php")];
        for (dirroot_prefix, glyph_path) in cases {
            let notation = PathNotation::new(dirroot_prefix);
            let repo_path = notation.to_repo_path(glyph_path);
            assert_eq!(notation.to_glyph(&format!("/{repo_path}")), glyph_path);
        }
    }
}
