use std::env;
use std::path::PathBuf;

/// Clang's triple parser rejects the `uclibceabihf` environment outright:
///
///     error: version 'uclibceabihf' in target triple
///            'armv7-unknown-linux-uclibceabihf' is invalid
///
/// bindgen forwards cargo's `TARGET` to libclang as `--target`, so every
/// header parse dies before it reads a line. Clang understands the
/// `gnueabihf` spelling, and for *parsing C headers* the two are the same
/// machine: 32-bit ARMv7, hard-float PCS, identical type sizes. The libc
/// difference does not leak in, because the headers themselves come from
/// `--sysroot`, which points at the uClibc sysroot either way.
///
/// Only the spelling handed to clang changes. The Rust target, the linker
/// and the libraries are untouched.
fn clang_target(target: &str) -> Option<String> {
    if !target.contains("uclibc") {
        return None; // clang already understands this triple
    }
    // One substitution covers every spelling, and cannot quietly no-op:
    //   uclibceabihf -> gnueabihf,  uclibceabi -> gnueabi,  uclibc -> gnu
    Some(target.replace("uclibc", "gnu"))
}

fn main() {
    let buildkit_root = env::var("BUILDKIT_ROOT").expect("BUILDKIT_ROOT not set");
    let buildkit_sysroot =
        format!("{}/arm-rockchip830-linux-uclibcgnueabihf/sysroot", buildkit_root);

    // Tell cargo to look for shared libraries in the specified directory
    println!("cargo:rustc-link-search=/usr/lib");
    println!("cargo:rustc-link-lib=rockit");
    println!("cargo:rustc-link-lib=rockchip_mpp");
    println!("cargo:rustc-link-lib=rga");
    println!("cargo:rustc-link-lib=rockiva");
    println!("cargo:rustc-link-lib=rknnmrt");

    // The bindgen::Builder is the main entry point
    // to bindgen, and lets you build up options for
    // the resulting bindings.
    // See `clang_target` above: clang cannot parse the uclibc triple at all.
    let target = env::var("TARGET").unwrap_or_default();
    let mut builder = bindgen::Builder::default();
    if let Some(t) = clang_target(&target) {
        builder = builder.clang_arg(format!("--target={}", t));
    }

    let bindings = builder
        .header("wrapper.h")
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()))
        .clang_arg(format!("--sysroot={}", buildkit_sysroot))
        .clang_arg(format!("-I{}/include", buildkit_sysroot))
        .clang_arg("-DUSE_ROCKCHIP_MPP")
        .clang_arg("-DRKPLATFORM=ON")
        .clang_arg("-DARCH64=OFF")
        .clang_arg("-DUAPI2")
        .clang_arg("-DRV1106_RV1103")
        .clang_arg("-DRKAIQ")
        .clang_arg("-D_LARGEFILE_SOURCE")
        .clang_arg("-D_LARGEFILE64_SOURCE")
        .clang_arg("-D_FILE_OFFSET_BITS=64")
        .generate()
        .expect("Unable to generate bindings");

    // Write the bindings to the $OUT_DIR/bindings.rs file.
    let out_path = PathBuf::from(env::var("OUT_DIR").unwrap());
    bindings.write_to_file(out_path.join("bindings.rs")).expect("Couldn't write bindings!");
}
