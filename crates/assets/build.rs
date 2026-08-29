fn main() {
    // `rust-embed` reports the files that existed while its derive macro ran,
    // but Cargo cannot otherwise notice a brand-new file nested beneath the
    // embedded asset tree. Watch the directory itself recursively so adding a
    // theme, font, icon, prompt, or image invalidates every profile's Assets
    // artifact instead of leaving an optimized binary with a stale catalog.
    println!("cargo:rerun-if-changed=../../assets");
}
