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

0.1.0's portable manifest went to winget-pkgs as microsoft/winget-pkgs#427198,
which, as of 2026-10-02, is open and waiting for a moderator's manual review, so
no one has a winget install of Azzurro yet. That pull request is not closed, and
0.1.0 is not left to merge as it stands. Once a release carrying the setup exe
is published and its `SHA256SUMS` signed, the pull request's branch
(`jzbz:Azzurro.Azzurro-0.1.0`) gets one commit that removes
`manifests/a/Azzurro/Azzurro/0.1.0/` and adds that release's installer-only
manifests in a directory of their own, written by hand as below, and the pull
request is retitled for that version. Validation runs again on the new commit.
The pull request keeps its history, and the first Azzurro anyone installs
through winget is the installer.

If a moderator merges 0.1.0 before then, there is no open branch left to swap,
and the first installer version goes to winget-pkgs as a new pull request of
its own. Its manifests are written by hand as below, or written by a Komac
that can read the setup exe, run with `--output <dir>`, corrected there and
sent with `komac submit <dir>`. Not by 2.16.0's `komac update`, which besides
not reading Inno Setup 7 keeps the previous manifest's `InstallerType` when
that was `portable`, and so would file the setup exe as a portable package.
Expect the bot to give that pull request the `Manifest-Metadata-Consistency`
label for the change of installer type, for a moderator to clear.

Whoever installed 0.1.0 then meets the one thing winget will not do: upgrade a
portable install to an installer. `winget upgrade Azzurro.Azzurro` refuses,
saying the install technology is different and the package has to be
uninstalled and installed again, and neither `--force` nor
`--uninstall-previous` gets past that. So, by hand, once, with Azzurro closed,
since a running azzurro.exe cannot be deleted:

    winget uninstall Azzurro.Azzurro
    winget install Azzurro.Azzurro

Nothing is lost. Uninstalling the portable package removes the exe winget
copied, the `azzurro` command and winget's own entry for it, and nothing else:
the settings live in `%APPDATA%\azzurro` and the artwork cache in
`%LOCALAPPDATA%\azzurro`, outside anything winget or the installer owns, and the
Azzurro that the second command installs finds them as they were. Running the
setup exe by hand on top of a winget portable install is the one thing not to
do: it leaves two copies, with the Start Menu pointing at one, the `azzurro`
command at the other, and winget still tracking the old one.

## The identifier

`Azzurro.Azzurro`, as submitted, naming the project twice. rPGP's first
submission named its project the same way, `rPGP.rPGP`, and winget's moderators
asked for `jzbz.rPGP` instead: the publisher segment is the account that
publishes. They may ask the same of this one, for `jzbz.Azzurro`. The identifier
is effectively permanent once a version is merged, since changing it later means
a new package and an orphaned old one that silently stops updating, so if it is
going to change, the swap above is the moment. The manifests would then live at
`manifests/j/jzbz/Azzurro/<version>/` instead.

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

Komac cannot write the first installer manifest. The setup exe is built by Inno
Setup 7, and Komac 2.16.0, the latest release as of 2026-10-02, reads Inno
installers through the `inno` crate 0.4.2, which refuses anything newer than
Inno Setup 6.7; support for 7 came in `inno` 0.6.0, which Komac's main branch
already uses. `komac update` also starts from the version winget-pkgs already
has, which for Azzurro is none yet, and Komac opens pull requests rather than
adding to one that is open. So the swap's manifests are written by hand,
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

Once an installer version is merged, later ones go through Komac as before,
with one URL, the setup exe's, and a dry run first:

    komac update Azzurro.Azzurro --version 0.1.2 \
      --urls https://github.com/jzbz/azzurro/releases/download/v0.1.2/azzurro-v0.1.2-x86_64-setup.exe \
      --dry-run

Komac downloads the installer, reads what it needs from it, computes the hash
and fills the schema; with `--submit` in place of `--dry-run` it then forks
`microsoft/winget-pkgs` and opens the pull request. The Microsoft CLA is a
one-time checkbox on that PR. That needs a Komac newer than 2.16.0, or one built
from its main branch: `inno` 0.6.0 reads this installer's privileges as lowest,
which Komac turns into `Scope: user`, and its ProductCode as
`jzbz.Azzurro_is1`. The dry run should show the list above. If it shows anything
else, run it again with `--output <dir>` in place of `--dry-run`, correct the
manifests there by hand, and send that directory with `komac submit <dir>`,
which submits manifests as they stand and analyzes no installer.

The hash comes from the signed file, not from Komac. In a directory holding the
release's `SHA256SUMS` and `SHA256SUMS.asc`:

    gpg --verify SHA256SUMS.asc SHA256SUMS
    awk '$2 == "azzurro-v0.1.1-x86_64-setup.exe" { print toupper($1) }' SHA256SUMS

The first has to report a good signature from
`252B 901C 8885 3CF9 F939  2559 2497 38C8 641C 3359`; the second prints the value
`InstallerSha256` must equal, in the upper case the manifests use. The setup
exe's line is the one that matters to winget. The installed azzurro.exe is the
bare exe's bytes exactly, which release.yml checks by installing the setup on
its runner, so the bare exe's line vouches for what ends up on disk as well.

`manifest/` here holds what was actually submitted, for review before it is sent
and as the starting point for the next version. Until the swap that is 0.1.0's
portable manifest, which is what the open pull request still carries; replace
it with what goes in. Keep `InstallerSha256` in step with the release's signed
`SHA256SUMS` rather than recomputing it: pinning the hash that signature covers
is the only thread connecting a winget install back to the key.

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
