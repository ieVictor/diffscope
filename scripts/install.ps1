<#
.SYNOPSIS
Installs diffscope for Windows x86_64 from GitHub Releases.

.DESCRIPTION
Downloads the release archive from https://github.com/ieVictor/diffscope/releases,
checks it against the published SHA-256 digest, and only then extracts and
installs it under the per-user install directory. On success the installer runs
"diffscope setup" unless -NoSetup is given.

.PARAMETER Version
Release tag to install, for example v0.2.0 (a leading "v" is optional).
Defaults to the latest release.

.PARAMETER InstallDir
Absolute directory that receives diffscope.exe.
Defaults to %LOCALAPPDATA%\Programs\diffscope\bin.

.PARAMETER NoSetup
Install the binary without running "diffscope setup".

.EXAMPLE
irm https://raw.githubusercontent.com/ieVictor/diffscope/master/scripts/install.ps1 | iex

.EXAMPLE
.\install.ps1 -Version v0.2.0 -NoSetup
#>
#Requires -Version 5.1
[CmdletBinding()]
param(
    [Parameter()][string]$Version,
    [Parameter()][string]$InstallDir,
    [Parameter()][switch]$NoSetup,
    [Parameter()][switch]$Help
)

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

$Repository = 'ieVictor/diffscope'
$LatestReleaseUrl = "https://api.github.com/repos/$Repository/releases/latest"
$ReleaseDownloadUrl = "https://github.com/$Repository/releases/download"

function Write-Usage {
    @'
Install diffscope for Windows x86_64 from GitHub Releases.

Usage: install.ps1 [options]

Options:
  -Version <tag>      Release tag to install, for example v0.2.0 (a leading
                      "v" is optional). Defaults to the latest release.
  -InstallDir <dir>   Absolute directory that receives diffscope.exe.
                      Defaults to %LOCALAPPDATA%\Programs\diffscope\bin.
  -NoSetup            Install the binary without running "diffscope setup".
  -Help               Print this message.

Supported platform: Windows x86_64 (MSVC). On Linux or macOS use
scripts/install.sh instead.
'@
}

function Stop-Installation {
    param([Parameter(Mandatory)][string]$Message)
    [Console]::Error.WriteLine("diffscope installer: $Message")
    exit 1
}

function Get-LatestTag {
    try {
        $response = Invoke-WebRequest -Uri $LatestReleaseUrl -UseBasicParsing
        $content = $response.Content
        # PowerShell 7 hands back a byte array when the response omits an
        # application/json content type, for example through a proxy.
        if ($content -is [byte[]]) {
            $content = [System.Text.Encoding]::UTF8.GetString($content)
        }
        $release = $content | ConvertFrom-Json
    } catch {
        Stop-Installation "could not query $LatestReleaseUrl : $($_.Exception.Message); pass -Version <tag> to install a specific release"
    }

    if (-not $release.tag_name) {
        Stop-Installation "$LatestReleaseUrl did not report a release tag; pass -Version <tag> to install a specific release"
    }

    return $release.tag_name
}

function Save-ReleaseFile {
    param(
        [Parameter(Mandatory)][string]$Url,
        [Parameter(Mandatory)][string]$Destination
    )

    try {
        Invoke-WebRequest -Uri $Url -OutFile $Destination -UseBasicParsing
    } catch {
        Stop-Installation "could not download $Url : $($_.Exception.Message)"
    }
}

function Get-ExpectedDigest {
    param(
        [Parameter(Mandatory)][string]$SumsPath,
        [Parameter(Mandatory)][string]$Asset
    )

    $digest = $null
    $matchCount = 0
    $pattern = '^(?<digest>[0-9a-fA-F]{64})\s+\*?(?<name>\S.*?)\s*$'

    foreach ($line in Get-Content -LiteralPath $SumsPath) {
        if ($line -match $pattern -and $Matches['name'] -eq $Asset) {
            $matchCount += 1
            $digest = $Matches['digest']
        }
    }

    if ($matchCount -ne 1) {
        return $null
    }

    return $digest
}

function Add-UserPath {
    param([Parameter(Mandatory)][string]$Directory)

    $normalized = $Directory.TrimEnd('\')
    $current = [Environment]::GetEnvironmentVariable('Path', 'User')
    $entries = @()
    if ($current) {
        $entries = @($current -split ';' | Where-Object { $_ -ne '' })
    }

    foreach ($entry in $entries) {
        if ($entry.TrimEnd('\') -ieq $normalized) {
            return $false
        }
    }

    [Environment]::SetEnvironmentVariable('Path', ((@($entries) + $normalized) -join ';'), 'User')
    return $true
}

if ($Help) {
    Write-Usage
    exit 0
}

if ([System.Environment]::OSVersion.Platform -ne [System.PlatformID]::Win32NT) {
    Stop-Installation 'this script installs the Windows build; on Linux or macOS use scripts/install.sh instead'
}

$architecture = $env:PROCESSOR_ARCHITEW6432
if (-not $architecture) {
    $architecture = $env:PROCESSOR_ARCHITECTURE
}
if (-not $architecture) {
    $architecture = 'unknown'
}

# Map the host to one of the released target triples, or fail: an unsupported
# platform never falls back to a binary built for a different target.
if ($architecture -ne 'AMD64') {
    Stop-Installation "unsupported platform Windows $architecture; released targets: x86_64 Windows (MSVC), x86_64/aarch64 Linux (static musl), x86_64/aarch64 macOS"
}
$target = 'x86_64-pc-windows-msvc'

if (-not $InstallDir) {
    if (-not $env:LOCALAPPDATA) {
        Stop-Installation '-InstallDir is required when %LOCALAPPDATA% is not set'
    }
    $InstallDir = Join-Path $env:LOCALAPPDATA 'Programs\diffscope\bin'
}
if (-not [System.IO.Path]::IsPathRooted($InstallDir)) {
    Stop-Installation "-InstallDir must be an absolute path, got $InstallDir"
}

if ($Version) {
    $tag = $Version.Trim()
    if (-not $tag.StartsWith('v')) {
        $tag = "v$tag"
    }
    if ($tag -notmatch '^v\d') {
        Stop-Installation "invalid version $Version; expected a release tag such as v0.2.0"
    }
} else {
    $tag = Get-LatestTag
}

$asset = "diffscope-$tag-$target.zip"
$releaseUrl = "$ReleaseDownloadUrl/$tag"

$work = Join-Path ([System.IO.Path]::GetTempPath()) ('diffscope-' + [System.Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Force -Path $work | Out-Null

try {
    $archivePath = Join-Path $work $asset
    $sumsPath = Join-Path $work 'SHA256SUMS'

    Write-Host "downloading $asset"
    Save-ReleaseFile -Url "$releaseUrl/$asset" -Destination $archivePath
    Save-ReleaseFile -Url "$releaseUrl/SHA256SUMS" -Destination $sumsPath

    $expectedDigest = Get-ExpectedDigest -SumsPath $sumsPath -Asset $asset
    if (-not $expectedDigest) {
        Stop-Installation "$asset is not listed exactly once in SHA256SUMS; refusing to install"
    }

    $actualDigest = (Get-FileHash -LiteralPath $archivePath -Algorithm SHA256).Hash
    if ($actualDigest.ToLowerInvariant() -ne $expectedDigest.ToLowerInvariant()) {
        Stop-Installation "checksum mismatch for $asset; refusing to install`nexpected $($expectedDigest.ToLowerInvariant())`nactual   $($actualDigest.ToLowerInvariant())"
    }

    $unpacked = Join-Path $work 'unpacked'
    New-Item -ItemType Directory -Force -Path $unpacked | Out-Null
    try {
        Expand-Archive -LiteralPath $archivePath -DestinationPath $unpacked -Force
    } catch {
        Stop-Installation "could not extract $asset : $($_.Exception.Message)"
    }

    # The release archives hold the binary at the archive root; anything else
    # means the download and this installer disagree, so stop instead of guessing.
    $sourceBinary = Join-Path $unpacked 'diffscope.exe'
    if (-not (Test-Path -LiteralPath $sourceBinary -PathType Leaf)) {
        Stop-Installation "$asset does not contain diffscope.exe at the archive root"
    }

    if (-not (Test-Path -LiteralPath $InstallDir -PathType Container)) {
        try {
            New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
        } catch {
            Stop-Installation "could not create ${InstallDir}: $($_.Exception.Message)"
        }
    }

    $destination = Join-Path $InstallDir 'diffscope.exe'
    try {
        Copy-Item -LiteralPath $sourceBinary -Destination $destination -Force
    } catch {
        Stop-Installation "could not install $asset into ${InstallDir}: $($_.Exception.Message)"
    }

    if (Add-UserPath -Directory $InstallDir) {
        Write-Host "added $InstallDir to the user PATH"
    }
    $env:Path = "$env:Path;$InstallDir"

    $reportedVersion = & $destination --version
    if ($LASTEXITCODE -ne 0) {
        Stop-Installation "$destination failed to run after installation"
    }
    Write-Host "installed $destination ($reportedVersion)"

    if ($NoSetup) {
        Write-Host "skipped harness configuration (-NoSetup); run `"$destination setup`" when ready"
    } else {
        Write-Host "configuring coding harnesses with `"$destination setup`""
        & $destination setup
        if ($LASTEXITCODE -ne 0) {
            Stop-Installation "$destination is installed, but `"diffscope setup`" failed; rerun `"$destination setup`" to retry"
        }
    }
} finally {
    Remove-Item -LiteralPath $work -Recurse -Force -ErrorAction SilentlyContinue
}
