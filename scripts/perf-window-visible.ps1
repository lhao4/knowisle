# 冷启动 K1：从 CreateProcess 到窗口首次可见（IsWindowVisible = true）。
#
# 为什么不用 CDP 那条路：那条必须给 WebView2 加 `--no-sandbox` 与
# `--remote-debugging-port`，两者都会改变浏览器的进程模型与启动路径，
# 测出来的不是用户实际走的路径。K1 要的正是「用户双击图标之后多久看见窗口」。
#
# 为什么忙等而不是 Start-Sleep：PowerShell 5.1 的 sleep 粒度约 15ms，
# 会把 10 轮的中位数糊成 15ms 的倍数。这里用 Stopwatch + 空转，
# 精度取决于 EnumWindows 本身。
#
# 用法：
#   powershell -NoProfile -ExecutionPolicy Bypass -File scripts/perf-window-visible.ps1 `
#     -Exe target/release/knowisle.exe -Rounds 10 -LimitMs 600
#
# 退出码：0 = 通过，1 = 超预算或无样本。
param(
  [Parameter(Mandatory = $true)][string]$Exe,
  [int]$Rounds = 10,
  [int]$LimitMs = 600,
  [int]$CooldownMs = 800
)

$ErrorActionPreference = 'Stop'

if (-not (Test-Path $Exe)) {
  Write-Output "找不到可执行文件: $Exe"
  exit 1
}
$Exe = (Resolve-Path $Exe).Path

Add-Type -Namespace W -Name U -MemberDefinition @'
[DllImport("user32.dll")] public static extern bool EnumWindows(EnumWindowsProc cb, IntPtr l);
public delegate bool EnumWindowsProc(IntPtr h, IntPtr l);
[DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
[DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
'@

function Wait-Visible([int]$targetPid, [int]$timeoutMs) {
  $script:seen = $false
  $cb = [W.U+EnumWindowsProc]{
    param($h, $l)
    $p = 0
    [void][W.U]::GetWindowThreadProcessId($h, [ref]$p)
    if ($p -eq $targetPid -and [W.U]::IsWindowVisible($h)) { $script:seen = $true }
    return $true
  }
  $sw = [Diagnostics.Stopwatch]::StartNew()
  while ($sw.ElapsedMilliseconds -lt $timeoutMs) {
    $script:seen = $false
    [void][W.U]::EnumWindows($cb, [IntPtr]::Zero)
    if ($script:seen) { return $sw.Elapsed.TotalMilliseconds }
  }
  return -1
}

# 先清掉残留实例，否则第一轮测到的可能是「已有进程」。
Get-Process knowisle -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Milliseconds 600

$samples = @()
for ($i = 1; $i -le $Rounds; $i++) {
  $sw = [Diagnostics.Stopwatch]::StartNew()
  $p = Start-Process -FilePath $Exe -PassThru
  $ms = Wait-Visible $p.Id 20000
  if ($ms -lt 0) { Write-Output "第 $i 轮 20s 内没等到窗口"; exit 1 }
  $samples += [math]::Round($ms, 1)
  Write-Output ("  轮 {0}: {1} ms" -f $i, $samples[-1])
  Stop-Process -Id $p.Id -Force -ErrorAction SilentlyContinue
  # WebView2 的子进程要单独清，否则下一轮会复用一个还在退出的浏览器进程。
  Get-Process msedgewebview2 -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
  Start-Sleep -Milliseconds $CooldownMs
}

$sorted = $samples | Sort-Object
$p50 = $sorted[[int][math]::Floor($sorted.Count * 0.5)]
$p95 = $sorted[[math]::Min($sorted.Count - 1, [int][math]::Floor($sorted.Count * 0.95))]

Write-Output ("K1 窗口首次可见  RAW " + ($samples -join ","))
Write-Output ("K1 P50 {0} ms / P95 {1} ms / MIN {2} / MAX {3}（预算 {4} ms）" -f $p50, $p95, $sorted[0], $sorted[-1], $LimitMs)

if ($p95 -gt $LimitMs) {
  Write-Output "K1 超预算"
  exit 1
}
Write-Output "K1 PASS"
exit 0
