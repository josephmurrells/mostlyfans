param(
    [Parameter(Mandatory = $true)]
    [string]$Version
)

$ErrorActionPreference = 'Stop'

$PawnIOModulesTag = '0.2.11'
$PawnIOModulesCommit = '52a7e536dff3e53c96917a28caac5e0fa6510696'
$PawnIOModulesRepo = 'https://github.com/namazso/PawnIO.Modules.git'
$FontLicenses = @('OFL.txt', 'UFL.txt', 'Hack-Regular.txt', 'emoji-icon-font-mit-license.txt', 'egui-icons.txt')

function Invoke-Native {
    param([string]$Command, [string[]]$Arguments)
    & $Command @Arguments
    if ($LASTEXITCODE -ne 0) {
        throw "$Command $($Arguments -join ' ') failed with exit code $LASTEXITCODE"
    }
}

function Assert-StaticRuntime {
    param([string]$Path)
    $text = [System.Text.Encoding]::ASCII.GetString([System.IO.File]::ReadAllBytes($Path))
    if ($text -match '(?i)vcruntime140|msvcp140|concrt140|api-ms-win-crt-') {
        throw "$Path links the Visual C++ runtime dynamically; build with +crt-static"
    }
}

function Assert-Zip {
    param([string]$Path, [string[]]$Entries)
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $archive = [System.IO.Compression.ZipFile]::OpenRead($Path)
    try {
        $names = $archive.Entries | ForEach-Object { $_.FullName }
        foreach ($entry in $Entries) {
            if ($names -notcontains $entry) {
                throw "$Path is missing $entry"
            }
        }
    }
    finally {
        $archive.Dispose()
    }
}

function Get-FontDirectory {
    $encoding = [Console]::OutputEncoding
    [Console]::OutputEncoding = [System.Text.Encoding]::UTF8
    try {
        $metadata = & cargo metadata --format-version 1 --locked --filter-platform x86_64-pc-windows-msvc
    }
    finally {
        [Console]::OutputEncoding = $encoding
    }
    if ($LASTEXITCODE -ne 0) {
        throw "cargo metadata failed with exit code $LASTEXITCODE"
    }
    $found = [regex]::Matches(($metadata -join ''), '"manifest_path":"([^"]*epaint_default_fonts-[^"]*?Cargo\.toml)"')
    if ($found.Count -ne 1) {
        throw "expected one epaint_default_fonts package, found $($found.Count)"
    }
    $manifest = [regex]::Unescape($found[0].Groups[1].Value)
    Join-Path (Split-Path -Parent $manifest) 'fonts'
}

function New-PawnIOSource {
    param([string]$Destination)
    $checkout = Join-Path $dist 'pawnio-modules'
    New-Item -ItemType Directory -Path $checkout | Out-Null
    Invoke-Native git @('-C', $checkout, 'init', '--quiet')
    Invoke-Native git @('-C', $checkout, 'fetch', '--quiet', '--depth', '1', $PawnIOModulesRepo, $PawnIOModulesCommit)
    $fetched = (& git -C $checkout rev-parse FETCH_HEAD).Trim()
    if ($fetched -ne $PawnIOModulesCommit) {
        throw "PawnIO.Modules fetch returned $fetched, expected $PawnIOModulesCommit"
    }
    Invoke-Native git @('-C', $checkout, 'archive', '--format=zip', "--prefix=PawnIO.Modules-$PawnIOModulesTag/", '-o', $Destination, $PawnIOModulesCommit)
    Remove-Item -Recurse -Force $checkout
}

$root = Split-Path -Parent $PSScriptRoot
Push-Location $root
try {
    $dist = Join-Path $root 'dist'
    $name = "MostlyFans-$Version-windows-x64"
    $stage = Join-Path $dist $name
    $licenses = Join-Path $stage 'licenses'
    $zip = Join-Path $dist "$name.zip"
    $source = Join-Path $dist "PawnIO.Modules-$PawnIOModulesTag-source.zip"

    if (Test-Path $dist) {
        Remove-Item -Recurse -Force $dist
    }
    New-Item -ItemType Directory -Path $licenses | Out-Null

    foreach ($exe in @('mostlyfans.exe', 'mostlyfans-diag.exe')) {
        $built = Join-Path $root "target\release\$exe"
        Assert-StaticRuntime $built
        Copy-Item $built $stage
    }
    foreach ($file in @('README.md', 'LICENSE', 'LICENSE-MPL-2.0', 'NOTICE')) {
        Copy-Item (Join-Path $root $file) $stage
    }
    Copy-Item (Join-Path $root 'crates\hw\modules\LICENSE-LGPL-2.1') $licenses

    $fonts = Get-FontDirectory
    foreach ($file in $FontLicenses) {
        Copy-Item (Join-Path $fonts $file) $licenses
    }

    Invoke-Native cargo @('about', 'generate', '--locked', '-c', 'release/about.toml', 'release/about.hbs', '-o', (Join-Path $stage 'THIRD-PARTY-LICENSES.txt'))

    Invoke-Native "$env:SystemRoot\System32\tar.exe" @('-a', '-c', '-f', $zip, '-C', $dist, $name)
    Assert-Zip $zip @("$name/mostlyfans.exe", "$name/mostlyfans-diag.exe", "$name/LICENSE", "$name/THIRD-PARTY-LICENSES.txt", "$name/licenses/LICENSE-LGPL-2.1")
    New-PawnIOSource -Destination $source
    Assert-Zip $source @("PawnIO.Modules-$PawnIOModulesTag/COPYING", "PawnIO.Modules-$PawnIOModulesTag/LpcIO.p")

    $sums = foreach ($file in @($zip, $source)) {
        $hash = (Get-FileHash -Algorithm SHA256 $file).Hash.ToLowerInvariant()
        "$hash  $(Split-Path -Leaf $file)"
    }
    [System.IO.File]::WriteAllText((Join-Path $dist 'SHA256SUMS.txt'), (($sums -join "`n") + "`n"))

    Get-ChildItem $dist -File | Format-Table Name, Length
}
finally {
    Pop-Location
}
