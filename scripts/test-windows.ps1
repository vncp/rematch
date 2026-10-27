#Requires -Version 7.0
# Native Windows acceptance. Everything except build artifacts and the checkout's ignored
# extension/agents link lives under an isolated, space-containing temporary user directory.
$ErrorActionPreference = 'Stop'
$Root = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$Sandbox = Join-Path ([IO.Path]::GetTempPath()) ('rematch windows ' + [guid]::NewGuid().ToString('N'))
$HomeDir = Join-Path $Sandbox 'User home'
$FixtureBin = Join-Path $Sandbox 'Fixture bin'
$OriginalLocation = Get-Location
$Variables = @('HOME', 'USERPROFILE', 'REMATCH_ROOT', 'PI_CODING_AGENT_DIR', 'OMP_PROFILE', 'PI_PROFILE', 'PI_CONFIG_DIR', 'OMP_BIN', 'OMP_PLUGINS_DIR', 'REMATCH', 'Path', 'GIT_AUTHOR_NAME', 'GIT_AUTHOR_EMAIL', 'GIT_COMMITTER_NAME', 'GIT_COMMITTER_EMAIL')
$Saved = @{}
foreach ($v in $Variables) { $Saved[$v] = [Environment]::GetEnvironmentVariable($v, 'Process') }
$UserPath = [Environment]::GetEnvironmentVariable('Path', 'User')
$AgentLink = Join-Path $Root 'extension\agents'
if (Test-Path -LiteralPath $AgentLink) { throw 'Run this smoke from a clean checkout: extension/agents already exists.' }

function Assert-True([bool]$Condition, [string]$Message) {
    if (-not $Condition) { throw "ASSERT: $Message" }
    Write-Host "ok: $Message"
}
function Invoke-Rematch([string[]]$Arguments) {
    $output = & $script:Rematch @Arguments
    if ($LASTEXITCODE -ne 0) { throw "rematch $Arguments failed ($LASTEXITCODE): $output" }
    $result = ($output -join "`n") | ConvertFrom-Json
    if (-not $result.ok) { throw "rematch $Arguments refused: $output" }
    return $result.data
}

try {
    New-Item -ItemType Directory -Force -Path $HomeDir, $FixtureBin | Out-Null
    $env:HOME = $HomeDir
    $env:USERPROFILE = $HomeDir
    $env:REMATCH_ROOT = Join-Path $HomeDir '.rematch'
    $env:PI_CODING_AGENT_DIR = Join-Path $HomeDir '.omp\agent'
    $env:OMP_PROFILE = 'default'
    foreach ($v in @('PI_PROFILE', 'PI_CONFIG_DIR', 'OMP_PLUGINS_DIR', 'REMATCH')) { [Environment]::SetEnvironmentVariable($v, $null, 'Process') }
    $env:GIT_AUTHOR_NAME = $env:GIT_COMMITTER_NAME = 'Rematch Windows test'
    $env:GIT_AUTHOR_EMAIL = $env:GIT_COMMITTER_EMAIL = 'windows-test@example.invalid'
    New-Item -ItemType Directory -Force -Path $env:PI_CODING_AGENT_DIR | Out-Null
    Set-Content -Path (Join-Path $env:PI_CODING_AGENT_DIR 'config.yml') -Value "modelRoles:`n  default: test/model`n  slow: test/model`n  smol: test/model" -Encoding utf8

    # A script-backed omp launcher is intentional: Windows users commonly have npm .cmd shims.
    # Only plugin management and --version are simulated; rematch, git, links and the OS are real.
    $FakePs = Join-Path $FixtureBin 'fake-omp.ps1'
    Set-Content -Path $FakePs -Encoding utf8 -Value @'
param([Parameter(ValueFromRemainingArguments=$true)][string[]]$Argv)
$ErrorActionPreference = 'Stop'
if ($Argv[0] -eq '--version') { Write-Output 'omp/18.12.6-windows-fixture'; exit 0 }
if ($Argv[0] -ne 'plugin') { throw "Unexpected omp call: $Argv" }
$plugins = Join-Path $env:HOME '.omp\plugins'
$module = Join-Path $plugins 'node_modules\@vncp\rematch'
if ($Argv[1] -eq 'link') {
    New-Item -ItemType Directory -Force -Path (Split-Path $module) | Out-Null
    New-Item -ItemType SymbolicLink -Path $module -Target $Argv[2] | Out-Null
    Set-Content -Path (Join-Path $plugins 'omp-plugins.lock.json') -Value '{"plugins":{"@vncp/rematch":{"version":"0.1.0","enabled":true}}}'
    exit 0
}
if ($Argv[1] -eq 'uninstall') {
    $item = Get-Item -LiteralPath $module -Force -ErrorAction SilentlyContinue
    if ($item) { $item.Delete() }
    Set-Content -Path (Join-Path $plugins 'omp-plugins.lock.json') -Value '{"plugins":{}}'
    exit 0
}
throw "Unexpected omp plugin action: $Argv"
'@
    $FakeCmd = Join-Path $FixtureBin 'omp.cmd'
    Set-Content -Path $FakeCmd -Encoding ascii -Value "@echo off`r`npwsh.exe -NoProfile -File `"$FakePs`" %*`r`nexit /b %errorlevel%"
    $env:OMP_BIN = $FakeCmd
    $env:Path = "$FixtureBin;$env:Path"

    & (Join-Path $Root 'scripts\install.ps1')
    $script:Rematch = Join-Path $HomeDir '.local\bin\rematch.exe'
    Assert-True (Test-Path -LiteralPath $Rematch) 'installer creates the executable link under the isolated home'
    $doctor = Invoke-Rematch @('doctor')
    Assert-True ($doctor.omp.version -like '*windows-fixture*') 'doctor launches the .cmd omp shim from a path containing spaces'
    $null = Invoke-Rematch @('off')
    $off = Invoke-Rematch @('match', '--cwd', $Sandbox)
    Assert-True ([bool]$off.off) 'off is honored by match'
    $null = Invoke-Rematch @('on')
    $null = Invoke-Rematch @('lake', 'migrate')
    $export = Invoke-Rematch @('lake', 'export', '--all')
    Assert-True (Test-Path (Join-Path $env:REMATCH_ROOT 'lake\manifest.json')) 'native CLI exports a lake manifest'

    $Manual = Join-Path $env:PI_CODING_AGENT_DIR 'skills\handwritten'
    New-Item -ItemType Directory -Force -Path $Manual | Out-Null
    Set-Content -Path (Join-Path $Manual 'SKILL.md') -Encoding utf8 -Value "---`nname: handwritten`ndescription: Use when checking the Windows profile.`n---`nA user-authored skill to preserve."
    $null = Invoke-Rematch @('profile', 'init', '--adopt')
    $profileSkill = Join-Path $env:REMATCH_ROOT 'profile\skills\handwritten\SKILL.md'
    Assert-True (Test-Path -LiteralPath $profileSkill) 'profile adopt retains the user skill'
    $null = Invoke-Rematch @('skills', 'sync', '--cwd', $Sandbox)
    $null = Invoke-Rematch @('skills', 'sync', '--cwd', $Sandbox)
    Assert-True (Test-Path (Join-Path $env:REMATCH_ROOT 'skills\handwritten\SKILL.md')) 'repeated shared-skill sync resolves profile links'

    $sentinel = Join-Path $HomeDir '.local\bin\unrelated.txt'
    Set-Content -Path $sentinel -Value 'not managed by rematch'
    & (Join-Path $Root 'scripts\uninstall.ps1')
    Assert-True (-not (Test-Path -LiteralPath $Rematch)) 'uninstaller removes its shim'
    Assert-True (Test-Path -LiteralPath $sentinel) 'uninstaller preserves an unrelated file'
    Assert-True (Test-Path -LiteralPath $profileSkill) 'uninstaller preserves the user profile repository'
    Write-Host 'Native Windows acceptance passed.'
} finally {
    Set-Location $OriginalLocation
    $link = Get-Item -LiteralPath $AgentLink -Force -ErrorAction SilentlyContinue
    if ($link -and $link.LinkType) { $link.Delete() }
    foreach ($v in $Variables) { [Environment]::SetEnvironmentVariable($v, $Saved[$v], 'Process') }
    [Environment]::SetEnvironmentVariable('Path', $UserPath, 'User')
    Remove-Item -LiteralPath $Sandbox -Recurse -Force -ErrorAction SilentlyContinue
}
