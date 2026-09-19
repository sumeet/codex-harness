$ErrorActionPreference = 'Stop'

$harnessProfile = if ($env:HARNESS_PROFILE) { $env:HARNESS_PROFILE } else { 'release-fast' }
$cargoProfile = switch ($harnessProfile) {
    'dev' { 'harness-dev' }
    'release-fast' { 'release-fast' }
    default { throw 'HARNESS_PROFILE must be dev or release-fast' }
}
$jobs = if ($env:HARNESS_BUILD_JOBS) { $env:HARNESS_BUILD_JOBS } else { '2' }
if ($jobs -notmatch '^[1-9][0-9]*$') {
    throw 'HARNESS_BUILD_JOBS must be a positive integer'
}
if ((Get-CimInstance Win32_OperatingSystem).FreePhysicalMemory -lt 4 * 1024 * 1024) {
    throw 'Close memory-heavy applications: building requires at least 4 GiB of available memory.'
}

$env:Path = [Environment]::GetEnvironmentVariable('Path', 'User') + ';' + $env:Path
$vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
if (!(Test-Path -LiteralPath $vswhere)) {
    throw 'Install Visual Studio Build Tools with the Desktop development with C++ workload and a Windows SDK.'
}
$visualStudio = & $vswhere -latest -products '*' -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
if (!$visualStudio) {
    throw 'Install the Desktop development with C++ workload in Visual Studio Build Tools.'
}
Import-Module (Join-Path $visualStudio 'Common7\Tools\Microsoft.VisualStudio.DevShell.dll')
Enter-VsDevShell -VsInstallPath $visualStudio -SkipAutomaticLocation -DevCmdArguments '-arch=x64 -host_arch=x64' | Out-Null
if (!(Get-Command cmake -ErrorAction SilentlyContinue)) {
    throw 'Install CMake: winget install --id Kitware.CMake --exact --scope user'
}
if (!(Get-Command cargo -ErrorAction SilentlyContinue)) {
    throw 'Install Rust through rustup, then reopen PowerShell.'
}

Push-Location (Split-Path -Parent $PSScriptRoot)
try {
    & cargo build -p harness_app --bin harness --profile $cargoProfile --jobs $jobs
    if ($LASTEXITCODE -ne 0) {
        throw "Harness build failed (exit code $LASTEXITCODE)."
    }
} finally {
    Pop-Location
}
