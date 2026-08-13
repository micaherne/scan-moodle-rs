# Rewrite Moodle — process spec

This describes what the `rewrite-moodle` command (built with the `rewrite` cargo feature) does to
a Moodle codebase. It is a process document, not an implementation guide.

The command mutates the target codebase on disk (it applies patches and rewrites source files in
place), so it must be run against a git checkout that can be diffed or reset afterwards.

## Inputs

The target codebase root is a command-line argument, the same as it is for `find-paths`,
`find-components` and `find-entrypoints`.

The patches are not read from a `patches/` directory on disk at run time — they are compressed and
embedded into the compiled binary. At run time they are extracted to a temporary directory, and
applied from there.

## The process

### 1. Apply the patches

Every `.patch` file among the extracted patches (searched recursively through subfolders) is
applied to the target codebase with `git apply --3way`. Non-`.patch` files (for example the
`README.md` and `.csv` files that currently live alongside the patches) are not patches and are
skipped.

If a patch fails to apply, the whole command stops immediately with a clear error identifying
which patch failed, plus whatever other troubleshooting detail is available (e.g. the target file
and git's own error output).

### 2. Scan the (now patched) codebase

Three scans are run, in code, directly against the library functions that already back the
`find-paths`, `find-components` and `find-entrypoints` commands — not by shelling out to the
`scan-moodle` binary itself:

- **Path scan**: every path reference in the codebase, with its category (see below).
- **Component scan**: every component (core, subsystems, plugins, subplugins), with its location
  relative to the codebase root.
- **Entrypoint scan**: every bootstrap file, CLI script and page in the codebase, each labelled
  with which of those three it is. Unlike the existing `find-entrypoints` command's default, this
  is the *unrestricted* scan — CLI scripts and pages are included, not just bootstrap files.

The results of all three scans are kept in memory for the next step.

### 3. Rewrite the eligible path references

The path scan categorises every reference it finds. Four of those categories are ever rewritten:

- **static-same-component** and **static-different-component** mean the reference resolves to a
  real, known file or directory inside a real, known component (the same component as the file
  containing the reference, or a different one, respectively).
- **variable-only** means the reference has the shape of a real path — rooted at `$CFG->dirroot`,
  with one or more runtime-only segments after it (e.g. `$CFG->dirroot . $includefile`) — but
  can't be resolved to a specific file or directory statically. See "variable-only rewriting"
  below.
- **config** means the reference targets config.php itself. See "config.php rewriting" below —
  it's rewritten too, but to something narrower than every other eligible category gets.

Every other category — component, pre-component, include-path-relative, plugin-type-root,
dynamic-component, dirroot-wrangling, root-wrangling, uncategorised — is left completely untouched
by this step, in every file, with no exceptions. This matters most for bootstrap files: they
routinely contain pre-component lines (real file-loading work done before `core\component` could
possibly be available yet), and those lines must never be changed no matter what other rule below
might seem to apply to the file they're in. It also matters for `core\component`'s own original
implementation (see "the `component` category" below): the rewrite this step produces must never
end up quietly modifying the very code that output calls into at run time.

#### The `component` category

`public/lib/classes/component.php` (the `core\component` class itself) and its unit test,
`public/lib/tests/component_test.php`, are Moodle's original, monolithic implementation of
component/path resolution — the code the patches applied in step 1 add `component_path()` and
`from_mono_path()` to, and the code every rewrite this step produces ends up calling into at run
time. Every reference in either file, regardless of what category it would otherwise fall into
based on what it targets, is categorised `component` instead and left completely untouched.
Rewriting the implementation those methods are made of, using the methods themselves, makes no
sense on its own terms, on top of the risk of the class ending up calling its own not-yet-defined
output while resolving a path.

#### The `include-path-relative` category

A plain string literal used as the sole value of a require/include construct (e.g.
`require_once('lib.php')`, with no concatenation or other wrapping) is normally resolved relative
to the directory of the file containing it, and rewritten the same as any other same-component
reference. But Moodle also adds `lib/pear` to PHP's own include path (see `public/lib/setup.php`'s
`ini_set('include_path', ...)` call), so that its bundled, PEAR-derived form-rendering library —
whose own files live under `lib/pear/HTML` and `lib/pear/PEAR`, and internally require each other
the same way — can be loaded from anywhere in the codebase without knowing exactly where `lib/pear`
is. A bare-literal reference like `require_once('HTML/QuickForm/element.php')`, found anywhere else
in the codebase, is resolved this way at run time: PHP searches the include path (which now
contains `lib/pear`) for a matching file, not the referencing file's own directory.

The path scan has no way to know that a given literal is one of these — it resolves every bare
literal as if relative to the referencing file's own directory, the same as any other same-component
reference, which for this shape produces a *plausible-looking but wrong* target: often a real file
happens to exist at that wrongly-computed location elsewhere in the same component, so the reference
would otherwise be confidently, and incorrectly, rewritten. Any bare-literal reference whose text
starts with `HTML/` or `PEAR/` is categorised `include-path-relative` instead, before that wrong
resolution is ever trusted, and left completely untouched — there is no `$CFG->dirroot`/`libdir`
dependency in the reference itself to remove; it's resolved entirely through the include path, a
mechanism this project isn't touching. (Theoretically the more precise check would be whether the
literal names a real file under `lib/pear`, rather than just checking its opening `HTML/`/`PEAR/`;
in the codebase this project targets, the prefix check alone is already accurate — every other
bare-literal reference is an ordinary same-directory include with no such prefix.)

The point of the rewrite is to remove the codebase's reliance on `$CFG->dirroot` and
`$CFG->libdir`, which are what currently ties code to a fixed, monolithic directory layout.
`$CFG->root` (the repository root, as opposed to `$CFG->dirroot`, the app root) is not being
removed, but it is only ever the right replacement when the *target* isn't owned by any real
component (marked "root" by the path scan, same as below) — never for a reference that resolves
into a real, named component.

Which rule applies to a given reference depends on whether the file it appears in is itself an
**entry point** — a file the entrypoint scan classified as a bootstrap file, a CLI script, or a
page (all three count).

#### Non-entry-point source files

- **Same component, and it's a real, named one**: the reference's target is a real, named
  component, and it's the same component the source file itself belongs to → rewrite to a path
  relative to `__DIR__`.
- **Same component, and it's the "root" placeholder**: some real files aren't part of any actual
  Moodle component (e.g. `public/backup/backup.class.php`) — the path scan resolves these to a
  "root" placeholder rather than a real component. When both the source file and the reference's
  target fall under that placeholder, rewrite to a path relative to `__DIR__`, exactly the same way
  as the real-component case above. Root-owned content isn't part of any component that could ever
  be split out, so its position relative to other root-owned content never moves — which is exactly
  what makes a `__DIR__`-relative path safe here, not a reason to leave the reference on
  `$CFG->dirroot`/`$CFG->libdir`: removing that dependency everywhere it can safely be removed is
  the point of this whole rewrite, and root-owned content is no exception just because it can't be
  addressed through `component_path()`.
- **Different component, target is a real component**: rewrite to
  `\core\component::component_path()` (see argument rules below).
- **Different component, target is the "root" placeholder**: the source file belongs to a real
  component but the target doesn't belong to any — `component_path()` can't be used since "root"
  isn't a real component that exists at run time. Rewrite these to use `$CFG->root` instead: the
  expression becomes `$CFG->root . '<path>'`, where `<path>` is the target's already-resolved
  path-in-component value for the "root" pseudo-component, used verbatim, with no trimming — since
  "root"'s own directory *is* the repository root, that value is already the whole reference minus
  `$CFG->root` itself, byte-for-byte, the same way the original `$CFG->dirroot`/`$CFG->root`
  reference was written. (Unlike `component_path()`'s second argument, this is a concatenation, not
  a separate function argument, so there is no leading `/` to trim.)

#### Entry-point source files

An entry point never gets a `__DIR__`-relative path, full stop, regardless of what it's targeting
or how stable that target's position relative to it happens to be today. This is a stronger
condition than "would the resulting path still resolve correctly": a `__DIR__`-relative path from
`public/mod/quiz/view.php` to root-owned content, say, is perfectly safe under the current layout —
neither side of that relationship is going anywhere once components start being split out — but
*how entry points themselves get deployed* isn't settled the way component boundaries are. Today an
entry point is a real script sitting directly in the deployed layout; this project might later
replace that with something else (e.g. a routing shim in its place, with the actual file packaged
alongside the rest of its component instead). `component_path()`/`$CFG->root` keep resolving
correctly under whatever that ends up being, because they go through the same lookup machinery
`core\component` itself uses; a hard-coded `__DIR__` climb only keeps working for as long as the
entry point stays exactly where it is today, and every one of them would have to be found and
rewritten again the moment it doesn't. The one exception is the reference to `config.php` itself,
which can never be rewritten to `component_path()`/`$CFG->root` (it's how the application boots,
before any component-resolution machinery exists to rewrite it *to*) — but, unlike everything else
in this section, that has nothing to do with entry-point status, and doesn't mean it's left alone
entirely; see "config.php rewriting" below. That exception is already handled upstream of this
step: any reference that resolves to a config.php location is always categorised `config`,
regardless of the source file, so it never reaches this section's rules in the first place.

Every other reference in an entry point follows exactly the same rule as it would in a
non-entry-point file (same-component/real, same-component/root, different-component/real,
different-component/root — see above), with one addition: both same-component cases above lose
their `__DIR__`-relative option and gain an entry-point override instead:

- **Same component, and it's a real, named one, and the source file is an entry point**: rewrite to
  `\core\component::component_path()` instead of a `__DIR__`-relative path.
- **Same component, and it's the "root" placeholder, and the source file is an entry point**:
  rewrite to `$CFG->root . '<path>'` instead of a `__DIR__`-relative path — the same replacement,
  and the same argument rule, as the different-component/root case below; which of the two
  same-component cases applies here makes no difference to the result once the source is an entry
  point.

Every other case — different-component/real, different-component/root — is unaffected by whether
the source is an entry point; the non-entry-point rules above already produce the right answer for
both.

#### Entry-point target files

A reference whose *target* — not its source — is itself a page, CLI script or bootstrap file gets
`$CFG->root . '<path>'` too, regardless of which component the target nominally belongs to and
regardless of the source's own entry-point status. This overrides every rule above, including the
same-component/real case: `<path>` here is the target's own plain repository-relative path (e.g.
`/public/lib/setup.php`), not a path relative to the source, and not a path within whatever
component the target resolves into for packaging purposes.

Entry points end up placed at their original repository-relative path directly under the project
root by a step outside this tool's own scope (a not-yet-written Composer plugin), because a page or
CLI script needs to sit at a fixed, predictable path for a web server or `php` invocation to find
it, and a bootstrap file specifically must be loadable before `core\component` exists to resolve
anything through. The same file also still gets copied into its nominal component's own package by
the tooling that builds those packages, which has no awareness of entry-point status — but that
copy is inert; the plugin-placed one is the only one that's ever safe to load. A `__DIR__`-relative
or `component_path()` reference would resolve into the inert package copy instead, and if the entry
point's real copy is also loaded elsewhere in the same request — which it usually will be, since
it's an entry point — PHP fatals on redeclaring the same classes/functions from two different files.

This specifically means two files being nominally in the same real component does not guarantee a
`__DIR__`-relative path stays safe between them the way it does for two root-owned files: root-owned
content genuinely never moves relative to other root-owned content, but an entry point is pinned
away from its own component's package while an ordinary file in that same component is not.

#### `variable-only` rewriting

Unlike every rule above, this one doesn't depend on whether the source file is an entry point, and
doesn't need a known target component at all: `\core\component::from_mono_path()` (added by the
same patch that adds `component_path()`) is a direct stand-in for `$CFG->dirroot . <path>` — it
concatenates the path it's given straight onto dirroot, exactly as the original code did, so no
resolution of the target is needed to use it safely.

A `variable-only` reference is rewritten to `\core\component::from_mono_path(<mono_path_expr>)`,
where `<mono_path_expr>` is the exact source text of everything in the original reference after
`$CFG->dirroot` — the concatenation operator (or, for a `"{$CFG->dirroot}/rest"`-style interpolated
string, the enclosing `{...}`) is dropped, but nothing else about the remainder is touched: no
leading `/` is added or trimmed, and no literal or variable segment is altered, regardless of
whether that leading `/` came from a literal immediately after `$CFG->dirroot` or was already part
of a variable's own value at that point in the original code. Passing anything other than the exact
original remainder would silently change the path `from_mono_path()` resolves to, since — unlike
`component_path()` — it does no normalisation of its own.

A `variable-only` reference not actually rooted at `$CFG->dirroot` (e.g. one rooted at
`$CFG->libdir` instead) has nothing for this rule to rewrite, and is left untouched, the same as an
ineligible category — `from_mono_path()` has no equivalent for anything but `$CFG->dirroot`. In the
codebase this project targets this doesn't come up in practice: every `variable-only` reference not
rooted at `$CFG->dirroot` turns out to live inside the two files the `component` category excludes
(see above).

#### `config.php` rewriting

A `config` reference is rewritten to a path relative to `__DIR__`, using exactly the same mechanics
as the non-entry-point same-component case above — even though the file containing it is, by
definition, always an entry point (an entry point is exactly a file that requires config.php,
directly or transitively). Nothing else in this spec gives an entry point a `__DIR__`-relative
path; this is deliberately the one exception, for two reasons specific to config.php and nothing
else:

- There is no more-robust alternative being given up. Every other entry-point override exists
  because `component_path()`/`$CFG->root` are *available* and more robust than `__DIR__`, so using
  `__DIR__` instead would be a needless downgrade. For config.php, `component_path()`/`$CFG->root`
  are never available in the first place — component.php doesn't exist yet at the point config.php
  is required — so there is nothing more robust to give up.
- It introduces no new fragility. A reference to config.php almost always starts as a bare relative
  literal (e.g. `require_once('../config.php')`), which is already exactly as tied to the
  referencing file's current location as a `__DIR__`-relative expression is — rewriting it doesn't
  make it any more fragile to a future change in where entry points live than it already was.

A `config` reference that's already `__DIR__`-relative has nothing left to do. Otherwise (a bare
literal, or written on `$CFG->dirroot`/`$CFG->root`) it's rewritten the same way as any other
`__DIR__`-relative case — see "`__DIR__`-relative path mechanics" below.

#### `component_path()` arguments

`\core\component::component_path($component, $path)`. The first argument is the target
component's frankenstyle name. The second argument is the "path in component" left over after the
component's own directory — e.g. `$CFG->dirroot . '/mod/assign/view.php'` becomes
`\core\component::component_path('mod_assign', 'view.php')`.

That leftover value needs care because it can mean three different things, and the rewrite must not
blur them together:

- Empty → the reference *is* the component's own directory. Pass it through as-is (empty).
- A single `/` → the same thing, but the original code had an explicit trailing separator on it.
  Pass it through as-is (`/`).
- Anything else → it always starts with a leading `/` (carried over from how the path was resolved)
  and may also end with a trailing separator if the original code had one. Trim the leading `/`;
  never trim a trailing one. (This is why the single-`/` case above doesn't need special-casing on
  its own — a lone separator is entirely a trailing separator, so the "never trim trailing" rule
  already leaves it alone.)

#### `__DIR__`-relative path mechanics

When a reference is rewritten to be relative to `__DIR__` (the non-entry-point same-component case
above, and the `config.php` case, which are the only cases that call for this rather than
`component_path()` or `$CFG->root`), the replacement expression is built like this:

- Find the longest directory prefix the source file's own directory shares with the target path —
  i.e. the deepest ancestor directory that contains both. `N` is how many directory levels lie
  below that shared ancestor on the source side (zero when the target already lives in the source
  file's own directory, i.e. the shared ancestor *is* the source directory). This is the shortest
  correct climb — it never climbs further than it has to just because, say, the target happens to
  sit in a different component.
- `<path>` is whatever of the target's path lies below that shared ancestor, with a leading `/`; a
  trailing separator on the original reference is preserved on `<path>` the same way. If nothing
  lies below the ancestor at all (the climb lands exactly on the target, with no trailing separator
  either), there is no `<path>` to append — the expression is just the climb on its own.
- If `N` is zero, the expression is `__DIR__` (plus `. '<path>'` if there is one).
- If `N` is exactly one, the expression is `dirname(__DIR__)` (plus `. '<path>'` if there is one) —
  bare, no count argument.
- If `N` is more than one, the expression is `dirname(__DIR__, N)` (plus `. '<path>'` if there is
  one).

This step writes the rewritten source back to the files on disk, so that step 4 sees the result of
the rewrite when it re-scans. That re-scan depends on the path-finder recognising
`dirname(__DIR__, N)`/`dirname(__FILE__, N)` — the two-argument form — as a directory climb; before
this feature existed it only recognised the equivalent nested form (`dirname(dirname(__DIR__))`),
which would have made every reference this step rewrites with `N` ≥ 2 invisible to step 4's
re-scan, silently vanishing from the "after" output instead of showing up rewritten. The
path-finder was extended alongside this feature specifically so that doesn't happen.

### 4. Re-scan the codebase

The path scan from step 2 is run again, against the now-rewritten codebase, to capture the
"after" state.

### 5. Write the output

If `--output-dir` was supplied, the path scan from step 2 ("before") and the path scan from step 4
("after") are each written to it as a CSV file, in the same format as `find-paths --categorise`
produces today. The directory is created if it doesn't already exist.

