use std::{env, fs, path::PathBuf};

fn main() {
    // Bin-only link arg; the bin has `test = false`, so `cargo test` runs the lib tests unelevated.
    println!("cargo:rerun-if-changed=dirstat.manifest");
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap()).join("dirstat.manifest");
    let rc = PathBuf::from(env::var("OUT_DIR").unwrap()).join("dirstat.rc");
    // Manifest resource id 1, type RT_MANIFEST (24).
    fs::write(&rc, format!("1 24 \"{}\"\n", manifest.display().to_string().replace('\\', "/"))).unwrap();
    embed_resource::compile(&rc, embed_resource::NONE).manifest_required().unwrap();
}
