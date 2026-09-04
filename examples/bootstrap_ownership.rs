//! Ad hoc report, not part of the tool proper: for every file with pre-component code of its own,
//! which component owns it — core, root (unowned/repository-glue files, already copied wholesale
//! regardless), or an actual plugin. The plugin case is the one that matters: such a file only
//! exists inside that plugin's own package unless correctly copied back, so it would silently go
//! missing from the project root a real install needs it at.
//!
//! "Has pre-component code of its own" means kind `BootstrapDependency` (always interesting — by
//! construction, the whole file runs pre-component), or a `Cli`/`Other` file where something else
//! runs *before* its own boundary line — not just a `Cli`/`Other` file with an extent full stop,
//! since almost every such file gets one (usually just its own config.php line, with nothing
//! before it, which isn't interesting for this report). That "is there real work before the
//! boundary" check isn't part of the tool itself (see `entrypoints.rs`'s own doc comment: it
//! doesn't matter for rewrite-safety, which is correct either way) — recomputed here, locally,
//! purely to keep this report's signal-to-noise usable.

use std::collections::HashMap;
use std::path::PathBuf;

use scan_moodle::moodle::entrypoints::{
    BootstrapKind, PreComponentExtent, classify, is_require_like,
};
use scan_moodle::moodle::resolver::{ComponentResolver, ROOT_COMPONENT};
use scan_moodle::moodle::scan::Scan;

fn main() {
    let root = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp/moodle"));

    let scan = Scan::run(&root).expect("scan failed");
    let resolver = ComponentResolver::new(&scan.discovered);
    let classifications = classify(&scan.files, &scan.notation);

    let results_by_file: HashMap<&str, &Vec<_>> = scan
        .files
        .iter()
        .map(|(file, results)| (file.as_str(), results))
        .collect();

    for c in &classifications {
        let has_real_pre_work = match &c.extent {
            Some(PreComponentExtent::UpToLine(edges)) => {
                let earliest = edges.iter().map(|edge| edge.line()).min();
                earliest.is_some_and(|earliest| {
                    results_by_file
                        .get(c.file.as_str())
                        .copied()
                        .into_iter()
                        .flatten()
                        .any(|r| is_require_like(r.parent.as_deref()) && r.line < earliest)
                })
            }
            Some(PreComponentExtent::WholeFile) => true,
            None => false,
        };
        if c.kind != BootstrapKind::BootstrapDependency && !has_real_pre_work {
            continue;
        }
        let component = resolver
            .resolve(&c.file)
            .map(|r| r.component)
            .unwrap_or_else(|| ROOT_COMPONENT.to_string());
        let extent = c
            .extent
            .as_ref()
            .map(|e| e.to_string())
            .unwrap_or_else(|| "-".to_string());
        let flag = if component != "core" && component != ROOT_COMPONENT {
            " <-- PLUGIN"
        } else {
            ""
        };
        println!(
            "{:<55} {:<10} {:<12} {}{}",
            c.file, c.kind, extent, component, flag
        );
    }
}
