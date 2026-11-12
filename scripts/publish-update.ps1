# Publish the release binaries of local-history as a self-update distribution
# (plan-single-app §3.5).
# Run: powershell -ExecutionPolicy Bypass -File scripts\publish-update.ps1 -Source D:\dist\lh
#      -Source    bắt buộc — thư mục phân phối (dir nội bộ hoặc UNC share); URL http(s) không hợp lệ ở đây.
#      -BuildDir  tùy chọn — thư mục chứa binary release (mặc định target\release của repo).
#      -Only      tùy chọn — subset của lh.exe, lh-gui.exe, lh-daemon.exe (mặc định cả 3).
# Việc script làm: copy các exe → $Source, tính sha256 từng file (Get-FileHash, lowercase),
# lấy version từ `lh.exe --version`, ghi manifest.json {"version", "artifacts": [{name, sha256}]}.
# Manifest đa artifact → updater cập nhật mọi exe đang tồn tại cạnh exe (plan §3.3 bước 6-8);
# nếu chỉ publish lh.exe (-Only) thì manifest chỉ liệt kê lh.exe — kịch bản cài 1 file vẫn hợp lệ.

param(
    [Parameter(Mandatory = $true)]
    [string]$Source,
    [string]$BuildDir = "target\release",
    [string[]]$Only
)

$ErrorActionPreference = "Stop"

$repo = Split-Path -Parent $PSScriptRoot
Set-Location $repo

# All publishable binaries, in canonical order; -Only picks a subset of these.
$allArtifacts = @("lh.exe", "lh-gui.exe", "lh-daemon.exe")

if ($Only) {
    $bad = @($Only | Where-Object { $allArtifacts -notcontains $_ })
    if ($bad.Count -gt 0) {
        Write-Error ("-Only chỉ nhận: " + ($allArtifacts -join ", ") + " — nhận được: " + ($bad -join ", "))
        exit 1
    }
    $artifacts = @($allArtifacts | Where-Object { $Only -contains $_ })
} else {
    $artifacts = $allArtifacts
}

# Build dir: absolute, or relative to the repo root (so the script runs from any cwd).
if (-not [IO.Path]::IsPathRooted($BuildDir)) {
    $BuildDir = Join-Path $repo $BuildDir
}
if (-not (Test-Path $BuildDir -PathType Container)) {
    Write-Error "BuildDir không tồn tại: $BuildDir"
    exit 1
}

# Version comes from the built lh.exe itself — the same value the updater
# compares against the running version (env!(CARGO_PKG_VERSION)).
$lhExe = Join-Path $BuildDir "lh.exe"
if (-not (Test-Path $lhExe)) {
    Write-Error "thiếu $lhExe — build release trước: cargo build --release"
    exit 1
}
$verLine = (& $lhExe --version).Trim()          # vd: "lh 0.1.0"
$ver = ($verLine -split '\s+')[-1]
if (-not $ver -match '^\d+\.\d+\.\d+') {
    Write-Error "không đọc được version từ '$verLine' ($lhExe --version)"
    exit 1
}

# Missing source files fail BEFORE anything is copied or written — the target
# dir is never left half-published.
foreach ($name in $artifacts) {
    if (-not (Test-Path (Join-Path $BuildDir $name))) {
        Write-Error "thiếu $BuildDir\$name — build release trước: cargo build --release"
        exit 1
    }
}

if (-not (Test-Path $Source)) {
    New-Item -ItemType Directory -Path $Source -Force | Out-Null
}

# Copy + hash the PUBLISHED copy (the bytes the updater will download and verify).
$rows = @()
foreach ($name in $artifacts) {
    Copy-Item -Path (Join-Path $BuildDir $name) -Destination (Join-Path $Source $name) -Force
    $sha = (Get-FileHash -Path (Join-Path $Source $name) -Algorithm SHA256).Hash.ToLower()
    $rows += [PSCustomObject]@{ name = $name; sha256 = $sha }
}

$manifest = [PSCustomObject]@{
    version   = $ver
    artifacts = $rows
}

# BOM-less UTF-8: serde_json (updater) rejects a UTF-8 BOM on manifest.json.
$manifestPath = Join-Path $Source "manifest.json"
$json = ConvertTo-Json -InputObject $manifest -Depth 4
[IO.File]::WriteAllText($manifestPath, $json + "`n", (New-Object System.Text.UTF8Encoding($false)))

Write-Host ("published " + $artifacts.Count + " artifact(s), version " + $ver + " → " + $Source)
foreach ($row in $rows) {
    Write-Host ("  " + $row.name + "  " + $row.sha256)
}
Write-Host ("  manifest: " + $manifestPath)
exit 0
