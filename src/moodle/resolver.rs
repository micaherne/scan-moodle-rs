//! Resolves a codebase-relative path to the frankenstyle component that owns it.

use std::collections::HashMap;

use super::components::{ComponentDiscovery, could_be_plugin_name, is_reserved_non_plugin_dir};

#[derive(Debug, Default)]
struct Node {
    /// The frankenstyle name of the component rooted at this node, if any.
    component: Option<String>,
    /// The plugin type rooted at this node, if any (e.g. 'theme' at 'public/theme', or a
    /// subplugin type such as 'quizaccess'). Used only to recognise a dynamic plugin name
    /// immediately below it; a plugin type itself is never a component.
    plugin_type: Option<String>,
    children: HashMap<String, Node>,
}

/// The pseudo-component for paths that lie in the repository but outside every real component —
/// 'public/config.php', 'public/backup/backup.class.php', '.github/...', dirroot itself. Resolving
/// to this is a positive answer ("nothing owns this"), as distinct from not resolving at all
/// ("we cannot tell what owns this").
pub const ROOT_COMPONENT: &str = "root";

/// A component match: the frankenstyle name, and the path within that component's directory. The
/// leading slash distinguishes three cases that would otherwise collapse into one: an empty
/// `path_in_component` means the path *is* the component's own directory ('public/lib' resolving
/// into `core`, whose directory is exactly that); a lone '/' means the same directory referenced
/// with an explicit trailing slash ('public/lib/'); and e.g. '/x.php' means an actual path within
/// it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolution {
    pub component: String,
    pub path_in_component: String,
}

/// A trie of known component and plugin type directories (built from a [`ComponentDiscovery`]),
/// used to resolve a path to the most specific component that contains it.
#[derive(Debug, Default)]
pub struct ComponentResolver {
    root: Node,
}

impl ComponentResolver {
    /// Builds a resolver from the components discovered in a codebase.
    pub fn new(discovery: &ComponentDiscovery) -> Self {
        let mut resolver = Self::default();

        for (component, path) in &discovery.components {
            // Subsystems without a directory of their own have an empty path; there is nothing
            // to index for them, and inserting one would make every path resolve to it.
            if !path.is_empty() {
                resolver.node_mut(path).component = Some(component.clone());
            }
        }
        for (plugin_type, path) in &discovery.plugin_types {
            resolver.node_mut(path).plugin_type = Some(plugin_type.clone());
        }

        resolver
    }

    fn node_mut(&mut self, path: &str) -> &mut Node {
        let mut node = &mut self.root;
        for segment in path.split('/') {
            node = node.children.entry(segment.to_string()).or_default();
        }
        node
    }

    /// Resolves `path` to the most specific component that contains it, along with the path
    /// within that component's directory. Falls back to the [`ROOT_COMPONENT`] pseudo-component
    /// when no component matches but the path can be shown to be outside all of them (see
    /// [`is_certainly_outside_every_component`]); since that pseudo-component's directory is the
    /// repository root, its `path_in_component` is `path` itself with a leading slash spliced on.
    /// Returns `None` when no component matches and that cannot be established.
    ///
    /// `path_in_component` is always a verbatim slice of `path` (never rebuilt by rejoining
    /// segments), so a doubled or trailing slash coming from how the source expression
    /// concatenated the path survives untouched into the output, just as `real_path` preserves it,
    /// rather than being silently normalised away.
    ///
    /// A path segment immediately below a known plugin type's root directory synthesises a
    /// component `{plugin_type}_{segment}` whenever it names a plugin that could exist there,
    /// whether or not this codebase actually has one by that name — the frankenstyle name is
    /// determined by the directory structure alone, not by which plugins happen to be installed
    /// in this particular checkout. This covers two forms: a dynamic marker (e.g. resolving
    /// 'theme/{$themename}/config.php' to 'theme_{$themename}'), which must occupy a whole segment
    /// on its own (no literal text alongside it), mirroring the Moodle convention of never
    /// interpolating more than one directory component into a plugin-name placeholder; and a
    /// literal name that passes Moodle's own plugin-naming rules (e.g. 'mod/xxxxxxx', a name this
    /// codebase has no such plugin under, still synthesises 'mod_xxxxxxx'). A literal name is only
    /// treated this way when it is not already a real, discovered child of that node (an actually
    /// installed plugin is resolved via its own indexed directory instead, further down this loop)
    /// and is not one of the fixed non-plugin subdirectories every plugin type root reserves, such
    /// as 'tests' or 'classes' (see [`is_reserved_non_plugin_dir`]) — those are plain,
    /// plugin-name-shaped words, but discovery itself never treats them as a plugin either.
    pub fn resolve(&self, path: &str) -> Option<Resolution> {
        // Empty segments are not directory names — a trailing or doubled slash is just noise from
        // a concatenation as far as *matching* goes — so they are skipped when walking the trie:
        // 'public//lib/' and 'public/lib' name the same directory. `ends[i]` is the byte offset in
        // `path` immediately after `segments[i]`, i.e. right before whatever separator (single,
        // doubled, or none) follows it; slicing `path` from there is how `path_in_component`
        // recovers the exact remainder without having to reconstruct it from the filtered list.
        let mut segments: Vec<&str> = Vec::new();
        let mut ends: Vec<usize> = Vec::new();
        let mut cursor = 0;
        for part in path.split('/') {
            let end = cursor + part.len();
            if !part.is_empty() {
                segments.push(part);
                ends.push(end);
            }
            cursor = end + 1;
        }

        let mut node = &self.root;
        let mut best = node.component.as_deref().map(|component| (0, component));
        // The segments below `node`, i.e. those the trie walk did not account for.
        let mut unmatched = &segments[..];

        for (i, &segment) in segments.iter().enumerate() {
            let next = node.children.get(segment);
            if let Some(plugin_type) = &node.plugin_type
                && (is_whole_dynamic_segment(segment)
                    || (next.is_none()
                        && could_be_plugin_name(segment)
                        && !is_reserved_non_plugin_dir(plugin_type, segment)))
            {
                return Some(Resolution {
                    component: format!("{plugin_type}_{segment}"),
                    path_in_component: remainder(path, &ends, i + 1),
                });
            }

            let Some(next) = next else {
                break;
            };
            node = next;
            unmatched = &segments[i + 1..];
            if let Some(component) = &node.component {
                best = Some((i + 1, component));
            }
        }

        if let Some((consumed, component)) = best {
            return Some(Resolution { component: component.to_string(), path_in_component: remainder(path, &ends, consumed) });
        }

        is_certainly_outside_every_component(unmatched)
            .then(|| Resolution { component: ROOT_COMPONENT.to_string(), path_in_component: remainder(path, &ends, 0) })
    }
}

/// The part of `path` left over after its first `consumed` non-empty segments, verbatim —
/// including any doubled or trailing slash that follows them — with a leading slash spliced on
/// when `consumed` is 0 (since `path` itself, being repository-relative, never has one). Keeping
/// that slash (rather than trimming it) is what lets an empty `path_in_component` mean "is the
/// component's own directory" while '/' distinctly means "that same directory, referenced with an
/// explicit trailing slash" — collapsing the two would lose that distinction. `ends[i]` is the
/// byte offset in `path` right after the i-th non-empty segment.
fn remainder(path: &str, ends: &[usize], consumed: usize) -> String {
    if consumed == 0 {
        return if path.is_empty() { String::new() } else { format!("/{path}") };
    }
    path[ends[consumed - 1]..].to_string()
}

/// Whether a path that matched no component is certainly owned by no component, rather than
/// merely not matching one. `unmatched` is the segments below wherever the trie walk stopped,
/// which lie outside every directory this codebase knows of.
///
/// The main loop in [`ComponentResolver::resolve`] already disposes of the one thing that could
/// make such a path in fact belong to a component: a plugin that could exist here but simply is
/// not installed *in this codebase*, such as 'public/local/mycustomthing' — that is resolved to a
/// synthesised component directly, whether or not it is actually present. So what reaches here has
/// already been established to sit under no plugin type root with a name that could pass for a
/// plugin (or it does sit under one, but the name definitely could not, e.g.
/// 'public/theme/styles.php' — no plugin can be named 'styles.php', so there is no absent owner to
/// speak of either way). An empty `unmatched` — the path stops exactly at some indexed directory,
/// including a plugin type's own root such as 'public/mod' — is therefore always outside every
/// component too: that directory belongs to Moodle core's own layout, not to any one plugin.
///
/// What remains is only the concern that `unmatched`'s *first* segment specifically might not mean
/// what it literally says. That first segment is the one the trie walk actually tried and failed
/// to match; if it is a '{...}' marker rather than a plain name (e.g. 'public/{$path}', right where
/// a component could start), its real value might have matched something after all, so it stays
/// refused. Everything after that first segment, though, was never looked up against anything —
/// the walk had already given up by then — so nothing there, marker or not, can turn that
/// already-failed first segment into a real subdirectory in hindsight. A '{...}' marker anywhere
/// past the first segment is therefore fine, whether it fills a whole segment on its own (e.g.
/// "public/install/lang/{$options['lang']}") or is fused with literal text, as a variable
/// interpolated straight into a filename is (e.g. "public/lang/en/{$file}.php", for
/// `"$CFG->dirroot/lang/en/$file.php"`, or "public/backup/converter/{$name}/lib.php" further still)
/// — it is just an unknown value sitting inside a directory that is already known, from the plain
/// segments around it, to belong to no component. What is refused wherever it appears, first
/// segment or not, is anything that is neither a plain literal name nor a '{...}' marker: a '..'
/// segment (the path escapes the repository root, so the rest of it describes somewhere else
/// entirely), a backslash (a Windows separator or a namespace, either way not a `/`-delimited path
/// this scheme can reason about), or anything else outside the characters that make up ordinary
/// file names, such as a glob or a URL query string that snuck in as if it were a path.
fn is_certainly_outside_every_component(unmatched: &[&str]) -> bool {
    let Some((first, rest)) = unmatched.split_first() else {
        return true;
    };
    if !is_plain_segment(first) {
        return false;
    }
    rest.iter().all(|segment| is_plain_segment(segment) || segment.contains('{'))
}

/// Whether `segment` is an ordinary, literal file or directory name.
fn is_plain_segment(segment: &str) -> bool {
    !segment.is_empty()
        && segment != "."
        && segment != ".."
        && segment.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '+' | '@' | '~' | ' '))
}

/// Whether `segment` is entirely a single dynamic marker (e.g. '{$themename}'), with no literal
/// text alongside it.
fn is_whole_dynamic_segment(segment: &str) -> bool {
    segment.starts_with('{') && segment.ends_with('}')
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    fn discovery(components: &[(&str, &str)], plugin_types: &[(&str, &str)]) -> ComponentDiscovery {
        ComponentDiscovery {
            components: components.iter().map(|&(name, path)| (name.to_string(), path.to_string())).collect(),
            subsystems: BTreeMap::new(),
            plugin_types: plugin_types.iter().map(|&(name, path)| (name.to_string(), path.to_string())).collect(),
        }
    }

    fn resolve(resolver: &ComponentResolver, path: &str) -> Option<(String, String)> {
        resolver.resolve(path).map(|r| (r.component, r.path_in_component))
    }

    #[test]
    fn resolves_exact_directory() {
        let resolver = ComponentResolver::new(&discovery(&[("mod_forum", "public/mod/forum")], &[]));
        assert_eq!(resolve(&resolver, "public/mod/forum"), Some(("mod_forum".to_string(), String::new())));
    }

    #[test]
    fn resolves_file_within_directory() {
        let resolver = ComponentResolver::new(&discovery(&[("mod_forum", "public/mod/forum")], &[]));
        assert_eq!(resolve(&resolver, "public/mod/forum/lib.php"), Some(("mod_forum".to_string(), "/lib.php".to_string())));
    }

    #[test]
    fn prefers_the_most_specific_match() {
        let resolver = ComponentResolver::new(&discovery(
            &[("mod_quiz", "public/mod/quiz"), ("quiz_overview", "public/mod/quiz/report/overview")],
            &[],
        ));
        assert_eq!(
            resolve(&resolver, "public/mod/quiz/report/overview/report.php"),
            Some(("quiz_overview".to_string(), "/report.php".to_string()))
        );
        assert_eq!(resolve(&resolver, "public/mod/quiz/lib.php"), Some(("mod_quiz".to_string(), "/lib.php".to_string())));
    }

    /// A literal name immediately below a plugin type root, that passes for a plugin name but is
    /// not one this codebase actually has, still synthesises the component it would have if it
    /// existed — the frankenstyle name is determined by the directory structure, not by which
    /// plugins happen to be installed in this particular checkout.
    #[test]
    fn plausible_but_uninstalled_plugin_name_synthesises_a_component() {
        let resolver =
            ComponentResolver::new(&discovery(&[("mod_forum", "public/mod/forum")], &[("mod", "public/mod")]));
        assert_eq!(resolve(&resolver, "public/mod/quiz/lib.php"), Some(("mod_quiz".to_string(), "/lib.php".to_string())));
        assert_eq!(resolve(&resolver, "public/mod/xxxxxxx"), Some(("mod_xxxxxxx".to_string(), String::new())));
    }

    /// 'tests' (and the other fixed, reserved subdirectory names) sits alongside every plugin
    /// type's actual plugins without being one itself, even though it is a perfectly
    /// plugin-name-shaped word — discovery never treats it as a plugin, so synthesis must not
    /// either. This is the regression that plain name-pattern matching alone would cause: e.g.
    /// 'public/lib/editor/tests/fixtures/x.php' must stay resolved via `editor`'s container match
    /// (`core_editor`, if it were registered there), never synthesise a bogus 'editor_tests'.
    #[test]
    fn reserved_non_plugin_dir_name_does_not_synthesise() {
        let resolver =
            ComponentResolver::new(&discovery(&[("mod_forum", "public/mod/forum")], &[("mod", "public/mod")]));
        for name in ["tests", "classes", "db", "lang", "amd"] {
            let path = format!("public/mod/{name}/x.php");
            assert_eq!(resolve(&resolver, &path), Some(("root".to_string(), format!("/{path}"))), "for {path}");
        }
        // 'auth' has one exception: a real plugin literally named 'db'.
        let resolver_auth =
            ComponentResolver::new(&discovery(&[], &[("auth", "public/auth")]));
        assert_eq!(resolve(&resolver_auth, "public/auth/db/foo.php"), Some(("auth_db".to_string(), "/foo.php".to_string())));
    }

    /// A path that stops exactly at a plugin type's own root directory (nothing below it at all)
    /// belongs to Moodle core's own layout, not to any one plugin, so it resolves to root just
    /// like any other location no component owns — with or without a trailing slash.
    #[test]
    fn bare_plugin_type_root_resolves_to_root() {
        let resolver =
            ComponentResolver::new(&discovery(&[("mod_forum", "public/mod/forum")], &[("mod", "public/mod")]));
        assert_eq!(resolve(&resolver, "public/mod"), Some(("root".to_string(), "/public/mod".to_string())));
        assert_eq!(resolve(&resolver, "public/mod/"), Some(("root".to_string(), "/public/mod/".to_string())));
    }

    #[test]
    fn subsystems_without_a_directory_are_not_inserted() {
        let resolver = ComponentResolver::new(&discovery(&[("core_access", "")], &[]));
        assert_eq!(
            resolve(&resolver, "anything/at/all.php"),
            Some(("root".to_string(), "/anything/at/all.php".to_string()))
        );
    }

    fn root_resolver() -> ComponentResolver {
        ComponentResolver::new(&discovery(
            &[("core", "public/lib"), ("mod_forum", "public/mod/forum")],
            &[("mod", "public/mod"), ("theme", "public/theme")],
        ))
    }

    #[test]
    fn path_outside_every_component_resolves_to_root() {
        let resolver = root_resolver();
        for path in ["public/config.php", "public/backup/backup.class.php", "vendor/autoload.php", ".github/x.yml"] {
            assert_eq!(
                resolve(&resolver, path),
                Some(("root".to_string(), format!("/{path}"))),
                "expected {path} to resolve to root"
            );
        }
    }

    /// The repository root and dirroot are themselves owned by nothing.
    #[test]
    fn repository_root_and_dirroot_resolve_to_root() {
        let resolver = root_resolver();
        assert_eq!(resolve(&resolver, ""), Some(("root".to_string(), String::new())));
        assert_eq!(resolve(&resolver, "public"), Some(("root".to_string(), "/public".to_string())));
        // A trailing slash carries real information — it is how e.g. "$CFG->dirroot .
        // DIRECTORY_SEPARATOR" renders — so it must survive into path_in_component exactly as it
        // appears in real_path, not get silently dropped.
        assert_eq!(resolve(&resolver, "public/"), Some(("root".to_string(), "/public/".to_string())));
    }

    /// A doubled slash is noise from a concatenation as far as matching goes — it does not stop
    /// 'public//mod/forum' from being recognised as mod_forum's directory — but it is still part
    /// of the literal path, so path_in_component preserves it verbatim, the same way real_path
    /// does, rather than collapsing it.
    #[test]
    fn empty_segments_are_ignored_for_matching_but_preserved_in_the_output() {
        let resolver = root_resolver();
        assert_eq!(resolve(&resolver, "public//mod/forum//lib.php"), Some(("mod_forum".to_string(), "//lib.php".to_string())));
        assert_eq!(resolve(&resolver, "public//config.php"), Some(("root".to_string(), "/public//config.php".to_string())));
    }

    /// An intermediate directory that merely happens to lie on the way to a component is not a
    /// plugin type root, so nothing can be installed into it.
    #[test]
    fn container_directory_of_a_component_resolves_to_root() {
        let resolver = ComponentResolver::new(&discovery(&[("qtype_essay", "public/question/type/essay")], &[]));
        assert_eq!(resolve(&resolver, "public/question/x.php"), Some(("root".to_string(), "/public/question/x.php".to_string())));
    }

    /// A trailing slash inside a real component's directory, not just at the repository root,
    /// must also survive into path_in_component — e.g. "$CFG->dirroot . '/lib/'" resolving into
    /// core's own directory.
    #[test]
    fn trailing_slash_inside_a_component_is_preserved() {
        let resolver = root_resolver();
        assert_eq!(resolve(&resolver, "public/lib/"), Some(("core".to_string(), "/".to_string())));
        assert_eq!(resolve(&resolver, "public/lib/tcpdf/"), Some(("core".to_string(), "/tcpdf/".to_string())));
    }

    /// The paths that must never resolve at all: the *first* segment the trie walk failed to
    /// match is the one place a '{...}' marker is genuinely ambiguous (its real value might have
    /// matched something), and a few shapes that are not a plain literal location at all.
    #[test]
    fn uncertain_paths_do_not_resolve_to_root() {
        let resolver = root_resolver();
        let cases = [
            // A marker right where the trie walk gave up — its real value could have matched a
            // known child, so it stays refused. Contrast with a marker further down the path
            // (see `a_marker_past_the_first_unmatched_segment_still_resolves_to_root`), which is
            // fine, since nothing downstream of a failed match ever gets looked up at all.
            "public/{$path}",
            "public{$dirpath}",
            // A path that climbed above the repository root, so it describes somewhere else.
            "../etc/passwd",
            "../../moodledata",
            // Windows separators and namespaces hide the real segment boundaries.
            "public\\filter\\algebra\\algebra2tex.pl",
            // A glob covers component directories as readily as root ones.
            "public/*",
            "public/*/lib.php",
            // Not a bare path.
            "public/backup/view.php?id=1",
        ];
        for path in cases {
            assert_eq!(resolve(&resolver, path), None, "expected {path} not to resolve");
        }
    }

    /// A '{...}' marker anywhere *past* the first segment the trie walk failed to match is fine,
    /// however many literal segments separate it from that failure point — nothing after a failed
    /// match is ever looked up, so a marker there can't retroactively change what the already-
    /// established literal directory above it resolves to. `lang` alone (immediately after the
    /// failure) and `backup/converter` (two literal segments deep) both demonstrate this the same
    /// way.
    #[test]
    fn a_marker_past_the_first_unmatched_segment_still_resolves_to_root() {
        let resolver = root_resolver();
        for path in ["public/lang/{$lang}/x.php", "public/backup/converter/{$name}/lib.php"] {
            assert_eq!(resolve(&resolver, path), Some(("root".to_string(), format!("/{path}"))), "for {path}");
        }
    }

    /// Core's own files sit directly inside some plugin type roots. No plugin can be named
    /// 'styles.php', so nothing uninstalled could be their owner.
    #[test]
    fn a_name_no_plugin_could_have_resolves_to_root_under_a_plugin_type_root() {
        let resolver = root_resolver();
        for path in ["public/theme/styles.php", "public/theme/yui_combo.php", "public/local/defaults.php"] {
            assert_eq!(resolve(&resolver, path), Some(("root".to_string(), format!("/{path}"))), "for {path}");
        }
    }

    /// A plausible-looking plugin name below a plugin type root synthesises a component even when
    /// nothing else is known about it in this codebase — whether or not any other plugin of that
    /// type happens to be registered.
    #[test]
    fn plausible_plugin_name_synthesises_regardless_of_what_else_is_installed() {
        let resolver = root_resolver();
        assert_eq!(
            resolve(&resolver, "public/theme/standard/pix/gradient.jpg"),
            Some(("theme_standard".to_string(), "/pix/gradient.jpg".to_string()))
        );
        assert_eq!(resolve(&resolver, "public/mod/a/lib.php"), Some(("mod_a".to_string(), "/lib.php".to_string())));
    }

    /// A '{...}' placeholder that is the *last* segment, sitting under a purely literal directory
    /// that is not any known plugin type or component (here 'install/lang', which is not indexed
    /// at all — 'install' is a directory-less subsystem), is just an unknown leaf name: whatever it
    /// stands for, it cannot turn 'install/lang' into a component boundary.
    #[test]
    fn trailing_placeholder_with_literal_context_resolves_to_root() {
        let resolver = root_resolver();
        let path = "public/install/lang/{$options['lang']}";
        assert_eq!(resolve(&resolver, path), Some(("root".to_string(), format!("/{path}"))));
    }

    /// The same reasoning applies when the placeholder is fused with literal text rather than
    /// filling the whole last segment — a variable interpolated straight into a filename, as in
    /// `"$CFG->dirroot/lang/en/$file.php"`. There is still nothing after it, so it is still just an
    /// unknown leaf name under the already-established 'lang/en' directory.
    #[test]
    fn trailing_placeholder_fused_with_literal_text_resolves_to_root() {
        let resolver = root_resolver();
        let path = "public/lang/en/{$file}.php";
        assert_eq!(resolve(&resolver, path), Some(("root".to_string(), format!("/{path}"))));
    }

    /// Ordinary punctuation in a file name is no reason to give up on it.
    #[test]
    fn unusual_but_plain_file_names_resolve_to_root() {
        let resolver = root_resolver();
        for path in ["lib/bundles/@popperjs/core.js", "public/a+b/c~d/PHP Charting Libraries.txt"] {
            assert_eq!(resolve(&resolver, path), Some(("root".to_string(), format!("/{path}"))), "for {path}");
        }
    }

    #[test]
    fn dynamic_plugin_name_resolves() {
        let resolver = ComponentResolver::new(&discovery(&[], &[("theme", "public/theme")]));
        assert_eq!(
            resolve(&resolver, "public/theme/{$themename}/config.php"),
            Some(("theme_{$themename}".to_string(), "/config.php".to_string()))
        );
    }

    #[test]
    fn dynamic_plugin_name_with_no_trailing_path() {
        let resolver = ComponentResolver::new(&discovery(&[], &[("theme", "public/theme")]));
        assert_eq!(resolve(&resolver, "public/theme/{$themename}"), Some(("theme_{$themename}".to_string(), String::new())));
    }

    #[test]
    fn dynamic_rule_applies_to_subplugin_type_roots_too() {
        let resolver = ComponentResolver::new(&discovery(
            &[("mod_quiz", "public/mod/quiz")],
            &[("quizaccess", "public/mod/quiz/accessrule")],
        ));
        assert_eq!(
            resolve(&resolver, "public/mod/quiz/accessrule/{$rule}/rule.php"),
            Some(("quizaccess_{$rule}".to_string(), "/rule.php".to_string()))
        );
    }

    #[test]
    fn dynamic_rule_requires_a_whole_segment() {
        // 'report-{$x}' mixes literal text with the marker, so it is not a dynamic plugin name;
        // there is no literal child of that name either, so nothing matches at all.
        let resolver = ComponentResolver::new(&discovery(&[], &[("theme", "public/theme")]));
        assert_eq!(resolve(&resolver, "public/theme/report-{$x}/config.php"), None);
    }

    #[test]
    fn dynamic_rule_only_applies_immediately_below_a_plugin_type_root() {
        // The dynamic segment here comes after 'mod/quiz' (a specific plugin), not after 'mod'
        // (the plugin type root), so it does not get treated as a dynamic plugin name; it just
        // falls into the leftover path under the already-resolved 'mod_quiz'.
        let resolver =
            ComponentResolver::new(&discovery(&[("mod_quiz", "public/mod/quiz")], &[("mod", "public/mod")]));
        assert_eq!(
            resolve(&resolver, "public/mod/quiz/{$x}/foo.php"),
            Some(("mod_quiz".to_string(), "/{$x}/foo.php".to_string()))
        );
    }
}
