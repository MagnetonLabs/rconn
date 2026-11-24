#Requires -RunAsAdministrator
# Remove the rconn host: stop and delete the service, its files and its settings/identity.
$ErrorActionPreference = 'Stop'
$dest = Join-Path $env:ProgramFiles 'rconn'
if (Get-Service rconn-host -ErrorAction SilentlyContinue) { & (Join-Path $dest 'rconn-host-service.exe') uninstall }
Get-Process rconn-host-agent -ErrorAction SilentlyContinue | Stop-Process -Force
Remove-Item -Recurse -Force $dest, (Join-Path $env:ProgramData 'rconn') -ErrorAction SilentlyContinue
Write-Host 'rconn host removed.'
