use std::{env, fs, path::PathBuf};

fn main() {
    let root = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let source = env::var_os("CELLD_LOCAL_STORE_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join("../../crates/celld/local_store.rs"));
    let source = source
        .canonicalize()
        .expect("local_store.rs must be present");
    println!("cargo:rerun-if-env-changed=CELLD_LOCAL_STORE_PATH");
    println!("cargo:rerun-if-changed={}", source.display());
    println!(
        "cargo:rerun-if-changed={}",
        source.parent().unwrap().join("local_store").display()
    );
    println!(
        "cargo:rustc-env=CELLD_TESTED_LOCAL_STORE={}",
        source.display()
    );
    let module = format!("#[path = {:?}] mod local_store;\n", source);
    fs::write(
        PathBuf::from(env::var("OUT_DIR").unwrap()).join("local_store_module.rs"),
        module,
    )
    .unwrap();
}
