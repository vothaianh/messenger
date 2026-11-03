// Some Apple SDK pieces link Swift overlays (`@rpath/libswiftCoreMedia.dylib`
// on Intel) without an rpath, so the app fails to launch there with "Library
// not loaded". The system's Swift libraries live in /usr/lib/swift.
fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo:rustc-link-arg=-Wl,-rpath,/usr/lib/swift");
    }
}
