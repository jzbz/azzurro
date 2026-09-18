# Fails if a Windows binary imports the Visual C++ runtime.
#
# .cargo/config.toml links the CRT statically, because an exe importing
# VCRUNTIME140.dll exits at once with 0xC0000135 on a machine without the
# redistributable — no window, no message. That setting is easy to lose without
# anything else noticing: RUSTFLAGS in the environment replaces it rather than
# adding to it, and GitHub's runners have the redistributable, so every test
# still passes on a binary that would not start on a clean install. Reading the
# import table is what notices.
#
# ci.yml runs this on the debug build it tests and release.yml on the exe it
# ships, so the check cannot drift between the two.
#
#   pwsh packaging/windows-imports.ps1 target\release\azzurro.exe
#
# The Universal CRT (api-ms-win-crt-*, ucrtbase.dll) is not checked: it has
# been part of Windows itself since 10, so importing it costs nobody a download.

param(
    [Parameter(Mandatory = $true)]
    [string] $Binary,

    # Where dumpbin.exe is, for a machine where vswhere cannot find it.
    [string] $Dumpbin
)

$ErrorActionPreference = 'Stop'

if (-not (Test-Path -LiteralPath $Binary -PathType Leaf)) {
    Write-Output "::error::no binary at $Binary"
    exit 1
}

# dumpbin comes with the MSVC tools Rust already links with, but it is only on
# PATH inside a developer prompt. vswhere sits at a fixed path wherever Visual
# Studio or its Build Tools are installed, and finds it without one.
if (-not $Dumpbin) {
    $vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
    if (-not (Test-Path -LiteralPath $vswhere)) {
        Write-Output "::error::no vswhere.exe at $vswhere, so dumpbin cannot be found"
        exit 1
    }
    $Dumpbin = & $vswhere -latest -products * `
        -find 'VC\Tools\MSVC\**\bin\Hostx64\x64\dumpbin.exe' | Select-Object -First 1
    if (-not $Dumpbin) {
        Write-Output "::error::vswhere found no dumpbin.exe in any Visual Studio installation"
        exit 1
    }
}

$output = & $Dumpbin /nologo /dependents $Binary
if ($LASTEXITCODE -ne 0) {
    $output | Write-Output
    Write-Output "::error::dumpbin failed with $LASTEXITCODE reading $Binary"
    exit 1
}

# The dependencies are the indented lines naming a DLL, under the ordinary
# heading and the delay-load one alike.
$imports = @($output | ForEach-Object {
    if ($_ -match '^\s+(\S+\.dll)\s*$') { $Matches[1] }
})

# Every Windows executable imports something, KERNEL32 at the least. An empty
# list means dumpbin's output was not understood, and passing on that would
# make this check a green light that looks at nothing.
if ($imports.Count -eq 0) {
    $output | Write-Output
    Write-Output "::error::read no imports from $Binary; dumpbin's output was not understood"
    exit 1
}

Write-Output "$Binary imports:"
$imports | ForEach-Object { Write-Output "  $_" }

# -match is case-insensitive, and the linker has written these names in both
# cases. The trailing wildcard covers VCRUNTIME140_1 and the debug D variants.
$runtime = @($imports | Where-Object { $_ -match '^(vcruntime140|msvcp140)' })
if ($runtime.Count -gt 0) {
    Write-Output "::error::$Binary imports the Visual C++ runtime: $($runtime -join ', ')"
    Write-Output "::error::It will not start on a machine without the redistributable. Is RUSTFLAGS"
    Write-Output "::error::set somewhere, replacing the +crt-static in .cargo/config.toml?"
    exit 1
}

Write-Output "No Visual C++ runtime imports."
