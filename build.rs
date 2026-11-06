// With the `static` feature, FFmpeg's libavcodec references dav1d, built next to
// it by scripts/build-ffmpeg.sh. Passed as a trailing link argument so it lands
// after libavcodec on the linker command line. The system's libva (which
// loads the GPU vendor's driver at runtime) and x264/x265 are linked
// dynamically; see scripts/build-ffmpeg.sh.
fn main() {
    println!("cargo:rerun-if-env-changed=FFMPEG_DIR");
    if std::env::var_os("CARGO_FEATURE_STATIC").is_none() {
        return;
    }
    if let Some(dir) = std::env::var_os("FFMPEG_DIR") {
        let lib = std::path::Path::new(&dir).join("lib/libdav1d.a");
        // Rerun once build-ffmpeg.sh has (re)built it.
        println!("cargo:rerun-if-changed={}", lib.display());
        if lib.exists() {
            println!("cargo:rustc-link-arg={}", lib.display());
        }
    }
    println!("cargo:rustc-link-lib=dylib=va-drm");
    for library in ["va", "x264", "x265"] {
        println!("cargo:rustc-link-lib=dylib={library}");
    }
}
