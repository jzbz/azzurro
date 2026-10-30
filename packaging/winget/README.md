# winget

Manifests for the Microsoft winget community repository. They do not live here
in any operational sense — winget reads them from `microsoft/winget-pkgs` — but
the first submission has to come from somewhere, and package metadata that
strangers read before installing is worth writing deliberately rather than
generating blind.

## What ships

A release carries two Windows files: the bare `azzurro-vX.Y.Z-x86_64.exe`,
which is the whole program, and `azzurro-vX.Y.Z-x86_64-setup.exe`, a per-user
Inno Setup installer that release.yml builds around that same exe
(`packaging/windows/`). winget installs the second one, as
`InstallerType: inno` with `Scope: user`, from the first release that carries
it. It asks for no administrator rights, puts azzurro.exe in
`%LOCALAPPDATA%\Programs\Azzurro`, and adds Azzurro to the Start Menu and to
Apps & Features. `winget uninstall` runs its uninstaller, which takes all of
that away again provided Azzurro is closed first, and leaves the settings and
the artwork cache where they are either way. With Azzurro still running, the
uninstaller also leaves azzurro.exe and its folder behind, to be deleted by hand
once Azzurro is closed (`packaging/windows/README.md`). The bare exe is still
on every release for whoever downloads it directly; winget simply does not
install it.

0.1.0, which has no installer, was submitted as the bare exe with
`InstallerType: portable`: winget would copy the file and register `azzurro` as
a command. That gives no Start Menu entry, and nothing in winget can give a
portable package one. No manifest schema from 1.6 to 1.28 has a field for a
shortcut, winget's own code creates none, and the request for it
(microsoft/winget-cli#2299) has been open since 2022. The entry has to come from
an installer, and an installer is also what removes it on uninstall.

The `azzurro` command comes with it in a different form. `Commands: [azzurro]`
is what makes winget register a portable package's alias; for an installer
winget does nothing with it beyond search, so it stays in the manifest for
that. The setup exe registers an App Paths entry for `azzurro.exe` under HKCU
instead, which is what Win+R and `start azzurro` look up from a normal, not
elevated, Run box or prompt. It does not put `azzurro` on PATH, and a terminal
that runs plain `azzurro` will not find it.

The manifest lists only the installer. A portable entry beside it would leave
two ways of installing one package to support indefinitely, and anyone who
took the portable one would go on upgrading without a Start Menu entry and
without knowing it was missing; winget can move nobody from one to the other
except by hand.

## The first submission

0.1.0's portable manifest went to winget-pkgs as microsoft/winget-pkgs#427198
and waited there for a moderator's manual review. It was never merged, so no
one has a winget install of 0.1.0, and it was not left to merge as it stood.
0.2.0 is the first release carrying the setup exe, and once it was published
and its `SHA256SUMS` signed, the pull request's branch
(`jzbz:Azzurro.Azzurro-0.1.0`) got one commit that removes
`manifests/a/Azzurro/Azzurro/0.1.0/` and adds 0.2.0's installer-only manifests
in `manifests/a/Azzurro/Azzurro/0.2.0/`, written by hand as below, and the pull
request was retitled for 0.2.0. It was merged on 2026-10-07, so the first
Azzurro anyone installs through winget is the installer, and nobody has a
portable install to move off.

## The identifier

`Azzurro.Azzurro`, naming the project twice, and merged that way: winget's
moderators asked for no change. (rPGP's first submission was named the same
way, `rPGP.rPGP`, and there they asked for `jzbz.rPGP`.) The identifier is now
permanent, since changing it would mean a new package and an orphaned old one
that silently stops updating. The manifests live at
`manifests/a/Azzurro/Azzurro/<version>/`.

The installer does not care either way. Its `AppId`, `jzbz.Azzurro`, names the
uninstall entry, `jzbz.Azzurro_is1`, which the manifest gives as its
`ProductCode`, and that is how winget recognizes the installed copy whatever the
package is called.

## The order matters

winget validation downloads the asset and checks its hash, so **the GitHub release
must be published, not a draft**. release.yml deliberately creates a draft, so the
winget step comes after the release is complete and public — after the signature
over SHA256SUMS, not before.

## Submitting

By hand, per release, and deliberately so — see below.

Komac could not write the first installer manifest. The setup exe is built by
Inno Setup 7, and Komac 2.16.0, the latest release as of 2026-10-02, read Inno
installers through the `inno` crate 0.4.2, which refuses anything newer than
Inno Setup 6.7; 2.17.0 (2026-10-14) reads Inno Setup up to 7.1.0. `komac
update` also starts from the version winget-pkgs already has, which for
Azzurro was none then, and Komac opens pull requests rather than adding to one
that is open. So the swap's manifests were written by hand,
starting from `manifest/`, and pushed to the pull request's branch the way
0.1.0 was submitted, with `gh api`. They have to read as follows:

- one installer, `Architecture: x64`, and `InstallerType: inno`, with no
  `portable` left anywhere, at the top of the file or under the installer;
- `Scope: user`, and no `ElevationRequirement`, since the installer never asks
  for one;
- `ProductCode: jzbz.Azzurro_is1`, which is `AppId` in
  `packaging/windows/azzurro.iss` with the `_is1` Inno adds, and how winget
  recognizes the installed copy;
- `InstallerUrl` the setup exe, and `InstallerSha256` the setup exe's line in
  the release's signed `SHA256SUMS`, checked as below;
- `Commands: [azzurro]` and `UpgradeBehavior: install`, both kept;
- the new version in all three files, and the locale file's `LicenseUrl` and
  `ReleaseNotesUrl` moved to the new tag, since both name it;
- the installer file's `ReleaseDate` the day the new release was published,
  not 0.1.0's 2026-09-01.

`AppsAndFeaturesEntries` can be left out: the uninstall entry's name, publisher
and version are the manifest's own. If it is written, it names `Azzurro`,
publisher `Jonathan Zeppettini`, the version being submitted and the same
ProductCode, and `InstallationMetadata`, if written, has
`DefaultInstallLocation: '%LocalAppData%\Programs\Azzurro'`.

Later versions go through Komac, with one URL, the setup exe's, writing the
manifests to a folder:

    komac update Azzurro.Azzurro --version 0.2.1 \
      --urls https://github.com/jzbz/azzurro/releases/download/v0.2.1/azzurro-v0.2.1-x86_64-setup.exe \
      --output <dir>

Komac downloads the installer, reads what it needs from it, computes the hash
and fills the schema, then stops at a Submit / Edit / Exit menu with Submit
chosen: **choose Exit**. The manifests are in `<dir>` by then. That needs
Komac 2.17.0 or newer (its `inno` 0.7.0), for Inno Setup 7; on Komac's main
branch before that, `inno` 0.6.0 read this installer's privileges as lowest,
which Komac turns into `Scope: user`, and its ProductCode as
`jzbz.Azzurro_is1`. Check what it wrote against the list above, and its
`InstallerSha256` against the setup exe's line in the verified `SHA256SUMS`
(below); correct anything by hand, then send them with `komac submit <dir>`,
which analyzes no installer and keeps every value, InstallerSha256 included,
though it writes the files out again in its own layout, without comments, and
asks once more before sending. Not Submit in `komac update`, nor its
`--submit`: that sends the hash Komac computed from its own download, which
nobody has compared with the signed one. The Microsoft CLA is a one-time
checkbox on the pull request.

The hash comes from the signed file, not from Komac. In a directory holding the
release's `SHA256SUMS` and `SHA256SUMS.asc`:

    gpg --verify SHA256SUMS.asc SHA256SUMS
    awk '$2 == "azzurro-v0.2.1-x86_64-setup.exe" { print toupper($1) }' SHA256SUMS

The first has to report a good signature from
`252B 901C 8885 3CF9 F939  2559 2497 38C8 641C 3359`; the second prints the value
`InstallerSha256` must equal, in the upper case the manifests use. The setup
exe's line is the one that matters to winget. The installed azzurro.exe is the
bare exe's bytes exactly, which release.yml checks by installing the setup on
its runner, so the bare exe's line vouches for what ends up on disk as well.

`manifest/` here holds what was actually submitted, for review before it is
sent and as the starting point for the next version. Since the swap that is
0.2.0's installer manifest, the one the pull request carries. Keep
`InstallerSha256` in step with the release's signed `SHA256SUMS` rather than
recomputing it: pinning the hash that signature covers is the only thread
connecting a winget install back to the key.

## Why there is no workflow for this

There was one, and it never ran. `winget.yml` fired on every published release,
found no `WINGET_PAT`, skipped itself and reported success — three releases across
two projects, each with a green check for having done nothing.

Setting the token would have been worse than leaving it unset. The job called a
third-party action and would have handed it that credential, which is exactly what
`release.yml` is written to avoid: it uses only GitHub's own actions, pinned by
commit, because a third-party action runs with the same access to the workflow as
anything else in it. The workflow was a trap primed to spring the day somebody
decided to finish the automation.

So the submission is a command, run by a person, from the machine that already
holds the signing keys. At this release cadence that is a smaller cost than a
credential in CI, and it puts the person who signed the checksums in the same
place as the person who pins the hash.

## What a winget user actually trusts

Not the PGP signature. winget pins `InstallerSha256`, verifies it client-side, and
refuses to install on a mismatch — but nothing in that chain reads SHA256SUMS.asc
or knows about key 249738C8641C3359. A winget user is trusting Microsoft's
validation pipeline, its moderators, and TLS to GitHub.

That is not an argument against winget. winget does give what it downloads a
Mark-of-the-Web, marked as from the Internet the way a browser marks it, but
once the file matches the manifest's hash and the source is a trusted one, as
the community repository is, it rewrites the mark to the Trusted zone before
running the installer, so the unsigned setup exe starts without a SmartScreen
prompt. That makes it the least unpleasant way to get unsigned code onto a
Windows machine, and strictly better than the browser download it replaces. The
exception is Smart App Control: on a Windows 11 machine where it is enforcing,
unsigned code from an unknown publisher is blocked whatever the mark says,
through winget or not. What all this is an argument for is keeping the signed
checksum file prominent in the README: it is the artifact that survives a
compromise of any of the above, and it lets anyone audit a packager's hash line
years later.
