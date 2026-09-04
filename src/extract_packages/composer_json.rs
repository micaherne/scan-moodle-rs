//! Builds and writes each package's `composer.json`.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io;
use std::path::Path;

use serde::Serialize;

use crate::moodle::entrypoints::{BootstrapKind, FileClassification};
use crate::moodle::resolver::{ComponentResolver, ROOT_COMPONENT};

use super::copy::component_dir_name;

const VENDOR: &str = "moodle";
/// The SPDX identifier Moodle's own upstream `composer.json` uses.
const LICENSE: &str = "GPL-3.0-or-later";

/// Known-bad stopgap: Moodle's own upstream `composer.json` declares `core_testing\` and
/// `core_phpunit\` under `autoload-dev`, mapped to `public/lib/testing/classes/` and
/// `public/lib/phpunit/classes/` — both of which are `core`'s own directory (`public/lib`), so
/// they land inside the `core` package once split out, same as the rest of it.
///
/// `autoload-dev` only gets consulted by Composer when the *hosting* package is literally the
/// project root, never when it's installed as a dependency — and `\core\testing_update_composer_dependencies()`
/// (`public/lib/testing/lib.php`) confirms this isn't just a theoretical Composer rule but exactly
/// what Moodle's own PHPUnit/Behat bootstrap already assumes: it explicitly checks whether it's
/// running as the Composer root package and, if not, skips installing/autoloading its dev tooling
/// entirely, trusting whatever *is* root to have handled it. Once `core` is a dependency of some
/// other project rather than the project root itself, nothing makes these two `autoload-dev`
/// entries fire, ever — so left as `autoload-dev`, `core_testing\`/`core_phpunit\` would simply
/// never be loadable again.
///
/// The fix here is to promote them to `core`'s plain (always-on) `autoload` instead, so they're at
/// least reachable. This is not the right shape for the problem: it makes two classes that exist
/// solely to support running tests unconditionally autoloadable for every consumer of `core`,
/// whether or not they're running any tests at all — a real architectural wrongness, not just a
/// style nitpick, even though the practical cost (a couple of always-registered autoload entries)
/// is negligible. There's no properly-separated fix in place; this is a known compromise, kept as
/// one deliberately rather than solved further.
const CORE_TESTING_AUTOLOAD: [(&str, &str); 2] = [
    ("core_testing\\", "testing/classes/"),
    ("core_phpunit\\", "phpunit/classes/"),
];

#[derive(Serialize)]
struct ComposerJson {
    name: String,
    #[serde(rename = "type")]
    package_type: &'static str,
    license: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    autoload: Option<Autoload>,
    #[serde(skip_serializing_if = "Option::is_none")]
    extra: Option<Extra>,
}

#[derive(Serialize)]
struct Autoload {
    #[serde(rename = "psr-4")]
    psr4: BTreeMap<String, String>,
}

#[derive(Serialize)]
struct Extra {
    moodle: MoodleExtra,
}

#[derive(Serialize)]
struct MoodleExtra {
    #[serde(skip_serializing_if = "Option::is_none")]
    component: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    entrypoints: Option<Entrypoints>,
}

/// One component's bootstrap-relevant files, grouped by kind, in the shape
/// `extra.moodle.entrypoints` takes. Each path is relative to the component's own root, with no
/// leading slash. An empty `Vec` is skipped entirely when serialising — see
/// [`Entrypoints::is_empty`] for why a wholly-empty one is never serialised at all.
///
/// Only two buckets: [`BootstrapKind::Cli`] gets its own, since a sysadmin or cron job invokes
/// those directly and needs to know which files that is true of. Everything else —
/// [`BootstrapKind::Other`] and [`BootstrapKind::BootstrapDependency`] alike — lands in `other`:
/// this metadata exists purely to tell a Composer plugin which files must be physically placed
/// back at a fixed location, and both of those kinds need exactly that, for exactly the same
/// reason (neither can be found through `core\component` once split into a package). There is no
/// programmatic way to tell an ordinary page apart from internal framework plumbing that happens
/// to reach the boundary the same way, so this project doesn't try to.
#[derive(Default, Serialize)]
pub(super) struct Entrypoints {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    cli: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    other: Vec<String>,
}

impl Entrypoints {
    fn is_empty(&self) -> bool {
        self.cli.is_empty() && self.other.is_empty()
    }
}

/// Groups `entry_points` by which component each one resolves into, stripping the leading slash
/// [`crate::moodle::resolver::Resolution::path_in_component`] always carries — composer.json wants
/// a plain relative path, not this project's own internal convention. Entries resolving into
/// `root` (or not resolving to a real, non-empty path within a component at all) are dropped
/// entirely: there would be nothing to do with them if they were kept — [`write_root`], unlike
/// [`write`], has no `extra.moodle.entrypoints` field to put them in, so a `root` entry in the map
/// this builds would just go unread. `moodle-root` is an ordinary Composer package like every other
/// one this tool produces — it just doesn't carry this particular piece of metadata today.
pub(super) fn group_by_component(
    entry_points: &[FileClassification],
    resolver: &ComponentResolver,
) -> HashMap<String, Entrypoints> {
    let mut by_component: HashMap<String, Entrypoints> = HashMap::new();
    for classification in entry_points {
        let resolution = match resolver.resolve(&classification.file) {
            Some(resolution)
                if !resolution.path_in_component.is_empty()
                    && resolution.component != ROOT_COMPONENT =>
            {
                resolution
            }
            _ => continue,
        };
        let path = resolution
            .path_in_component
            .trim_start_matches('/')
            .to_string();
        let bucket = by_component.entry(resolution.component).or_default();
        match classification.kind {
            BootstrapKind::Cli => bucket.cli.push(path),
            BootstrapKind::Other | BootstrapKind::BootstrapDependency => bucket.other.push(path),
        }
    }
    by_component
}

/// `moodle/moodle-<component>` — the package name every component's own `composer.json` (real
/// or `root`) is written with. Shared with [`write_metapackage`] so the dependencies it lists can
/// never drift out of sync with what each package actually calls itself.
fn package_name(component: &str) -> String {
    format!("{VENDOR}/{}", component_dir_name(component))
}

/// Writes `component`'s `composer.json` into `package_dir` — for a real component only; `root`
/// goes through [`write_root`] instead, which starts from Moodle's own upstream file rather than
/// building one from scratch.
///
/// `extra.moodle.component` is written only when `package_dir` (already fully populated by the
/// copy step that runs before this) has no `version.php` of its own: a plugin can be named from
/// its own `version.php` at runtime — Composer's own plugin-type handler reads it — and setting
/// `extra.moodle.component` as well on a package that already has one is not just redundant but
/// flagged and ignored by Moodle's Composer plugin. A subsystem (e.g. `core_completion`) has no
/// `version.php` to read, so its component name has to come from here instead. `extra` itself
/// (and `extra.moodle`) is omitted entirely when it would otherwise end up empty.
/// `extra.moodle.entrypoints` is likewise omitted (not written as an empty object) when
/// `entry_points` is `None` or empty.
pub(super) fn write(
    package_dir: &Path,
    component: &str,
    entry_points: Option<Entrypoints>,
) -> io::Result<()> {
    debug_assert_ne!(
        component, ROOT_COMPONENT,
        "the root package goes through write_root instead"
    );
    let entry_points = entry_points.filter(|entrypoints| !entrypoints.is_empty());
    let component_name = (!package_dir.join("version.php").exists()).then(|| component.to_string());

    let mut psr4 = BTreeMap::from([(format!("{component}\\"), "classes/".to_string())]);
    // See CORE_TESTING_AUTOLOAD's own doc comment for why this is here and why it's not great.
    if component == "core" {
        psr4.extend(
            CORE_TESTING_AUTOLOAD
                .iter()
                .map(|(namespace, dir)| (namespace.to_string(), dir.to_string())),
        );
    }

    let extra = (component_name.is_some() || entry_points.is_some()).then_some(Extra {
        moodle: MoodleExtra {
            component: component_name,
            entrypoints: entry_points,
        },
    });

    let composer = ComposerJson {
        name: package_name(component),
        package_type: "moodle-component",
        license: LICENSE,
        autoload: Some(Autoload { psr4 }),
        extra,
    };

    let json = serde_json::to_string_pretty(&composer).expect("ComposerJson always serialises");
    fs::write(package_dir.join("composer.json"), format!("{json}\n"))
}

/// Writes `moodle-root`'s `composer.json`. The one-pass copy already placed Moodle's own upstream
/// `composer.json` verbatim at `package_dir/composer.json` if the codebase being extracted has one
/// (an ordinary, unowned repository-root file, so it resolves into `root` the same as any other) —
/// this overwrites just its `name` and `type` fields in place, leaving everything else (`require`,
/// `require-dev`, `autoload`, `autoload-dev`, `provide`, ...) exactly as upstream has it, key order
/// included. Deliberately not resolving what any of those fields should now mean in a split-up
/// codebase (e.g. `autoload-dev`'s paths may point at content that moved into another package) —
/// that is a real open question, not something to silently paper over here.
///
/// Falls back to the bare `{name, type, license}` shape every other package gets if there's no
/// existing `composer.json` to build on (or it isn't valid JSON) — the codebase being extracted may
/// simply not have one.
pub(super) fn write_root(package_dir: &Path) -> io::Result<()> {
    let path = package_dir.join("composer.json");

    let mut composer = fs::read_to_string(&path)
        .ok()
        .and_then(|existing| serde_json::from_str::<serde_json::Value>(&existing).ok())
        .filter(|value| value.is_object())
        .unwrap_or_else(|| serde_json::Value::Object(serde_json::Map::new()));

    let object = composer
        .as_object_mut()
        .expect("filtered to an object above, or just built one");
    object.insert(
        "name".to_string(),
        serde_json::Value::String(package_name(ROOT_COMPONENT)),
    );
    object.insert(
        "type".to_string(),
        serde_json::Value::String("moodle-root".to_string()),
    );
    object
        .entry("license")
        .or_insert_with(|| serde_json::Value::String(LICENSE.to_string()));

    let json = serde_json::to_string_pretty(&composer).expect("a JSON object always serialises");
    fs::write(path, format!("{json}\n"))
}

/// `moodle-standard`'s own directory/package name — `moodle-standard` isn't a real component
/// [`component_dir_name`] would ever produce on its own, it's fabricated purely for this one
/// metapackage, so it's spelled out here rather than derived.
pub(super) const METAPACKAGE_DIR_NAME: &str = "moodle-standard";

#[derive(Serialize)]
struct Metapackage {
    name: String,
    #[serde(rename = "type")]
    package_type: &'static str,
    license: &'static str,
    require: BTreeMap<String, String>,
}

/// Writes the `moodle-standard` metapackage's `composer.json` into `package_dir`: a single package
/// that requires every one of `components` (every component this run actually produced a package
/// for — `root` included, via [`ROOT_COMPONENT`]), so that requiring just `moodle-standard` pulls
/// in the whole split-up codebase. Composer's own `metapackage` type is used deliberately — it
/// tells Composer there is no code of its own to install, only dependencies to resolve, which
/// matches what this directory actually contains (nothing but this one file).
///
/// Every dependency is constrained to `*`. None of these packages ever get a tagged version (see
/// `git.rs`'s own doc comment: each one is `dev-main` and nothing else), so whatever project ends
/// up requiring `moodle-standard` needs `minimum-stability: dev` regardless of what constraint is
/// written here — there is no tagged version `*` could otherwise prefer instead.
pub(super) fn write_metapackage(
    package_dir: &Path,
    components: impl IntoIterator<Item = impl AsRef<str>>,
) -> io::Result<()> {
    let require = components
        .into_iter()
        .map(|component| (package_name(component.as_ref()), "*".to_string()))
        .collect();

    let composer = Metapackage {
        name: format!("{VENDOR}/{METAPACKAGE_DIR_NAME}"),
        package_type: "metapackage",
        license: LICENSE,
        require,
    };

    let json = serde_json::to_string_pretty(&composer).expect("Metapackage always serialises");
    fs::write(package_dir.join("composer.json"), format!("{json}\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::moodle::components::{ComponentDiscovery, discover_components};
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn temp_dir(name: &str) -> PathBuf {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "scan-moodle-composer-json-test-{name}-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_file(root: &Path, relative: &str, contents: &str) {
        let path = root.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    fn discovery_with_mod_quiz(root: &Path) -> ComponentDiscovery {
        write_file(
            root,
            "lib/components.json",
            r#"{"plugintypes": {"mod": "mod"}, "subsystems": {}}"#,
        );
        write_file(root, "mod/quiz/lib.php", "<?php\n");
        discover_components(root).unwrap()
    }

    fn classification(file: &str, kind: BootstrapKind) -> FileClassification {
        FileClassification {
            file: file.to_string(),
            kind,
            extent: None,
        }
    }

    #[test]
    fn groups_entry_points_by_component_and_strips_leading_slash() {
        let root = temp_dir("group-root");
        let discovered = discovery_with_mod_quiz(&root);
        let resolver = ComponentResolver::new(&discovered);

        let entry_points = vec![
            classification("mod/quiz/view.php", BootstrapKind::Other),
            classification("mod/quiz/cli/upgrade.php", BootstrapKind::Cli),
            classification("lib/setup.php", BootstrapKind::Other),
            classification(
                "lib/phpminimumversionlib.php",
                BootstrapKind::BootstrapDependency,
            ),
            // Resolves to root — must be dropped, not attributed to any package.
            classification("config.php", BootstrapKind::Other),
        ];

        let grouped = group_by_component(&entry_points, &resolver);

        assert_eq!(grouped["mod_quiz"].other, vec!["view.php".to_string()]);
        assert_eq!(grouped["mod_quiz"].cli, vec!["cli/upgrade.php".to_string()]);
        // Other and BootstrapDependency land in the same bucket — see `Entrypoints`'s own doc
        // comment for why the split isn't carried through to composer.json.
        assert_eq!(
            grouped["core"].other,
            vec![
                "setup.php".to_string(),
                "phpminimumversionlib.php".to_string()
            ]
        );
        assert!(!grouped.contains_key(ROOT_COMPONENT));

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn real_component_gets_autoload_and_entrypoints() {
        let dir = temp_dir("write-component");
        let mut entry_points = Entrypoints::default();
        entry_points.other.push("view.php".to_string());

        write(&dir, "mod_quiz", Some(entry_points)).unwrap();

        let content = fs::read_to_string(dir.join("composer.json")).unwrap();
        let value: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(value["name"], "moodle/moodle-mod_quiz");
        assert_eq!(value["type"], "moodle-component");
        assert_eq!(value["license"], "GPL-3.0-or-later");
        assert_eq!(value["autoload"]["psr-4"]["mod_quiz\\"], "classes/");
        assert_eq!(value["extra"]["moodle"]["component"], "mod_quiz");
        assert_eq!(
            value["extra"]["moodle"]["entrypoints"]["other"][0],
            "view.php"
        );
        assert!(value["extra"]["moodle"]["entrypoints"].get("cli").is_none());

        fs::remove_dir_all(&dir).ok();
    }

    /// See `CORE_TESTING_AUTOLOAD`'s doc comment: a known-bad stopgap that promotes core's
    /// testing-support classes from upstream's (never-reachable-as-a-dependency) `autoload-dev`
    /// into plain `autoload`, specifically and only for `core`.
    #[test]
    fn core_specifically_gets_the_testing_support_autoload_entries_too() {
        let dir = temp_dir("write-core");

        write(&dir, "core", None).unwrap();

        let content = fs::read_to_string(dir.join("composer.json")).unwrap();
        let value: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(value["autoload"]["psr-4"]["core\\"], "classes/");
        assert_eq!(
            value["autoload"]["psr-4"]["core_testing\\"],
            "testing/classes/"
        );
        assert_eq!(
            value["autoload"]["psr-4"]["core_phpunit\\"],
            "phpunit/classes/"
        );

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn only_core_gets_the_testing_support_autoload_entries() {
        let dir = temp_dir("write-non-core");

        write(&dir, "mod_quiz", None).unwrap();

        let content = fs::read_to_string(dir.join("composer.json")).unwrap();
        let value: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert!(value["autoload"]["psr-4"].get("core_testing\\").is_none());
        assert!(value["autoload"]["psr-4"].get("core_phpunit\\").is_none());

        fs::remove_dir_all(&dir).ok();
    }

    /// `extra.moodle.component` is written when the package has no `version.php` of its own —
    /// mirroring a subsystem, which has none to name itself from at runtime — even with no entry
    /// points. Only `extra.moodle.entrypoints` is conditional on entry points existing.
    #[test]
    fn component_with_no_version_php_and_no_entry_points_still_gets_extra_moodle_component() {
        let dir = temp_dir("write-no-entrypoints");

        write(&dir, "mod_quiz", None).unwrap();

        let content = fs::read_to_string(dir.join("composer.json")).unwrap();
        let value: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(value["extra"]["moodle"]["component"], "mod_quiz");
        assert!(value["extra"]["moodle"].get("entrypoints").is_none());

        fs::remove_dir_all(&dir).ok();
    }

    /// A package that already has its own `version.php` (a real plugin, not a subsystem) must not
    /// also get `extra.moodle.component`: Moodle's Composer plugin reads the component name from
    /// `version.php` in that case, and warns that `extra.moodle.component` is being ignored if
    /// it's set as well. With no entry points either, `extra` ends up wholly empty and is omitted.
    #[test]
    fn component_with_version_php_and_no_entry_points_omits_extra_entirely() {
        let dir = temp_dir("write-with-version-php");
        write_file(
            &dir,
            "version.php",
            "<?php\n$plugin->component = 'mod_quiz';\n",
        );

        write(&dir, "mod_quiz", None).unwrap();

        let content = fs::read_to_string(dir.join("composer.json")).unwrap();
        let value: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert!(value.get("extra").is_none());

        fs::remove_dir_all(&dir).ok();
    }

    /// Same as above, but with entry points: `extra.moodle.entrypoints` still has to be written
    /// (nothing else carries that metadata), just without `component` alongside it.
    #[test]
    fn component_with_version_php_keeps_entrypoints_but_omits_component() {
        let dir = temp_dir("write-with-version-php-and-entrypoints");
        write_file(
            &dir,
            "version.php",
            "<?php\n$plugin->component = 'mod_quiz';\n",
        );
        let mut entry_points = Entrypoints::default();
        entry_points.other.push("view.php".to_string());

        write(&dir, "mod_quiz", Some(entry_points)).unwrap();

        let content = fs::read_to_string(dir.join("composer.json")).unwrap();
        let value: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert!(value["extra"]["moodle"].get("component").is_none());
        assert_eq!(
            value["extra"]["moodle"]["entrypoints"]["other"][0],
            "view.php"
        );

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn root_falls_back_to_the_bare_stub_when_there_is_no_upstream_composer_json() {
        let dir = temp_dir("write-root-no-upstream");

        write_root(&dir).unwrap();

        let content = fs::read_to_string(dir.join("composer.json")).unwrap();
        let value: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(value["name"], "moodle/moodle-root");
        assert_eq!(value["type"], "moodle-root");
        assert_eq!(value["license"], "GPL-3.0-or-later");
        assert!(value.get("autoload").is_none());
        assert!(value.get("require").is_none());

        fs::remove_dir_all(&dir).ok();
    }

    /// The whole point of [`write_root`]: everything the one-pass copy already placed at
    /// `composer.json` (Moodle's own upstream file) survives untouched — including fields this
    /// tool has no opinion on at all (`require`, `require-dev`, `autoload-dev`, `provide`) and the
    /// original key order — except `name` and `type`, which get overwritten in place.
    #[test]
    fn root_preserves_the_upstream_composer_json_except_name_and_type() {
        let dir = temp_dir("write-root-with-upstream");
        // The one-pass copy would have already put this here verbatim; write_root starts from it.
        fs::write(
            dir.join("composer.json"),
            r#"{
    "name": "moodle/moodle",
    "license": "GPL-3.0-or-later",
    "type": "moodle-core",
    "require": {
        "php": ">=8.3.0"
    },
    "require-dev": {
        "filp/whoops": "^2.15"
    },
    "autoload-dev": {
        "psr-4": {
            "core_phpunit\\": "public/lib/phpunit/classes/"
        }
    }
}
"#,
        )
        .unwrap();

        write_root(&dir).unwrap();

        let content = fs::read_to_string(dir.join("composer.json")).unwrap();
        let value: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(value["name"], "moodle/moodle-root");
        assert_eq!(value["type"], "moodle-root");
        assert_eq!(value["require"]["php"], ">=8.3.0");
        assert_eq!(value["require-dev"]["filp/whoops"], "^2.15");
        assert_eq!(
            value["autoload-dev"]["psr-4"]["core_phpunit\\"],
            "public/lib/phpunit/classes/"
        );

        let keys: Vec<&String> = value.as_object().unwrap().keys().collect();
        assert_eq!(
            keys,
            vec![
                "name",
                "license",
                "type",
                "require",
                "require-dev",
                "autoload-dev"
            ],
            "key order should match the upstream file, not be alphabetised"
        );

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn metapackage_requires_every_component_by_its_own_package_name() {
        let dir = temp_dir("write-metapackage");

        write_metapackage(&dir, ["mod_quiz", "core", ROOT_COMPONENT]).unwrap();

        let content = fs::read_to_string(dir.join("composer.json")).unwrap();
        let value: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(value["name"], "moodle/moodle-standard");
        assert_eq!(value["type"], "metapackage");
        assert_eq!(value["license"], "GPL-3.0-or-later");
        assert_eq!(value["require"]["moodle/moodle-mod_quiz"], "*");
        assert_eq!(value["require"]["moodle/moodle-core"], "*");
        assert_eq!(value["require"]["moodle/moodle-root"], "*");
        assert!(value.get("autoload").is_none());

        fs::remove_dir_all(&dir).ok();
    }

    /// `require` is a `BTreeMap`, so this is really pinning `serde_json`'s own behaviour, but it's
    /// worth pinning explicitly: a metapackage listing hundreds of dependencies is only usable if
    /// they're easy to scan, and insertion order (`components_used` is a `HashSet`, so this would
    /// otherwise be arbitrary and vary from run to run) would defeat that.
    #[test]
    fn metapackage_requires_are_sorted_alphabetically() {
        let dir = temp_dir("write-metapackage-sorted");

        write_metapackage(&dir, ["mod_quiz", "core", "auth_manual"]).unwrap();

        let content = fs::read_to_string(dir.join("composer.json")).unwrap();
        let value: serde_json::Value = serde_json::from_str(&content).unwrap();
        let keys: Vec<&String> = value["require"].as_object().unwrap().keys().collect();
        assert_eq!(
            keys,
            vec![
                "moodle/moodle-auth_manual",
                "moodle/moodle-core",
                "moodle/moodle-mod_quiz",
            ]
        );

        fs::remove_dir_all(&dir).ok();
    }
}
