//! The one-pass copy that turns a (rewritten) Moodle codebase into a directory of per-component
//! trees, one file write each.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use rayon::prelude::*;
use walkdir::WalkDir;

use crate::moodle::resolver::{ComponentResolver, ROOT_COMPONENT};
use crate::moodle::scan::relative_unix_path;

/// The directory a component's copy is written to, under the destination. `root` — the
/// pseudo-component covering everything no real component owns — is deliberately not special-cased:
/// it becomes `moodle-root` by exactly the same rule as every other name.
pub(super) fn component_dir_name(component: &str) -> String {
    format!("moodle-{component}")
}

/// What the copy actually did.
pub(super) struct CopyOutcome {
    pub(super) copied: usize,
    pub(super) bytes: usize,
    pub(super) failed: usize,
    pub(super) walked: std::time::Duration,
    pub(super) elapsed: std::time::Duration,
    /// Every path that didn't resolve to a real component (or resolved to its own component
    /// directory, which a file can never be) and so fell back to `root` — reported rather than
    /// silently absorbed, same reasoning as [`crate::moodle::resolver`]'s own fallback.
    pub(super) unresolved: Vec<String>,
    /// Every component (real or `root`) that actually received at least one file — the set of
    /// packages that genuinely need a `composer.json`, as opposed to every component `discover`
    /// happens to know about (a subsystem with no directory of its own, e.g., never receives a
    /// file, so never becomes a package).
    pub(super) components_used: HashSet<String>,
}

/// Copies every file under `root` into a per-component tree under `dest`, resolving each one via
/// `resolver`. A single pass over the tree: the walk is collected up front and then handed to a
/// dedicated, oversized thread pool for copying, since copying is bottlenecked on the filesystem
/// (each thread spends nearly all its time blocked inside the `copy` syscall) rather than the CPU
/// — piping the walk straight into a parallel iterator instead would serialise the copies behind
/// it, since every worker would take a turn on the same lock to get its next file.
///
/// `.git` is skipped wherever it appears in the tree, not just at `root`'s own top level — Moodle
/// plugins occasionally vendor a dependency as its own git checkout, and none of that history
/// belongs in the package copy (each package gets its own fresh git history — see `git.rs`).
///
/// `root`'s own `composer.lock` (only ever at the top level — this is not a general
/// wherever-it-appears exclusion the way `.git` is) is skipped too: it pins the resolved versions
/// for the *pre-split* dependency graph, which no longer matches the `root` package's own
/// `composer.json` (see [`super::composer_json::write_root`]) once that graph is split across
/// packages, so shipping it would be actively misleading rather than merely stale.
pub(super) fn copy_all(root: &Path, dest: &Path, resolver: &ComponentResolver) -> CopyOutcome {
    let created_dirs: Mutex<HashSet<PathBuf>> = Mutex::new(HashSet::new());
    let components_used: Mutex<HashSet<String>> = Mutex::new(HashSet::new());
    let copied = AtomicUsize::new(0);
    let bytes = AtomicUsize::new(0);
    let failed = AtomicUsize::new(0);
    let unresolved = Mutex::new(Vec::new());
    let start = Instant::now();

    let files: Vec<PathBuf> = WalkDir::new(root)
        .into_iter()
        .filter_entry(|entry| {
            entry.file_name() != ".git"
                && !(entry.depth() == 1 && entry.file_name() == "composer.lock")
        })
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_file())
        .map(|entry| entry.into_path())
        .collect();
    let walked = start.elapsed();

    let copy_pool = rayon::ThreadPoolBuilder::new()
        .num_threads(
            std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(4)
                * 8,
        )
        .build()
        .expect("failed to build copy thread pool");

    copy_pool.install(|| {
        files.par_iter().for_each(|path| {
            let relative = relative_unix_path(root, path);

            let (component, path_in_component) = match resolver.resolve(&relative) {
                Some(resolution) if !resolution.path_in_component.is_empty() => {
                    (resolution.component, resolution.path_in_component)
                }
                _ => {
                    unresolved.lock().unwrap().push(relative.clone());
                    (ROOT_COMPONENT.to_string(), format!("/{relative}"))
                }
            };
            components_used.lock().unwrap().insert(component.clone());

            let target = dest
                .join(component_dir_name(&component))
                .join(path_in_component.trim_start_matches('/'));

            if let Some(parent) = target.parent() {
                let known = created_dirs.lock().unwrap().contains(parent);
                if !known {
                    if let Err(err) = std::fs::create_dir_all(parent) {
                        eprintln!("warning: failed to create {}: {err}", parent.display());
                        failed.fetch_add(1, Ordering::Relaxed);
                        return;
                    }
                    created_dirs.lock().unwrap().insert(parent.to_path_buf());
                }
            }

            match std::fs::copy(path, &target) {
                Ok(size) => {
                    copied.fetch_add(1, Ordering::Relaxed);
                    bytes.fetch_add(size as usize, Ordering::Relaxed);
                }
                Err(err) => {
                    eprintln!(
                        "warning: failed to copy {} to {}: {err}",
                        path.display(),
                        target.display()
                    );
                    failed.fetch_add(1, Ordering::Relaxed);
                }
            }
        });
    });

    CopyOutcome {
        copied: copied.load(Ordering::Relaxed),
        bytes: bytes.load(Ordering::Relaxed),
        failed: failed.load(Ordering::Relaxed),
        walked,
        elapsed: start.elapsed(),
        unresolved: unresolved.into_inner().unwrap(),
        components_used: components_used.into_inner().unwrap(),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;
    use crate::moodle::components::{ComponentDiscovery, discover_components};

    fn temp_dir(name: &str) -> PathBuf {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "scan-moodle-copy-test-{name}-{}-{unique}",
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

    /// A minimal codebase `discover_components` can actually read: one plugin type ('mod') with
    /// one plugin ('mod_quiz'), plus a couple of root-owned files.
    fn minimal_codebase(root: &Path) {
        write_file(
            root,
            "lib/components.json",
            r#"{"plugintypes": {"mod": "mod"}, "subsystems": {}}"#,
        );
        write_file(root, "mod/quiz/lib.php", "<?php\n// quiz lib\n");
        write_file(root, "mod/quiz/view.php", "<?php\n// quiz view\n");
        write_file(root, "config.php", "<?php\n// config\n");
        write_file(root, ".git/HEAD", "ref: refs/heads/main\n");
    }

    #[test]
    fn copies_files_into_per_component_directories() {
        let root = temp_dir("basic-root");
        let dest = temp_dir("basic-dest");
        minimal_codebase(&root);

        let discovered: ComponentDiscovery = discover_components(&root).unwrap();
        let resolver = ComponentResolver::new(&discovered);

        let outcome = copy_all(&root, &dest, &resolver);

        assert_eq!(outcome.failed, 0);
        // mod/quiz/lib.php, mod/quiz/view.php, config.php, and lib/components.json itself (which
        // resolves into 'core', since 'lib' is core's own directory).
        assert_eq!(outcome.copied, 4);
        assert!(outcome.components_used.contains("mod_quiz"));
        assert!(outcome.components_used.contains(ROOT_COMPONENT));
        assert!(
            dest.join("moodle-mod_quiz/lib.php").is_file(),
            "quiz's lib.php should land under moodle-mod_quiz"
        );
        assert!(
            dest.join("moodle-root/config.php").is_file(),
            "unowned config.php should land under moodle-root"
        );

        fs::remove_dir_all(&root).ok();
        fs::remove_dir_all(&dest).ok();
    }

    #[test]
    fn never_copies_a_git_directory_at_any_depth() {
        let root = temp_dir("git-skip-root");
        let dest = temp_dir("git-skip-dest");
        minimal_codebase(&root);
        // A vendored dependency with its own nested checkout.
        write_file(
            &root,
            "mod/quiz/vendor/lib/.git/HEAD",
            "ref: refs/heads/main\n",
        );

        let discovered = discover_components(&root).unwrap();
        let resolver = ComponentResolver::new(&discovered);

        copy_all(&root, &dest, &resolver);

        assert!(!dest.join("moodle-root/.git").exists());
        let mut nested_git_dirs = 0;
        for entry in walkdir::WalkDir::new(&dest) {
            let entry = entry.unwrap();
            if entry.file_name() == ".git" {
                nested_git_dirs += 1;
            }
        }
        assert_eq!(nested_git_dirs, 0);

        fs::remove_dir_all(&root).ok();
        fs::remove_dir_all(&dest).ok();
    }

    #[test]
    fn never_copies_roots_own_composer_lock() {
        let root = temp_dir("lock-skip-root");
        let dest = temp_dir("lock-skip-dest");
        minimal_codebase(&root);
        write_file(&root, "composer.lock", "{}\n");
        // A plugin's own vendored lockfile, at some depth below root — unrelated to root's own
        // composer.lock, so it's copied normally (this exclusion is deliberately not a
        // wherever-it-appears rule the way `.git` is — see `copy_all`'s doc comment).
        write_file(&root, "mod/quiz/vendor/lib/composer.lock", "{}\n");

        let discovered = discover_components(&root).unwrap();
        let resolver = ComponentResolver::new(&discovered);

        copy_all(&root, &dest, &resolver);

        assert!(!dest.join("moodle-root/composer.lock").exists());
        assert!(
            dest.join("moodle-mod_quiz/vendor/lib/composer.lock")
                .is_file(),
            "a nested composer.lock belonging to a vendored dependency should still be copied"
        );

        fs::remove_dir_all(&root).ok();
        fs::remove_dir_all(&dest).ok();
    }
}
