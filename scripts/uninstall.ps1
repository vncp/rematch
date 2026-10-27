#Requires -Version 5.1
# Windows counterpart of uninstall.sh.
$ErrorActionPreference = 'Continue'

$Root = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$BinDir = Join-Path $HOME '.local\bin'
$Shim = Join-Path $BinDir 'rematch.exe'
$ReleaseBin = Join-Path $Root 'target\release\rematch.exe'

$OnPath = Get-Command rematch.exe -ErrorAction SilentlyContinue
if ($OnPath) { & $OnPath.Source uninstall --yes }
elseif (Test-Path $Shim) { & $Shim uninstall --yes }
elseif (Test-Path $ReleaseBin) { & $ReleaseBin uninstall --yes }

if (Get-Command omp -ErrorAction SilentlyContinue) {
    & omp plugin uninstall '@vncp/rematch' --scope user
    if ($LASTEXITCODE -ne 0) { & omp plugin uninstall (Join-Path $Root 'extension') --scope user }
}

$item = Get-Item -LiteralPath $Shim -Force -ErrorAction SilentlyContinue
if ($item) {
    $item.Delete()
    Write-Host "==> Removed $Shim"
}

$UserPath = [Environment]::GetEnvironmentVariable('Path', 'User')
$Entries = @($UserPath -split ';' | Where-Object { $_ })
if (($Entries -contains $BinDir) -and -not (Get-ChildItem -LiteralPath $BinDir -Force -ErrorAction SilentlyContinue)) {
    [Environment]::SetEnvironmentVariable('Path', (($Entries | Where-Object { $_ -ne $BinDir }) -join ';'), 'User')
    Write-Host "==> Removed the empty $BinDir from your user PATH"
}
