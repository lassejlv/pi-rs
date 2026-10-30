//! Embed every file under `js/`, compressed, as a module source table (`MODULES` in `$OUT_DIR/modules.rs`), so
//! a glue module is added by creating its file: no shared table to edit.

use std::path::{Path, PathBuf};

fn collect(dir: &Path, root: &Path, out: &mut Vec<(String, PathBuf)>) {
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .expect("read js dir")
        .filter_map(Result::ok)
        .collect();
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let path = entry.path();
        if path.is_dir() {
            collect(&path, root, out);
        } else if path.extension().is_some_and(|e| e == "js") {
            let rel = path
                .strip_prefix(root)
                .expect("under js/")
                .to_string_lossy()
                .replace('\\', "/");
            out.push((rel, path.canonicalize().expect("canonical path")));
        }
    }
}

fn main() {
    let root = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("manifest dir")).join("js");
    println!("cargo:rerun-if-changed=js");
    let mut files = Vec::new();
    collect(&root, &root, &mut files);
    let mut src = String::from(
        "/// `(path under js/, source)` of every embedded glue file.\npub(crate) static MODULES: &[(&str, pi_embed::Embedded)] = &[\n",
    );
    for (rel, path) in files {
        src.push_str(&format!(
            "    ({rel:?}, pi_embed::include_str!({:?})),\n",
            path.display().to_string()
        ));
    }
    src.push_str("];\n");
    let out = PathBuf::from(std::env::var("OUT_DIR").expect("out dir")).join("modules.rs");
    std::fs::write(out, src).expect("write modules.rs");
}
