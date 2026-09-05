[CmdletBinding()]
param([string]$Compiler, [switch]$SkipBuild)
$ErrorActionPreference = 'Stop'
$repoRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
Push-Location -LiteralPath $repoRoot
try {
    if (-not $SkipBuild) {
        cargo build --release --locked --package codex-taskbar
        if ($LASTEXITCODE -ne 0) { throw 'Release build failed' }
    }
    $metadata = cargo metadata --no-deps --format-version 1 | ConvertFrom-Json
    if ($LASTEXITCODE -ne 0) { throw 'Cargo metadata failed' }
    $version = ($metadata.packages | Where-Object name -eq 'codex-taskbar').version
    if (-not $Compiler) {
        $Compiler = @(
            (Join-Path ${env:ProgramFiles(x86)} 'Inno Setup 6\ISCC.exe'),
            (Join-Path $repoRoot 'dist\build-tools\inno\ISCC.exe')
        ) | Where-Object { Test-Path -LiteralPath $_ } | Select-Object -First 1
    }
    if (-not $Compiler) { throw 'Install Inno Setup 6 or specify -Compiler.' }
    & $Compiler "/DAppVersion=$version" (Join-Path $PSScriptRoot 'windows-installer.iss')
    if ($LASTEXITCODE -ne 0) { throw 'Installer compilation failed' }
    $installer = Join-Path $repoRoot "dist\codex-taskbar-$version-windows-x64-setup.exe"
    $digest = (Get-FileHash -LiteralPath $installer -Algorithm SHA256).Hash.ToLowerInvariant()
    "$digest *$([IO.Path]::GetFileName($installer))" | Set-Content -LiteralPath "$installer.sha256" -Encoding ascii
    Get-FileHash -LiteralPath $installer -Algorithm SHA256
} finally { Pop-Location }
