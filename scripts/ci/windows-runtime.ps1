function Get-CargoOutputDirectory {
    param(
        [Parameter(Mandatory)][object[]]$Messages,
        [Parameter(Mandatory)][string]$PackageId
    )
    $directories = @($Messages | Where-Object {
        $_.reason -eq 'build-script-executed' -and $_.package_id -eq $PackageId
    } | ForEach-Object { $_.out_dir } | Sort-Object -Unique)
    if ($directories.Count -ne 1) {
        throw "Expected one build output for $PackageId, found $($directories.Count)"
    }
    $directories[0]
}

function Get-WindowsImports([string]$Binary) {
    # Includes both normal and delay-load import tables, for x64 and ARM64.
    $output = & llvm-readobj.exe --coff-imports $Binary
    if ($LASTEXITCODE -ne 0) { throw "Cannot inspect PE imports: $Binary" }
    foreach ($line in $output) {
        if ($line -match '^\s+Name: (.+\.dll)\s*$') { $Matches[1].Trim() }
    }
}

function Copy-WindowsRuntime {
    param(
        [Parameter(Mandatory)][string[]]$Binaries,
        [Parameter(Mandatory)][string[]]$SearchDirectories,
        [Parameter(Mandatory)][string]$Destination
    )

    # Only OS-provided libraries may remain external. Do not resolve against the
    # runner's PATH/System32: it may have runtimes absent on a clean user machine.
    $systemDlls = @(
        'advapi32.dll', 'bcrypt.dll', 'bcryptprimitives.dll', 'combase.dll',
        'comctl32.dll', 'crypt32.dll', 'dwmapi.dll', 'gdi32.dll', 'imm32.dll',
        'kernel32.dll', 'ntdll.dll', 'ole32.dll', 'oleaut32.dll', 'opengl32.dll',
        'shell32.dll', 'user32.dll', 'uxtheme.dll', 'ws2_32.dll'
    )
    $available = @{}
    foreach ($directory in $SearchDirectories) {
        foreach ($file in Get-ChildItem -LiteralPath $directory -Filter '*.dll' -File) {
            if ($available.ContainsKey($file.Name) -and $available[$file.Name] -ne $file.FullName) {
                throw "Ambiguous runtime DLL: $($file.Name)"
            }
            $available[$file.Name] = $file.FullName
        }
    }
    $pending = [System.Collections.Generic.Queue[string]]::new()
    foreach ($binary in $Binaries) { $pending.Enqueue((Resolve-Path -LiteralPath $binary).Path) }
    $copied = @{}
    New-Item -ItemType Directory -Force $Destination | Out-Null
    while ($pending.Count) {
        $binary = $pending.Dequeue()
        $name = Split-Path $binary -Leaf
        if ($copied.ContainsKey($name)) {
            if ($copied[$name] -ne $binary) { throw "Conflicting runtime binary: $name" }
            continue
        }
        $copied[$name] = $binary
        Copy-Item -LiteralPath $binary -Destination $Destination
        foreach ($dependency in Get-WindowsImports $binary) {
            if ($dependency -match '^(api-ms-win-|ext-ms-win-)' -or $dependency -in $systemDlls) {
                continue
            }
            if (-not $available.ContainsKey($dependency)) {
                throw "Unresolved runtime DLL '$dependency' imported by '$name'"
            }
            $pending.Enqueue($available[$dependency])
        }
    }
    Write-Host "Packaged runtime: $(($copied.Keys | Sort-Object) -join ', ')"
}
