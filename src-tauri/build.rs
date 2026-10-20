fn main() {
    tauri_build::build();
    // ONNX Runtime's prebuilt Windows binaries import DirectML.dll even though
    // Parla only uses the CPU execution provider. Delay-loading it means the DLL
    // is never loaded, so parla.exe runs without it (PARLA-32).
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows")
        && std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc")
    {
        println!("cargo:rustc-link-arg=/DELAYLOAD:DirectML.dll");
        println!("cargo:rustc-link-arg=delayimp.lib");
    }
    // Tauri embeds the desktop manifest in the app binary, but not the unit
    // test executable. The opt-in native smoke test also needs Common Controls
    // v6 (TaskDialogIndirect) before Windows resolves its imports.
    if std::env::var_os("CARGO_FEATURE_NATIVE_SMOKE").is_some()
        && std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows")
    {
        println!(
            r#"cargo:rustc-link-arg=/MANIFESTDEPENDENCY:type='win32' name='Microsoft.Windows.Common-Controls' version='6.0.0.0' processorArchitecture='*' publicKeyToken='6595b64144ccf1df' language='*'"#
        );
    }
}
