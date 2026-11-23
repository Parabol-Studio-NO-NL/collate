//! Windows only: embed the app icon and version info (VERSIONINFO) into `designcraft.exe`, so it
//! shows in Explorer, the taskbar, the Start menu and Alt-Tab.
//!
//! A missing Windows resource compiler is a warning unless `DESIGNCRAFT_REQUIRE_WINRES=1`.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=../../assets/app-icon/designcraft.ico");
    println!("cargo:rerun-if-env-changed=DESIGNCRAFT_REQUIRE_WINRES");
    println!("cargo:rerun-if-env-changed=CODESIGN_VERSION");
    println!("cargo:rerun-if-env-changed=CODESIGN_UPDATE_FEED");
    println!("cargo:rustc-env=CODESIGN_VERSION={}", std::env::var("CODESIGN_VERSION").unwrap_or_else(|_| "0.1.0-dev".into()));
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") && std::env::var_os("CARGO_FEATURE_SPARKLE").is_some() {
        println!("cargo:rustc-link-arg-bin=designcraft=-Wl,-rpath,@executable_path/../Frameworks");
    }
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let mut res = winresource::WindowsResource::new();
    res.set_icon("../../assets/app-icon/designcraft.ico")
        .set("ProductName", "DesignCraft")
        .set("FileDescription", "DesignCraft page layout")
        .set("CompanyName", "Learning Machines LLC")
        .set("LegalCopyright", "Copyright (c) the DesignCraft authors. MIT OR Apache-2.0.")
        .set("OriginalFilename", "designcraft.exe")
        .set("InternalName", "designcraft");
    if let Err(e) = res.compile() {
        if std::env::var_os("DESIGNCRAFT_REQUIRE_WINRES").is_some() {
            eprintln!("embedding Windows resources failed: {e}");
            std::process::exit(1);
        }
        println!("cargo:warning=designcraft.exe built without icon/version resources: {e}");
    }
}
