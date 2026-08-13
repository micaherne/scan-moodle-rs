//! Builds and writes each package's `composer.json`.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io;
use std::path::Path;

use serde::Serialize;

use crate::moodle::entrypoints::{EntrypointKind, FileClassification};
use crate::moodle::resolver::{ComponentResolver, ROOT_COMPONENT};

use super::copy::component_dir_name;

const VENDOR: &str = "moodlehq";
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
/// is negligible. The properly-separated fix — e.g. a dedicated testing package `core` only pulls
/// in for test scenarios — hasn't been designed. Revisit before this is treated as final.
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
    entrypoints: Entrypoints,
}

/// One component's entry points, grouped by kind, in the shape `extra.moodle.entrypoints` takes.
/// Each path is relative to the component's own root, with no leading slash. An empty `Vec` is
/// skipped entirely when serialising — see [`Entrypoints::is_empty`] for why a wholly-empty one is
/// never serialised at all.
#[derive(Default, Serialize)]
pub(super) struct Entrypoints {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    page: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    cli: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    bootstrap: Vec<String>,
}

impl Entrypoints {
    fn is_empty(&self) -> bool {
        self.page.is_empty() && self.cli.is_empty() && self.bootstrap.is_empty()
    }
}

/// Groups `entry_points` by which component each one resolves into, stripping the leading slash
/// [`crate::moodle::resolver::Resolution::path_in_component`] always carries — composer.json wants
/// a plain relative path, not this project's own internal convention. Entries resolving into
/// `root` (or not resolving to a real, non-empty path within a component at all) are dropped
/// entirely: `moodle-root`'s package never carries entrypoint metadata, regardless of which files
/// happen to live there — it's intended to be copied wholesale into a user's Moodle project by a
/// (not yet written) Composer plugin, not consumed via ordinary Composer autoloading.
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
            EntrypointKind::Page => bucket.page.push(path),
            EntrypointKind::Cli => bucket.cli.push(path),
            EntrypointKind::Bootstrap => bucket.bootstrap.push(path),
        }
    }
    by_component
}

/// Writes `component`'s `composer.json` into `package_dir` — for a real component only; `root`
/// goes through [`write_root`] instead, which starts from Moodle's own upstream file rather than
/// building one from scratch. `extra.moodle.entrypoints` is omitted entirely (not written as an
/// empty object) when `entry_points` is `None` or empty.
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

    let mut psr4 = BTreeMap::from([(format!("{component}\\"), "classes/".to_string())]);
    // See CORE_TESTING_AUTOLOAD's own doc comment for why this is here and why it's not great.
    if component == "core" {
        psr4.extend(
            CORE_TESTING_AUTOLOAD
                .iter()
                .map(|(namespace, dir)| (namespace.to_string(), dir.to_string())),
        );
    }

    let composer = ComposerJson {
        name: format!("{VENDOR}/{}", component_dir_name(component)),
        package_type: "moodle-component",
        license: LICENSE,
        autoload: Some(Autoload { psr4 }),
        extra: entry_points.map(|entrypoints| Extra {
            moodle: MoodleExtra { entrypoints },
        }),
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
        serde_json::Value::String(format!("{VENDOR}/{}", component_dir_name(ROOT_COMPONENT))),
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

    fn classification(file: &str, kind: EntrypointKind) -> FileClassification {
        FileClassification {
            file: file.to_string(),
            kind,
            line: None,
        }
    }

    #[test]
    fn groups_entry_points_by_component_and_strips_leading_slash() {
        let root = temp_dir("group-root");
        let discovered = discovery_with_mod_quiz(&root);
        let resolver = ComponentResolver::new(&discovered);

        let entry_points = vec![
            classification("mod/quiz/view.php", EntrypointKind::Page),
            classification("mod/quiz/cli/upgrade.php", EntrypointKind::Cli),
            classification("lib/setup.php", EntrypointKind::Bootstrap),
            // Resolves to root — must be dropped, not attributed to any package.
            classification("config.php", EntrypointKind::Page),
        ];

        let grouped = group_by_component(&entry_points, &resolver);

        assert_eq!(grouped["mod_quiz"].page, vec!["view.php".to_string()]);
        assert_eq!(grouped["mod_quiz"].cli, vec!["cli/upgrade.php".to_string()]);
        assert_eq!(grouped["core"].bootstrap, vec!["setup.php".to_string()]);
        assert!(!grouped.contains_key(ROOT_COMPONENT));

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn real_component_gets_autoload_and_entrypoints() {
        let dir = temp_dir("write-component");
        let mut entry_points = Entrypoints::default();
        entry_points.page.push("view.php".to_string());

        write(&dir, "mod_quiz", Some(entry_points)).unwrap();

        let content = fs::read_to_string(dir.join("composer.json")).unwrap();
        let value: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(value["name"], "moodlehq/moodle-mod_quiz");
        assert_eq!(value["type"], "moodle-component");
        assert_eq!(value["license"], "GPL-3.0-or-later");
        assert_eq!(value["autoload"]["psr-4"]["mod_quiz\\"], "classes/");
        assert_eq!(
            value["extra"]["moodle"]["entrypoints"]["page"][0],
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

    #[test]
    fn component_with_no_entry_points_omits_extra_entirely() {
        let dir = temp_dir("write-no-entrypoints");

        write(&dir, "mod_quiz", None).unwrap();

        let content = fs::read_to_string(dir.join("composer.json")).unwrap();
        let value: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert!(value.get("extra").is_none());

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn root_falls_back_to_the_bare_stub_when_there_is_no_upstream_composer_json() {
        let dir = temp_dir("write-root-no-upstream");

        write_root(&dir).unwrap();

        let content = fs::read_to_string(dir.join("composer.json")).unwrap();
        let value: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(value["name"], "moodlehq/moodle-root");
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
        assert_eq!(value["name"], "moodlehq/moodle-root");
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
}
