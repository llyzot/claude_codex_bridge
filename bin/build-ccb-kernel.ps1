param([switch]$Debug)
$ErrorActionPreference = 'Stop'
$repoRoot = Split-Path -Parent $PSScriptRoot
$manifest = Join-Path $repoRoot 'rust\Cargo.toml'
$cargoArgs = @('build', '--manifest-path', $manifest, '-p', 'ccb-kernel', '--locked')
$profile = 'release'
if ($Debug) { $profile = 'debug' } else { $cargoArgs += '--release' }
& cargo @cargoArgs
if ($LASTEXITCODE -ne 0) { throw 'CCB kernel build failed' }
$artifact = Join-Path $repoRoot "rust\target\$profile\ccb-kernel.exe"
if (-not (Test-Path -LiteralPath $artifact)) { throw "Missing kernel artifact: $artifact" }
Copy-Item -LiteralPath $artifact -Destination (Join-Path $PSScriptRoot 'ccb-kernel.exe') -Force
Copy-Item -LiteralPath (Join-Path $repoRoot "rust\target\$profile\ccb-native.exe") -Destination (Join-Path $PSScriptRoot 'ccb-native.exe') -Force
Write-Output 'Built ccb-kernel in the repository bin directory.'
