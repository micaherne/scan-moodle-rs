use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use clap::{Parser, Subcommand};
use csv::WriterBuilder;
use rayon::prelude::*;

#[cfg(feature = "rewrite")]
use scan_moodle::extract_packages;
use scan_moodle::moodle;
use scan_moodle::moodle::entrypoints::BootstrapKind;
use scan_moodle::moodle::resolver::ComponentResolver;
use scan_moodle::moodle::scan::{Scan, categorise_all};
#[cfg(feature = "rewrite")]
use scan_moodle::rewrite;

/// Byte-order mark that hints to Excel that the CSV that follows is UTF-8.
const UTF8_BOM: [u8; 3] = [0xEF, 0xBB, 0xBF];

#[derive(Parser)]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

// The shared `Find` prefix isn't redundant internal naming — clap derives each subcommand's real,
// documented CLI name (`find-paths`, `find-components`, `find-entrypoints`, see README.md) straight
// from these variant names, and the prefix is the shared word in that actual command vocabulary.
// Renaming the variants to silence this would rename the commands themselves unless every one also
// got an explicit `#[command(name = ...)]` pin — not worth it for a naming lint that doesn't apply
// here to begin with.
#[allow(clippy::enum_variant_names)]
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
    /// Identify every file in a Moodle codebase's require/include graph that must be placed back
    /// at a fixed location rather than found through `core\component` — split into `cli` (lives
    /// under a 'cli' directory), `other` (everything else that reaches component.php directly,
    /// pages included), and `bootstrap-dependency` (never reaches component.php itself, only
    /// loaded before some other file's own boundary line runs)
    FindEntrypoints {
        /// Path to the Moodle codebase to scan
        root: PathBuf,
        /// Write CSV output to this file instead of stdout
        #[arg(short = 'o', long = "output-file")]
        output_file: Option<PathBuf>,
    },
    /// Rewrite a Moodle codebase to remove its reliance on $CFG->dirroot/$CFG->libdir (only
    /// available when the `rewrite` feature is enabled). Mutates `root` in place — see
    /// REWRITE_SPEC.md for the full process.
    #[cfg(feature = "rewrite")]
    RewriteMoodle {
        /// Path to the Moodle codebase to patch
        root: PathBuf,
        /// Write the before/after path scans (as CSV) to this directory
        #[arg(long = "output-dir")]
        output_dir: Option<PathBuf>,
    },
    /// Rewrite a vanilla Moodle codebase (only available when the `rewrite` feature is enabled;
    /// mutates `root` in place, exactly as `rewrite-moodle` does) and copy the result into a
    /// directory of self-contained Composer packages, one per component plus a `moodle-root`
    /// catch-all
    #[cfg(feature = "rewrite")]
    ExtractPackages {
        /// Path to the vanilla Moodle codebase to rewrite and extract
        root: PathBuf,
        /// Directory to write the per-package copies into
        dest: PathBuf,
        /// Delete the destination's existing contents first, so nothing from an earlier run
        /// survives into this one
        #[arg(long = "clean")]
        clean: bool,
        /// Write the rewrite step's before/after path scans (as CSV) to this directory
        #[arg(long = "output-dir")]
        output_dir: Option<PathBuf>,
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
    moodle::components::discover_components(root).map_err(|err| {
        eprintln!("error: failed to discover components: {err}");
        ExitCode::FAILURE
    })
}

/// `Some(value)` as its string form, `None` as an empty string.
fn opt_to_string<T: ToString>(value: Option<T>) -> String {
    value.map(|v| v.to_string()).unwrap_or_default()
}

/// Collapses a field onto a single line so each CSV record stays one row in Excel.
fn sanitize(field: &str) -> String {
    field.replace('\t', " ").replace('\n', "\\n")
}

fn format_summary(
    scanned: usize,
    elapsed: std::time::Duration,
    cli_count: usize,
    other_count: usize,
    dependency_count: usize,
) -> String {
    format!(
        "Scanned {scanned} PHP files in {elapsed:.2?}, found {cli_count} cli, {other_count} other, {dependency_count} bootstrap-dependency files"
    )
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    match cli.command {
        Commands::FindPaths {
            root,
            output_file,
            resolve_components,
            categorise,
        } => find_paths_command(
            &root,
            output_file.as_deref(),
            resolve_components,
            categorise,
        ),
        Commands::FindComponents {
            root,
            output_file,
            type_dirs,
        } => find_components_command(&root, output_file.as_deref(), type_dirs),
        Commands::FindEntrypoints { root, output_file } => {
            find_entrypoints_command(&root, output_file.as_deref())
        }
        #[cfg(feature = "rewrite")]
        Commands::RewriteMoodle { root, output_dir } => rewrite::run(&root, output_dir.as_deref()),
        #[cfg(feature = "rewrite")]
        Commands::ExtractPackages {
            root,
            dest,
            clean,
            output_dir,
        } => extract_packages::run(&root, &dest, clean, output_dir.as_deref()),
    }
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

    // --categorise needs source_component/target_component to decide same-vs-different-component
    // and dynamic-component/dirroot-wrangling/root-wrangling categories, so it implies
    // --resolve-components' columns.
    let resolve_components = resolve_components || categorise;

    let start = Instant::now();
    let scan = Scan::from_discovery(root, discovered);
    let scanned = scan.files.len();
    let found: usize = scan.files.iter().map(|(_, results)| results.len()).sum();

    let mut header = vec![
        "file",
        "line",
        "start",
        "end",
        "code",
        "kind",
        "glyph_path",
        "normalised_path",
    ];
    if resolve_components {
        header.extend(["source_component", "target_component", "path_in_component"]);
    }
    if categorise {
        header.push("category");
    }
    let mut csv_writer = match create_csv_writer(output_file, &header) {
        Ok(writer) => writer,
        Err(code) => return code,
    };

    if categorise {
        // categorise_all resolves components and categorises every reference up front, in
        // parallel across rayon's worker threads, so only the (inherently serial) CSV write
        // happens on this thread.
        let dirroot = moodle::dirroot_prefix(root).trim_end_matches('/');
        for reference in categorise_all(&scan, dirroot) {
            let record = vec![
                sanitize(&reference.file),
                reference.result.line.to_string(),
                opt_to_string(reference.result.start_pos),
                opt_to_string(reference.result.end_pos),
                sanitize(&reference.result.code),
                reference.result.kind.to_string(),
                sanitize(&reference.result.path),
                sanitize(&reference.result.real_path),
                reference.source_component.unwrap_or_default(),
                reference
                    .target
                    .as_ref()
                    .map(|t| t.component.clone())
                    .unwrap_or_default(),
                reference
                    .target
                    .map(|t| t.path_in_component)
                    .unwrap_or_default(),
                reference.category.to_string(),
            ];
            csv_writer.write_record(record).ok();
        }
    } else if resolve_components {
        let resolver = ComponentResolver::new(&scan.discovered);
        let records: Vec<Vec<String>> = scan
            .files
            .par_iter()
            .flat_map_iter(|(file, results)| {
                let source_component = resolver.resolve(file).map(|r| r.component);
                let resolver = &resolver;
                results.iter().map(move |result| {
                    let target = resolver.resolve(&result.real_path);
                    vec![
                        sanitize(file),
                        result.line.to_string(),
                        opt_to_string(result.start_pos),
                        opt_to_string(result.end_pos),
                        sanitize(&result.code),
                        result.kind.to_string(),
                        sanitize(&result.path),
                        sanitize(&result.real_path),
                        source_component.clone().unwrap_or_default(),
                        target
                            .as_ref()
                            .map(|t| t.component.clone())
                            .unwrap_or_default(),
                        target.map(|t| t.path_in_component).unwrap_or_default(),
                    ]
                })
            })
            .collect();
        for record in records {
            csv_writer.write_record(record).ok();
        }
    } else {
        for (file, results) in &scan.files {
            for result in results {
                let record = vec![
                    sanitize(file),
                    result.line.to_string(),
                    opt_to_string(result.start_pos),
                    opt_to_string(result.end_pos),
                    sanitize(&result.code),
                    result.kind.to_string(),
                    sanitize(&result.path),
                    sanitize(&result.real_path),
                ];
                csv_writer.write_record(record).ok();
            }
        }
    }

    csv_writer.flush().ok();
    let elapsed = start.elapsed();

    eprintln!("Scanned {scanned} PHP files in {elapsed:.2?}, found {found} paths");
    if scan.failed_reads > 0 {
        eprintln!("Failed to read {} files", scan.failed_reads);
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

fn find_entrypoints_command(root: &Path, output_file: Option<&Path>) -> ExitCode {
    let discovered = match discover_or_exit(root) {
        Ok(discovered) => discovered,
        Err(code) => return code,
    };

    let start = Instant::now();

    // Unlike find-paths, this needs every file's results in memory at once, since the underlying
    // closures are graph reachability queries over the whole codebase, not a per-file computation
    // that can be streamed out as it's found.
    let scan = Scan::from_discovery(root, discovered);
    let scanned = scan.files.len();
    let classifications = moodle::entrypoints::classify(&scan.files, &scan.notation);

    let mut csv_writer = match create_csv_writer(output_file, &["file", "kind", "line"]) {
        Ok(writer) => writer,
        Err(code) => return code,
    };
    for classification in &classifications {
        let record = vec![
            sanitize(&classification.file),
            classification.kind.to_string(),
            opt_to_string(classification.extent.clone()),
        ];
        csv_writer.write_record(record).ok();
    }
    csv_writer.flush().ok();

    let elapsed = start.elapsed();
    let cli_count = classifications
        .iter()
        .filter(|c| c.kind == BootstrapKind::Cli)
        .count();
    let dependency_count = classifications
        .iter()
        .filter(|c| c.kind == BootstrapKind::BootstrapDependency)
        .count();
    let other_count = classifications.len() - cli_count - dependency_count;
    eprintln!(
        "{}",
        format_summary(scanned, elapsed, cli_count, other_count, dependency_count)
    );
    if scan.failed_reads > 0 {
        eprintln!("Failed to read {} files", scan.failed_reads);
    }

    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::format_summary;
    use std::time::Duration;

    #[test]
    fn summary_mentions_all_three_kinds() {
        let message = format_summary(12, Duration::from_secs(1), 2, 5, 3);
        assert!(message.contains("found 2 cli, 5 other, 3 bootstrap-dependency files"));
    }
}
