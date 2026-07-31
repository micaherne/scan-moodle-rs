# scan-moodle

A command line tool for inspecting a Moodle codebase. It finds hardcoded references to internal
file paths in PHP code, and lists the components (core, subsystems, plugins and subplugins) that
make up the codebase.

Both commands write CSV, to stdout by default.

## Usage

```
scan-moodle <COMMAND>
```

### `find-paths`

Scans a Moodle codebase for path references.

```
scan-moodle find-paths <ROOT> [OPTIONS]
```

| Argument | Description |
| --- | --- |
| `<ROOT>` | Path to the Moodle codebase to scan |

| Option | Description |
| --- | --- |
| `-o`, `--output-file <FILE>` | Write CSV output to this file instead of stdout |
| `--resolve-components` | Add `source_component`, `target_component` and `path_in_component` columns |
| `--categorise` | Add a `category` column classifying each reference by how it relates to Moodle's bootstrap sequence and component system (implies `--resolve-components`' columns) |

Output columns: `file`, `line`, `start`, `end`, `code`, `kind`, `glyph_path`, `normalised_path`.

`--categorise` classifies each reference into one of: `config` (targets config.php itself),
`pre-component` (in a bootstrap file, before core\component is loaded), `dynamic-component`
(resolves to a component with a variable name), `dirroot-wrangling` / `root-wrangling` (is exactly
`$CFG->dirroot` / `$CFG->root` itself, with or without a trailing separator),
`static-same-component` / `static-different-component` (resolves to a literal file or directory),
`variable-only` (doesn't resolve, but is shaped like a path with a variable in it), or
`uncategorised`.

### `find-components`

Lists all components in a Moodle codebase.

```
scan-moodle find-components <ROOT> [OPTIONS]
```

| Argument | Description |
| --- | --- |
| `<ROOT>` | Path to the Moodle codebase to scan |

| Option | Description |
| --- | --- |
| `-o`, `--output-file <FILE>` | Write CSV output to this file instead of stdout |
| `--type-dirs` | Output the subsystem and plugin type directories instead of individual components |

Output columns: `component`, `path` — or `kind`, `name`, `path` with `--type-dirs`.

## Global options

| Option | Description |
| --- | --- |
| `-h`, `--help` | Print help |
| `-V`, `--version` | Print version |
