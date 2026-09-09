$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
. "$PSScriptRoot/windows-runtime.ps1"

$messages = @(
    [pscustomobject]@{ reason = 'compiler-artifact'; package_id = 'engine'; filenames = @('app.exe') }
    [pscustomobject]@{ reason = 'build-script-executed'; package_id = 'engine'; out_dir = 'current/engine/out' }
    [pscustomobject]@{ reason = 'build-script-executed'; package_id = 'desktop'; out_dir = 'current/desktop/out' }
    [pscustomobject]@{ reason = 'build-finished'; success = $true }
)
if ((Get-CargoOutputDirectory -Messages $messages -PackageId 'engine') -ne 'current/engine/out') {
    throw 'Did not select the engine build output'
}
foreach ($case in @('missing', 'ambiguous')) {
    $inputMessages = if ($case -eq 'missing') { $messages[0] } else {
        $messages + [pscustomobject]@{ reason = 'build-script-executed'; package_id = 'engine'; out_dir = 'other/out' }
    }
    $failure = $null
    try { Get-CargoOutputDirectory -Messages $inputMessages -PackageId 'engine' } catch {
        $failure = $_.Exception.Message
    }
    if ($failure -notlike 'Expected one build output for engine, found *') {
        throw "Did not reject $case Cargo build output: $failure"
    }
}

# Exercise dependency traversal independently of the runner's installed DLLs.
$root = Join-Path $PSScriptRoot "../../build/runtime-tests/$([guid]::NewGuid())"
$source = New-Item -ItemType Directory -Path "$root/source" -Force
foreach ($name in @('app.exe', 'pdfium.dll', 'image.dll', 'runtime.dll', 'unused.dll')) {
    Set-Content -LiteralPath "$source/$name" -Value $name
}
$graph = @{
    'app.exe' = @('IMAGE.dll', 'kernel32.dll', 'api-ms-win-crt-runtime-l1-1-0.dll')
    'image.dll' = @('runtime.dll')
    'runtime.dll' = @('image.dll') # Cycles and case-insensitive imports terminate.
    'pdfium.dll' = @('user32.dll') # Explicit dynamic-load root is retained.
}
function Get-WindowsImports([string]$Binary) { $graph[(Split-Path $Binary -Leaf)] }

Copy-WindowsRuntime -Binaries @("$source/app.exe", "$source/pdfium.dll") `
    -SearchDirectories @($source.FullName) -Destination "$root/complete"
$actual = (Get-ChildItem "$root/complete" -File).Name | Sort-Object
if (($actual -join ',') -ne 'app.exe,image.dll,pdfium.dll,runtime.dll') {
    throw "Unexpected runtime contents: $actual"
}

$graph['image.dll'] = @('missing.dll')
$failure = $null
try {
    Copy-WindowsRuntime -Binaries @("$source/app.exe") `
        -SearchDirectories @($source.FullName) -Destination "$root/missing"
} catch { $failure = $_.Exception.Message }
if ($failure -notlike "*Unresolved runtime DLL 'missing.dll' imported by 'image.dll'*") {
    throw "Missing dependency did not fail correctly: $failure"
}

$duplicate = New-Item -ItemType Directory -Path "$root/duplicate"
Set-Content -LiteralPath "$duplicate/runtime.dll" -Value 'conflicting runtime'
$failure = $null
try {
    Copy-WindowsRuntime -Binaries @("$source/app.exe") `
        -SearchDirectories @($source.FullName, $duplicate.FullName) -Destination "$root/ambiguous"
} catch { $failure = $_.Exception.Message }
if ($failure -notlike '*Ambiguous runtime DLL: runtime.dll*') {
    throw "Ambiguous dependency did not fail correctly: $failure"
}
Write-Host 'Windows runtime packaging tests passed.'
