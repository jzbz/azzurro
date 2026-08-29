fn main() {
    // Pin the widget style. The app draws its own controls, but std-widgets
    // still supplies the ListView scrollbars and the volume slider, and leaving
    // the style to the platform default would give macOS cupertino scrollbars
    // and Linux fluent ones inside an otherwise identical window.
    let config = slint_build::CompilerConfiguration::new().with_style("fluent".into());

    slint_build::compile_with_config("ui/app-window.slint", config)
        .expect("compiling ui/app-window.slint");

    windows_icon();
}

/// Put the icon inside the Windows executable.
///
/// The other two platforms carry their icon beside the binary — the `.app`
/// bundle has an `.icns` in its Resources and the desktop entry names a PNG
/// the icon theme resolves. Windows has neither: an `.exe` is on its own, and
/// an `.exe` with no icon resource gets the generic one in the taskbar, in
/// Explorer and in Alt-Tab.
///
/// The `.ico` holds 16 through 256 because Windows picks a size per context and
/// scaling a 256 down to 16 loses the mark entirely. It is built from the same
/// SVG as everything else by `packaging/icons.sh`.
#[cfg(windows)]
fn windows_icon() {
    println!("cargo:rerun-if-changed=desktop/blue.azzurro.Azzurro.ico");

    let mut res = winresource::WindowsResource::new();
    res.set_icon("desktop/blue.azzurro.Azzurro.ico");
    // What Explorer shows under Properties. Without these it says nothing at
    // all, which reads as something that arrived from nowhere.
    res.set("ProductName", "Azzurro");
    res.set("FileDescription", "A controller for BluOS players");
    res.set("LegalCopyright", "MIT licensed");

    // Not fatal. A resource compiler is part of the Windows SDK and is there on
    // any machine that can build this at all, but a build that is otherwise
    // fine should not be stopped by an icon: the binary runs either way, and it
    // is better to say so than to refuse.
    if let Err(e) = res.compile() {
        println!("cargo:warning=no icon embedded: {e}");
    }
}

#[cfg(not(windows))]
fn windows_icon() {}
