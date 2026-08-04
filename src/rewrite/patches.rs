//! Access to the `patches/` directory embedded into the binary at build time (see `build.rs`
//! at the repository root, which packs it into this `tar`+`zstd` archive).

use std::io::{self, Read};

static ARCHIVE: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/patches.tar.zst"));

/// One `.patch` file's contents, together with the path it lived at under `patches/` (used to
/// identify it in error messages) — e.g. `generic/course-lib.patch`.
pub(crate) struct Patch {
    pub(crate) name: String,
    pub(crate) contents: Vec<u8>,
}

/// Every `.patch` file embedded in the binary (recursively, across subfolders), decompressed
/// straight into memory — nothing is written to disk. Non-`.patch` files (e.g. `README.md` and
/// `.csv` files) are skipped. Returned sorted by name, so applying them in this order is
/// deterministic and reproducible run to run.
pub(crate) fn all() -> io::Result<Vec<Patch>> {
    let decoder = zstd::Decoder::new(ARCHIVE)?;
    let mut archive = tar::Archive::new(decoder);

    let mut patches = Vec::new();
    for entry in archive.entries()? {
        let mut entry = entry?;
        if !entry.header().entry_type().is_file() {
            continue;
        }

        let name = entry.path()?.to_string_lossy().into_owned();
        if !name.ends_with(".patch") {
            continue;
        }

        let mut contents = Vec::new();
        entry.read_to_end(&mut contents)?;
        patches.push(Patch { name, contents });
    }
    patches.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(patches)
}
