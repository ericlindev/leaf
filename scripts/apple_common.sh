#!/usr/bin/env sh

# scripts/apple_common.sh

set -ex

setup_env() {
    # The script is assumed to run in the root of the workspace

    # Default values
    mode=release
    release_flag=--release
    package=leaf-ffi
    name=leaf
    lib=lib$name.a

    if [ "$1" = "debug" ]; then
        mode=debug
        release_flag=
    fi

    export IPHONEOS_DEPLOYMENT_TARGET=10.0
    export MACOSX_DEPLOYMENT_TARGET=10.12

    # Disable LLVM bitcode embedding (deprecated since Xcode 14).
    # Rust objects: -C embed-bitcode=no
    # C/C++ objects (aws-lc-rs etc.): -fembed-bitcode=off via CFLAGS
    export RUSTFLAGS="${RUSTFLAGS:+$RUSTFLAGS }-C embed-bitcode=no"
    export CFLAGS="${CFLAGS:+$CFLAGS }-fembed-bitcode=off"

    # Output directories
    BASE_DIR="target/apple/$mode"
    INCLUDE_DIR="$BASE_DIR/include"
}

clean_dir() {
    rm -rf "$BASE_DIR"
    mkdir -p "$BASE_DIR"
    mkdir -p "$INCLUDE_DIR"
}

# Strips LLVM bitcode from pre-compiled stdlib objects and local symbols.
# Input:  $1 = path to raw .a from cargo
# Output: $2 = path to write stripped .a
# The linker merge (ld -r) collapses all objects into one and naturally
# discards the __LLVM,__bitcode sections that Rust stdlib ships.
strip_archive() {
    _src="$(cd "$(dirname "$1")" && pwd)/$(basename "$1")"
    _dst_dir="$(dirname "$2")"
    mkdir -p "$_dst_dir"
    _dst="$(cd "$_dst_dir" && pwd)/$(basename "$2")"
    _arch="$3"
    _work=$(mktemp -d)
    # Extract, merge (drops bitcode + deduplicates), strip locals, re-archive
    ( cd "$_work" && ar x "$_src" )
    xcrun ld -r -arch "$_arch" "$_work"/*.o -o "$_work/merged.o"
    strip -x "$_work/merged.o" -o "$_work/stripped.o" 2>/dev/null || cp "$_work/merged.o" "$_work/stripped.o"
    ar rcs "$_dst" "$_work/stripped.o"
    rm -rf "$_work"
}

build_macos_libs() {
    rustup target add x86_64-apple-darwin
    rustup target add aarch64-apple-darwin

    cargo build -p $package $release_flag --no-default-features --features "default-aws-lc" --target x86_64-apple-darwin
    cargo build -p $package $release_flag --no-default-features --features "default-aws-lc" --target aarch64-apple-darwin

    mkdir -p "$BASE_DIR/macos"

    # Strip each arch, then lipo
    strip_archive "target/x86_64-apple-darwin/$mode/$lib"  "$BASE_DIR/macos/x86_64.a" x86_64
    strip_archive "target/aarch64-apple-darwin/$mode/$lib"  "$BASE_DIR/macos/arm64.a"  arm64

    lipo -create \
        -arch x86_64 "$BASE_DIR/macos/x86_64.a" \
        -arch arm64 "$BASE_DIR/macos/arm64.a" \
        -output "$BASE_DIR/macos/$lib"
    rm -f "$BASE_DIR/macos/x86_64.a" "$BASE_DIR/macos/arm64.a"
}

build_ios_libs() {
    rustup target add aarch64-apple-ios
    rustup target add x86_64-apple-ios
    rustup target add aarch64-apple-ios-sim

    cargo build -p $package $release_flag --no-default-features --features "default-aws-lc" --target aarch64-apple-ios
    cargo build -p $package $release_flag --no-default-features --features "default-aws-lc" --target x86_64-apple-ios
    cargo build -p $package $release_flag --no-default-features --features "default-aws-lc" --target aarch64-apple-ios-sim

    mkdir -p "$BASE_DIR/ios"
    mkdir -p "$BASE_DIR/ios-sim"

    strip_archive "target/aarch64-apple-ios/$mode/$lib" "$BASE_DIR/ios/$lib" arm64

    strip_archive "target/x86_64-apple-ios/$mode/$lib"      "$BASE_DIR/ios-sim/x86_64.a" x86_64
    strip_archive "target/aarch64-apple-ios-sim/$mode/$lib"  "$BASE_DIR/ios-sim/arm64.a"  arm64
    lipo -create \
        -arch x86_64 "$BASE_DIR/ios-sim/x86_64.a" \
        -arch arm64 "$BASE_DIR/ios-sim/arm64.a" \
        -output "$BASE_DIR/ios-sim/$lib"
    rm -f "$BASE_DIR/ios-sim/x86_64.a" "$BASE_DIR/ios-sim/arm64.a"
}

generate_header() {
    # Check for cbindgen
    if ! command -v cbindgen >/dev/null 2>&1; then
        cargo install cbindgen
    fi

    mkdir -p "$INCLUDE_DIR"
    cbindgen \
        --config "$package/cbindgen.toml" \
        "$package/src/lib.rs" > "$INCLUDE_DIR/$name.h"

    cat << EOF > "$INCLUDE_DIR/module.modulemap"
module $name {
    header "$name.h"
    export *
}
EOF
}
