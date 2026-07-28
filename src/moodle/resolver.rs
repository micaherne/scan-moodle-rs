//! Resolves a codebase-relative path to the frankenstyle component that owns it.

use std::collections::HashMap;

use super::components::ComponentDiscovery;

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

/// A component match: the frankenstyle name, and the path within that component's directory
/// (with a leading slash, or empty if the path is the component's own root).
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
    /// within that component's directory. Returns `None` if no component matches.
    ///
    /// A path segment is treated as a dynamic plugin name — e.g. resolving
    /// 'theme/{$themename}/config.php' to 'theme_{$themename}' — when it is the first dynamic
    /// (i.e. containing '{') segment reached while walking `path`, occupies a whole segment on
    /// its own (no literal text alongside it in that segment), and immediately follows a known
    /// plugin type's root directory. This mirrors the Moodle convention of never interpolating
    /// more than one directory component into a plugin-name placeholder.
    pub fn resolve(&self, path: &str) -> Option<Resolution> {
        let segments: Vec<&str> = path.split('/').collect();
        let mut node = &self.root;
        let mut best = node.component.as_deref().map(|component| (0, component));

        for (i, &segment) in segments.iter().enumerate() {
            if let Some(plugin_type) = &node.plugin_type
                && is_whole_dynamic_segment(segment)
            {
                return Some(Resolution {
                    component: format!("{plugin_type}_{segment}"),
                    path_in_component: join_with_leading_slash(&segments[i + 1..]),
                });
            }

            let Some(next) = node.children.get(segment) else {
                break;
            };
            node = next;
            if let Some(component) = &node.component {
                best = Some((i + 1, component));
            }
        }

        best.map(|(consumed, component)| Resolution {
            component: component.to_string(),
            path_in_component: join_with_leading_slash(&segments[consumed..]),
        })
    }
}

/// Whether `segment` is entirely a single dynamic marker (e.g. '{$themename}'), with no literal
/// text alongside it.
fn is_whole_dynamic_segment(segment: &str) -> bool {
    segment.starts_with('{') && segment.ends_with('}')
}

fn join_with_leading_slash(segments: &[&str]) -> String {
    if segments.is_empty() { String::new() } else { format!("/{}", segments.join("/")) }
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

    #[test]
    fn unknown_path_does_not_match() {
        let resolver = ComponentResolver::new(&discovery(&[("mod_forum", "public/mod/forum")], &[]));
        assert_eq!(resolve(&resolver, "public/mod/quiz/lib.php"), None);
    }

    #[test]
    fn subsystems_without_a_directory_are_not_inserted() {
        let resolver = ComponentResolver::new(&discovery(&[("core_access", "")], &[]));
        assert_eq!(resolve(&resolver, "anything/at/all.php"), None);
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
