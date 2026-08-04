# Miscellaneous patches

These patches were identified through manual code inspection, not via the step 2 analysis pipeline. They fix code that would break if a component were relocated out of dirroot, but they do not appear in the analysis as dynamic-component paths — the relevant lines are classified as variable-only because the path is passed as a string argument to a function rather than constructed inline.

They are distinct from the step 4 patches, which address rows produced by the analysis.

## Patches

| Patch | File | Problem | Fix |
|-------|------|---------|-----|
| `mod-glossary-lib.patch` | `public/mod/glossary/lib.php` | `get_list_of_plugins('mod/glossary/formats', ...)` resolves relative to dirroot, so breaks if `mod_glossary` moves | Pass `core_component::get_component_directory('mod_glossary')` as the `$basedir` argument |
| `mod-feedback-lib.patch` | `public/mod/feedback/lib.php` | `get_list_of_plugins($dir)` with `$dir = 'mod/feedback/item'` has the same issue | Resolve base via `core_component::get_component_directory('mod_feedback')`; change `$dir` to be relative to the component root (`'item'`) |
| `admin-tool-mobile-classes-api.patch` | `public/admin/tool/mobile/classes/api.php` | For not-logged-in users, `get_plugins_supporting_mobile()` builds `$plugintypes` with a hard-coded `$CFG->dirroot.'/auth'` entry; the dirroot-relative path won't follow a relocated auth directory (the value is only used as an array key here, but the construction is still dirroot-bound) | Start from `core_component::get_plugin_types()` and filter to the `auth` key, so the type directory comes from the component system |
