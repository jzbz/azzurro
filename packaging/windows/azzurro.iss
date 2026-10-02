; Azzurro's Windows installer: a per-user Inno Setup script around the one
; self-contained azzurro.exe that every release already ships.
;
; The windows job of .github/workflows/release.yml compiles it with the Inno
; Setup it pins, 7.1.0, and the script uses directives that compiler has and
; 6.x does not, so an older one stops at the first rather than building
; something else. To compile it by hand, from the root of the repository:
;
;   ISCC.exe --define=AppVersion=0.1.1
;            --define=SourceExe=C:\full\path\to\azzurro-v0.1.1-x86_64.exe
;            --output-dir=C:\somewhere\outside\the\tree
;            packaging\windows\azzurro.iss
;
; (one line). Without --output-dir ISCC writes into packaging\windows\Output.
; AppVersion has to be the version the exe carries, X.Y.Z with nothing after
; it, since it also becomes the setup's numeric file version; the workflow
; reads it from the exe's ProductVersion. packaging/windows/README.md says how
; to check that a given setup exe wraps the released azzurro.exe byte for
; byte.
;
; ASCII only, so the question of which encoding ISCC reads it in never
; arises: it has taken UTF-8 without a byte-order mark since 6.3.0, and since
; 7.0.2 it refuses to compile a byte that is not valid in the encoding it
; settles on. Comments sit on lines of their own: a trailing ";" is part of a
; directive's value.

#ifndef AppVersion
  #error Pass --define=AppVersion=X.Y.Z, the version of the exe being wrapped
#endif
#ifndef SourceExe
  #error Pass --define=SourceExe= with the full path of azzurro-vX.Y.Z-x86_64.exe
#endif

[Setup]
; Never change this. Inno names the uninstall entry after it (jzbz.Azzurro_is1
; under HKCU), which is how a newer installer finds the installed one and
; replaces it in place, and the winget manifest pins that name as the
; ProductCode. Account, then program, the way rPGP's jzbz.rPGP is named. It
; need not equal the winget identifier: winget recognizes the install by the
; ProductCode, whatever the package is called.
AppId=jzbz.Azzurro
AppName=Azzurro
AppVersion={#AppVersion}
AppPublisher=Jonathan Zeppettini
AppPublisherURL=https://jz.bz/
AppSupportURL=https://github.com/jzbz/azzurro/issues
AppUpdatesURL=https://github.com/jzbz/azzurro/releases
AppCopyright=Copyright (c) 2026 Jonathan Zeppettini
; The Apps & Features name, kept to the bare name rather than "Azzurro 0.1.1":
; winget matches the entry against the manifest's PackageName and Publisher,
; and reads the version from DisplayVersion, which is AppVersion.
UninstallDisplayName=Azzurro
UninstallDisplayIcon={app}\azzurro.exe
VersionInfoVersion={#AppVersion}

; Per user, and never elevated. Setup asks for no administrator rights, so it
; runs as the person who will use Azzurro, and the "Launch Azzurro" box at the
; end starts the app as them too: an elevated installer run by a standard user
; through an administrator's password would start it as the administrator, and
; the players and stations it saved would be in the wrong profile. In this
; mode the {auto...} constants mean the current user's folders, the uninstall
; entry and the App Paths key below go under HKCU, and no admin is needed to
; remove it. PrivilegesRequiredOverridesAllowed is left unset, so Setup
; ignores /ALLUSERS rather than turning this into a per-machine install.
PrivilegesRequired=lowest
; %LOCALAPPDATA%\Programs\Azzurro. Not %LOCALAPPDATA%\azzurro, which is the
; artwork cache's directory and no installer's business.
DefaultDirName={autopf}\Azzurro
DisableDirPage=yes
DisableProgramGroupPage=yes

; A 64-bit Setup, new in Inno Setup 7, for a 64-bit program. It also makes
; ArchitecturesAllowed and ArchitecturesInstallIn64BitMode default to
; x64compatible: x64 Windows, and Arm64 Windows 11 running the exe under
; emulation, which is where azzurro.exe itself runs.
SetupArchitecture=x64
; Windows 10, the oldest Windows Rust's x86_64-pc-windows-msvc target still
; supports. Inno's default would let Windows 7 install a program that cannot
; start there.
MinVersion=10.0

SetupIconFile=..\..\crates\azzurro-gui\desktop\blue.azzurro.Azzurro.ico
WizardStyle=modern dynamic
; The default, written out because the installer's bytes depend on it, and
; the release checks that two compiles of the same exe come out identical.
Compression=lzma2/max
; Overridden by --output-filename in the workflow, which names the setup
; after the tag or commit being built, as it names the bare exe.
OutputBaseFilename=azzurro-v{#AppVersion}-x86_64-setup

; An upgrade has to replace an azzurro.exe that may be running, so Setup asks
; Restart Manager to close it first. The request is made with the messages
; Windows sends every program at sign-out, and Azzurro, like rPGP, handles
; none of them itself and leaves the answer to Windows' default, which
; agrees. rPGP's installer, which makes the same request, closed a running
; rPGP that way on Windows 11, in a silent upgrade as winget runs one, and
; replaced it in under two seconds; Azzurro's has not yet been tried over a
; running copy. A silent setup closes it without asking; an interactive one
; names it and asks first. It is not started again afterwards, since Setup
; restarts only programs registered for that, and Azzurro is not. Whatever
; Azzurro had not finished is lost as it would be at sign-out, a change still
; on its way to disk among it. Its files are each replaced whole, by a
; rename, so what is in them stays as it was.
;
; yes, the default, written out because force was tried for rPGP and is the
; wrong choice. The two differ only where the request fails: for a copy that
; does not answer it, or one it cannot reach (in rPGP's test, one started
; from another non-interactive window station). force ends that copy, along
; with whatever was being done in it, and nobody is asked. With yes Setup
; gives up instead, with exit code 5, and changes nothing: the old version
; stays installed and that copy keeps running. winget's default return codes
; for an Inno installer map exit code 5 to cancelled by the user, so winget
; says the upgrade was cancelled, though nobody cancelled it; closing Azzurro
; and running the upgrade again fixes it. An upgrade that fails until Azzurro
; is closed is the better of the two.
CloseApplications=yes

[Files]
; ignoreversion: an upgrade, a reinstall and a winget downgrade all replace
; the exe whatever version it carries. notimestamp: the exe's time on the
; runner is the moment it was built or copied, and storing it would make two
; compiles of the same bytes differ.
Source: "{#SourceExe}"; DestDir: "{app}"; DestName: "azzurro.exe"; Flags: ignoreversion notimestamp

[Icons]
; The Start Menu entry, and the reason this installer exists. No desktop
; shortcut. The icon is the one embedded in the exe, and no AppUserModelID is
; set, because the app sets none at run time and the two have to agree for
; the taskbar to group a pinned shortcut with the running window.
Name: "{autoprograms}\Azzurro"; Filename: "{app}\azzurro.exe"; WorkingDir: "{app}"; Comment: "Control BluOS players"

[Registry]
; The azzurro command, in place of the one winget's portable package would
; have put on PATH: an App Paths entry makes Win+R "azzurro" and `start
; azzurro` find the exe, from a normal, not elevated, Run box or prompt.
; When rPGP's was tried from elevated, non-interactive processes on a test
; machine, an HKCU App Paths name did not resolve, so nothing is promised
; there.
; It does not put azzurro on PATH for a shell, and nothing here changes PATH.
; Removed, key and all, on uninstall.
Root: HKA; Subkey: "Software\Microsoft\Windows\CurrentVersion\App Paths\azzurro.exe"; ValueType: string; ValueName: ""; ValueData: "{app}\azzurro.exe"; Flags: uninsdeletekey

[Run]
; Offered at the end of an interactive install only. skipifsilent covers
; /SILENT as well as /VERYSILENT, so winget, which passes one or the other,
; never starts the app.
Filename: "{app}\azzurro.exe"; WorkingDir: "{app}"; Description: "{cm:LaunchProgram,Azzurro}"; Flags: nowait postinstall skipifsilent

; No [UninstallDelete] section, and there must never be one. The uninstaller
; removes what this script installed and then the folder if it is empty.
; Azzurro's own files live in %APPDATA%\azzurro (the players it has seen, the
; stations typed in by hand, recent searches, the home screen's order and
; each player's last decoder tier) and %LOCALAPPDATA%\azzurro (the artwork
; cache, which prunes itself), both written by the app and both outside
; {app}. The stations are kept nowhere else, any other copy of Azzurro the
; same account runs reads both, and removing this one is not a request to
; delete them. The release workflow installs and uninstalls this on its
; runner and fails if a file placed in either beforehand is gone.
