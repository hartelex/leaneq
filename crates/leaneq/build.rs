use std::path::PathBuf;

fn main() {
    let manifest_dir =
        PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("Cargo sets manifest dir"));
    let icon_path = manifest_dir.join("../../icon.ico");
    println!("cargo:rerun-if-changed={}", icon_path.display());

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let version = format!(
            "v{}",
            std::env::var("CARGO_PKG_VERSION").expect("Cargo sets package version")
        );
        winresource::WindowsResource::new()
            .set_language(0x0409)
            .set_icon(
                icon_path
                    .to_str()
                    .expect("icon path must be valid UTF-8 for the resource compiler"),
            )
            .set("FileDescription", "an equalizer program that's lightweight")
            .set("ProductName", "leaneq")
            .set("CompanyName", "hartelex")
            .set("LegalCopyright", "Copyright © 2026 hartelex")
            .set("Comments", "https://github.com/hartelex/leaneq")
            .set("InternalName", "leaneq")
            .set("OriginalFilename", "leaneq.exe")
            .set("FileVersion", &version)
            .set("ProductVersion", &version)
            .compile()
            .expect("failed to embed icon.ico in the Windows executable");
    }
}
