//! Discovers Moodle "frankenstyle" components (core, subsystems, plugins and subplugins) by
//! reading 'lib/components.json' and then scanning the filesystem, mirroring the rules
//! implemented by `\core\component` (public/lib/classes/component.php).

use std::collections::BTreeMap;
use std::error::Error;
use std::fs;
use std::path::Path;

use serde::Deserialize;

use super::dirroot_prefix;

/// Directory names ignored when scanning a plugin type's root directory for plugins. 'auth' has
/// an exception for a plugin literally named 'db' (`\core\component::fetch_plugins`).
const IGNORED_DIRS: &[&str] =
    &["CVS", "_vti_cnf", "amd", "classes", "db", "fonts", "lang", "pix", "simpletest", "templates", "tests", "yui"];

/// Plugin types that may declare their own subplugins via a 'db/subplugins.json' file
/// (`\core\component::$supportsubplugins`). Do not add more without checking upstream first.
const SUBPLUGIN_CAPABLE_TYPES: &[&str] = &["mod", "editor", "tool", "local"];

#[derive(Debug, Deserialize)]
struct ComponentsJson {
    plugintypes: BTreeMap<String, String>,
    subsystems: BTreeMap<String, Option<String>>,
    #[serde(default)]
    deprecatedplugintypes: BTreeMap<String, String>,
    #[serde(default)]
    deletedplugintypes: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
struct SubpluginsJson {
    #[serde(default)]
    subplugintypes: BTreeMap<String, String>,
    #[serde(default)]
    plugintypes: BTreeMap<String, String>,
}

/// The result of scanning a Moodle codebase for its components.
#[derive(Debug)]
pub struct ComponentDiscovery {
    /// Frankenstyle component name (e.g. 'core', 'core_calendar', 'mod_forum', 'quizaccess_seb')
    /// to its location, relative to `root`.
    pub components: BTreeMap<String, String>,
    /// Core subsystem name (e.g. 'calendar') to its location, relative to `root`. `None` means
    /// the subsystem has no directory of its own (e.g. 'access'), mirroring
    /// `\core\component::get_core_subsystems`.
    pub subsystems: BTreeMap<String, Option<String>>,
    /// Plugin type name (e.g. 'mod', or a subplugin type such as 'quizaccess') to its root
    /// directory, relative to `root`.
    pub plugin_types: BTreeMap<String, String>,
}

/// Discovers all components in the codebase at `root`.
pub fn discover_components(root: &Path) -> Result<ComponentDiscovery, Box<dyn Error>> {
    let content = fs::read_to_string(root.join("lib/components.json"))?;
    let json: ComponentsJson = serde_json::from_str(&content)?;

    let mut components = BTreeMap::new();
    components.insert("core".to_string(), format!("{}lib", dirroot_prefix(root)));

    let subsystems = json.subsystems.clone();
    for (subsystem, path) in &subsystems {
        components.insert(format!("core_{subsystem}"), path.clone().unwrap_or_default());
    }

    let mut plugin_types = BTreeMap::new();
    plugin_types.extend(json.plugintypes);
    plugin_types.extend(json.deprecatedplugintypes);
    plugin_types.extend(json.deletedplugintypes);

    // Subplugin types behave exactly like top-level plugin types once discovered, and their
    // frankenstyle names use only the subtype (e.g. 'quizaccess_seb', not
    // 'mod_quiz_quizaccess_seb'), so collect them first and scan them as a second pass.
    let mut subplugin_types: BTreeMap<String, String> = BTreeMap::new();

    for (plugintype, type_dir) in &plugin_types {
        let plugins = scan_plugin_type(root, plugintype, type_dir, &json.subsystems);

        if SUBPLUGIN_CAPABLE_TYPES.contains(&plugintype.as_str()) {
            for (pluginname, plugin_dir) in &plugins {
                for (subtype, subtype_dir) in read_subplugin_types(root, plugin_dir) {
                    if plugin_types.contains_key(&subtype)
                        || json.subsystems.contains_key(&subtype)
                        || subplugin_types.contains_key(&subtype)
                    {
                        eprintln!(
                            "warning: ignoring duplicate subplugin type '{subtype}' declared by '{plugintype}_{pluginname}'"
                        );
                        continue;
                    }
                    subplugin_types.insert(subtype, subtype_dir);
                }
            }
        }

        for (pluginname, plugin_dir) in plugins {
            components.insert(format!("{plugintype}_{pluginname}"), plugin_dir);
        }
    }

    for (subtype, type_dir) in &subplugin_types {
        let plugins = scan_plugin_type(root, subtype, type_dir, &json.subsystems);
        for (pluginname, plugin_dir) in plugins {
            components.insert(format!("{subtype}_{pluginname}"), plugin_dir);
        }
    }

    plugin_types.extend(subplugin_types);

    Ok(ComponentDiscovery { components, subsystems, plugin_types })
}

/// Lists the plugins of `plugintype` found directly under `type_dir` (relative to `root`), as a
/// map of plugin name to its location relative to `root`.
fn scan_plugin_type(
    root: &Path,
    plugintype: &str,
    type_dir: &str,
    subsystems: &BTreeMap<String, Option<String>>,
) -> BTreeMap<String, String> {
    let mut plugins = BTreeMap::new();

    let Ok(entries) = fs::read_dir(root.join(type_dir)) else {
        return plugins;
    };

    for entry in entries.flatten() {
        if !entry.path().is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();

        let is_auth_db_exception = plugintype == "auth" && name == "db";
        if !is_auth_db_exception && IGNORED_DIRS.contains(&name.as_str()) {
            continue;
        }
        if !is_valid_plugin_name(plugintype, &name, subsystems) {
            continue;
        }

        plugins.insert(name.clone(), format!("{type_dir}/{name}"));
    }

    plugins
}

/// Reads the subplugin types declared by the plugin at `plugin_dir` (relative to `root`), if
/// any, as a map of subtype name to its location relative to `root`.
fn read_subplugin_types(root: &Path, plugin_dir: &str) -> BTreeMap<String, String> {
    let subplugins_path = root.join(plugin_dir).join("db").join("subplugins.json");
    let Ok(content) = fs::read_to_string(&subplugins_path) else {
        return BTreeMap::new();
    };
    let Ok(json) = serde_json::from_str::<SubpluginsJson>(&content) else {
        eprintln!("warning: failed to parse {}", subplugins_path.display());
        return BTreeMap::new();
    };

    if !json.subplugintypes.is_empty() {
        // Preferred: paths are relative to the plugin's own directory.
        json.subplugintypes.into_iter().map(|(subtype, relative_dir)| (subtype, format!("{plugin_dir}/{relative_dir}"))).collect()
    } else {
        // Deprecated fallback: paths predate the Moodle 5.1 dirroot/root split, so they are
        // missing the 'public/' segment rather than being relative to the plugin's directory.
        json.plugintypes
            .into_iter()
            .map(|(subtype, path)| {
                let path = if path.starts_with("public/") { path } else { format!("public/{path}") };
                (subtype, path)
            })
            .collect()
    }
}

/// This method validates a plugin name. Mirrors `\core\component::is_valid_plugin_name`.
fn is_valid_plugin_name(plugintype: &str, pluginname: &str, subsystems: &BTreeMap<String, Option<String>>) -> bool {
    if plugintype == "mod" {
        // Modules must not have the same name as a core subsystem that has its own directory.
        if subsystems.get(pluginname).is_some_and(|path| path.is_some()) {
            return false;
        }
        // Modules MUST NOT have any underscores, component normalisation would break otherwise.
        return is_mod_plugin_name(pluginname);
    }

    if plugintype == "qtype" && pluginname == "random" {
        // qtype_random no longer exists and must never be reused (see backup/restore handling).
        return false;
    }

    is_generic_plugin_name(pluginname)
}

/// `^[a-z][a-z0-9]*$`
fn is_mod_plugin_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    match bytes.first() {
        Some(&first) if first.is_ascii_lowercase() => bytes[1..].iter().all(|&b| b.is_ascii_lowercase() || b.is_ascii_digit()),
        _ => false,
    }
}

/// `^[a-z](?:[a-z0-9_](?!__))*[a-z0-9]+$`. The lookahead only rejects a run of three or more
/// consecutive underscores; equivalently, this rejects "___" anywhere and a trailing underscore.
fn is_generic_plugin_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    if bytes.len() < 2 {
        return false;
    }
    if !bytes[0].is_ascii_lowercase() {
        return false;
    }
    if !bytes[1..].iter().all(|&b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_') {
        return false;
    }
    if *bytes.last().unwrap() == b'_' {
        return false;
    }

    let mut underscore_run = 0;
    for &b in bytes {
        if b == b'_' {
            underscore_run += 1;
            if underscore_run >= 3 {
                return false;
            }
        } else {
            underscore_run = 0;
        }
    }

    true
}
