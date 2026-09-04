# scan-moodle

A command line tool for inspecting a Moodle codebase, and (optionally) rewriting it to remove
its reliance on `$CFG->dirroot`/`$CFG->libdir` and splitting it into per-component Composer
packages.

It finds hardcoded references to internal file paths in PHP code, lists the components (core,
subsystems, plugins and subplugins) that make up the codebase, identifies files that must live at
a fixed location rather than being found via `core\component`, and — with the `rewrite` feature —
rewrites path references and extracts self-contained Composer packages.

## Building

```
cargo build --release
```

builds `find-paths`, `find-components` and `find-entrypoints`. `rewrite-moodle` and
`extract-packages` additionally require the `rewrite` feature:

```
cargo build --release --features rewrite
```

## Usage

```
scan-moodle <COMMAND>
```

CSV-producing commands write to stdout by default.

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
| `--categorise` | Add a `category` column (implies `--resolve-components`'s columns) |

Output columns: `file`, `line`, `start`, `end`, `code`, `kind`, `glyph_path`, `normalised_path`,
plus `source_component`/`target_component`/`path_in_component` with `--resolve-components`, plus
`category` with `--categorise`.

- `glyph_path` is the resolved target with the repository root collapsed to `@` (e.g.
  `@/lib/setup.php`); `normalised_path` is the same target as a plain path relative to the
  repository root, no leading slash (e.g. `public/lib/setup.php`).
- `kind` — what shape of expression the reference is: `dirroot` (`$CFG->dirroot`-rooted), `libdir`
  (`$CFG->libdir`-rooted), `root` (`$CFG->root`-rooted), `dir` (`__DIR__`- or
  `dirname(__DIR__[, N])`-rooted), `file` (`__FILE__`-rooted), `require-literal` (a bare string
  literal used as the sole value of a require/include), or `traced-variable` (a bare variable used
  as the sole value of a require/include, traced back to its nearest same-file assignment).
- `category` (with `--categorise`) — how the reference relates to Moodle's bootstrap sequence and
  component system:

  | Category | Meaning |
  | --- | --- |
  | `component` | In `core\component`'s own source file or its unit test |
  | `config` | Targets config.php itself |
  | `pre-component` | In a bootstrap file, before `core\component` is available |
  | `pre-component-literal` | Same, but a bare require/include literal specifically |
  | `include-path-relative` | A bare literal resolved via PHP's include path, not the referencing file's own directory (Moodle's bundled PEAR library under `lib/pear`) |
  | `plugin-type-root` | Exactly a plugin type's own root directory (e.g. `mod/`, `theme/`) |
  | `dynamic-component` | Resolves to a component named with a runtime variable (e.g. `mod_{$modname}`) |
  | `dirroot-wrangling` | Exactly `$CFG->dirroot`, with or without a trailing separator |
  | `root-wrangling` | Exactly `$CFG->root`, with or without a trailing separator |
  | `static-same-component` | Resolves to a literal file/directory in the same component as the reference |
  | `static-different-component` | Resolves to a literal file/directory in a different component |
  | `variable-only` | Shaped like a path with one or more runtime-only segments; doesn't resolve |
  | `uncategorised` | None of the above |

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

Output columns: `component`, `path` (path relative to `<ROOT>`) — or `kind`, `name`, `path` with
`--type-dirs`, where `kind` is `subsystem` or `plugintype`.

### `find-entrypoints`

Identifies every file in a Moodle codebase's require/include graph that must be placed back at a
fixed location rather than found via `core\component`.

```
scan-moodle find-entrypoints <ROOT> [OPTIONS]
```

| Argument | Description |
| --- | --- |
| `<ROOT>` | Path to the Moodle codebase to scan |

| Option | Description |
| --- | --- |
| `-o`, `--output-file <FILE>` | Write CSV output to this file instead of stdout |

Output columns: `file`, `kind`, `line`.

- `kind` — `cli` (lives under a `cli/` directory and reaches `core\component`'s own source file
  directly), `other` (reaches it directly too, everywhere else — ordinary pages included), or
  `bootstrap-dependency` (never reaches it directly; only ever loaded before some other file's own
  boundary is crossed).
- `line` — for `cli`/`other`, the line number(s) in that file which are still reached before
  `core\component` is available (more than one, joined with `+`, if the file has more than one
  independent path to that point — e.g. an early-exit guard clause plus the real bootstrap
  sequence). For `bootstrap-dependency`, the literal text `whole-file`. Empty for
  `core\component`'s own source file.

### `rewrite-moodle` (requires `--features rewrite`)

Rewrites a Moodle codebase in place to remove its reliance on `$CFG->dirroot`/`$CFG->libdir`,
routing eligible path references through `core\component` instead. See `REWRITE_SPEC.md` for the
full process.

```
scan-moodle rewrite-moodle <ROOT> [OPTIONS]
```

| Argument | Description |
| --- | --- |
| `<ROOT>` | Path to the Moodle codebase to patch |

| Option | Description |
| --- | --- |
| `--output-dir <DIR>` | Write the before/after path scans (as CSV, same format as `find-paths --categorise`) to this directory |

`<ROOT>` must be a git checkout, since this command applies patches and rewrites source files on
disk — commit or stash any existing changes first so the result can be diffed or reset.

### `extract-packages` (requires `--features rewrite`)

Runs the same rewrite process as `rewrite-moodle` against a **vanilla** (un-rewritten) Moodle
codebase, then copies the result into a directory of self-contained Composer packages: one per
component, a `moodle-root` package for every file owned by no component, and a `moodle-standard`
metapackage that requires all of them.

```
scan-moodle extract-packages <ROOT> <DEST> [OPTIONS]
```

| Argument | Description |
| --- | --- |
| `<ROOT>` | Path to the vanilla Moodle codebase to rewrite and extract |
| `<DEST>` | Directory to write the per-package copies into |

| Option | Description |
| --- | --- |
| `--clean` | Delete `<DEST>`'s existing contents first |
| `--output-dir <DIR>` | Write the rewrite step's before/after path scans (as CSV) to this directory |

`<ROOT>` must be a git checkout, for the same reason as `rewrite-moodle`, and is mutated in place
by the rewrite step before it's copied — reset it to vanilla before running this again.

Each package directory under `<DEST>` is named `moodle-<component>` (`moodle-root` for the root
package, `moodle-standard` for the metapackage), and is its own git repository on a `main` branch
with everything committed — Composer resolves each one as `dev-main`; none carries a `version`
field of its own. Each `composer.json`:

- `name`: `moodle/moodle-<component>` (`moodle/moodle-root`, `moodle/moodle-standard`).
- `type`: `moodle-component` for a real component, `moodle-root` for the root package,
  `metapackage` for `moodle-standard`.
- `license`: `GPL-3.0-or-later`.
- `autoload` (real components only): PSR-4, mapping the component's frankenstyle namespace to its
  own `classes/` directory.
- `extra.moodle.component` (real components only): the component's frankenstyle name — present
  only when the package has no `version.php` of its own (i.e. a subsystem, which can't name itself
  from `version.php` at runtime the way a plugin can).
- `extra.moodle.entrypoints` (real components only): `cli` and/or `other` arrays of that
  component's own entry-point files (see `find-entrypoints`'s `kind` column above; `cli` and
  `other` share the same meaning here, `bootstrap-dependency` files land in `other` too), each path
  relative to the component's own root. Omitted when the component has neither kind.
- `require`: on `moodle-standard` only, every other package produced by this run, each constrained
  to `*`.

`moodle-root`'s `composer.json` is Moodle's own upstream `composer.json`, copied as-is except for
`name` and `type`; its `composer.lock` is not copied. Neither `autoload` nor
`extra.moodle.entrypoints`/`extra.moodle.component` is ever written for `moodle-root`.

## Global options

| Option | Description |
| --- | --- |
| `-h`, `--help` | Print help |
| `-V`, `--version` | Print version |
