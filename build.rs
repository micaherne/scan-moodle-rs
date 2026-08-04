use std::env;
use std::fs::File;
use std::path::Path;

/// Packs the `patches/` directory into a single `tar.zst` archive at build time, so the
/// `rewrite` feature can embed it into the binary with `include_bytes!` rather than reading
/// `patches/` off disk at run time.
fn main() {
    println!("cargo:rerun-if-changed=patches");

    if env::var_os("CARGO_FEATURE_REWRITE").is_none() {
        return;
    }

    let manifest_dir = env::var("CARGO_MANIFEST_DIR").unwrap();
    let patches_dir = Path::new(&manifest_dir).join("patches");

    let out_dir = env::var("OUT_DIR").unwrap();
    let archive_path = Path::new(&out_dir).join("patches.tar.zst");

    let archive_file = File::create(&archive_path)
        .unwrap_or_else(|e| panic!("failed to create {}: {e}", archive_path.display()));

    // Level 22 is zstd's maximum compression level.
    let encoder = zstd::Encoder::new(archive_file, 22).expect("failed to create zstd encoder");

    let mut tar_builder = tar::Builder::new(encoder);
    tar_builder
        .append_dir_all(".", &patches_dir)
        .unwrap_or_else(|e| panic!("failed to archive {}: {e}", patches_dir.display()));

    let encoder = tar_builder
        .into_inner()
        .expect("failed to finalize patches tar archive");
    encoder.finish().expect("failed to finalize zstd stream");
}
