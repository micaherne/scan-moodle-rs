use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use mago_allocator::LocalArena;
use mago_database::file::{File, FileType};
use rayon::prelude::*;
use walkdir::WalkDir;

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

    let php_files: Vec<PathBuf> = WalkDir::new(&root)
        .into_iter()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_file())
        .map(|entry| entry.into_path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "php"))
        .collect();

    let parsed = AtomicUsize::new(0);
    let failed = AtomicUsize::new(0);
    let with_parse_errors = AtomicUsize::new(0);

    let start = Instant::now();

    php_files.par_iter().for_each(|path| {
        let file = match File::read(&root, path, FileType::Host) {
            Ok(file) => file,
            Err(err) => {
                eprintln!("warning: failed to read {}: {err}", path.display());
                failed.fetch_add(1, Ordering::Relaxed);
                return;
            }
        };

        let arena = LocalArena::new();
        let program = mago_syntax::parser::parse_file(&arena, &file);
        if !program.errors.is_empty() {
            with_parse_errors.fetch_add(1, Ordering::Relaxed);
        }

        parsed.fetch_add(1, Ordering::Relaxed);
    });

    let elapsed = start.elapsed();

    println!("Parsed {} PHP files in {:.2?}", parsed.load(Ordering::Relaxed), elapsed);
    let failed = failed.load(Ordering::Relaxed);
    if failed > 0 {
        println!("Failed to read {failed} files");
    }
    let with_parse_errors = with_parse_errors.load(Ordering::Relaxed);
    if with_parse_errors > 0 {
        println!("{with_parse_errors} files had parse errors");
    }

    ExitCode::SUCCESS
}
