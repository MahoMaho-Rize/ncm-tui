# Install ncm-tui on Windows from a prebuilt zip.
#
#   irm https://mahomaho-rize.com/ncm-tui/install.ps1 | iex
#
# Optional environment:
#   NCM_TUI_VERSION       pin a version, e.g. 0.1.3
#   NCM_TUI_INSTALL_DIR   install directory (default: %LOCALAPPDATA%\ncm-tui)
#   NCM_TUI_BASE_URL      artifact base

$ErrorActionPreference = "Stop"
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12

$BaseUrl = if ($env:NCM_TUI_BASE_URL) { $env:NCM_TUI_BASE_URL.TrimEnd("/") } else { "https://mahomaho-rize.com/ncm-tui" }
$Version = $env:NCM_TUI_VERSION
$BinDir = if ($env:NCM_TUI_INSTALL_DIR) { $env:NCM_TUI_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA "ncm-tui" }

function Get-NcmTuiTarget {
    $arch = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString()
    switch ($arch) {
        "X64" { "x86_64-pc-windows-msvc" }
        "Arm64" { "x86_64-pc-windows-msvc" }
        default { throw "ncm-tui only publishes a 64-bit Windows build, not $arch" }
    }
}

if (-not $Version) {
    $Version = (Invoke-WebRequest -UseBasicParsing -Uri "$BaseUrl/latest").Content.Trim()
}
$Version = $Version.Trim()
if ($Version.StartsWith("v")) {
    $Version = $Version.Substring(1)
}

$Target = Get-NcmTuiTarget
$Archive = "ncm-tui-$Target.zip"
$Tmp = Join-Path ([System.IO.Path]::GetTempPath()) ("ncm-tui-" + [guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Path $Tmp | Out-Null
try {
    $ZipPath = Join-Path $Tmp $Archive
    Write-Host "installing ncm-tui $Version ($Target)"
    Invoke-WebRequest -UseBasicParsing -Uri "$BaseUrl/v$Version/$Archive" -OutFile $ZipPath
    $Sums = (Invoke-WebRequest -UseBasicParsing -Uri "$BaseUrl/v$Version/sha256sums.txt").Content
    $Expected = $null
    foreach ($Line in ($Sums -split "\r?\n")) {
        $Parts = $Line.Trim() -split "\s+", 2
        if ($Parts.Count -eq 2 -and $Parts[1] -eq $Archive) {
            $Expected = $Parts[0].ToLowerInvariant()
            break
        }
    }
    if (-not $Expected) {
        throw "no checksum for $Archive"
    }
    $Actual = (Get-FileHash -Algorithm SHA256 -Path $ZipPath).Hash.ToLowerInvariant()
    if ($Actual -ne $Expected) {
        throw "checksum mismatch for $Archive"
    }

    Expand-Archive -LiteralPath $ZipPath -DestinationPath $Tmp -Force
    $Exe = Join-Path $Tmp "ncm-tui.exe"
    if (-not (Test-Path -LiteralPath $Exe)) {
        throw "archive did not contain ncm-tui.exe"
    }

    New-Item -ItemType Directory -Path $BinDir -Force | Out-Null
    Copy-Item -Force -LiteralPath $Exe -Destination (Join-Path $BinDir "ncm-tui.exe")
    Write-Host "installed $(Join-Path $BinDir 'ncm-tui.exe')"

    $Normalized = $BinDir.TrimEnd("\")
    $OnPath = ($env:Path -split ";" | ForEach-Object { $_.TrimEnd("\") }) -contains $Normalized
    if (-not $OnPath) {
        Write-Host "add $BinDir to PATH, for example:"
        Write-Host "  [Environment]::SetEnvironmentVariable('Path', [Environment]::GetEnvironmentVariable('Path', 'User') + ';$BinDir', 'User')"
    }
}
finally {
    Remove-Item -Recurse -Force -LiteralPath $Tmp -ErrorAction SilentlyContinue
}
