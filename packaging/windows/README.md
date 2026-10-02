# Windows installer

`azzurro.iss` is the Inno Setup script for `azzurro-vX.Y.Z-x86_64-setup.exe`,
the per-user installer each release carries beside the bare
`azzurro-vX.Y.Z-x86_64.exe`. It wraps that same exe, byte for byte, and adds
what a single file cannot have: a Start Menu entry, an Apps & Features entry, an
uninstaller that removes both, and an App Paths entry so that Win+R `azzurro`
finds the program from a normal, not elevated, Run box. It is what winget
installs, from the first release that carries it; `packaging/winget/README.md`
covers that side, including what becomes of the first submission, which is
still open.

It is the same script as rPGP's `packaging/windows/rpgp.iss`, with Azzurro's
names, icon and folders, and release.yml builds and checks it the same way.

## What it does, and does not

- Installs for the current user only, into `%LOCALAPPDATA%\Programs\Azzurro`,
  and never asks for an administrator. Per user, so that the account that
  installs Azzurro is the account that runs it and keeps its settings.
- One shortcut, in the Start Menu. No desktop shortcut, no PATH change.
- A "Launch Azzurro" box at the end of an interactive install; a silent install,
  which is how winget runs it, starts nothing.
- Upgrades in place: the same `AppId` finds the installed copy. A running
  Azzurro is closed first, by the request Windows also makes of it at sign-out,
  and without asking anyone when the upgrade is silent, as winget's is. Where it
  cannot be closed that way, the upgrade fails and leaves the old version
  installed and running rather than ending it (`CloseApplications` in the
  script says why). Setup then exits with code 5, which winget's default return
  codes for an Inno installer map to cancelled by the user, so winget says the
  upgrade was cancelled, though nobody cancelled it. Closing Azzurro and running
  the upgrade again fixes it. That is how rPGP's identical setting behaved on a
  test machine; Azzurro has not yet been upgraded over a running copy.
- Never touches `%APPDATA%\azzurro` (the players it has seen, the stations typed
  in by hand, recent searches, the home screen's order, each player's last
  decoder tier) or `%LOCALAPPDATA%\azzurro` (the artwork cache). The uninstaller
  removes what the installer wrote and nothing else, and release.yml fails a
  build whose uninstaller disturbs a file placed in either directory first.
  Those two directories are also where the bare exe keeps its files, so moving
  from it to the installer, or back, finds everything where it was.

Close Azzurro before uninstalling it. Inno's uninstaller has no way to close a
running copy: it removes the shortcut, the App Paths entry and the uninstall
entry, and leaves `azzurro.exe` and its folder behind, which can be deleted once
Azzurro is closed.

The exe inside is the bare one, so the Visual C++ runtime stays linked into it
(`.cargo/config.toml`): it still ships on its own, and the installer carries no
redistributable.

## How it is built

The windows job of `.github/workflows/release.yml` downloads Inno Setup 7.1.0
(x64) from jrsoftware's GitHub release, checks it against a pinned SHA-256, and
compiles this script twice: once from the staged exe, and once from a copy of it
at another path and under another name, into another directory. The two outputs
have to be identical or the build fails, so the setup exe is a function of the
exe, this script and that compiler and nothing else. It then installs the result
on the runner, checks the shortcut, the App Paths entry, the uninstall entry and
the installed exe's hash, uninstalls it, and checks that everything is gone
apart from the stand-ins for Azzurro's own files. Last, before the upload, it
checks that both exes are still the bytes the build staged and compiled. The
comment above the Inno Setup step says how to move the pin.

## Checking a setup exe

The setup exe has its own line in the release's signed `SHA256SUMS`, so the
signature covers it like any other asset. That it wraps the signed bare exe and
nothing else can be checked on Windows, by rebuilding it or installing it, and
the payload alone from Linux.

Rebuild it. With the bare exe from the release, this script from the release's
tag, and the same Inno Setup, 7.1.0's x64 edition, from the repository root:

    ISCC.exe --define=AppVersion=0.1.1 --define=SourceExe=C:\full\path\to\azzurro-v0.1.1-x86_64.exe --output-dir=C:\scratch packaging\windows\azzurro.iss
    certutil -hashfile C:\scratch\azzurro-v0.1.1-x86_64-setup.exe SHA256

The hash should equal the setup exe's line in `SHA256SUMS`. Neither the output's
file name nor where the exe or the checkout sits enters the setup's bytes, so
the paths can be anything. What has been shown so far is compiles on one
machine agreeing, minutes apart, from the exe at two paths under two names,
into different directories, and from the script in checkouts at two different
paths with LF or CRLF line endings; a rebuild on a second machine matching the
runner's output has not been tried yet.

Or install it, on a machine where losing nothing matters, and hash
`%LOCALAPPDATA%\Programs\Azzurro\azzurro.exe`: it should equal the bare exe's
line.

From Linux, innoextract is no help, since its newest code knows Inno Setup only
up to 6.4, but the Rust `inno` crate reads Inno Setup 7 from 0.6.0, the version
Komac's main branch uses. With its `extract` feature, `inno::Inno::new` on the
setup exe opened as a plain `std::fs::File` (a `BufReader` around it trips a
bug in 0.6.0), then `files()`, yields `{app}\azzurro.exe` and its bytes, whose
SHA-256 should equal the bare exe's line. That shows the payload is the signed
exe, though not what else the setup does with it; only the rebuild above
compares every byte.
