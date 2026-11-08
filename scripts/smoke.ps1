# Smoke end-to-end cho local-history M3 (daemon + CLI).
# Chạy: powershell -ExecutionPolicy Bypass -File scripts\smoke.ps1
# Bước: build -> temp store/ws -> daemon nền -> tạo/sửa file 3 lần -> history -> diff ->
#       show -> revert -> gc --dry-run -> dọn dẹp.
# Mỗi bước in PASS/FAIL; exit 0 khi tất cả pass, ngược lại in bước fail + exit 1.

$ErrorActionPreference = "Stop"
$repo = Split-Path -Parent $PSScriptRoot
Set-Location $repo

# env build/toolchain (shell mới không giữ env cũ — set lại như hướng dẫn dự án)
$env:CARGO_HOME  = "C:\Users\maiquocthinh\scoop\persist\rustup-msvc\.cargo"
$env:RUSTUP_HOME = "C:\Users\maiquocthinh\scoop\persist\rustup-msvc\.rustup"
$env:PATH = "$env:PATH;C:\Users\maiquocthinh\scoop\persist\rustup-msvc\.cargo\bin"

$cliExe    = Join-Path $repo "target\debug\lh.exe"
$daemonExe = Join-Path $repo "target\debug\lh-daemon.exe"

$script:failures = @()
function Step-Pass([string]$name) { Write-Host ("  PASS  " + $name) -ForegroundColor Green }
function Step-Fail([string]$name, [string]$detail) {
    Write-Host ("  FAIL  " + $name) -ForegroundColor Red
    if ($detail) { Write-Host ("        " + ($detail -replace "`r?`n", " | ")) }
    $script:failures += $name
}
function Check([string]$name, [bool]$ok, [string]$detail = "") {
    if ($ok) { Step-Pass $name } else { Step-Fail $name $detail }
}

# ---- [1/9] build cli + daemon ----
Write-Host "== [1/9] cargo build -p cli -p daemon =="
cargo build -p cli -p daemon
if ($LASTEXITCODE -ne 0) {
    Step-Fail "build cli+daemon" ("cargo exit " + $LASTEXITCODE)
    exit 1
}
if (-not (Test-Path $cliExe) -or -not (Test-Path $daemonExe)) {
    Step-Fail "build cli+daemon" "thiếu target\debug\lh.exe hoặc lh-daemon.exe"
    exit 1
}
Step-Pass "build cli+daemon"

# ---- [2/9] temp store + temp workspace (qua env LOCAL_HISTORY_DIR) ----
Write-Host "== [2/9] tạo temp store + workspace =="
$tmp = [IO.Path]::GetTempPath()
$storeDir = Join-Path $tmp ("lh-smoke-store-" + [guid]::NewGuid().ToString("N").Substring(0, 8))
$wsDir    = Join-Path $tmp ("lh-smoke-ws-"    + [guid]::NewGuid().ToString("N").Substring(0, 8))
New-Item -ItemType Directory -Path $storeDir, $wsDir | Out-Null
$env:LOCAL_HISTORY_DIR = $storeDir
Write-Host ("  store: " + $storeDir)
Write-Host ("  ws   : " + $wsDir)
Step-Pass "temp dirs + $env:LOCAL_HISTORY_DIR"

$daemon = $null
try {
    # ---- [3/9] start daemon nền ----
    Write-Host "== [3/9] start lh-daemon (nền) =="
    $daemon = Start-Process -FilePath $daemonExe -ArgumentList @("--dir", "`"$wsDir`"") -PassThru -WindowStyle Hidden
    $statusFile = Join-Path $storeDir "daemon.json"
    $deadline = (Get-Date).AddSeconds(10)
    while (-not (Test-Path $statusFile) -and (Get-Date) -lt $deadline) { Start-Sleep -Milliseconds 200 }
    Check "daemon nền chạy (daemon.json xuất hiện)" ((Test-Path $statusFile) -and (-not $daemon.HasExited)) ("pid=" + $daemon.Id)

    # ---- [4/9] tạo + sửa file 3 lần (debounce 400ms -> sleep đủ giữa các lần) ----
    Write-Host "== [4/9] tạo + sửa file 3 lần =="
    $file = Join-Path $wsDir "notes.txt"
    $v1 = "dong 1"
    $v2 = "dong 1`ndong 2"
    $v3 = "dong 1 doi"
    foreach ($v in @($v1, $v2, $v3)) {
        Set-Content -Path $file -Value $v -NoNewline -Encoding Ascii
        Start-Sleep -Milliseconds 1300   # > debounce 400ms + collector tick 100ms
    }
    Step-Pass "ghi 3 lần (chờ debounce mỗi lần)"

    # ---- [5/9] lh history: >= 3 version External ----
    Write-Host "== [5/9] lh history + status + workspaces =="
    $histJson = & $cliExe --json history $file
    if ($LASTEXITCODE -ne 0) {
        Step-Fail "lh history --json" ("exit " + $LASTEXITCODE)
    } else {
        $hist = $histJson | ConvertFrom-Json
        $nExternal = @($hist.versions | Where-Object { $_.kind -eq "external" }).Count
        Check "history >= 3 version external" ($nExternal -ge 3) ("external=" + $nExternal + ", total=" + $hist.count)
    }
    $human = & $cliExe history $file
    Check "lh history (bảng) exit 0" ($LASTEXITCODE -eq 0) ("exit " + $LASTEXITCODE)
    Check "bảng có kind=external" (($human -join "`n") -match "external")

    # status + workspaces (e2e cho pid_alive qua tasklist + liệt kê ws qua engine)
    $stJson = & $cliExe --json status
    if ($LASTEXITCODE -ne 0) {
        Step-Fail "lh status --json" ("exit " + $LASTEXITCODE)
    } else {
        $st = $stJson | ConvertFrom-Json
        Check "status: daemon đang chạy (pid sống)" ($st.daemon.alive -eq $true) ("daemon=" + $stJson)
        Check "status: 1 workspace, >=3 version" ((@($st.workspaces).Count -eq 1) -and ($st.workspaces[0].versions -ge 3)) ("ws=" + ($st.workspaces | ConvertTo-Json -Compress))
    }
    $wsJson = & $cliExe --json workspaces
    if ($LASTEXITCODE -ne 0) {
        Step-Fail "lh workspaces --json" ("exit " + $LASTEXITCODE)
    } else {
        $wsList = @($wsJson | ConvertFrom-Json)
        Check "workspaces: đúng 1 ws" ($wsList.Count -eq 1) ("count=" + $wsList.Count)
    }

    # ---- [6/9] lh diff 1 3 ----
    Write-Host "== [6/9] lh diff =="
    $diffOut = & $cliExe diff $file 1 3
    $diffText = $diffOut -join "`n"
    $diffOk = ($LASTEXITCODE -eq 0) -and ($diffText -like "*@@*") -and ($diffText -like "*+dong 1 doi*")
    Check "diff seq 1..3 có unified diff (+/-)" $diffOk ("exit=" + $LASTEXITCODE + " out=" + $diffText)

    # ---- [7/9] lh show --at 1 (và latest) ----
    Write-Host "== [7/9] lh show =="
    $showOut = (& $cliExe show $file --at 1) -join "`n"
    Check "show --at 1 khớp nội dung lần ghi đầu" ($LASTEXITCODE -eq 0 -and $showOut.Trim() -ceq $v1) ("exit=" + $LASTEXITCODE + " nhận='" + $showOut + "'")
    $showLast = (& $cliExe show $file --at latest) -join "`n"
    Check "show --at latest khớp nội dung lần ghi cuối" ($LASTEXITCODE -eq 0 -and $showLast.Trim() -ceq $v3) ("nhận='" + $showLast + "'")

    # ---- [8/9] lh revert --yes --at 1 ----
    Write-Host "== [8/9] lh revert =="
    $revOut = & $cliExe revert $file --yes --at 1
    Check "revert --yes --at 1 exit 0" ($LASTEXITCODE -eq 0) ("exit=" + $LASTEXITCODE + " out=" + ($revOut -join "`n"))
    $disk = Get-Content -Path $file -Raw
    Check "file trên disk khớp v1 sau revert" ($disk -ceq $v1) ("nhận='" + $disk + "'")

    # ---- [9/9] lh gc --dry-run ----
    Write-Host "== [9/9] lh gc --dry-run =="
    $gcOut = & $cliExe gc --dry-run
    $gcText = $gcOut -join "`n"
    Check "gc --dry-run chạy không lỗi" ($LASTEXITCODE -eq 0 -and $gcText -like "*dry-run*") ("exit=" + $LASTEXITCODE)
    Write-Host (($gcOut | ForEach-Object { "        " + $_ }) -join "`n")
} finally {
    Write-Host "== dọn dẹp =="
    if ($daemon -and -not $daemon.HasExited) {
        Stop-Process -Id $daemon.Id -Force -ErrorAction SilentlyContinue
        Start-Sleep -Milliseconds 300   # chờ nhả lock trước khi xóa tempdir
    }
    foreach ($d in @($storeDir, $wsDir)) {
        if ($d -and (Test-Path $d)) { Remove-Item -Recurse -Force $d -ErrorAction SilentlyContinue }
    }
    Write-Host "  đã kill daemon + xóa temp dirs"
}

if ($script:failures.Count -gt 0) {
    Write-Host ("SMOKE FAIL — " + $script:failures.Count + " bước fail: " + ($script:failures -join ", ")) -ForegroundColor Red
    exit 1
}
Write-Host "SMOKE PASS — tất cả bước xanh" -ForegroundColor Green
exit 0
