//! Embeds the icon and version details in the Windows executable, so Explorer and the
//! taskbar show the app's icon even before a window opens. Other platforms take their
//! icon from the package (`.desktop` file, macOS bundle), so this does nothing there.
//!
//! The `cfg(windows)` gate is on the build machine: a Windows binary cross-compiled from
//! Linux or macOS has no embedded icon. Release builds run on Windows.
//!
//! Set `REQUIRE_WINDOWS_ICON=1` to fail instead of warn when the icon cannot be embedded,
//! as CI and release builds do.
//!
//! Copied from `genetic_algo`'s `ui/build.rs` and adapted.

const REQUIRE_ICON: &str = "REQUIRE_WINDOWS_ICON";

fn main() {
    println!("cargo:rerun-if-changed=../../assets/icons/icon.ico");
    println!("cargo:rerun-if-env-changed={REQUIRE_ICON}");
    // Both gates matter: `cfg(windows)` is the build machine (where `winresource` is
    // available), the variable is the target the binary is for.
    #[cfg(windows)]
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        embed_windows_resources();
    }
}

#[cfg(windows)]
fn embed_windows_resources() {
    let mut resources = winresource::WindowsResource::new();
    resources
        .set_icon("../../assets/icons/icon.ico")
        .set("ProductName", "Photostacker")
        .set(
            "FileDescription",
            "Photostacker: stacks exposure brackets and bursts as layers",
        );
    // A machine without a resource compiler (the Windows SDK's rc.exe, or MinGW's windres)
    // still builds the app, just without the embedded icon, unless the icon is required.
    if let Err(error) = resources.compile() {
        let message = format!("the Windows icon was not embedded: {error}");
        if std::env::var(REQUIRE_ICON).as_deref() == Ok("1") {
            panic!("{message}");
        }
        println!("cargo:warning={message}");
    }
}
