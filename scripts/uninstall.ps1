#Requires -Version 5.1
# Windows counterpart of uninstall.sh.
$ErrorActionPreference = 'Stop'

$Root = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$HomeDir = if ($env:HOME) { $env:HOME } elseif ($env:USERPROFILE) { $env:USERPROFILE } else { $HOME }
$BinDir = Join-Path $HomeDir '.local\bin'
$Shim = Join-Path $BinDir 'rematch.exe'
$ReleaseBin = Join-Path $Root 'target\release\rematch.exe'

# Only run this checkout's binary. Another rematch.exe earlier on PATH may manage a
# different installation and must not be asked to uninstall this one.
if (-not (Test-Path -LiteralPath $ReleaseBin)) {
    throw "Cannot find $ReleaseBin. Rebuild this checkout before uninstalling; no files were removed."
}
& $ReleaseBin uninstall --yes
if ($LASTEXITCODE -ne 0) { throw "rematch uninstall failed ($LASTEXITCODE); the plugin and shim were left in place." }

if (Get-Command omp -ErrorAction SilentlyContinue) {
    & omp plugin uninstall '@vncp/rematch' --scope user
    if ($LASTEXITCODE -ne 0) { & omp plugin uninstall (Join-Path $Root 'extension') --scope user }
    if ($LASTEXITCODE -ne 0) { throw "omp plugin uninstall failed ($LASTEXITCODE); remove the plugin registration before deleting its shim." }
}

$item = Get-Item -LiteralPath $Shim -Force -ErrorAction SilentlyContinue
if ($item) {
    $target = if ($item.LinkType) { @($item.Target)[0] } else { $null }
    if ($target -and [IO.Path]::GetFullPath($target) -eq [IO.Path]::GetFullPath($ReleaseBin)) {
        $item.Delete()
        Write-Host "==> Removed $Shim"
    } else {
        Write-Warning "Left $Shim untouched: it is not the link installed by this checkout."
    }
}

$UserPath = [Environment]::GetEnvironmentVariable('Path', 'User')
$Entries = @($UserPath -split ';' | Where-Object { $_ })
if (($Entries -contains $BinDir) -and -not (Get-ChildItem -LiteralPath $BinDir -Force -ErrorAction SilentlyContinue)) {
    [Environment]::SetEnvironmentVariable('Path', (($Entries | Where-Object { $_ -ne $BinDir }) -join ';'), 'User')
    Write-Host "==> Removed the empty $BinDir from your user PATH"
}
