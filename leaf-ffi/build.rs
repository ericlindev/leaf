use std::{env, fs, path::Path};

fn main() {
    let target = env::var("TARGET").expect("TARGET not set");
    if target.contains("apple-ios") {
        add_chkstk_stub();
    }
}

fn add_chkstk_stub() {
    let out_dir = env::var("OUT_DIR").expect("OUT_DIR not set");
    let stub_path = Path::new(&out_dir).join("chkstk_stub.c");

    // Work around an arm64 iOS linker failure from the current toolchain/dependency
    // combination. This intentionally provides only symbol resolution, not real
    // stack probing, and should be revisited once the upstream issue is fixed.
    fs::write(&stub_path, "void __chkstk_darwin(void) {}\n").expect("write chkstk stub");

    cc::Build::new()
        .file(&stub_path)
        .out_dir(&out_dir)
        .compile("chkstk_stub");

    println!("cargo:rustc-link-search=native={out_dir}");
    println!("cargo:rustc-link-lib=static=chkstk_stub");
}
