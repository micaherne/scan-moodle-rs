use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use rayon::prelude::*;
use walkdir::WalkDir;

use scan_moodle::path_finder::{PathNotation, find_paths};

/// Since Moodle 5.1, $CFG->dirroot lives in a 'public/' subdirectory of the repository root;
/// earlier layouts have dirroot and the repository root coincide.
fn detect_dirroot_prefix(root: &Path) -> &'static str {
    if root.join("public").join("lib").join("setup.php").is_file() { "public/" } else { "" }
}

/// `path`, relative to `root`, as a forward-slash-separated string.
fn relative_unix_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .components()
        .map(|component| component.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

/// Collapses a field onto a single line so the tab-separated stdout output stays one record per
/// line.
fn sanitize(field: &str) -> String {
    field.replace('\t', " ").replace('\n', "\\n")
}

fn main() -> ExitCode {
    let mut args = std::env::args_os().skip(1);
    let Some(root) = args.next().map(PathBuf::from) else {
        eprintln!("usage: scan-moodle <path-to-moodle-codebase>");
        return ExitCode::FAILURE;
    };

    if !root.is_dir() {
        eprintln!("error: {} is not a directory", root.display());
        return ExitCode::FAILURE;
    }

    let notation = PathNotation::new(detect_dirroot_prefix(&root));

    let scanned = AtomicUsize::new(0);
    let failed = AtomicUsize::new(0);
    let found = AtomicUsize::new(0);
    let start = Instant::now();

    let out = Mutex::new(BufWriter::new(std::io::stdout()));

    WalkDir::new(&root)
        .into_iter()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_file())
        .map(|entry| entry.into_path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "php"))
        .par_bridge()
        .for_each(|path| {
            scanned.fetch_add(1, Ordering::Relaxed);
            let relative = relative_unix_path(&root, &path);
            let bytes = match std::fs::read(&path) {
                Ok(bytes) => bytes,
                Err(err) => {
                    eprintln!("warning: failed to read {}: {err}", path.display());
                    failed.fetch_add(1, Ordering::Relaxed);
                    return;
                }
            };
            let source = String::from_utf8_lossy(&bytes);
            let results = find_paths(&source, &relative, &notation);
            if results.is_empty() {
                return;
            }

            found.fetch_add(results.len(), Ordering::Relaxed);
            let mut out = out.lock().unwrap();
            for result in &results {
                writeln!(out, "{}\t{}\t{}", sanitize(&relative), sanitize(&result.code), sanitize(&result.path)).ok();
            }
        });

    out.into_inner().unwrap().flush().ok();
    let elapsed = start.elapsed();

    eprintln!("Scanned {} PHP files in {:.2?}, found {} paths", scanned.load(Ordering::Relaxed), elapsed, found.load(Ordering::Relaxed));
    let failed = failed.load(Ordering::Relaxed);
    if failed > 0 {
        eprintln!("Failed to read {failed} files");
    }

    ExitCode::SUCCESS
}
