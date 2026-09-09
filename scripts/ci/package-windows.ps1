$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $true
Set-StrictMode -Version Latest
. "$PSScriptRoot/windows-runtime.ps1"

$metadata = cargo metadata --no-deps --format-version 1 --locked | ConvertFrom-Json
$version = ($metadata.packages | Where-Object name -eq 'faxe-desktop').version
$build = Join-Path $pwd "target\$env:CARGO_BUILD_TARGET\release"
# A fresh directory also prevents DLLs from an earlier package surviving a rerun.
$stage = Join-Path $pwd "packaging\windows\stage\$env:PACKAGE_ARCH\$([guid]::NewGuid())"
$dist = Join-Path $pwd 'dist'
New-Item -ItemType Directory -Force $stage, $dist | Out-Null
$redist = @(Get-ChildItem "$env:VCToolsRedistDir\$env:PACKAGE_ARCH\Microsoft.VC*.CRT" -Directory)
if ($redist.Count -ne 1) { throw 'Expected one MSVC CRT redistributable directory' }
$pdfium = @(Get-ChildItem "$build\build\faxe-engine-*\out\pdfium-8044\.verified")
if ($pdfium.Count -ne 1) { throw 'Expected one PDFium artifact' }
$pdfiumRoot = $pdfium[0].Directory.FullName
# PDFium is loaded dynamically, so seed it explicitly before following imports.
Copy-WindowsRuntime -Binaries @("$build\faxe.exe", "$build\faxe-cli.exe", "$pdfiumRoot\bin\pdfium.dll") `
    -SearchDirectories @("$env:FAXE_VCPKG_PREFIX\bin", $redist[0].FullName, "$pdfiumRoot\bin") `
    -Destination $stage
Copy-Item LICENSE $stage
Copy-Item licenses "$stage\licenses" -Recurse
Copy-Item "$pdfiumRoot\licenses" "$stage\licenses\pdfium-artifact" -Recurse
Copy-Item "$pdfiumRoot\LICENSE" "$stage\licenses\pdfium-artifact\ARCHIVE-LICENSE"
foreach ($notice in Get-ChildItem "$env:FAXE_VCPKG_PREFIX\share\*\copyright") {
    $destination = "$stage\licenses\vcpkg\$($notice.Directory.Name)"
    New-Item -ItemType Directory -Force $destination | Out-Null
    Copy-Item $notice.FullName $destination
}
$iconAssets = @(Get-ChildItem "$build\build\faxe-desktop-*\out\msix-assets" -Directory)
if ($iconAssets.Count -ne 1) { throw 'Expected one generated MSIX icon asset directory' }
New-Item -ItemType Directory -Force "$stage\Assets" | Out-Null
Copy-Item "$($iconAssets[0].FullName)\*.png" "$stage\Assets"
Copy-Item assets/icons/faxe.ico "$stage\faxe.ico"
$revision = git rev-parse HEAD
@"
FAXE $version, revision $revision
GPL-3.0-only. The matching faxe-source artifact in this Actions run includes
FAXE, PJPROJECT, vendored Rust dependencies including SpanDSP, and build scripts.
Keep that corresponding source available alongside redistributed binaries.
"@ | Set-Content "$stage\SOURCE.txt"

$identity = $env:FAXE_STORE_IDENTITY
$publisher = $env:FAXE_STORE_PUBLISHER
$displayName = $env:FAXE_STORE_DISPLAY_NAME
if (-not $identity -and -not $publisher -and -not $displayName) {
    $identity = 'com.coral.faxe.CI'
    $publisher = 'CN=FAXE CI'
    $displayName = 'FAXE CI'
} elseif (-not $identity -or -not $publisher -or -not $displayName) {
    throw 'Set all three FAXE_STORE_* variables, or leave all unset for CI packages'
}
$replacements = @{
    IDENTITY_NAME = $identity
    IDENTITY_PUBLISHER = $publisher
    PUBLISHER_DISPLAY_NAME = $displayName
    ARCHITECTURE = $env:PACKAGE_ARCH
    VERSION = "$version.0"
}
$manifest = Get-Content packaging/windows/AppxManifest.xml.in -Raw
foreach ($entry in $replacements.GetEnumerator()) {
    $manifest = $manifest.Replace("@$($entry.Key)@", [System.Security.SecurityElement]::Escape($entry.Value))
}
if ($manifest -match '@[A-Z_]+@') { throw 'Unresolved MSIX manifest placeholders' }
$null = [xml]$manifest
$manifest | Set-Content "$stage\AppxManifest.xml" -Encoding utf8
$sdkBin = Join-Path $env:WindowsSdkDir "bin\$($env:WindowsSDKVersion.TrimEnd('\'))"
$makeAppx = Join-Path $sdkBin "$env:PACKAGE_ARCH\makeappx.exe"
if (-not (Test-Path $makeAppx)) { $makeAppx = Join-Path $sdkBin 'x64\makeappx.exe' }
& $makeAppx pack /o /d $stage /p "$dist\FAXE-$version-windows-$env:PACKAGE_ARCH-unsigned.msix"
Compress-Archive -Path "$stage\*" -DestinationPath "$dist\FAXE-$version-windows-$env:PACKAGE_ARCH-portable.zip" -Force
