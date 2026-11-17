//! Link probe for the Xft text renderer.
//!
//! Xft and Fontconfig are part of every X11 desktop, but the linker needs the
//! development symlinks. Probing here reports the missing package instead of
//! failing later with an unresolved `-lXft`.

use std::process::Command;

fn main() {
    for package in ["xft", "fontconfig"] {
        let found = Command::new("pkg-config")
            .args(["--exists", package])
            .status()
            .is_ok_and(|status| status.success());
        if !found {
            panic!("pkg-config cannot find {package}; install the {package} development package");
        }
    }
    println!("cargo:rustc-link-lib=Xft");
    println!("cargo:rustc-link-lib=fontconfig");
    println!("cargo:rerun-if-changed=build.rs");
}
