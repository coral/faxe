# Cargo's Windows DLL search path replaces PATH from .cargo/config.toml.
# Restore the native dependencies for cargo run and cargo test.
$ErrorActionPreference = 'Stop'
if ($env:FAXE_BUILD_PATH) {
    $env:PATH = "$env:FAXE_BUILD_PATH;$env:PATH"
}
& $args[0] @($args | Select-Object -Skip 1)
exit $LASTEXITCODE
