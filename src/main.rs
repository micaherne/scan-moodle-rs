use std::collections::{HashMap, HashSet};
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

use scan_moodle::moodle;
use scan_moodle::moodle::categorise;
use scan_moodle::moodle::components::discover_components;
use scan_moodle::moodle::entrypoints::{self, EntrypointKind};
use scan_moodle::moodle::resolver::ComponentResolver;
use scan_moodle::moodle::thirdparty;
use scan_moodle::path_finder::{PathNotation, PathResult, find_paths};

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
        /// Add a category column classifying each reference by how it relates to Moodle's
        /// bootstrap sequence and component system (implies --resolve-components' columns)
        #[arg(long = "categorise")]
        categorise: bool,
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
    /// Identify Moodle "entry point" (page/CLI) and "bootstrap" files from the codebase's
    /// require/include graph
    FindEntrypoints {
        /// Path to the Moodle codebase to scan
        root: PathBuf,
        /// Write CSV output to this file instead of stdout
        #[arg(short = 'o', long = "output-file")]
        output_file: Option<PathBuf>,
        /// Only report files reachable without the synthetic config.php requires chain, which by
        /// default sweeps every entry point into the bootstrap set too (every page reaches
        /// component.php via config.php)
        #[arg(long = "bootstrap-only")]
        bootstrap_only: bool,
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

    let mut csv_writer = WriterBuilder::new()
        .terminator(csv::Terminator::CRLF)
        .from_writer(writer);
    csv_writer.write_record(header).ok();

    Ok(csv_writer)
}

/// Discovers `root`'s components, or prints an error and returns the exit code to use if `root`
/// isn't a directory or component discovery itself fails — the same two checks every command
/// that scans a codebase needs before it can do anything else.
fn discover_or_exit(root: &Path) -> Result<moodle::components::ComponentDiscovery, ExitCode> {
    if !root.is_dir() {
        eprintln!("error: {} is not a directory", root.display());
        return Err(ExitCode::FAILURE);
    }
    discover_components(root).map_err(|err| {
        eprintln!("error: failed to discover components: {err}");
        ExitCode::FAILURE
    })
}

/// `Some(value)` as its string form, `None` as an empty string.
fn opt_to_string<T: ToString>(value: Option<T>) -> String {
    value.map(|v| v.to_string()).unwrap_or_default()
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

fn format_summary(
    scanned: usize,
    elapsed: std::time::Duration,
    entrypoint_count: usize,
    bootstrap_count: usize,
    bootstrap_only: bool,
) -> String {
    if bootstrap_only {
        format!(
            "Scanned {scanned} PHP files in {elapsed:.2?}, found {bootstrap_count} bootstrap file{}",
            if bootstrap_count == 1 { "" } else { "s" }
        )
    } else {
        format!(
            "Scanned {scanned} PHP files in {elapsed:.2?}, found {entrypoint_count} entry point{} and {bootstrap_count} bootstrap file{}",
            if entrypoint_count == 1 { "" } else { "s" },
            if bootstrap_count == 1 { "" } else { "s" }
        )
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    match cli.command {
        Commands::FindPaths {
            root,
            output_file,
            resolve_components,
            categorise,
        } => find_paths_command(&root, output_file.as_deref(), resolve_components, categorise),
        Commands::FindComponents {
            root,
            output_file,
            type_dirs,
        } => find_components_command(&root, output_file.as_deref(), type_dirs),
        Commands::FindEntrypoints {
            root,
            output_file,
            bootstrap_only,
        } => find_entrypoints_command(&root, output_file.as_deref(), bootstrap_only),
    }
}

/// Every PHP file eligible for path-finding analysis, filtered identically for every command that
/// scans one: third-party vendored code, and anything [`moodle::is_excluded_from_scan`] rules out
/// (e.g. 'config-dist.php', a template that is never real code).
fn php_paths<'a>(
    root: &'a Path,
    thirdparty_locations: &'a HashSet<String>,
) -> impl ParallelIterator<Item = PathBuf> + 'a {
    WalkDir::new(root)
        .into_iter()
        .filter_entry(|entry| {
            !thirdparty::is_thirdparty(
                thirdparty_locations,
                &relative_unix_path(root, entry.path()),
            )
        })
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_file())
        .map(|entry| entry.into_path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "php"))
        .filter(move |path| !moodle::is_excluded_from_scan(&relative_unix_path(root, path)))
        .par_bridge()
}

fn find_paths_command(
    root: &Path,
    output_file: Option<&Path>,
    resolve_components: bool,
    categorise: bool,
) -> ExitCode {
    let discovered = match discover_or_exit(root) {
        Ok(discovered) => discovered,
        Err(code) => return code,
    };

    let notation = PathNotation::from_root(root);
    let thirdparty_locations = thirdparty::find_thirdparty_locations(root, &discovered);

    // --categorise needs source_component/target_component to decide same-vs-different-component
    // and directory/dynamic-component categories, so it implies --resolve-components' columns.
    let resolve_components = resolve_components || categorise;
    let resolver = resolve_components.then(|| ComponentResolver::new(&discovered));

    let failed = AtomicUsize::new(0);
    let start = Instant::now();

    // Every file's results are collected up front, rather than streamed straight to output as
    // find-paths alone does not need to: --categorise needs the whole codebase's require/include
    // graph before it can categorise even a single reference (see entrypoints::classify), the same
    // reason find-entrypoints does this.
    let files: Vec<(String, Vec<PathResult>)> = php_paths(root, &thirdparty_locations)
        .filter_map(|path| {
            let relative = relative_unix_path(root, &path);
            let bytes = match std::fs::read(&path) {
                Ok(bytes) => bytes,
                Err(err) => {
                    eprintln!("warning: failed to read {}: {err}", path.display());
                    failed.fetch_add(1, Ordering::Relaxed);
                    return None;
                }
            };
            let source = String::from_utf8_lossy(&bytes);
            let results = find_paths(&source, &relative, &notation);
            Some((relative, results))
        })
        .collect();

    let scanned = files.len();
    let found: usize = files.iter().map(|(_, results)| results.len()).sum();

    // The small set of bootstrap files, and the line within each before which it has no possible
    // access to core\component yet (see entrypoints::classify's `bootstrap_only` mode) — the input
    // categorise::categorise needs for its PreComponent category.
    let config_locations = categorise.then(|| entrypoints::config_locations(&notation));
    let boundary_lines: Option<HashMap<String, u32>> = categorise.then(|| {
        entrypoints::classify(&files, &notation, true)
            .into_iter()
            .filter_map(|classification| classification.line.map(|line| (classification.file, line)))
            .collect()
    });

    let mut header = vec![
        "file",
        "line",
        "start",
        "end",
        "code",
        "kind",
        "glyph_path",
        "real_path",
    ];
    if resolve_components {
        header.extend(["source_component", "target_component", "path_in_component"]);
    }
    if categorise {
        header.push("category");
    }
    let csv_writer = match create_csv_writer(output_file, &header) {
        Ok(writer) => writer,
        Err(code) => return code,
    };

    let out = Mutex::new(csv_writer);

    files.par_iter().for_each(|(relative, results)| {
        if results.is_empty() {
            return;
        }

        // The source file is a real path on disk, so it always has a well-defined component
        // (if any) and no meaningful sub-path within it.
        let source_component = resolver.as_ref().and_then(|resolver| resolver.resolve(relative)).map(|r| r.component);
        let file_boundary_line = boundary_lines.as_ref().and_then(|lines| lines.get(relative).copied());

        // Build every record up front so only the actual write is serialized on `out`;
        // sanitizing, resolving components and categorising are pure computation and should run
        // fully in parallel across the rayon worker threads.
        let records: Vec<Vec<String>> = results
            .iter()
            .map(|result| {
                let mut record = vec![
                    sanitize(relative),
                    result.line.to_string(),
                    opt_to_string(result.start_pos),
                    opt_to_string(result.end_pos),
                    sanitize(&result.code),
                    result.kind.to_string(),
                    sanitize(&result.path),
                    sanitize(&result.real_path),
                ];
                let target = resolver.as_ref().and_then(|resolver| resolver.resolve(&result.real_path));
                if resolve_components {
                    record.push(source_component.clone().unwrap_or_default());
                    record.push(
                        target
                            .as_ref()
                            .map(|r| r.component.clone())
                            .unwrap_or_default(),
                    );
                    record.push(target.as_ref().map(|r| r.path_in_component.clone()).unwrap_or_default());
                }
                if categorise {
                    let category = categorise::categorise(
                        result,
                        config_locations.as_ref().expect("categorise implies config_locations is set"),
                        file_boundary_line,
                        source_component.as_deref(),
                        target.as_ref(),
                    );
                    record.push(category.to_string());
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

    eprintln!("Scanned {scanned} PHP files in {elapsed:.2?}, found {found} paths");
    let failed = failed.load(Ordering::Relaxed);
    if failed > 0 {
        eprintln!("Failed to read {failed} files");
    }

    ExitCode::SUCCESS
}

fn find_components_command(root: &Path, output_file: Option<&Path>, type_dirs: bool) -> ExitCode {
    let discovered = match discover_or_exit(root) {
        Ok(discovered) => discovered,
        Err(code) => return code,
    };

    if type_dirs {
        let mut csv_writer = match create_csv_writer(output_file, &["kind", "name", "path"]) {
            Ok(writer) => writer,
            Err(code) => return code,
        };
        for (name, path) in &discovered.subsystems {
            csv_writer
                .write_record(["subsystem", name, path.as_deref().unwrap_or("")])
                .ok();
        }
        for (name, path) in &discovered.plugin_types {
            csv_writer.write_record(["plugintype", name, path]).ok();
        }
        csv_writer.flush().ok();

        eprintln!(
            "Found {} subsystems and {} plugin types",
            discovered.subsystems.len(),
            discovered.plugin_types.len()
        );

        return ExitCode::SUCCESS;
    }

    let mut csv_writer = match create_csv_writer(output_file, &["component", "path"]) {
        Ok(writer) => writer,
        Err(code) => return code,
    };

    for (component, path) in &discovered.components {
        csv_writer
            .write_record([component.as_str(), path.as_str()])
            .ok();
    }
    csv_writer.flush().ok();

    eprintln!("Found {} components", discovered.components.len());

    ExitCode::SUCCESS
}

fn find_entrypoints_command(
    root: &Path,
    output_file: Option<&Path>,
    bootstrap_only: bool,
) -> ExitCode {
    let discovered = match discover_or_exit(root) {
        Ok(discovered) => discovered,
        Err(code) => return code,
    };

    let notation = PathNotation::from_root(root);
    let thirdparty_locations = thirdparty::find_thirdparty_locations(root, &discovered);

    let failed = AtomicUsize::new(0);
    let start = Instant::now();

    // Unlike find-paths, this needs every file's results in memory at once, since the
    // bootstrap/entry-point closures are graph reachability queries over the whole codebase, not
    // a per-file computation that can be streamed out as it's found.
    let files: Vec<(String, Vec<PathResult>)> = php_paths(root, &thirdparty_locations)
        .filter_map(|path| {
            let relative = relative_unix_path(root, &path);
            let bytes = match std::fs::read(&path) {
                Ok(bytes) => bytes,
                Err(err) => {
                    eprintln!("warning: failed to read {}: {err}", path.display());
                    failed.fetch_add(1, Ordering::Relaxed);
                    return None;
                }
            };
            let source = String::from_utf8_lossy(&bytes);
            let results = find_paths(&source, &relative, &notation);
            Some((relative, results))
        })
        .collect();

    let scanned = files.len();
    let classifications = entrypoints::classify(&files, &notation, bootstrap_only);

    let mut csv_writer = match create_csv_writer(output_file, &["file", "kind", "line"]) {
        Ok(writer) => writer,
        Err(code) => return code,
    };
    for classification in &classifications {
        let record = vec![
            sanitize(&classification.file),
            classification.kind.to_string(),
            opt_to_string(classification.line),
        ];
        csv_writer.write_record(record).ok();
    }
    csv_writer.flush().ok();

    let elapsed = start.elapsed();
    let bootstrap_count = classifications
        .iter()
        .filter(|c| c.kind == EntrypointKind::Bootstrap)
        .count();
    let entrypoint_count = classifications.len() - bootstrap_count;
    eprintln!(
        "{}",
        format_summary(
            scanned,
            elapsed,
            entrypoint_count,
            bootstrap_count,
            bootstrap_only
        )
    );
    let failed = failed.load(Ordering::Relaxed);
    if failed > 0 {
        eprintln!("Failed to read {failed} files");
    }

    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::format_summary;
    use std::time::Duration;

    #[test]
    fn bootstrap_only_summary_avoids_entry_point_language() {
        let message = format_summary(12, Duration::from_secs(1), 0, 3, true);
        assert!(message.contains("found 3 bootstrap files"));
        assert!(!message.contains("entry point"));
    }

    #[test]
    fn default_summary_mentions_entry_points_and_bootstrap_files() {
        let message = format_summary(12, Duration::from_secs(1), 2, 3, false);
        assert!(message.contains("found 2 entry points and 3 bootstrap files"));
    }
}
