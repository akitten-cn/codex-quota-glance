# Codex Taskbar Windows x64 Portable packaging script.
#
# Only the repository dist directory and its explicit staging child are touched.
# The script requires a working cargo toolchain.

[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'

function Get-FullPath([string] $Path) {
    return [System.IO.Path]::GetFullPath($Path)
}

function Assert-UnderDist([string] $Path, [string] $DistRoot, [string] $Description) {
    $fullPath = Get-FullPath $Path
    $prefix = $DistRoot.TrimEnd([System.IO.Path]::DirectorySeparatorChar, [System.IO.Path]::AltDirectorySeparatorChar) + [System.IO.Path]::DirectorySeparatorChar
    if (-not $fullPath.StartsWith($prefix, [System.StringComparison]::OrdinalIgnoreCase)) {
        throw "$Description is outside the repository dist directory: $fullPath"
    }
    return $fullPath
}

function Assert-NotLink([string] $Path, [string] $Description) {
    if (Test-Path -LiteralPath $Path) {
        $item = Get-Item -LiteralPath $Path -Force
        if ($item.LinkType) {
            throw "$Description is a link; refusing to avoid an unsafe path: $Path"
        }
    }
}

function Invoke-CargoBuild([string] $RepoRoot) {
    Push-Location -LiteralPath $RepoRoot
    try {
        & cargo build --release --package codex-taskbar
        if ($LASTEXITCODE -ne 0) {
            throw "cargo build --release failed with exit code: $LASTEXITCODE"
        }
    }
    finally {
        Pop-Location
    }
}

$repoRoot = Get-FullPath (Join-Path $PSScriptRoot '..')
$repoManifest = Join-Path $repoRoot 'Cargo.toml'
$appManifest = Join-Path $repoRoot 'apps\codex-taskbar\Cargo.toml'
if (-not (Test-Path -LiteralPath $repoManifest -PathType Leaf)) {
    throw "workspace Cargo.toml not found: $repoManifest"
}
if (-not (Test-Path -LiteralPath $appManifest -PathType Leaf)) {
    throw "codex-taskbar package Cargo.toml not found: $appManifest"
}

# workspace.package is the version source; also require the target package to use it.
$workspaceManifestText = Get-Content -LiteralPath $repoManifest -Raw
$workspaceVersionMatch = [System.Text.RegularExpressions.Regex]::Match(
    $workspaceManifestText,
    '(?ms)^\[workspace\.package\].*?^version\s*=\s*"([^"]+)"'
)
if (-not $workspaceVersionMatch.Success) {
    throw 'Could not read a version from [workspace.package].'
}
$version = $workspaceVersionMatch.Groups[1].Value
$appManifestText = Get-Content -LiteralPath $appManifest -Raw
if ($appManifestText -notmatch '(?m)^name\s*=\s*"codex-taskbar"') {
    throw "Target package is not named codex-taskbar: $appManifest"
}
if ($appManifestText -notmatch '(?m)^version\.workspace\s*=\s*true') {
    throw 'codex-taskbar does not use the workspace version; refusing to guess.'
}

$distRoot = Get-FullPath (Join-Path $repoRoot 'dist')
New-Item -ItemType Directory -Path $distRoot -Force | Out-Null
$resolvedDistRoot = (Resolve-Path -LiteralPath $distRoot).Path
Assert-NotLink $distRoot 'dist directory'
if (-not $resolvedDistRoot.StartsWith($repoRoot.TrimEnd('\') + '\', [System.StringComparison]::OrdinalIgnoreCase)) {
    throw "Resolved dist directory is outside the repository: $resolvedDistRoot"
}

$stagingName = ".staging-codex-taskbar-$version"
$stagingRoot = Join-Path $resolvedDistRoot $stagingName
$zipName = "codex-taskbar-$version-windows-x64-portable.zip"
$zipPath = Join-Path $resolvedDistRoot $zipName
$releaseExeName = "codex-taskbar-$version-windows-x64.exe"
$releaseExePath = Join-Path $resolvedDistRoot $releaseExeName
$checksumsPath = Join-Path $resolvedDistRoot 'SHA256SUMS.txt'

# Resolve and validate every path before delete/overwrite; staging is the only recursive delete.
$safeStagingRoot = Assert-UnderDist $stagingRoot $resolvedDistRoot 'staging directory'
$safeZipPath = Assert-UnderDist $zipPath $resolvedDistRoot 'Portable ZIP'
$safeReleaseExePath = Assert-UnderDist $releaseExePath $resolvedDistRoot 'Release executable'
$safeChecksumsPath = Assert-UnderDist $checksumsPath $resolvedDistRoot 'SHA256SUMS.txt'
Assert-NotLink $stagingRoot 'staging directory'
Assert-NotLink $zipPath 'Portable ZIP'
Assert-NotLink $releaseExePath 'Release executable'
Assert-NotLink $checksumsPath 'SHA256SUMS.txt'

$exePath = Join-Path $repoRoot 'target\release\codex-taskbar.exe'
$usageFileName = [string]::Concat([char]0x4F7F, [char]0x7528, [char]0x8BF4, [char]0x660E, '.txt')
$usagePath = Join-Path $stagingRoot $usageFileName

try {
    Invoke-CargoBuild $repoRoot
    if (-not (Test-Path -LiteralPath $exePath -PathType Leaf)) {
        throw "Release build completed but executable was not found: $exePath"
    }

    if (Test-Path -LiteralPath $safeStagingRoot) {
        Remove-Item -LiteralPath $safeStagingRoot -Recurse -Force
    }
    New-Item -ItemType Directory -Path $safeStagingRoot -Force | Out-Null

    Copy-Item -LiteralPath $exePath -Destination (Join-Path $safeStagingRoot 'codex-taskbar.exe') -Force
    Copy-Item -LiteralPath $exePath -Destination $safeReleaseExePath -Force
    # Keep this source file ASCII-only for Windows PowerShell 5.1. The UTF-8
    # Base64 payload decodes to the Chinese user guide without parser encoding risks.
    $usageTextUtf8Base64 = 'Q29kZXggVGFza2JhciBQb3J0YWJsZSDkvb/nlKjor7TmmI4KCumAgueUqOeOr+WigwotIFdpbmRvd3MgMTAvMTEgeDY044CCCi0g5L6/5pC65YyF5LiN6ZyA6KaB5a6J6KOF5Zmo77yb6K+35bCG5pW05LiqIFpJUCDop6PljovliLDkvaDmnInlhpnlhaXmnYPpmZDnmoTnm67lvZXjgIIKCummluasoei/kOihjAoxLiDlj4zlh7sgY29kZXgtdGFza2Jhci5leGXjgIIKMi4g56iL5bqP5Lya5Zyo6YCa55+l5Yy65Z+f5pi+56S65Zu+5qCH77yb5aaC6KKrIFdpbmRvd3Mg5oqY5Y+g77yM6K+35bGV5byA5omY55uY5Yy65Z+f44CCCjMuIOWPs+mUruaJmOebmOWbvuagh+aJk+W8gOiuvue9ru+8jOmAieaLqeS7u+WKoeagj+S9jee9ruOAgeaYvuekuuWZqOWSjOaYvuekuumhueebruOAggoK5pWw5o2u55uu5b2VCi0g6K6+572u44CB5pel5b+X5ZKM5Yet5o2u55Sx56iL5bqP5oyJ5b2T5YmN55So5oi355qEIExvY2FsQXBwRGF0YSDop4TliJnkv53lrZjvvIzkuI3mlL7lnKjmnKwgWklQIOWGheOAggotIOS+v+aQuuWMheacrOi6q+S4jeWMheWQqyBzZXR0aW5nc+OAgWxvZ3PjgIHlh63mja7miJYgdGFyZ2V0IOebruW9leS4reeahOWFtuS7luaWh+S7tuOAggoK5Y246L295pa55byPCjEuIOWPs+mUruaJmOebmOWbvuagh+W5tumAgOWHuueoi+W6j+OAggoyLiDliKDpmaTop6Pljovlh7rnmoTmlbTkuKrkvr/mkLrnm67lvZXljbPlj6/jgIIKMy4g5aaC6ZyA5ZCM5pe25riF55CG5Liq5Lq66K6+572u5ZKM5pel5b+X77yM6K+35Zyo6YCA5Ye65ZCO5oyJ56iL5bqP5pWw5o2u55uu5b2V6K+05piO5omL5Yqo5Yig6Zmk5a+55bqU55qEIENvZGV4VGFza2JhciDmlbDmja7nm67lvZXjgIIKCui0ueeUqOivtOaYjgotIOWumOaWuei0puaIt+ivpuaDheS4reeahOi0ueeUqOaYryBDb2RleCBBcHAgU2VydmVyIOaPkOS+m+eahOWumOaWueS8sOeul++8jOS7heS+m+WPguiAg++8jOS4jeaYr+WunumZheiuoumYhei0puWNleOAggotIE5ldyBBUEkg6YeR6aKd5L2/55So6K6+572u5Lit55qEIHF1b3RhX3VuaXRzX3Blcl9jbnkg5omL5Yqo5oqY566X77yM5LiN5Luj6KGo5a6Y5pa5IEFQSSDotKbljZXjgII='
    $usageBytes = [Convert]::FromBase64String($usageTextUtf8Base64)
    [System.IO.File]::WriteAllBytes($usagePath, $usageBytes)

    if (Test-Path -LiteralPath $safeZipPath) {
        Remove-Item -LiteralPath $safeZipPath -Force
    }
    Compress-Archive -LiteralPath @(
        (Join-Path $safeStagingRoot 'codex-taskbar.exe'),
        $usagePath
    ) -DestinationPath $safeZipPath -CompressionLevel Optimal -Force

    $zipHash = (Get-FileHash -LiteralPath $safeZipPath -Algorithm SHA256).Hash.ToLowerInvariant()
    $exeHash = (Get-FileHash -LiteralPath $safeReleaseExePath -Algorithm SHA256).Hash.ToLowerInvariant()
    $checksumText = "$exeHash *$releaseExeName`r`n$zipHash *$zipName`r`n"
    [System.IO.File]::WriteAllText($safeChecksumsPath, $checksumText, [System.Text.UTF8Encoding]::new($false))
}
finally {
    # Revalidate before cleanup so future edits cannot redirect Remove-Item outside dist.
    if (Test-Path -LiteralPath $safeStagingRoot) {
        $verifiedStaging = Assert-UnderDist $safeStagingRoot $resolvedDistRoot 'staging cleanup directory'
        Remove-Item -LiteralPath $verifiedStaging -Recurse -Force
    }
}

$zipInfo = Get-Item -LiteralPath $safeZipPath
$finalHash = (Get-FileHash -LiteralPath $safeZipPath -Algorithm SHA256).Hash.ToLowerInvariant()
Write-Host "Created: $safeZipPath"
Write-Host "Updater asset: $safeReleaseExePath"
Write-Host "Size: $($zipInfo.Length) bytes"
Write-Host "SHA256: $finalHash"
Write-Host "Checksums: $safeChecksumsPath"
