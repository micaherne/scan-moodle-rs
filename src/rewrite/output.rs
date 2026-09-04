//! Step 5 of the rewrite process (see `REWRITE_SPEC.md`): writes a path scan out as a CSV file,
//! in the same format `find-paths --categorise` produces, plus a `still_needs_rewrite` column
//! (see [`super::decide::still_needs_rewrite`]) so the "after" scan — most of which is expected to
//! already be in its final, rewritten form — can be filtered down to whatever isn't yet.
//!
//! [`AuditWriter`] additionally supports step 3's audit trail — the "before" scan plus a
//! `rewritten_to` column recording what each reference was rewritten to, if anything — written
//! one row at a time as step 3 decides each reference, rather than assembled into memory first.

use std::collections::HashSet;
use std::fs;
use std::io::{self, BufWriter, Write};
use std::path::Path;

use csv::WriterBuilder;

use crate::moodle::scan::CategorisedReference;

use super::decide::still_needs_rewrite;

/// Byte-order mark that hints to Excel that the CSV that follows is UTF-8.
const UTF8_BOM: [u8; 3] = [0xEF, 0xBB, 0xBF];

const HEADER: [&str; 12] = [
    "file",
    "line",
    "start",
    "end",
    "code",
    "kind",
    "glyph_path",
    "normalised_path",
    "source_component",
    "target_component",
    "path_in_component",
    "category",
];

/// Writes `references` to `path` as a CSV file, creating `path`'s parent directory first if it
/// doesn't already exist. Each row carries one more column beyond [`HEADER`],
/// `still_needs_rewrite` ("true"/"false"), from [`super::decide::still_needs_rewrite`] —
/// `entry_point_files` is passed straight through to it (see that function for what it means).
pub fn write_csv(
    path: &Path,
    references: &[CategorisedReference],
    entry_point_files: &HashSet<String>,
) -> io::Result<()> {
    let header: Vec<&str> = HEADER
        .iter()
        .copied()
        .chain(["still_needs_rewrite"])
        .collect();
    let mut csv_writer = create_writer(path, &header)?;
    for reference in references {
        let mut record = record_fields(reference).to_vec();
        record.push(still_needs_rewrite(reference, entry_point_files).to_string());
        csv_writer.write_record(record).ok();
    }
    csv_writer.flush()
}

/// Incrementally writes step 3's audit trail: the same rows [`write_csv`] would, plus one more
/// `rewritten_to` column, one row at a time as each reference is decided — see the module docs.
pub struct AuditWriter {
    csv_writer: csv::Writer<BufWriter<fs::File>>,
}

impl AuditWriter {
    /// Opens `path` for writing and writes its header, creating `path`'s parent directory first
    /// if it doesn't already exist.
    pub fn create(path: &Path) -> io::Result<Self> {
        let header: Vec<&str> = HEADER.iter().copied().chain(["rewritten_to"]).collect();
        Ok(Self {
            csv_writer: create_writer(path, &header)?,
        })
    }

    /// Writes one row for `reference`. `rewritten_to` is the replacement expression `reference`
    /// was rewritten to, or empty if it was left unchanged — whether because it wasn't eligible
    /// for rewriting at all, or because it was eligible but step 3 decided to leave it as-is.
    pub fn write_row(&mut self, reference: &CategorisedReference, rewritten_to: &str) {
        let mut record = record_fields(reference).to_vec();
        record.push(sanitize(rewritten_to));
        self.csv_writer.write_record(record).ok();
    }

    /// Flushes every row written so far to disk. Rows are also flushed when the writer is
    /// dropped, but only silently on a best-effort basis; call this to actually observe a failure.
    pub fn finish(mut self) -> io::Result<()> {
        self.csv_writer.flush()
    }
}

/// Opens `path` for writing, with a UTF-8 BOM and `header` as the first CSV record, creating
/// `path`'s parent directory first if it doesn't already exist.
fn create_writer(path: &Path, header: &[&str]) -> io::Result<csv::Writer<BufWriter<fs::File>>> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let mut writer = BufWriter::new(fs::File::create(path)?);
    writer.write_all(&UTF8_BOM)?;

    let mut csv_writer = WriterBuilder::new()
        .terminator(csv::Terminator::CRLF)
        .from_writer(writer);
    csv_writer.write_record(header).ok();
    Ok(csv_writer)
}

/// `reference`'s fields, in the order [`HEADER`] names them.
fn record_fields(reference: &CategorisedReference) -> [String; 12] {
    [
        sanitize(&reference.file),
        reference.result.line.to_string(),
        opt_to_string(reference.result.start_pos),
        opt_to_string(reference.result.end_pos),
        sanitize(&reference.result.code),
        reference.result.kind.to_string(),
        sanitize(&reference.result.path),
        sanitize(&reference.result.real_path),
        reference.source_component.clone().unwrap_or_default(),
        reference
            .target
            .as_ref()
            .map(|target| target.component.clone())
            .unwrap_or_default(),
        reference
            .target
            .as_ref()
            .map(|target| target.path_in_component.clone())
            .unwrap_or_default(),
        reference.category.to_string(),
    ]
}

/// `Some(value)` as its string form, `None` as an empty string.
fn opt_to_string<T: ToString>(value: Option<T>) -> String {
    value.map(|value| value.to_string()).unwrap_or_default()
}

/// Collapses a field onto a single line so each CSV record stays one row in Excel.
fn sanitize(field: &str) -> String {
    field.replace('\t', " ").replace('\n', "\\n")
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;
    use crate::moodle::categorise::PathCategory;
    use crate::moodle::resolver::Resolution;
    use crate::path_finder::{PathKind, PathResult};

    fn temp_file(name: &str) -> PathBuf {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "scan-moodle-output-test-{name}-{}-{unique}.csv",
            std::process::id()
        ))
    }

    fn reference() -> CategorisedReference {
        CategorisedReference {
            file: "public/mod/quiz/view.php".to_string(),
            result: PathResult {
                path: "@/mod/quiz/lib.php".to_string(),
                real_path: "public/mod/quiz/lib.php".to_string(),
                kind: PathKind::Dirroot,
                line: 12,
                code: "$CFG->dirroot . '/mod/quiz/lib.php'".to_string(),
                parent: Some("require_once".to_string()),
                start_pos: Some(20),
                end_pos: Some(56),
                separator: "/".to_string(),
                mono_path_expr: String::new(),
                scope_end_line: None,
            },
            source_component: Some("mod_quiz".to_string()),
            target: Some(Resolution {
                component: "mod_quiz".to_string(),
                path_in_component: "/lib.php".to_string(),
            }),
            category: PathCategory::StaticSameComponent,
        }
    }

    #[test]
    fn writes_a_header_and_one_row_per_reference() {
        let path = temp_file("basic");
        write_csv(&path, &[reference()], &HashSet::new()).unwrap();

        let content = fs::read(&path).unwrap();
        assert_eq!(
            &content[..3],
            &UTF8_BOM,
            "expected a UTF-8 BOM at the start of the file"
        );

        let mut reader = csv::ReaderBuilder::new().from_reader(&content[3..]);
        let headers: Vec<String> = reader.headers().unwrap().iter().map(String::from).collect();
        assert_eq!(
            headers,
            HEADER
                .iter()
                .copied()
                .chain(["still_needs_rewrite"])
                .map(String::from)
                .collect::<Vec<_>>()
        );

        let records: Vec<csv::StringRecord> = reader.records().collect::<Result<_, _>>().unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(&records[0][0], "public/mod/quiz/view.php");
        assert_eq!(&records[0][8], "mod_quiz"); // source_component
        assert_eq!(&records[0][9], "mod_quiz"); // target_component
        assert_eq!(&records[0][10], "/lib.php"); // path_in_component
        assert_eq!(&records[0][11], "static-same-component"); // category
        // reference()'s code is empty, which is never what decide() would produce, so this
        // eligible reference still needs rewriting.
        assert_eq!(&records[0][12], "true"); // still_needs_rewrite

        fs::remove_file(&path).ok();
    }

    #[test]
    fn still_needs_rewrite_is_false_once_the_code_already_matches() {
        let path = temp_file("already-rewritten");
        let mut already_rewritten = reference();
        already_rewritten.result.code = "__DIR__ . '/lib.php'".to_string();
        write_csv(&path, &[already_rewritten], &HashSet::new()).unwrap();

        let content = fs::read(&path).unwrap();
        let mut reader = csv::ReaderBuilder::new().from_reader(&content[3..]);
        let records: Vec<csv::StringRecord> = reader.records().collect::<Result<_, _>>().unwrap();
        assert_eq!(&records[0][12], "false"); // still_needs_rewrite

        fs::remove_file(&path).ok();
    }

    #[test]
    fn creates_missing_parent_directories() {
        let path = temp_file("nested")
            .parent()
            .unwrap()
            .join("nested-output-dir")
            .join("out.csv");
        write_csv(&path, &[reference()], &HashSet::new()).unwrap();
        assert!(path.is_file());

        fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn audit_writer_appends_a_rewritten_to_column() {
        let path = temp_file("audit");
        let mut audit = AuditWriter::create(&path).unwrap();
        audit.write_row(&reference(), "__DIR__ . '/lib.php'");
        audit.write_row(&reference(), "");
        audit.finish().unwrap();

        let content = fs::read(&path).unwrap();
        let mut reader = csv::ReaderBuilder::new().from_reader(&content[3..]);
        let headers: Vec<String> = reader.headers().unwrap().iter().map(String::from).collect();
        assert_eq!(headers.last().unwrap(), "rewritten_to");
        assert_eq!(headers.len(), HEADER.len() + 1);

        let records: Vec<csv::StringRecord> = reader.records().collect::<Result<_, _>>().unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(&records[0][12], "__DIR__ . '/lib.php'");
        assert_eq!(&records[1][12], "");

        fs::remove_file(&path).ok();
    }
}
