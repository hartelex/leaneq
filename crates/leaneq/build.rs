use std::path::PathBuf;

fn main() {
    let manifest_dir =
        PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("Cargo sets manifest dir"));
    let icon_path = manifest_dir.join("../../icon.ico");
    println!("cargo:rerun-if-changed={}", icon_path.display());

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        winresource::WindowsResource::new()
            .set_icon(
                icon_path
                    .to_str()
                    .expect("icon path must be valid UTF-8 for the resource compiler"),
            )
            .compile()
            .expect("failed to embed icon.ico in the Windows executable");
    }
}
