$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $true
Set-StrictMode -Version Latest

# The runner's standalone LLVM matches the native Rust host architecture.
# VsDevShell prepends x64 LLVM to PATH even on ARM64: those compilers can
# cross-compile, but bindgen cannot load their x64 libclang.dll into ARM64 Rust.
$libclangPath = Join-Path $env:ProgramFiles 'LLVM\bin'
if (-not (Test-Path (Join-Path $libclangPath 'libclang.dll'))) {
    throw "The runner's native libclang.dll was not found in $libclangPath"
}

$vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
$vs = & $vswhere -latest -products '*' -property installationPath
if (-not $vs) { throw 'Visual Studio C++ build tools were not found' }
# VsDevShell supports x64 host tools, including under Windows ARM64 emulation.
& "$vs\Common7\Tools\Launch-VsDevShell.ps1" -Arch $env:PACKAGE_ARCH -HostArch amd64 -SkipAutomaticLocation

$vcpkg = Join-Path $env:RUNNER_TEMP 'faxe-vcpkg'
git clone --filter=blob:none https://github.com/microsoft/vcpkg.git $vcpkg
git -C $vcpkg checkout 04a9d8e5212d01ee1dd9478eadd9caade4f8b0d4
& "$vcpkg\bootstrap-vcpkg.bat" -disableMetrics
$installed = Join-Path $env:RUNNER_TEMP 'faxe-vcpkg-installed'
$triplet = "$env:PACKAGE_ARCH-windows"
& "$vcpkg\vcpkg.exe" install --triplet $triplet --host-triplet $triplet `
    "--x-manifest-root=$pwd\packaging\windows" "--x-install-root=$installed"
$prefix = Join-Path $installed $triplet
$pkgconf = Get-ChildItem "$prefix\tools" -Recurse -Filter pkgconf.exe | Select-Object -First 1
if (-not $pkgconf) { throw 'vcpkg did not install pkgconf' }
$clang = (Get-Command clang.exe -ErrorAction Stop).Source
$clangxx = (Get-Command clang++.exe -ErrorAction Stop).Source

$settings = @{
    CC = $clang
    CXX = $clangxx
    LIBCLANG_PATH = $libclangPath
    PKG_CONFIG = $pkgconf.FullName
    PKG_CONFIG_PATH = "$prefix\lib\pkgconfig;$prefix\share\pkgconfig"
    FAXE_VCPKG_PREFIX = $prefix
    PATH = "$prefix\bin;$env:PATH"
}
foreach ($name in @('INCLUDE', 'LIB', 'LIBPATH', 'WindowsSdkDir', 'WindowsSDKVersion', 'VCToolsRedistDir')) {
    $settings[$name] = [Environment]::GetEnvironmentVariable($name)
}
foreach ($entry in $settings.GetEnumerator()) {
    if (-not $entry.Value) { throw "Missing Windows build setting $($entry.Key)" }
    "$($entry.Key)=$($entry.Value)" | Out-File -FilePath $env:GITHUB_ENV -Encoding utf8 -Append
}
