param(
    [string]$ShortcutDirectory = [Environment]::GetFolderPath('Programs')
)

$ErrorActionPreference = 'Stop'

$projectDirectory = Split-Path -Parent $PSScriptRoot
$harnessProfile = if ($env:HARNESS_PROFILE) { $env:HARNESS_PROFILE } else { 'release-fast' }
$outputDirectory = switch ($harnessProfile) {
    'dev' { 'harness-dev' }
    'release-fast' { 'release-fast' }
    default { throw 'HARNESS_PROFILE must be dev or release-fast' }
}
$targetDirectory = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { 'target' }
if (![IO.Path]::IsPathRooted($targetDirectory)) {
    $targetDirectory = Join-Path $projectDirectory $targetDirectory
}
$binary = Join-Path $targetDirectory "$outputDirectory\harness.exe"
if (!(Test-Path -LiteralPath $binary -PathType Leaf)) {
    throw 'Build Harness first: .\script\build-standalone.ps1'
}
$binary = (Resolve-Path -LiteralPath $binary).ProviderPath
if ([string]::IsNullOrWhiteSpace($ShortcutDirectory)) {
    throw 'The current user does not have a Start menu Programs folder.'
}
New-Item -ItemType Directory -Path $ShortcutDirectory -Force | Out-Null
$shortcutPath = Join-Path (Resolve-Path -LiteralPath $ShortcutDirectory).ProviderPath 'Codex Harness.lnk'
$shell = New-Object -ComObject WScript.Shell
$shortcut = $shell.CreateShortcut($shortcutPath)
$shortcut.TargetPath = $binary
$shortcut.WorkingDirectory = $projectDirectory
$shortcut.Description = 'Open Codex Harness'
$shortcut.Save()

Write-Output "Shortcut created: $shortcutPath"
Write-Output 'When installed in the Start menu, press the Windows key and search for Harness.'
