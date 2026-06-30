<#
Forge installer for Windows. Downloads the prebuilt binary from GitHub Releases.

  irm https://raw.githubusercontent.com/Adulari/forge/main/install.ps1 | iex

Overrides (set as environment variables before running):
  FORGE_VERSION      tag to install (default: latest release)
  FORGE_INSTALL_DIR  where to put forge.exe (default: %LOCALAPPDATA%\Programs\forge)
#>

$ErrorActionPreference = 'Stop'
# PowerShell 5.1 defaults to TLS 1.0; GitHub requires 1.2+.
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12

$Repo = 'Adulari/forge'
$InstallDir = if ($env:FORGE_INSTALL_DIR) { $env:FORGE_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA 'Programs\forge' }

function Die($msg) { Write-Error "install: $msg"; exit 1 }

# Only an x86_64 Windows binary is published.
$arch = $env:PROCESSOR_ARCHITECTURE
if ($arch -ne 'AMD64') {
    Die "unsupported Windows arch: $arch (prebuilt binary: x86_64/AMD64). Build from source: cargo install --git https://github.com/$Repo forge-cli"
}
$target = 'x86_64-pc-windows-msvc'

$headers = @{ 'User-Agent' = 'forge-installer' }

# Resolve the version to install (latest release unless FORGE_VERSION is set).
$version = $env:FORGE_VERSION
if (-not $version) {
    try {
        $rel = Invoke-RestMethod -Uri "https://api.github.com/repos/$Repo/releases/latest" -Headers $headers
        $version = $rel.tag_name
    } catch { Die "could not resolve latest release tag: $($_.Exception.Message)" }
    if (-not $version) { Die 'could not resolve latest release tag' }
}

$asset = "forge-$target.zip"
$base  = "https://github.com/$Repo/releases/download/$version"
$tmp   = Join-Path ([System.IO.Path]::GetTempPath()) ("forge-install-" + [System.Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Force -Path $tmp | Out-Null

try {
    Write-Host "install: downloading $asset $version..."
    $zip = Join-Path $tmp $asset
    try { Invoke-WebRequest -Uri "$base/$asset" -OutFile $zip -Headers $headers }
    catch { Die "download failed: $base/$asset ($($_.Exception.Message))" }

    # Verify against checksums.txt if present (best-effort).
    try {
        $sums = Invoke-WebRequest -Uri "$base/checksums.txt" -Headers $headers -UseBasicParsing
        $line = ($sums.Content -split "`n" | Where-Object { $_ -match [regex]::Escape($asset) } | Select-Object -First 1)
        if ($line) {
            $want = ($line -split '\s+')[0].ToLower()
            $got  = (Get-FileHash -Path $zip -Algorithm SHA256).Hash.ToLower()
            if ($want -ne $got) { Die "checksum mismatch for $asset" }
            Write-Host 'install: checksum ok'
        }
    } catch { Write-Host 'install: checksums.txt unavailable, skipping verification' }

    Expand-Archive -Path $zip -DestinationPath $tmp -Force
    $exe = Join-Path $tmp "forge-$target\forge.exe"
    if (-not (Test-Path $exe)) { Die "archive did not contain forge.exe" }

    # Note the currently-installed version (if any) to report update-vs-fresh. This script only ever
    # writes the binary below — it never touches your config (%APPDATA%\forge) or sessions/API keys
    # (%LOCALAPPDATA%\forge\data), so re-running it to update or reinstall preserves all settings.
    $dest = Join-Path $InstallDir 'forge.exe'
    $prev = $null
    if (Test-Path $dest) {
        try { $prev = (& $dest --version 2>$null | Select-Object -First 1).Split(' ')[-1] } catch { $prev = $null }
    }

    New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
    Copy-Item -Path $exe -Destination $dest -Force
    if ($prev) {
        Write-Host "install: forge $version -> $dest (was $prev; your config and sessions are preserved)"
    } else {
        Write-Host "install: forge $version -> $dest"
    }

    # Add to the user PATH if it isn't already there.
    $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    $onPath = ($userPath -split ';') -contains $InstallDir
    if (-not $onPath) {
        $newPath = if ([string]::IsNullOrEmpty($userPath)) { $InstallDir } else { "$userPath;$InstallDir" }
        [Environment]::SetEnvironmentVariable('Path', $newPath, 'User')
        $env:Path = "$env:Path;$InstallDir"  # current session, so `forge` works immediately
        Write-Host "install: added $InstallDir to your user PATH (restart other shells to pick it up)"
    }
    Write-Host 'install: done. Run `forge setup` to get started.'
}
finally {
    Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
}
