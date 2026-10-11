param(
  [Parameter(Position = 0)]
  [ValidateSet("install", "uninstall", "help")]
  [string]$Command = "help",
  [string]$InstallPrefix = "$env:LOCALAPPDATA\codex-dual",
  [string]$SourceArchive = "",
  [string]$ExpectedSha256 = "",
  [ValidateSet("python", "rust")]
  [string]$Runtime = "python",
  [switch]$DryRun,
  [switch]$Upgrade,
  [string]$OperationId = "",
  [string]$ExpectedManifest = "",
  [switch]$Yes
)

$ErrorActionPreference = "Stop"
if ($Upgrade -and ($Runtime -ne "rust" -or $Command -ne "install")) { throw "-Upgrade requires install -Runtime rust." }
if (($OperationId -or $ExpectedManifest) -and ($Runtime -ne "rust" -or ($Command -ne "uninstall" -and -not $Upgrade))) {
  throw "-OperationId and -ExpectedManifest require Rust uninstall or upgrade."
}
if ($Runtime -eq "rust" -and $Command -ne "help" -and -not $PSBoundParameters.ContainsKey("InstallPrefix")) {
  throw "Rust installation requires an explicit -InstallPrefix for the native runtime."
}
$installer = Join-Path $PSScriptRoot "platforms\windows\installer\install.ps1"
if (-not (Test-Path -LiteralPath $installer)) {
  throw "Windows installer implementation not found: $installer"
}

& $installer -Command $Command -InstallPrefix $InstallPrefix -SourceArchive $SourceArchive -ExpectedSha256 $ExpectedSha256 -Runtime $Runtime -DryRun:$DryRun -Upgrade:$Upgrade -OperationId $OperationId -ExpectedManifest $ExpectedManifest -Yes:$Yes
exit $LASTEXITCODE
