use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use clap::{Parser, Subcommand};
use csv::WriterBuilder;
use rayon::prelude::*;
use walkdir::WalkDir;

use scan_moodle::moodle::components::discover_components;
use scan_moodle::moodle::resolver::ComponentResolver;
use scan_moodle::moodle::thirdparty;
use scan_moodle::path_finder::{PathNotation, find_paths};

/// Byte-order mark that hints to Excel that the CSV that follows is UTF-8.
const UTF8_BOM: [u8; 3] = [0xEF, 0xBB, 0xBF];

#[derive(Parser)]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Scan a Moodle codebase for path references
    FindPaths {
        /// Path to the Moodle codebase to scan
        root: PathBuf,
        /// Write CSV output to this file instead of stdout
        #[arg(short = 'o', long = "output-file")]
        output_file: Option<PathBuf>,
        /// Add source_component and target_component columns, resolved from the codebase's
        /// components
        #[arg(long = "resolve-components")]
        resolve_components: bool,
    },
    /// List all components (core, subsystems, plugins and subplugins) in a Moodle codebase
    FindComponents {
        /// Path to the Moodle codebase to scan
        root: PathBuf,
        /// Write CSV output to this file instead of stdout
        #[arg(short = 'o', long = "output-file")]
        output_file: Option<PathBuf>,
        /// Output the subsystem and plugin type directories instead of individual components
        #[arg(long = "type-dirs")]
        type_dirs: bool,
    },
}

/// Builds a CSV writer with a UTF-8 BOM and CRLF line terminator for Excel compatibility,
/// writing to `output_file` if given, or stdout otherwise. Writes `header` as the first record.
fn create_csv_writer(
    output_file: Option<&Path>,
    header: &[&str],
) -> Result<csv::Writer<BufWriter<Box<dyn Write + Send>>>, ExitCode> {
    let writer: Box<dyn Write + Send> = match output_file {
        Some(path) => match File::create(path) {
            Ok(file) => Box::new(file),
            Err(err) => {
                eprintln!("error: failed to create {}: {err}", path.display());
                return Err(ExitCode::FAILURE);
            }
        },
        None => Box::new(std::io::stdout()),
    };

    let mut writer = BufWriter::new(writer);
    writer.write_all(&UTF8_BOM).ok();

    let mut csv_writer = WriterBuilder::new().terminator(csv::Terminator::CRLF).from_writer(writer);
    csv_writer.write_record(header).ok();

    Ok(csv_writer)
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

/// Collapses a field onto a single line so each CSV record stays one row in Excel.
fn sanitize(field: &str) -> String {
    field.replace('\t', " ").replace('\n', "\\n")
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    match cli.command {
        Commands::FindPaths { root, output_file, resolve_components } => {
            find_paths_command(&root, output_file.as_deref(), resolve_components)
        }
        Commands::FindComponents { root, output_file, type_dirs } => {
            find_components_command(&root, output_file.as_deref(), type_dirs)
        }
    }
}

fn find_paths_command(root: &Path, output_file: Option<&Path>, resolve_components: bool) -> ExitCode {
    if !root.is_dir() {
        eprintln!("error: {} is not a directory", root.display());
        return ExitCode::FAILURE;
    }

    let notation = PathNotation::from_root(root);

    let discovered = match discover_components(root) {
        Ok(discovered) => discovered,
        Err(err) => {
            eprintln!("error: failed to discover components: {err}");
            return ExitCode::FAILURE;
        }
    };
    let thirdparty_locations = thirdparty::find_thirdparty_locations(root, &discovered);
    let resolver = resolve_components.then(|| ComponentResolver::new(&discovered));

    let scanned = AtomicUsize::new(0);
    let failed = AtomicUsize::new(0);
    let found = AtomicUsize::new(0);
    let start = Instant::now();

    let mut header = vec!["file", "line", "start", "end", "code", "kind", "glyph_path", "real_path"];
    if resolve_components {
        header.extend(["source_component", "target_component", "path_in_component"]);
    }
    let csv_writer = match create_csv_writer(output_file, &header) {
        Ok(writer) => writer,
        Err(code) => return code,
    };

    let out = Mutex::new(csv_writer);

    WalkDir::new(root)
        .into_iter()
        .filter_entry(|entry| !thirdparty::is_thirdparty(&thirdparty_locations, &relative_unix_path(root, entry.path())))
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_file())
        .map(|entry| entry.into_path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "php"))
        .par_bridge()
        .for_each(|path| {
            scanned.fetch_add(1, Ordering::Relaxed);
            let relative = relative_unix_path(root, &path);
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
            // The source file is a real path on disk, so it always has a well-defined component
            // (if any) and no meaningful sub-path within it.
            let source_component = resolver.as_ref().and_then(|resolver| resolver.resolve(&relative)).map(|r| r.component);

            // Build every record up front so only the actual write is serialized on `out`;
            // sanitizing and resolving components are pure computation and should run fully in
            // parallel across the rayon worker threads.
            let records: Vec<Vec<String>> = results
                .iter()
                .map(|result| {
                    let mut record = vec![
                        sanitize(&relative),
                        result.line.to_string(),
                        result.start_pos.map(|pos| pos.to_string()).unwrap_or_default(),
                        result.end_pos.map(|pos| pos.to_string()).unwrap_or_default(),
                        sanitize(&result.code),
                        result.kind.to_string(),
                        sanitize(&result.path),
                        sanitize(&result.real_path),
                    ];
                    if let Some(resolver) = &resolver {
                        let target = resolver.resolve(&result.real_path);
                        record.push(source_component.clone().unwrap_or_default());
                        record.push(target.as_ref().map(|r| r.component.clone()).unwrap_or_default());
                        record.push(target.map(|r| r.path_in_component).unwrap_or_default());
                    }
                    record
                })
                .collect();

            let mut out = out.lock().unwrap();
            for record in records {
                out.write_record(record).ok();
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

fn find_components_command(root: &Path, output_file: Option<&Path>, type_dirs: bool) -> ExitCode {
    if !root.is_dir() {
        eprintln!("error: {} is not a directory", root.display());
        return ExitCode::FAILURE;
    }

    let discovered = match discover_components(root) {
        Ok(discovered) => discovered,
        Err(err) => {
            eprintln!("error: failed to read components: {err}");
            return ExitCode::FAILURE;
        }
    };

    if type_dirs {
        let mut csv_writer = match create_csv_writer(output_file, &["kind", "name", "path"]) {
            Ok(writer) => writer,
            Err(code) => return code,
        };
        for (name, path) in &discovered.subsystems {
            csv_writer.write_record(["subsystem", name, path.as_deref().unwrap_or("")]).ok();
        }
        for (name, path) in &discovered.plugin_types {
            csv_writer.write_record(["plugintype", name, path]).ok();
        }
        csv_writer.flush().ok();

        eprintln!("Found {} subsystems and {} plugin types", discovered.subsystems.len(), discovered.plugin_types.len());

        return ExitCode::SUCCESS;
    }

    let mut csv_writer = match create_csv_writer(output_file, &["component", "path"]) {
        Ok(writer) => writer,
        Err(code) => return code,
    };

    for (component, path) in &discovered.components {
        csv_writer.write_record([component.as_str(), path.as_str()]).ok();
    }
    csv_writer.flush().ok();

    eprintln!("Found {} components", discovered.components.len());

    ExitCode::SUCCESS
}
