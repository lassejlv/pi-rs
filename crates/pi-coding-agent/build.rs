//! Embeds the package assets the TS package ships next to its code (`README.md`, `CHANGELOG.md`,
//! `docs/`, `examples/`). `config.rs` materializes them on demand so the paths named in the system
//! prompt exist (see `config::get_package_assets_dir`). Files that compress well (the Markdown and
//! source files) are embedded gzip-compressed to keep the binary small.

use std::fmt::Write as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};

const ROOT_FILES: &[&str] = &["README.md", "CHANGELOG.md"];
const ROOT_DIRS: &[&str] = &["docs", "examples"];

fn collect(dir: &Path, base: &Path, out: &mut Vec<(String, PathBuf)>) {
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("read {}: {e}", dir.display()))
        .map(|entry| entry.unwrap().path())
        .collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            collect(&path, base, out);
        } else if path.is_file() {
            let rel = path.strip_prefix(base).unwrap();
            let rel = rel.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect::<Vec<_>>().join("/");
            out.push((rel, path));
        }
    }
}

/// FNV-1a over names and contents: names the materialized directory, so a changed asset set
/// never reuses a stale copy.
fn fnv1a(hash: &mut u64, bytes: &[u8]) {
    for byte in bytes {
        *hash ^= u64::from(*byte);
        *hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
}

fn main() {
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let mut files: Vec<(String, PathBuf)> = Vec::new();
    for name in ROOT_FILES {
        let path = manifest_dir.join(name);
        println!("cargo:rerun-if-changed={}", path.display());
        files.push((name.to_string(), path));
    }
    for name in ROOT_DIRS {
        let dir = manifest_dir.join(name);
        println!("cargo:rerun-if-changed={}", dir.display());
        collect(&dir, &manifest_dir, &mut files);
    }

    let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let compressed_dir = out_dir.join("package_assets");
    std::fs::create_dir_all(&compressed_dir).unwrap();
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let mut code = String::from(
        "/// Package assets embedded by `build.rs`: (relative path, contents, executable, gzip-compressed).\npub static PACKAGE_ASSETS: &[(&str, &[u8], bool, bool)] = &[\n",
    );
    for (index, (rel, path)) in files.iter().enumerate() {
        let contents = std::fs::read(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        fnv1a(&mut hash, rel.as_bytes());
        fnv1a(&mut hash, &[0]);
        fnv1a(&mut hash, &contents);
        #[cfg(unix)]
        let executable = {
            use std::os::unix::fs::PermissionsExt;
            std::fs::metadata(path).unwrap().permissions().mode() & 0o111 != 0
        };
        #[cfg(not(unix))]
        let executable = false;
        // Keep the compressed copy only when it saves at least a tenth (images are compressed).
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
        encoder.write_all(&contents).unwrap();
        let gzipped = encoder.finish().unwrap();
        let (embedded, compressed) = if gzipped.len() < contents.len() - contents.len() / 10 {
            let gz_path = compressed_dir.join(format!("{index}.gz"));
            std::fs::write(&gz_path, &gzipped).unwrap();
            (gz_path, true)
        } else {
            (path.clone(), false)
        };
        writeln!(code, "    ({rel:?}, include_bytes!({:?}), {executable}, {compressed}),", embedded.display().to_string())
            .unwrap();
    }
    code.push_str("];\n");
    writeln!(code, "/// Hash of [`PACKAGE_ASSETS`] (names and contents).\npub const PACKAGE_ASSETS_HASH: &str = \"{hash:016x}\";").unwrap();

    let out = out_dir.join("package_assets.rs");
    std::fs::write(out, code).unwrap();
}
