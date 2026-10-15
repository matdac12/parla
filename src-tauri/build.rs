fn main() {
    tauri_build::build();
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
