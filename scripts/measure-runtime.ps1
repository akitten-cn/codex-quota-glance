<#
.SYNOPSIS
对 Codex Taskbar Release 进程进行可重复的资源稳定性采样。

.DESCRIPTION
默认启动完全使用固定假数据的 --visual-preview-idle，不读取真实 Codex/New API 数据；
三秒呼吸过渡结束后原生动画定时器停止，因此该模式代表静态空闲而不是持续执行。
脚本按固定间隔记录 Working Set、Private Bytes、CPU、线程、句柄、GDI 与 USER
对象，并同时生成 CSV 原始样本和 JSON 汇总。除非显式传入 -KeepRunning，脚本只
终止自己启动的精确 PID，不会查找或结束其他 codex-taskbar 实例。
#>

[CmdletBinding()]
param(
    [Parameter()]
    [string]$ExecutablePath,

    [Parameter()]
    [ValidateSet("visual-preview-idle", "visual-preview", "visual-preview-details", "visual-preview-strip")]
    [string]$Mode = "visual-preview-idle",

    [Parameter()]
    [ValidateRange(5, 86400)]
    [int]$DurationSeconds = 900,

    [Parameter()]
    [ValidateRange(1, 300)]
    [int]$IntervalSeconds = 5,

    [Parameter()]
    [string]$OutputDirectory,

    [Parameter()]
    [switch]$KeepRunning
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$repositoryRoot = Split-Path -Parent $PSScriptRoot
if ([string]::IsNullOrWhiteSpace($ExecutablePath)) {
    $ExecutablePath = Join-Path $repositoryRoot "target\release\codex-taskbar.exe"
}
$resolvedExecutable = (Resolve-Path -LiteralPath $ExecutablePath).Path

if ([string]::IsNullOrWhiteSpace($OutputDirectory)) {
    $OutputDirectory = Join-Path $repositoryRoot "artifacts\performance"
}
$null = New-Item -ItemType Directory -Path $OutputDirectory -Force
$resolvedOutputDirectory = (Resolve-Path -LiteralPath $OutputDirectory).Path

if (-not ("CodexTaskbar.Performance.NativeMethods" -as [type])) {
    Add-Type -TypeDefinition @"
using System;
using System.Runtime.InteropServices;

namespace CodexTaskbar.Performance
{
    public static class NativeMethods
    {
        [DllImport("user32.dll")]
        public static extern uint GetGuiResources(IntPtr processHandle, uint flags);
    }
}
"@
}

$argument = "--$Mode"
$timestamp = Get-Date -Format "yyyyMMdd-HHmmss"
$prefix = Join-Path $resolvedOutputDirectory "runtime-$Mode-$timestamp"
$csvPath = "$prefix.csv"
$summaryPath = "$prefix.summary.json"
$logicalProcessors = [Math]::Max(1, [Environment]::ProcessorCount)
$samples = [System.Collections.Generic.List[object]]::new()
$startedAt = [DateTimeOffset]::Now
$stopwatch = [System.Diagnostics.Stopwatch]::StartNew()
$process = $null
$previousCpuSeconds = 0.0
$previousElapsedSeconds = 0.0

try {
    $process = Start-Process -FilePath $resolvedExecutable -ArgumentList $argument -WindowStyle Hidden -PassThru
    Start-Sleep -Milliseconds 750

    while ($stopwatch.Elapsed.TotalSeconds -lt $DurationSeconds) {
        $process.Refresh()
        if ($process.HasExited) {
            throw "采样进程提前退出，退出码：$($process.ExitCode)"
        }

        $elapsedSeconds = $stopwatch.Elapsed.TotalSeconds
        $cpuSeconds = $process.TotalProcessorTime.TotalSeconds
        $elapsedDelta = $elapsedSeconds - $previousElapsedSeconds
        $cpuDelta = $cpuSeconds - $previousCpuSeconds
        $cpuPercent = if ($elapsedDelta -gt 0) {
            100.0 * $cpuDelta / $elapsedDelta / $logicalProcessors
        } else {
            0.0
        }

        $samples.Add([pscustomobject][ordered]@{
            timestamp                 = [DateTimeOffset]::Now.ToString("o")
            elapsed_seconds           = [Math]::Round($elapsedSeconds, 3)
            working_set_mib           = [Math]::Round($process.WorkingSet64 / 1MB, 3)
            private_bytes_mib         = [Math]::Round($process.PrivateMemorySize64 / 1MB, 3)
            virtual_memory_mib        = [Math]::Round($process.VirtualMemorySize64 / 1MB, 3)
            cpu_percent_normalized    = [Math]::Round($cpuPercent, 4)
            cpu_total_seconds         = [Math]::Round($cpuSeconds, 4)
            thread_count              = $process.Threads.Count
            handle_count              = $process.HandleCount
            gdi_object_count          = [CodexTaskbar.Performance.NativeMethods]::GetGuiResources($process.Handle, 0)
            user_object_count         = [CodexTaskbar.Performance.NativeMethods]::GetGuiResources($process.Handle, 1)
        })

        $previousCpuSeconds = $cpuSeconds
        $previousElapsedSeconds = $elapsedSeconds
        $remainingSeconds = $DurationSeconds - $stopwatch.Elapsed.TotalSeconds
        if ($remainingSeconds -le 0) {
            break
        }
        Start-Sleep -Seconds ([Math]::Min($IntervalSeconds, [Math]::Ceiling($remainingSeconds)))
    }
}
finally {
    $stopwatch.Stop()
    if ($null -ne $process -and -not $KeepRunning -and -not $process.HasExited) {
        Stop-Process -Id $process.Id -Force
        $process.WaitForExit()
    }
}

if ($samples.Count -eq 0) {
    throw "没有生成任何性能样本"
}

$samples | Export-Csv -LiteralPath $csvPath -NoTypeInformation -Encoding UTF8
$first = $samples[0]
$last = $samples[$samples.Count - 1]
$observedHours = [Math]::Max($last.elapsed_seconds / 3600.0, 1.0 / 3600.0)
$summary = [pscustomobject][ordered]@{
    schema_version             = 1
    executable                 = Split-Path -Leaf $resolvedExecutable
    mode                       = $Mode
    process_id                 = $process.Id
    started_at                 = $startedAt.ToString("o")
    duration_seconds_requested = $DurationSeconds
    duration_seconds_observed  = [Math]::Round($last.elapsed_seconds, 3)
    interval_seconds           = $IntervalSeconds
    sample_count               = $samples.Count
    logical_processors         = $logicalProcessors
    working_set_mib            = [pscustomobject][ordered]@{
        first = $first.working_set_mib
        last  = $last.working_set_mib
        min   = ($samples.working_set_mib | Measure-Object -Minimum).Minimum
        max   = ($samples.working_set_mib | Measure-Object -Maximum).Maximum
        delta = [Math]::Round($last.working_set_mib - $first.working_set_mib, 3)
        endpoint_growth_mib_per_hour = [Math]::Round(($last.working_set_mib - $first.working_set_mib) / $observedHours, 3)
    }
    private_bytes_mib          = [pscustomobject][ordered]@{
        first = $first.private_bytes_mib
        last  = $last.private_bytes_mib
        min   = ($samples.private_bytes_mib | Measure-Object -Minimum).Minimum
        max   = ($samples.private_bytes_mib | Measure-Object -Maximum).Maximum
        delta = [Math]::Round($last.private_bytes_mib - $first.private_bytes_mib, 3)
        endpoint_growth_mib_per_hour = [Math]::Round(($last.private_bytes_mib - $first.private_bytes_mib) / $observedHours, 3)
    }
    cpu_percent_normalized     = [pscustomobject][ordered]@{
        average = [Math]::Round(($samples.cpu_percent_normalized | Measure-Object -Average).Average, 4)
        maximum = ($samples.cpu_percent_normalized | Measure-Object -Maximum).Maximum
    }
    thread_count               = [pscustomobject][ordered]@{
        first = $first.thread_count
        last  = $last.thread_count
        max   = ($samples.thread_count | Measure-Object -Maximum).Maximum
        delta = $last.thread_count - $first.thread_count
    }
    handle_count               = [pscustomobject][ordered]@{
        first = $first.handle_count
        last  = $last.handle_count
        max   = ($samples.handle_count | Measure-Object -Maximum).Maximum
        delta = $last.handle_count - $first.handle_count
    }
    gdi_object_count           = [pscustomobject][ordered]@{
        first = $first.gdi_object_count
        last  = $last.gdi_object_count
        max   = ($samples.gdi_object_count | Measure-Object -Maximum).Maximum
        delta = $last.gdi_object_count - $first.gdi_object_count
    }
    user_object_count          = [pscustomobject][ordered]@{
        first = $first.user_object_count
        last  = $last.user_object_count
        max   = ($samples.user_object_count | Measure-Object -Maximum).Maximum
        delta = $last.user_object_count - $first.user_object_count
    }
    csv_path                   = $csvPath
}
$summary | ConvertTo-Json -Depth 4 | Set-Content -LiteralPath $summaryPath -Encoding UTF8

Write-Output "CSV=$csvPath"
Write-Output "SUMMARY=$summaryPath"
$summary | ConvertTo-Json -Depth 4
