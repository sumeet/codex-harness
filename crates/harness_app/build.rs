fn main() {
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        // The embedded Zed editor needs the same stack reserve as Zed's Windows binary.
        println!("cargo:rustc-link-arg=/stack:8388608");
    }
}
