//! Ported from moodle-splittable's `tests/Parser/PathFindingVisitorTest.php`.

use scan_moodle::path_finder::PathKind;
use scan_moodle::path_finder::PathNotation;
use scan_moodle::path_finder::find_paths;

/// Ported from `PathFindingVisitorTest::pathProvider` / `testGetPaths`.
#[test]
fn get_paths_from_fixture_csv() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/paths.csv");
    let mut reader = csv::ReaderBuilder::new()
        .has_headers(false)
        .flexible(true)
        .from_path(path)
        .unwrap();
    let notation = PathNotation::new("public/");

    let mut checked = 0;
    for record in reader.records() {
        let record = record.unwrap();
        if record.len() < 3 {
            continue;
        }
        let relative_path = &record[0];
        let code = &record[1];
        let expected = &record[2];
        if !(code.contains("$CFG") || code.contains("__DIR__") || code.contains("__FILE__")) {
            continue;
        }

        let source = format!("<?php {code};");
        let paths = find_paths(&source, relative_path, &notation);
        assert_eq!(
            paths.len(),
            1,
            "expected exactly one path for {code:?} ({relative_path})"
        );
        assert_eq!(
            paths[0].path, expected,
            "mismatch for {code:?} ({relative_path})"
        );
        checked += 1;
    }
    assert!(
        checked > 2000,
        "expected to check most of the fixture rows, only checked {checked}"
    );
}

/// Ported from `PathFindingVisitorTest::testBareCfgDirroot`.
#[test]
fn bare_cfg_dirroot() {
    let notation = PathNotation::new("");
    let paths = find_paths("<?php $CFG->dirroot;", "some/file.php", &notation);
    assert_eq!(paths.len(), 1);
    assert_eq!(paths[0].path, "@");
}

/// Ported from `PathFindingVisitorTest::testAssignmentTargetIsNotAPath`.
///
/// A bare $CFG->libdir/$CFG->dirroot on the left of an assignment defines the variable; it is not
/// a file reference and must never be recorded, but the right-hand side still is a path.
#[test]
fn assignment_target_is_not_a_path() {
    let notation = PathNotation::new("public/");
    let paths = find_paths(
        r#"<?php $CFG->libdir = "$CFG->dirroot/lib";"#,
        "public/install.php",
        &notation,
    );
    assert_eq!(paths.len(), 1);
    assert_eq!(paths[0].path, "@/lib");
}

/// Resolves a single path expression found in `file`, with `dirroot_prefix` as the path from the
/// repository root to $CFG->dirroot ('public/' for Moodle 5.1+, '' for earlier layouts).
fn resolve_single(file: &str, code: &str, dirroot_prefix: &str) -> String {
    let notation = PathNotation::new(dirroot_prefix);
    let source = format!("<?php {code};");
    let paths = find_paths(&source, file, &notation);
    assert_eq!(
        paths.len(),
        1,
        "expected exactly one path for {code:?} ({file})"
    );
    paths[0].path.clone()
}

/// Ported from `PathFindingVisitorTest::rootGlyphProvider` / `testRootGlyphResolution`.
#[test]
fn root_glyph_resolution() {
    let cases: &[(&str, &str, &str, &str)] = &[
        // Files inside dirroot (public/) resolve to '@'.
        (
            "public/lib/moodlelib.php",
            r#"$CFG->dirroot . '/lib/setup.php'"#,
            "public/",
            "@/lib/setup.php",
        ),
        (
            "public/mod/assign/view.php",
            r#"__DIR__ . '/locallib.php'"#,
            "public/",
            "@/mod/assign/locallib.php",
        ),
        ("public/lib/moodlelib.php", "$CFG->dirroot", "public/", "@"),
        (
            "public/lib/moodlelib.php",
            r#"$CFG->dirroot . '/'"#,
            "public/",
            "@/",
        ),
        // '#/public' is a pure string prefix for '@', so separator-less concatenations stay
        // faithful: dirroot glued straight onto a variable or a literal keeps the byte semantics.
        (
            "public/admin/tool/xmldb/actions/x.php",
            "$CFG->dirroot . $dirpath",
            "public/",
            "@{$dirpath}",
        ),
        (
            "public/lib/moodlelib.php",
            r#"$CFG->dirroot . 'unexistingdirectory'"#,
            "public/",
            "@unexistingdirectory",
        ),
        // A dynamic operand that binds looser than concatenation (here a ternary) keeps the
        // grouping parentheses it had in the source, so the marker stays a safely embeddable
        // expression: '@…submission.{($textfile ? …)}', not a bare '{$textfile ? …}'.
        (
            "public/mod/assign/feedback/editpdf/tests/feedback_test.php",
            r#"$CFG->dirroot . '/mod/assign/feedback/editpdf/tests/fixtures/submission.' . ($textfile ? 'txt' : 'pdf')"#,
            "public/",
            "@/mod/assign/feedback/editpdf/tests/fixtures/submission.{($textfile ? 'txt' : 'pdf')}",
        ),
        // The Moodle 5.1 compatibility shim lives above dirroot; its include reaches into
        // public/ and so still resolves to '@'.
        (
            "lib/setup.php",
            r#"dirname(__DIR__) . '/public/lib/setup.php'"#,
            "public/",
            "@/lib/setup.php",
        ),
        // Files above dirroot that reference siblings above dirroot keep the '#' (repo-root)
        // glyph.
        (
            "admin/cli/cron.php",
            r#"__DIR__ . '/../../config.php'"#,
            "public/",
            "#/config.php",
        ),
        ("lib/setup.php", "$CFG->dirroot", "public/", "@"),
        // Pre-5.1 layout: repo root is dirroot, prefix empty, everything resolves to '@'.
        (
            "lib/moodlelib.php",
            r#"$CFG->dirroot . '/lib/setup.php'"#,
            "",
            "@/lib/setup.php",
        ),
        (
            "mod/assign/view.php",
            r#"__DIR__ . '/locallib.php'"#,
            "",
            "@/mod/assign/locallib.php",
        ),
    ];
    for (file, code, dirroot_prefix, expected) in cases {
        assert_eq!(
            resolve_single(file, code, dirroot_prefix),
            *expected,
            "code={code}"
        );
    }
}

/// Ported from `PathFindingVisitorTest::monoPathExprProvider` / `testMonoPathExpr`.
#[test]
fn mono_path_expr() {
    let notation = PathNotation::new("public/");
    let cases: &[(&str, &str)] = &[
        // The faithful expression for everything after $CFG->dirroot — preserving
        // DIRECTORY_SEPARATOR verbatim and including the leading separator.
        ("$CFG->dirroot . $includefile", "$includefile"),
        (r#"$CFG->dirroot . '/' . $filename"#, r#"'/' . $filename"#),
        (
            "$CFG->dirroot . DIRECTORY_SEPARATOR . $data",
            "DIRECTORY_SEPARATOR . $data",
        ),
        // The case that the lossy path could not represent: DS between two expressions.
        (
            "$CFG->dirroot . DIRECTORY_SEPARATOR . $normalisedpath . DIRECTORY_SEPARATOR . $filename",
            "DIRECTORY_SEPARATOR . $normalisedpath . DIRECTORY_SEPARATOR . $filename",
        ),
        (
            "$CFG->dirroot . DIRECTORY_SEPARATOR . implode(DIRECTORY_SEPARATOR, $path)",
            "DIRECTORY_SEPARATOR . implode(DIRECTORY_SEPARATOR, $path)",
        ),
        ("$CFG->dirroot . self::BLANK_PDF", "self::BLANK_PDF"),
        (
            "$CFG->dirroot . static::get_h5p_core_library_base($classes[$classname])",
            "static::get_h5p_core_library_base($classes[$classname])",
        ),
        (
            "$CFG->dirroot . $themetestdir . self::get_behat_tests_path()",
            "$themetestdir . self::get_behat_tests_path()",
        ),
        // Interpolated-string forms.
        (r#""{$CFG->dirroot}/$badgeimage""#, r#"'/' . $badgeimage"#),
        (
            r#""{$CFG->dirroot}/{$data['filepath']}""#,
            r#"'/' . $data['filepath']"#,
        ),
        (
            r#""{$CFG->dirroot}" . autoloader::get_h5p_editor_library_base($languagescript)"#,
            "autoloader::get_h5p_editor_library_base($languagescript)",
        ),
        // Not rooted at $CFG->dirroot: no mono-path expression (never variable-only).
        (r#"$CFG->libdir . '/foo.php'"#, ""),
        (r#"__DIR__ . '/locallib.php'"#, ""),
        ("$CFG->dirroot", ""),
    ];
    for (code, expected) in cases {
        let source = format!("<?php {code};");
        let paths = find_paths(&source, "public/lib/moodlelib.php", &notation);
        assert_eq!(paths.len(), 1, "expected exactly one path for {code:?}");
        assert_eq!(paths[0].mono_path_expr, *expected, "code={code}");
    }
}

/// Each of the existing anchors is classified with its own [`PathKind`].
#[test]
fn existing_kinds_are_classified_correctly() {
    let notation = PathNotation::new("public/");
    let cases: &[(&str, PathKind)] = &[
        ("$CFG->dirroot . '/lib/setup.php'", PathKind::Dirroot),
        ("$CFG->libdir . '/clilib.php'", PathKind::Libdir),
        ("$CFG->libdir", PathKind::Libdir),
        ("__DIR__ . '/locallib.php'", PathKind::Dir),
        ("__FILE__ . '.bak'", PathKind::File),
        ("dirname(__FILE__) . '/locallib.php'", PathKind::Dir),
    ];
    for (code, expected_kind) in cases {
        let source = format!("<?php {code};");
        let paths = find_paths(&source, "public/lib/moodlelib.php", &notation);
        assert_eq!(paths.len(), 1, "expected exactly one path for {code:?}");
        assert_eq!(paths[0].kind, *expected_kind, "code={code}");
    }
}

/// $CFG->root behaves exactly like $CFG->dirroot/libdir: bare or as the leading concat operand.
#[test]
fn cfg_root_is_recognised() {
    let notation = PathNotation::new("public/");

    let paths = find_paths("<?php $CFG->root;", "public/lib/moodlelib.php", &notation);
    assert_eq!(paths.len(), 1);
    assert_eq!(paths[0].path, "#");
    assert_eq!(paths[0].kind, PathKind::Root);

    let paths = find_paths(
        "<?php $CFG->root . '/README.md';",
        "public/lib/moodlelib.php",
        &notation,
    );
    assert_eq!(paths.len(), 1);
    assert_eq!(paths[0].path, "#/README.md");
    assert_eq!(paths[0].real_path, "README.md");
    assert_eq!(paths[0].kind, PathKind::Root);
}

/// A bare string literal that is the *sole* value of a require/include construct is treated as
/// definitely a path, resolved relative to the current file's directory.
#[test]
fn require_bare_literal_resolves_relative_to_current_file() {
    let notation = PathNotation::new("public/");
    let paths = find_paths(
        "<?php require('locallib.php');",
        "public/mod/forum/lib.php",
        &notation,
    );
    assert_eq!(paths.len(), 1);
    assert_eq!(paths[0].kind, PathKind::RequireLiteral);
    assert_eq!(paths[0].real_path, "public/mod/forum/locallib.php");
    assert_eq!(paths[0].parent, Some("require".to_string()));
}

/// A require/include's bare literal is classified as such regardless of whether it also starts
/// with '../' — that only affects how it resolves (relative to the current file's directory,
/// same as any other bare literal), not the classification.
#[test]
fn require_bare_literal_starting_with_dots_resolves_correctly() {
    let notation = PathNotation::new("public/");
    let paths = find_paths(
        "<?php require_once('../config.php');",
        "public/mod/forum/lib.php",
        &notation,
    );
    assert_eq!(paths.len(), 1);
    assert_eq!(paths[0].kind, PathKind::RequireLiteral);
    assert_eq!(paths[0].real_path, "public/mod/config.php");
    assert_eq!(paths[0].parent, Some("require_once".to_string()));
}

/// include/include_once are recognised the same way as require/require_once.
#[test]
fn include_bare_literal_resolves_relative_to_current_file() {
    let notation = PathNotation::new("public/");
    let paths = find_paths(
        "<?php include('settings.php');",
        "public/admin/index.php",
        &notation,
    );
    assert_eq!(paths.len(), 1);
    assert_eq!(paths[0].kind, PathKind::RequireLiteral);
    assert_eq!(paths[0].real_path, "public/admin/settings.php");
    assert_eq!(paths[0].parent, Some("include".to_string()));

    let paths = find_paths(
        "<?php include_once('settings.php');",
        "public/admin/index.php",
        &notation,
    );
    assert_eq!(paths.len(), 1);
    assert_eq!(paths[0].parent, Some("include_once".to_string()));
}

/// require of a concatenation is not a "bare" literal, so it falls back to ordinary detection —
/// which, with no recognised anchor, finds nothing.
#[test]
fn require_of_a_concat_is_not_a_bare_literal() {
    let notation = PathNotation::new("public/");
    let paths = find_paths(
        "<?php require($x . 'lib.php');",
        "public/mod/forum/lib.php",
        &notation,
    );
    assert_eq!(paths.len(), 0);
}

/// Outside a require/include, a bare string literal is not a recognised path at all, whether or
/// not it starts with '../' — matching '../' was found to flag far more non-path strings (e.g.
/// XPath expressions) than actual file references, so it is not treated as a signal on its own.
#[test]
fn bare_literal_outside_require_is_not_a_path() {
    let notation = PathNotation::new("public/");
    for code in ["<?php $x = 'lib.php';", "<?php $x = '../config.php';"] {
        let paths = find_paths(code, "public/mod/forum/lib.php", &notation);
        assert_eq!(paths.len(), 0, "expected no paths for {code}");
    }
}

/// A concatenation whose leftmost operand is a bare literal is not a recognised path either, even
/// when that literal starts with '../'.
#[test]
fn concat_with_bare_literal_leftmost_is_not_a_path() {
    let notation = PathNotation::new("public/");
    let paths = find_paths(
        "<?php $x = '../config' . '.php';",
        "public/mod/forum/lib.php",
        &notation,
    );
    assert_eq!(paths.len(), 0);
}
