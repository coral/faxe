function Assert-FaxeAppContainerManifest {
    param([Parameter(Mandatory)][xml]$Manifest)
    $foundation = 'http://schemas.microsoft.com/appx/manifest/foundation/windows10'
    $uap10 = 'http://schemas.microsoft.com/appx/manifest/uap/windows10/10'
    $ns = [System.Xml.XmlNamespaceManager]::new($Manifest.NameTable)
    $ns.AddNamespace('m', $foundation)
    $applications = @($Manifest.SelectNodes('/m:Package/m:Applications/m:Application', $ns))
    if ($applications.Count -eq 0) { throw 'MSIX has no applications' }
    foreach ($app in $applications) {
        if ($app.GetAttribute('TrustLevel', $uap10) -cne 'appContainer' -or
            $app.GetAttribute('RuntimeBehavior', $uap10) -cne 'packagedClassicApp') {
            throw 'Every MSIX application must use packagedClassicApp / appContainer'
        }
    }
    $capabilities = @($Manifest.SelectNodes('/m:Package/m:Capabilities/*', $ns))
    $names = @($capabilities | ForEach-Object { $_.GetAttribute('Name') } | Sort-Object)
    if (($names -join ',') -cne 'internetClientServer,privateNetworkClientServer' -or
        @($capabilities | Where-Object { $_.NamespaceURI -cne $foundation -or $_.LocalName -cne 'Capability' }).Count) {
        throw 'MSIX must declare only Internet and private-network client/server capabilities'
    }
    if ($Manifest.SelectNodes('//*[@Category="windows.fullTrustProcess"]').Count) {
        throw 'MSIX must not contain a full-trust process extension'
    }
}
