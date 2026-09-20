fn main() {
    // Pin the widget style. The app draws its own controls, but std-widgets
    // still supplies the ListView scrollbars and the volume slider, and leaving
    // the style to the platform default would give macOS cupertino scrollbars
    // and Linux fluent ones inside an otherwise identical window.
    let config = slint_build::CompilerConfiguration::new().with_style("fluent".into());

    // The names the testing backend searches by, which the compiler leaves out
    // unless it is asked for them. Without it `ElementHandle` refuses every
    // query — "requires the presence of debug info" — and a test that wanted
    // to press a row or type into a field could only fall back to invoking the
    // window's own callbacks, which is the boundary those tests exist to
    // cross.
    //
    // Asked for in every profile rather than in debug alone. Gating it on
    // `PROFILE` kept the element names out of the shipped binary, and that is
    // worth 86 KiB: built here twice over the same tree, thin LTO and
    // stripped, the Linux release binary was 39,966,600 bytes without them and
    // 40,054,792 with — two tenths of one percent. What the saving cost was a
    // UI compiled one way for the tests and another for the artifact, which is
    // the thing .cargo/config.toml refuses for the Windows runtime in the same
    // words: what is tested has to be what ships. The ten element queries in
    // the suite could not run under `--release` either, so every layout and
    // accessibility assertion was only ever made against the debug
    // compilation. Unconditional, the whole workspace suite passes under both
    // profiles, which is also what makes the timing rig over
    // `Backend::sent_items` worth running.
    let config = config.with_debug_info(true);

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

    // `FileDescription` is the **name** Windows shows, not a description of
    // anything: it is what the firewall prompt asks about, what Task Manager
    // lists under Description, and what a user is deciding to trust. Given a
    // sentence it shows the sentence — "Windows Firewall has blocked some
    // features of A controller for BluOS players", which names no application
    // anybody has heard of. It gets the app's name.
    res.set("FileDescription", "Azzurro");
    res.set("ProductName", "Azzurro");
    // The sentence goes here, where Explorer files it under Comments and
    // nothing mistakes it for a name.
    res.set("Comments", "A controller for BluOS players");
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
