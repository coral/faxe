param(
    [string]$PackagesDirectory = 'store-package',
    [switch]$CheckOnly
)

$ErrorActionPreference = 'Stop'

function Invoke-StoreJson {
    param([string[]]$CommandArguments)
    $output = & msstore @CommandArguments
    if ($LASTEXITCODE -ne 0) { throw "Store $($CommandArguments[0]) command failed; check app access and the ms environment credentials." }
    return ($output -join "`n" | ConvertFrom-Json)
}

function Write-StoreSummary {
    param([string]$Message)
    Write-Host $Message
    if ($env:GITHUB_STEP_SUMMARY) { Add-Content -LiteralPath $env:GITHUB_STEP_SUMMARY -Value $Message }
}

if ($env:PRODUCT_ID -ne '9PK6QF9MMNGJ') {
    throw 'PRODUCT_ID must be the FAXE Store ID: 9PK6QF9MMNGJ.'
}

$app = Invoke-StoreJson @('apps', 'get', $env:PRODUCT_ID)
if ($app.id -ne $env:PRODUCT_ID -or $app.packageIdentityName -ne 'Oblique.FAXEFAXoverSIP') {
    throw 'The Store API returned an unexpected application identity.'
}
Write-StoreSummary 'Connected to FAXE in Partner Center.'

if ($CheckOnly) {
    if ($app.pendingApplicationSubmission.id -or $app.lastPublishedApplicationSubmission.id) {
        & msstore submission status $env:PRODUCT_ID
        if ($LASTEXITCODE -ne 0) { throw 'Could not retrieve Store submission status.' }
    }
    Write-StoreSummary 'Read-only check complete. No submission was changed.'
    return
}

# msstore publish can delete an existing draft. Refuse to touch one, including
# a submission currently in certification. Serialize CI callers in the workflow.
if (-not $app.lastPublishedApplicationSubmission.id) {
    throw 'The first Store submission must be published and live before CI can submit updates.'
}
if ($app.pendingApplicationSubmission) {
    throw 'A Store draft or submission already exists. Finish or resolve it in Partner Center, then rerun this job.'
}
if ($env:GITHUB_REF -notmatch '^refs/tags/v(\d+\.\d+\.\d+)$') {
    throw 'Store updates require a stable vX.Y.Z release tag.'
}
$expectedVersion = "$($Matches[1]).0"
$bundles = @(Get-ChildItem -LiteralPath $PackagesDirectory -Filter '*.msixbundle' -File)
if ($bundles.Count -ne 1 -or $bundles[0].Length -eq 0) {
    throw 'Expected exactly one nonempty MSIX bundle from this release run.'
}

$zip = [IO.Compression.ZipFile]::OpenRead($bundles[0].FullName)
try {
    $entry = $zip.GetEntry('AppxMetadata/AppxBundleManifest.xml')
    if (-not $entry) { throw 'Missing MSIX bundle manifest.' }
    $reader = [IO.StreamReader]::new($entry.Open())
    try { [xml]$manifest = $reader.ReadToEnd() } finally { $reader.Dispose() }
} finally { $zip.Dispose() }
$identity = $manifest.Bundle.Identity
if ($identity.Name -ne $app.packageIdentityName -or $identity.Publisher -ne $app.publisherName -or $identity.Version -ne $expectedVersion) {
    throw 'Bundle identity, publisher or version does not match the Store application and release tag.'
}
$architectures = @($manifest.Bundle.Packages.Package | Where-Object Type -eq 'application' | ForEach-Object Architecture | Sort-Object)
if (($architectures -join ',') -ne 'arm64,x64') { throw 'The bundle must contain both ARM64 and x64 applications.' }

# The CLI clones the last published listing, replaces packages, uploads, commits,
# and waits for commit processing (not the entire Microsoft certification).
& msstore publish $bundles[0].FullName --appId $env:PRODUCT_ID
if ($LASTEXITCODE -ne 0) {
    throw 'Store submission failed. Inspect Partner Center before retrying; an uploaded draft may remain.'
}
Write-StoreSummary "Submitted FAXE $expectedVersion to Microsoft. Certification and publication continue in Partner Center using the existing submission availability settings."
