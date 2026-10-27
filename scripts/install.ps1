#Requires -Version 5.1
# Windows counterpart of install.sh. rematch's library is made of symlinks, which Windows
# lets a normal user create only with Developer Mode on (Settings > System > For developers).
$ErrorActionPreference = 'Stop'

$Root = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
Set-Location $Root

# The cargo feature `lake` (default on) links arrow/parquet for `rematch lake ...` (ADR 001);
# nothing in a session uses it. REMATCH_NO_LAKE=1 builds without it.
$Features = @()
if ($env:REMATCH_NO_LAKE -eq '1') { $Features = @('--no-default-features') }
Write-Host "==> Building rematch (release$(if ($Features) { ', ' + ($Features -join ' ') }))..."
$Locked = @()
if (Test-Path (Join-Path $Root 'Cargo.lock')) { $Locked = @('--locked') }
& cargo build --release @Locked @Features
if ($LASTEXITCODE -ne 0) { throw "cargo build failed ($LASTEXITCODE)" }
$Bin = Join-Path $Root 'target\release\rematch.exe'

# mklink creates unprivileged symlinks in Developer Mode (PowerShell 5.1's New-Item does not).
# A single -ArgumentList string reaches cmd verbatim, so quoted paths survive both 5.1 and 7.
function New-Symlink([string]$Link, [string]$Target, [switch]$Directory) {
    $flag = if ($Directory) { '/D ' } else { '' }
    $p = Start-Process -FilePath cmd.exe -ArgumentList "/c mklink $flag`"$Link`" `"$Target`" >NUL" -NoNewWindow -Wait -PassThru
    if ($p.ExitCode -ne 0) {
        throw "cannot create symlink $Link -> $Target. Turn on Developer Mode (Settings > System > For developers) or run this script elevated."
    }
}

function Remove-Link([string]$Path) {
    $item = Get-Item -LiteralPath $Path -Force -ErrorAction SilentlyContinue
    if (-not $item) { return }
    if (-not $item.LinkType) { throw "$Path exists and is not a symlink; move it away and re-run" }
    # Removes the link only, never its target.
    $item.Delete()
}

# Provisional agents (kind `agent`) are symlinks in $REMATCH_ROOT\agents; omp discovers
# task agents in <plugin root>\agents, so the plugin root links there.
$RematchDir = if ($env:REMATCH_ROOT) { $env:REMATCH_ROOT } else { Join-Path $HOME '.rematch' }
New-Item -ItemType Directory -Force -Path (Join-Path $RematchDir 'agents') | Out-Null
$AgentsLink = Join-Path $Root 'extension\agents'
Remove-Link $AgentsLink
New-Symlink $AgentsLink (Join-Path $RematchDir 'agents') -Directory
Write-Host "==> Linked $AgentsLink -> $RematchDir\agents (provisional agents)"

Write-Host "==> Linking omp plugin from $Root\extension..."
& omp plugin link (Join-Path $Root 'extension') --scope user
if ($LASTEXITCODE -ne 0) { throw "omp plugin link failed ($LASTEXITCODE)" }

# The shim: a symlink to the release binary, so rebuilds need no re-install.
$BinDir = Join-Path $HOME '.local\bin'
$Shim = Join-Path $BinDir 'rematch.exe'
New-Item -ItemType Directory -Force -Path $BinDir | Out-Null
Remove-Link $Shim
New-Symlink $Shim $Bin
Write-Host "==> Installed shim at $Shim"

$UserPath = [Environment]::GetEnvironmentVariable('Path', 'User')
$Entries = @($UserPath -split ';' | Where-Object { $_ })
if ($Entries -notcontains $BinDir) {
    [Environment]::SetEnvironmentVariable('Path', (($Entries + $BinDir) -join ';'), 'User')
    Write-Host "==> Added $BinDir to your user PATH (new shells pick it up)"
}
$env:Path = "$BinDir;$env:Path"

Write-Host '==> Running rematch doctor...'
& $Shim doctor --text

Write-Host @'

==> Optional: configure dedicated model roles in ~/.omp/agent/config.yml:

modelRoles:
  rematch-editor: <provider>/<strongest-model-id>
  rematch-classifier: <provider>/<fast-model-id>

(If omitted, rematch-editor falls back to slow -> default, and rematch-classifier falls back to smol -> tiny -> default.)
'@
