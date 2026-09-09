param(
    [Parameter(Mandatory)][string]$PackagesDirectory,
    [string]$OutputDirectory = 'dist'
)

$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $true
Set-StrictMode -Version Latest

function Read-PackageManifest {
    param([string]$Path, [string]$EntryName)
    $archive = [System.IO.Compression.ZipFile]::OpenRead($Path)
    try {
        $entry = $archive.GetEntry($EntryName)
        if (-not $entry) { throw "Missing $EntryName in $Path" }
        $reader = [System.IO.StreamReader]::new($entry.Open())
        try { return [xml]$reader.ReadToEnd() } finally { $reader.Dispose() }
    } finally { $archive.Dispose() }
}

# Read identities from the packages, not their filenames. Reject partial or
# mixed builds before invoking MakeAppx, which also validates package identity.
$packages = @(Get-ChildItem -LiteralPath $PackagesDirectory -Filter '*.msix' -File)
if ($packages.Count -ne 2) { throw 'Expected exactly two MSIX packages (x64 and arm64)' }
$identities = @($packages | ForEach-Object {
    (Read-PackageManifest -Path $_.FullName -EntryName 'AppxManifest.xml').Package.Identity
})
$architectures = @($identities | ForEach-Object { $_.ProcessorArchitecture } | Sort-Object)
if (($architectures -join ',') -cne 'arm64,x64') {
    throw 'Expected one x64 and one arm64 MSIX package'
}
$identity = $identities[0]
foreach ($other in $identities) {
    foreach ($attribute in @('Name', 'Publisher', 'Version')) {
        if ($other.GetAttribute($attribute) -cne $identity.GetAttribute($attribute)) {
            throw "MSIX packages have different $attribute values"
        }
    }
}
$packageVersion = [version]$identity.Version
if ($packageVersion.Revision -ne 0) { throw 'Expected a Store package version ending in .0' }
$version = $packageVersion.ToString(3)

# This job only needs the SDK packager, not the Rust/vcpkg build environment.
$sdkBin = Join-Path ${env:ProgramFiles(x86)} 'Windows Kits\10\bin'
$sdk = Get-ChildItem -LiteralPath $sdkBin -Directory |
    Where-Object { $_.Name -match '^10\.0\.\d+\.0$' -and (Test-Path (Join-Path $_.FullName 'x64\makeappx.exe')) } |
    Sort-Object { [version]$_.Name } -Descending | Select-Object -First 1
if (-not $sdk) { throw 'Windows SDK x64 makeappx.exe was not found' }
$makeAppx = Join-Path $sdk.FullName 'x64\makeappx.exe'

# MakeAppx /d must contain only the packages, excluding the portable ZIPs.
$stage = Join-Path ([System.IO.Path]::GetTempPath()) "faxe-msixbundle-$([guid]::NewGuid())"
New-Item -ItemType Directory -Path $stage | Out-Null
try {
    foreach ($package in $packages) { Copy-Item -LiteralPath $package.FullName -Destination $stage }
    New-Item -ItemType Directory -Force -Path $OutputDirectory | Out-Null
    $bundle = Join-Path (Resolve-Path $OutputDirectory).Path "FAXE-$version-windows-unsigned.msixbundle"
    # Without /bv MakeAppx uses the current date/time as the bundle version.
    & $makeAppx bundle /o /bv $identity.Version /d $stage /p $bundle
    if ($LASTEXITCODE -ne 0) { throw "MakeAppx bundle failed with exit code $LASTEXITCODE" }

    $manifest = Read-PackageManifest -Path $bundle -EntryName 'AppxMetadata/AppxBundleManifest.xml'
    foreach ($attribute in @('Name', 'Publisher', 'Version')) {
        if ($manifest.Bundle.Identity.GetAttribute($attribute) -cne $identity.GetAttribute($attribute)) {
            throw "Bundle $attribute does not match its packages"
        }
    }
    $bundledPackages = @($manifest.Bundle.Packages.Package)
    $bundledArchitectures = @($bundledPackages | ForEach-Object { $_.Architecture } | Sort-Object)
    if (($bundledArchitectures -join ',') -cne 'arm64,x64') {
        throw 'Bundle must contain exactly the x64 and arm64 packages'
    }
    foreach ($package in $bundledPackages) {
        if ($package.Type -cne 'application' -or $package.Version -cne $identity.Version) {
            throw 'Bundle contains an unexpected package type or version'
        }
    }
    Write-Host "Created $bundle with x64 and arm64 packages ($($identity.Name), $($identity.Version))"
} finally {
    Remove-Item -LiteralPath $stage -Recurse -Force
}
