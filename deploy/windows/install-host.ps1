#Requires -RunAsAdministrator
<#
  Install the rconn host on this Windows machine (service + agent), configured for your server.

  .\install-host.ps1 -Server wss://rconn.example.com:443/v1/ws -Fingerprint sha256:... -IdentityDir .\host1

  -IdentityDir is the folder made by `rconn-signaling enroll-host` on your server.
  -Fingerprint is the server pin printed by the server installer (omit for a public certificate).
#>
param(
  [Parameter(Mandatory = $true)][string]$Server,
  [string]$Fingerprint,
  [Parameter(Mandatory = $true)][string]$IdentityDir
)
$ErrorActionPreference = 'Stop'
$dest = Join-Path $env:ProgramFiles 'rconn'
$exe = Join-Path $dest 'rconn-host-service.exe'

if (Get-Service rconn-host -ErrorAction SilentlyContinue) {
  Write-Host 'Removing the previous rconn host service...'
  & $exe uninstall
}
New-Item -ItemType Directory -Force $dest | Out-Null
Copy-Item (Join-Path $PSScriptRoot 'host\*') $dest -Force

$cfg = @('configure', '--server', $Server, '--identity-from', (Resolve-Path $IdentityDir).Path)
if ($Fingerprint) { $cfg += @('--fingerprint', $Fingerprint) }
& $exe @cfg
if ($LASTEXITCODE -ne 0) { throw 'configure failed' }
& $exe install
if ($LASTEXITCODE -ne 0) { throw 'service install failed' }

Write-Host ''
Write-Host "rconn host installed in $dest and running as the 'rconn-host' service."
Write-Host "Settings: $env:ProgramData\rconn\host.toml   Logs: $env:ProgramData\rconn\*.log"
