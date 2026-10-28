//! No build step: it exists so `OUT_DIR` names the Cargo profile folder
//! this crate is built in, which tells a build from an install (the
//! store-upgrade guard, `in_build_folder`).

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
}
