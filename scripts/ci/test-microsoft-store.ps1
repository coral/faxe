# Offline integration checks: fake Store CLI and a minimal bundle; no credentials.
$ErrorActionPreference = 'Stop'
$temporary = Join-Path ([IO.Path]::GetTempPath()) ([IO.Path]::GetRandomFileName())
$null = New-Item -ItemType Directory -Path $temporary
$savedEnvironment = @{}
foreach ($name in @('PRODUCT_ID', 'GITHUB_REF', 'GITHUB_STEP_SUMMARY')) {
    $savedEnvironment[$name] = [Environment]::GetEnvironmentVariable($name)
}
$global:faxeStoreTeststoreCalls = [Collections.Generic.List[string]]::new()
function msstore {
    $global:faxeStoreTeststoreCalls.Add(($args -join ' '))
    $global:LASTEXITCODE = 0
    switch ($args[0]) {
        'apps' {
            if ($global:faxeStoreTestfailLookup) { $global:LASTEXITCODE = 1; return }
            $global:faxeStoreTestapp | ConvertTo-Json -Depth 10
        }
        'publish' { if ($global:faxeStoreTestfailPublish) { $global:LASTEXITCODE = 1 } }
    }
}
function Write-TestBundle {
    param([string]$Version = '1.2.3.0', [string]$Publisher = 'CN=fixture', [string]$Arm = 'arm64')
    $path = Join-Path $temporary 'FAXE.msixbundle'
    if (Test-Path $path) { Remove-Item $path }
    $zip = [IO.Compression.ZipFile]::Open($path, 'Create')
    try {
        $entry = $zip.CreateEntry('AppxMetadata/AppxBundleManifest.xml')
        $writer = [IO.StreamWriter]::new($entry.Open())
        try {
            $writer.Write("<Bundle><Identity Name='Oblique.FAXEFAXoverSIP' Publisher='$Publisher' Version='$Version'/><Packages><Package Type='application' Architecture='x64'/><Package Type='application' Architecture='$Arm'/></Packages></Bundle>")
        } finally { $writer.Dispose() }
    } finally { $zip.Dispose() }
}
function Test-Submission {
    param([string]$Failure = '', [switch]$CheckOnly, [switch]$ExpectPublish)
    $global:faxeStoreTeststoreCalls.Clear()
    $caught = $null
    try {
        & "$PSScriptRoot/publish-microsoft-store.ps1" -PackagesDirectory $temporary -CheckOnly:$CheckOnly
    } catch { $caught = $_.Exception.Message }
    if ($Failure) {
        if (-not $caught -or $caught -notlike "*$Failure*") { throw "Expected '$Failure', got '$caught'." }
    } elseif ($caught) { throw $caught }
    $published = @($global:faxeStoreTeststoreCalls | Where-Object { $_ -like 'publish *' }).Count
    if ($published -ne [int]$ExpectPublish.IsPresent) { throw "Unexpected publish calls: $published" }
}
try {
    $env:PRODUCT_ID = '9PK6QF9MMNGJ'
    $env:GITHUB_REF = 'refs/tags/v1.2.3'
    $env:GITHUB_STEP_SUMMARY = ''
    $global:faxeStoreTestapp = @{
        id = $env:PRODUCT_ID
        packageIdentityName = 'Oblique.FAXEFAXoverSIP'
        publisherName = 'CN=fixture'
        lastPublishedApplicationSubmission = @{ id = 'published' }
        pendingApplicationSubmission = $null
    }
    Write-TestBundle
    Test-Submission -ExpectPublish
    $global:faxeStoreTestapp.pendingApplicationSubmission = @{ id = 'in-certification' }
    Test-Submission -Failure 'already exists'
    Test-Submission -CheckOnly
    $global:faxeStoreTestapp.pendingApplicationSubmission = $null
    $global:faxeStoreTestapp.lastPublishedApplicationSubmission = $null
    Test-Submission -Failure 'first Store submission'
    $global:faxeStoreTestapp.lastPublishedApplicationSubmission = @{ id = 'published' }
    $global:faxeStoreTestfailLookup = $true
    Test-Submission -Failure 'command failed'
    $global:faxeStoreTestfailLookup = $false
    $env:GITHUB_REF = 'refs/heads/master'
    Test-Submission -Failure 'release tag'
    $env:GITHUB_REF = 'refs/tags/v1.2.3'
    Write-TestBundle -Version '1.2.2.0'
    Test-Submission -Failure 'does not match'
    Write-TestBundle -Publisher 'CN=wrong'
    Test-Submission -Failure 'does not match'
    Write-TestBundle -Arm 'x64'
    Test-Submission -Failure 'both ARM64 and x64'
    Write-TestBundle
    Copy-Item (Join-Path $temporary 'FAXE.msixbundle') (Join-Path $temporary 'extra.msixbundle')
    Test-Submission -Failure 'exactly one'
    Remove-Item (Join-Path $temporary 'extra.msixbundle')
    $global:faxeStoreTestfailPublish = $true
    Test-Submission -Failure 'Store submission failed' -ExpectPublish
    $env:PRODUCT_ID = 'wrong'
    Test-Submission -Failure 'PRODUCT_ID must'
    Write-Host 'Passed 12 Store submission checks (offline).'
} finally {
    Remove-Item -LiteralPath $temporary -Recurse -Force
    foreach ($name in $savedEnvironment.Keys) {
        [Environment]::SetEnvironmentVariable($name, $savedEnvironment[$name])
    }
    Remove-Item Function:msstore
    Remove-Variable -Scope Global -Name faxeStoreTest*
}
