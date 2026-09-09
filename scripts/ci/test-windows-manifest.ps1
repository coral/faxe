$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
. "$PSScriptRoot/windows-manifest.ps1"
$template = Get-Content "$PSScriptRoot/../../packaging/windows/AppxManifest.xml.in" -Raw
foreach ($architecture in @('x64', 'arm64')) {
    Assert-FaxeAppContainerManifest -Manifest ([xml]$template.Replace('@ARCHITECTURE@', $architecture))
}
$uap10 = 'http://schemas.microsoft.com/appx/manifest/uap/windows10/10'
foreach ($case in @('full-trust', 'restricted-capability', 'no-internet', 'no-lan', 'extra-full-trust-app')) {
    $manifest = [xml]$template
    switch ($case) {
        'full-trust' { $null = $manifest.Package.Applications.Application.SetAttribute('TrustLevel', $uap10, 'mediumIL') }
        'restricted-capability' {
            $capability = $manifest.CreateElement('rescap', 'Capability', 'http://schemas.microsoft.com/appx/manifest/foundation/windows10/restrictedcapabilities')
            $capability.SetAttribute('Name', 'runFullTrust')
            $null = $manifest.Package.Capabilities.AppendChild($capability)
        }
        'no-internet' {
            $node = $manifest.Package.Capabilities.Capability | Where-Object Name -eq 'internetClientServer'
            $null = $node.ParentNode.RemoveChild($node)
        }
        'no-lan' {
            $node = $manifest.Package.Capabilities.Capability | Where-Object Name -eq 'privateNetworkClientServer'
            $null = $node.ParentNode.RemoveChild($node)
        }
        'extra-full-trust-app' {
            $app = $manifest.Package.Applications.Application.CloneNode($true)
            $null = $app.SetAttribute('TrustLevel', $uap10, 'mediumIL')
            $null = $manifest.Package.Applications.AppendChild($app)
        }
    }
    $rejected = $false
    try { Assert-FaxeAppContainerManifest -Manifest $manifest } catch { $rejected = $true }
    if (-not $rejected) { throw "Manifest policy accepted $case" }
}
Write-Host 'AppContainer manifest checks passed for x64/arm64 and rejected all invalid configurations'
