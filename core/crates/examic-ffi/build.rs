//! Android link fix-ups for transcribe.cpp.
//!
//! transcribe.cpp's build describes its system libraries the glibc way:
//! `-lstdc++ -lpthread`. Android has neither: threads are part of bionic's
//! libc, and the NDK's C++ runtime is libc++. Two one-line linker scripts,
//! found before the NDK's own directories, translate: `-lpthread` links
//! nothing, and `-lstdc++` links the static libc++, so libexamic_core.so needs
//! no libc++_shared.so next to it.

use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("android") {
        return;
    }
    let dir = PathBuf::from(std::env::var_os("OUT_DIR").unwrap()).join("android-link");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("libpthread.so"),
        "/* pthreads are part of bionic's libc */\n",
    )
    .unwrap();
    std::fs::write(dir.join("libstdc++.so"), "INPUT(-lc++_static -lc++abi)\n").unwrap();
    println!("cargo:rustc-link-search=native={}", dir.display());
}
