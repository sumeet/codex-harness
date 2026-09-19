$ErrorActionPreference = 'Stop'

function Get-HarnessFlag([string]$Name) {
    $value = [string][Environment]::GetEnvironmentVariable($Name)
    switch -Regex ($value) {
        '^(1|true|yes)$' { return $true }
        '^(0|false|no)?$' { return $false }
        default { throw "$Name must be 0 or 1" }
    }
}

$skipBuild = Get-HarnessFlag 'HARNESS_SKIP_BUILD'
$foreground = Get-HarnessFlag 'HARNESS_FOREGROUND'
$harnessProfile = if ($env:HARNESS_PROFILE) { $env:HARNESS_PROFILE } else { 'release-fast' }
$outputDirectory = switch ($harnessProfile) {
    'dev' { 'harness-dev' }
    'release-fast' { 'release-fast' }
    default { throw 'HARNESS_PROFILE must be dev or release-fast' }
}
if (!$skipBuild) {
    & (Join-Path $PSScriptRoot 'build-standalone.ps1')
}

$projectDirectory = Split-Path -Parent $PSScriptRoot
$targetDirectory = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { 'target' }
if (![IO.Path]::IsPathRooted($targetDirectory)) {
    $targetDirectory = Join-Path $projectDirectory $targetDirectory
}
$binary = Join-Path $targetDirectory "$outputDirectory\harness.exe"
if (!(Test-Path -LiteralPath $binary)) {
    throw 'Harness has not been built. Run without HARNESS_SKIP_BUILD.'
}
$codexDirectory = Join-Path $env:LOCALAPPDATA 'Programs\OpenAI\Codex\bin'
if (Test-Path -LiteralPath (Join-Path $codexDirectory 'codex.exe')) {
    $env:Path = $codexDirectory + ';' + $env:Path
}
if ($foreground) {
    & $binary @args
    if ($LASTEXITCODE -ne 0) { throw "Harness exited with code $LASTEXITCODE" }
    return
}

$logDirectory = Join-Path $env:LOCALAPPDATA 'harness\logs'
New-Item -ItemType Directory -Path $logDirectory -Force | Out-Null
$logStamp = Get-Date -Format 'yyyyMMdd-HHmmss-fff'
$logFile = Join-Path $logDirectory "harness-$logStamp.log"
$startOptions = @{
    FilePath = $binary
    WorkingDirectory = $projectDirectory
    RedirectStandardOutput = (Join-Path $logDirectory "harness-$logStamp.stdout.log")
    RedirectStandardError = $logFile
    PassThru = $true
}
if ($args.Count) {
    # Start-Process joins arguments into a Windows command line, so preserve quotes and trailing backslashes.
    $startOptions.ArgumentList = ($args | ForEach-Object {
        '"' + ([string]$_ -replace '(\\*)"', '$1$1\"' -replace '(\\+)$', '$1$1') + '"'
    }) -join ' '
}
$process = Start-Process @startOptions
if ($process.WaitForExit(500)) {
    Get-Content -LiteralPath $logFile -Tail 20
    throw "Harness exited during startup (exit code $($process.ExitCode))."
}
Write-Output "Harness started (PID $($process.Id)). Log: $logFile"
