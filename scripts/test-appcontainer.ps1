# Build an isolated development package; requires Windows Developer Mode.
[CmdletBinding()]
param([switch]$Register, [switch]$Launch)
$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $true
Set-StrictMode -Version Latest
Set-Location (Split-Path $PSScriptRoot -Parent)
. "$PSScriptRoot/ci/windows-runtime.ps1"
. "$PSScriptRoot/ci/windows-manifest.ps1"

# Reuse the environment produced by setup-windows.ps1 -Local.
foreach ($line in Get-Content .cargo/config.toml) {
    if ($line -match '^(FAXE_BUILD_PATH|FAXE_VCPKG_PREFIX|VCToolsRedistDir|WindowsSdkDir|WindowsSDKVersion) = (".*")$') {
        [Environment]::SetEnvironmentVariable($Matches[1], ($Matches[2] | ConvertFrom-Json), 'Process')
    }
}
$env:PATH = "$env:FAXE_BUILD_PATH;$env:PATH"
$metadata = cargo metadata --no-deps --format-version 1 --locked | ConvertFrom-Json
$engineId = ($metadata.packages | Where-Object name -eq 'faxe-engine').id
$desktopId = ($metadata.packages | Where-Object name -eq 'faxe-desktop').id
$messages = @(cargo build --workspace --locked --message-format=json | ForEach-Object { $_ | ConvertFrom-Json })
$engine = @($messages | Where-Object { $_.reason -eq 'build-script-executed' -and $_.package_id -eq $engineId })
$desktop = @($messages | Where-Object { $_.reason -eq 'build-script-executed' -and $_.package_id -eq $desktopId })
if ($engine.Count -ne 1 -or $desktop.Count -ne 1) { throw 'Expected unique engine and desktop build outputs' }
$binary = @($messages | Where-Object { $_.reason -eq 'compiler-artifact' -and $_.target.name -eq 'faxe' -and $_.executable })[0].executable
$stage = Join-Path $pwd "build/appcontainer/$([guid]::NewGuid())"
New-Item -ItemType Directory -Force "$stage/Assets" | Out-Null
$pdfium = Join-Path $engine[0].out_dir 'pdfium-8044/bin'
$redist = @(Get-ChildItem "$env:VCToolsRedistDir/x64/Microsoft.VC*.CRT" -Directory)
Copy-WindowsRuntime -Binaries @($binary, "$pdfium/pdfium.dll") -SearchDirectories @("$env:FAXE_VCPKG_PREFIX/bin", $redist[0].FullName, $pdfium) -Destination $stage
Copy-Item "$($desktop[0].out_dir)/msix-assets/*.png" "$stage/Assets"
$manifest = Get-Content packaging/windows/AppxManifest.xml.in -Raw
# A newer version is required when registering a different staging directory.
$now = [DateTime]::UtcNow
$version = "1.$([int]($now.Date - [DateTime]'2020-01-01').TotalDays).$($now.Hour * 60 + $now.Minute).$($now.Second)"
$values = @{ IDENTITY_NAME = 'com.coral.faxe.AppContainerTest'; IDENTITY_PUBLISHER = 'CN=FAXE AppContainer Test'; PUBLISHER_DISPLAY_NAME = 'FAXE Local Test'; VERSION = $version; ARCHITECTURE = 'x64' }
foreach ($item in $values.GetEnumerator()) { $manifest = $manifest.Replace("@$($item.Key)@", $item.Value) }
Assert-FaxeAppContainerManifest -Manifest ([xml]$manifest)
$manifest = $manifest.Replace('FAXE ( FAX over SIP )', 'FAXE AppContainer Test')
$manifest | Set-Content "$stage/AppxManifest.xml" -Encoding utf8
$makeAppx = Join-Path $env:WindowsSdkDir "bin/$($env:WindowsSDKVersion.TrimEnd('\'))/x64/makeappx.exe"
& $makeAppx pack /o /d $stage /p "$stage.msix"
Write-Host "Test package: $stage.msix"
$stage | Set-Content build/appcontainer/latest-stage.txt
if ($Register -or $Launch) {
    # Windows PowerShell provides the native Appx module.
    $env:FAXE_TEST_STAGE = $stage
    & powershell.exe -NoProfile -Command '$ErrorActionPreference = "Stop"; Add-AppxPackage -Register (Join-Path $env:FAXE_TEST_STAGE "AppxManifest.xml"); $p = Get-AppxPackage -Name com.coral.faxe.AppContainerTest; if ($p.InstallLocation -ne $env:FAXE_TEST_STAGE) { throw "Windows did not register the new test location" }'
}
if ($Launch) {
    & powershell.exe -NoProfile -Command '$p = Get-AppxPackage -Name com.coral.faxe.AppContainerTest; Start-Process explorer.exe -ArgumentList ("shell:AppsFolder\" + $p.PackageFamilyName + "!FAXE")'
}

