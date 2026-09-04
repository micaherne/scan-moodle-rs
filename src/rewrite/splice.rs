//! The write-back half of step 3 (see `REWRITE_SPEC.md`): turns [`super::decide::decide`]'s
//! decisions into actual edits on the files under the target codebase.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::path::Path;

use crate::moodle::scan::CategorisedReference;

use super::decide::{decide, is_eligible};
use super::output::AuditWriter;

/// Rewrites every eligible reference among `references` directly on the files under `root`.
/// Eligibility is [`is_eligible`]'s — [`crate::moodle::categorise::PathCategory::Config`],
/// [`crate::moodle::categorise::PathCategory::PreComponentLiteral`],
/// [`crate::moodle::categorise::PathCategory::StaticSameComponent`],
/// [`crate::moodle::categorise::PathCategory::StaticDifferentComponent`] and
/// [`crate::moodle::categorise::PathCategory::VariableOnly`]. Every other category is skipped, as
/// is any eligible reference [`decide`] produces no replacement for (which for `Config` and
/// `VariableOnly` is a real possibility, not just a theoretical one — see [`decide`]). Returns how
/// many references were actually changed.
///
/// If `audit` is given, one row is written to it for *every* reference in `references` — not
/// just the eligible ones — as each is decided, rather than assembled into memory first; see
/// [`AuditWriter`].
pub fn apply(
    root: &Path,
    references: &[CategorisedReference],
    entry_point_files: &HashSet<String>,
    mut audit: Option<&mut AuditWriter>,
) -> io::Result<usize> {
    let mut edits_by_file: HashMap<&str, Vec<(usize, usize, String)>> = HashMap::new();
    let mut rewritten = 0;

    for reference in references {
        let mut rewritten_to = String::new();

        if is_eligible(reference.category)
            && let Some(replacement) = decide(reference, entry_point_files)
        {
            let start = reference
                .result
                .start_pos
                .expect("find_paths always records a span");
            let end = reference
                .result
                .end_pos
                .expect("find_paths always records a span");
            edits_by_file
                .entry(reference.file.as_str())
                .or_default()
                .push((start, end, replacement.clone()));
            rewritten_to = replacement;
            rewritten += 1;
        }

        if let Some(audit) = audit.as_deref_mut() {
            audit.write_row(reference, &rewritten_to);
        }
    }

    for (file, mut edits) in edits_by_file {
        // Applied back-to-front so each edit's byte range is still valid once the edits after it
        // in the file have already been spliced in. Distinct path references can never overlap —
        // path_finder::finder never recurses into an expression it has already matched — so
        // ordering alone is enough; no range-overlap check is needed.
        edits.sort_by_key(|&(start, ..)| std::cmp::Reverse(start));

        let path = root.join(file);
        let bytes = fs::read(&path)?;
        // Mirrors exactly how the file was decoded for scanning (see
        // moodle::scan::Scan::from_discovery), so the byte offsets recorded against it during
        // step 2 still land on the same characters here.
        let mut source = String::from_utf8_lossy(&bytes).into_owned();
        for (start, end, replacement) in &edits {
            source.replace_range(*start..*end, replacement);
        }
        fs::write(&path, source)?;
    }

    Ok(rewritten)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;
    use crate::moodle::categorise::PathCategory;
    use crate::moodle::resolver::Resolution;
    use crate::path_finder::{PathNotation, find_paths};

    /// A fresh, empty temporary directory for one test, so tests can run in parallel without
    /// touching each other's files.
    fn temp_dir(name: &str) -> PathBuf {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "scan-moodle-splice-test-{name}-{}-{unique}",
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

    #[test]
    fn rewrites_a_single_reference_in_place() {
        let root = temp_dir("single");
        write_file(
            &root,
            "public/mod/quiz/view.php",
            "<?php\nrequire_once($CFG->dirroot . '/mod/quiz/lib.php');\n",
        );

        let source = fs::read_to_string(root.join("public/mod/quiz/view.php")).unwrap();
        let notation = PathNotation::new("public/");
        let mut results = find_paths(&source, "public/mod/quiz/view.php", &notation);
        assert_eq!(results.len(), 1);

        let reference = CategorisedReference {
            file: "public/mod/quiz/view.php".to_string(),
            result: results.remove(0),
            source_component: None,
            target: Some(Resolution {
                component: "mod_quiz".to_string(),
                path_in_component: "/lib.php".to_string(),
            }),
            category: PathCategory::StaticSameComponent,
        };

        let rewritten = apply(&root, &[reference], &HashSet::new(), None).unwrap();

        assert_eq!(rewritten, 1);
        assert_eq!(
            fs::read_to_string(root.join("public/mod/quiz/view.php")).unwrap(),
            "<?php\nrequire_once(__DIR__ . '/lib.php');\n"
        );

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn multiple_edits_in_the_same_file_do_not_clobber_each_other() {
        let root = temp_dir("multi");
        write_file(
            &root,
            "public/mod/quiz/view.php",
            "<?php\nrequire_once($CFG->dirroot . '/mod/quiz/lib.php');\nrequire_once($CFG->dirroot . '/mod/quiz/locallib.php');\n",
        );

        let source = fs::read_to_string(root.join("public/mod/quiz/view.php")).unwrap();
        let notation = PathNotation::new("public/");
        let results = find_paths(&source, "public/mod/quiz/view.php", &notation);
        assert_eq!(results.len(), 2);

        let references: Vec<CategorisedReference> = results
            .into_iter()
            .map(|result| {
                let path_in_component = if result.real_path.ends_with("locallib.php") {
                    "/locallib.php"
                } else {
                    "/lib.php"
                };
                CategorisedReference {
                    file: "public/mod/quiz/view.php".to_string(),
                    result,
                    source_component: None,
                    target: Some(Resolution {
                        component: "mod_quiz".to_string(),
                        path_in_component: path_in_component.to_string(),
                    }),
                    category: PathCategory::StaticSameComponent,
                }
            })
            .collect();

        let rewritten = apply(&root, &references, &HashSet::new(), None).unwrap();

        assert_eq!(rewritten, 2);
        assert_eq!(
            fs::read_to_string(root.join("public/mod/quiz/view.php")).unwrap(),
            "<?php\nrequire_once(__DIR__ . '/lib.php');\nrequire_once(__DIR__ . '/locallib.php');\n"
        );

        fs::remove_dir_all(&root).ok();
    }

    /// A pre-component bare literal is rewritten to `__DIR__`-relative even when its target is
    /// itself a bootstrap file (which would otherwise force a `\core\component::get_path()` call,
    /// the way it does for an ordinary `StaticSameComponent`/`StaticDifferentComponent` reference) —
    /// `core\component` isn't loaded yet this early, so the entry-point-target override must never
    /// reach this category.
    #[test]
    fn a_pre_component_literal_targeting_a_bootstrap_file_still_gets_dir_relative() {
        let root = temp_dir("pre-component-literal");
        write_file(
            &root,
            "public/lib/setup.php",
            "<?php\nrequire_once('component.php');\n",
        );

        let source = fs::read_to_string(root.join("public/lib/setup.php")).unwrap();
        let notation = PathNotation::new("public/");
        let mut results = find_paths(&source, "public/lib/setup.php", &notation);
        assert_eq!(results.len(), 1);
        // The bare literal resolves relative to the referencing file's own directory, exactly the
        // shape `dir_relative_expression` is being exercised against below.
        assert_eq!(results[0].real_path, "public/lib/component.php");

        let reference = CategorisedReference {
            file: "public/lib/setup.php".to_string(),
            result: results.remove(0),
            source_component: None,
            // `target` is irrelevant to this category — decide() never looks at it — but supplied
            // for realism: `component.php` really is a bootstrap file (it's `core\component` itself).
            target: Some(Resolution {
                component: "core".to_string(),
                path_in_component: "/component.php".to_string(),
            }),
            category: PathCategory::PreComponentLiteral,
        };

        let entry_point_files = HashSet::from([
            "public/lib/setup.php".to_string(),
            "public/lib/component.php".to_string(),
        ]);
        let rewritten = apply(&root, &[reference], &entry_point_files, None).unwrap();

        assert_eq!(rewritten, 1);
        assert_eq!(
            fs::read_to_string(root.join("public/lib/setup.php")).unwrap(),
            "<?php\nrequire_once(__DIR__ . '/component.php');\n"
        );

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn an_ineligible_category_is_left_untouched() {
        let root = temp_dir("ineligible");
        write_file(
            &root,
            "public/mod/quiz/view.php",
            "<?php\nrequire_once($CFG->dirroot);\n",
        );

        let source = fs::read_to_string(root.join("public/mod/quiz/view.php")).unwrap();
        let notation = PathNotation::new("public/");
        let mut results = find_paths(&source, "public/mod/quiz/view.php", &notation);
        assert_eq!(results.len(), 1);

        let reference = CategorisedReference {
            file: "public/mod/quiz/view.php".to_string(),
            result: results.remove(0),
            source_component: None,
            target: Some(Resolution {
                component: "root".to_string(),
                path_in_component: "/public".to_string(),
            }),
            category: PathCategory::DirrootWrangling,
        };

        let rewritten = apply(&root, &[reference], &HashSet::new(), None).unwrap();

        assert_eq!(rewritten, 0);
        assert_eq!(
            fs::read_to_string(root.join("public/mod/quiz/view.php")).unwrap(),
            "<?php\nrequire_once($CFG->dirroot);\n"
        );

        fs::remove_dir_all(&root).ok();
    }

    /// Every reference gets an audit row — not just the eligible ones — with `rewritten_to`
    /// reflecting what actually happened: the replacement text when one was applied, empty
    /// otherwise (whether ineligible outright, or eligible but left unchanged by `decide`).
    #[test]
    fn audit_writer_gets_one_row_per_reference_regardless_of_eligibility() {
        let root = temp_dir("audit");
        write_file(
            &root,
            "public/mod/quiz/view.php",
            "<?php\nrequire_once($CFG->dirroot . '/mod/quiz/lib.php');\nrequire_once($CFG->dirroot);\n",
        );

        let source = fs::read_to_string(root.join("public/mod/quiz/view.php")).unwrap();
        let notation = PathNotation::new("public/");
        let results = find_paths(&source, "public/mod/quiz/view.php", &notation);
        assert_eq!(results.len(), 2);

        let references: Vec<CategorisedReference> = results
            .into_iter()
            .map(|result| {
                let eligible = result.real_path.ends_with("lib.php");
                CategorisedReference {
                    file: "public/mod/quiz/view.php".to_string(),
                    result,
                    source_component: None,
                    target: Some(if eligible {
                        Resolution {
                            component: "mod_quiz".to_string(),
                            path_in_component: "/lib.php".to_string(),
                        }
                    } else {
                        Resolution {
                            component: "root".to_string(),
                            path_in_component: "/public".to_string(),
                        }
                    }),
                    category: if eligible {
                        PathCategory::StaticSameComponent
                    } else {
                        PathCategory::DirrootWrangling
                    },
                }
            })
            .collect();

        let audit_path = root.join("audit.csv");
        let mut audit = AuditWriter::create(&audit_path).unwrap();
        let rewritten = apply(&root, &references, &HashSet::new(), Some(&mut audit)).unwrap();
        audit.finish().unwrap();

        assert_eq!(rewritten, 1);

        let content = fs::read(&audit_path).unwrap();
        let mut reader = csv::ReaderBuilder::new().from_reader(&content[3..]);
        let records: Vec<csv::StringRecord> = reader.records().collect::<Result<_, _>>().unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(&records[0][12], "__DIR__ . '/lib.php'");
        assert_eq!(&records[1][12], "");

        fs::remove_dir_all(&root).ok();
    }
}
