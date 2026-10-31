//! Embed the application icon in the Windows executable.
//!
//! Nothing to do for any other target: `embed-resource` checks the target,
//! not the host, and returns without compiling when it is not Windows.

fn main() {
    println!("cargo:rerun-if-changed=assets/windows.rc");
    println!("cargo:rerun-if-changed=assets/icon.ico");
    embed_resource::compile("assets/windows.rc", embed_resource::NONE)
        .manifest_optional()
        .unwrap();
}
