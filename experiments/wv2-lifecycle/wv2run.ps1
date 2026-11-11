param([string]$Exe, [string]$Mode = 'destroy', [int]$Cycles = 3, [string]$Out)
# Starts the harness and samples every second: all descendant processes of the harness
# (the WebView2 tree), their count, working set and private bytes, plus the host's own.
$stdout = "$Out.stdout.txt"
$p = Start-Process -FilePath $Exe -ArgumentList $Mode, $Cycles -PassThru -RedirectStandardOutput $stdout -WindowStyle Normal
$t0 = Get-Date
"t,wv_count,wv_ws_mb,wv_priv_mb,host_ws_mb,host_priv_mb,wv_names" | Out-File $Out -Encoding utf8
while (-not $p.HasExited) {
    $all = Get-CimInstance Win32_Process -Property ProcessId, ParentProcessId, Name, WorkingSetSize, PrivatePageCount
    $kids = @{}; $frontier = @($p.Id)
    while ($frontier.Count) {
        $next = @()
        foreach ($f in $frontier) { foreach ($c in $all | Where-Object { $_.ParentProcessId -eq $f -and -not $kids.ContainsKey($_.ProcessId) }) { $kids[$c.ProcessId] = $c; $next += $c.ProcessId } }
        $frontier = $next
    }
    $host_ = $all | Where-Object { $_.ProcessId -eq $p.Id }
    $wv = @($kids.Values)
    $ws = ($wv | Measure-Object WorkingSetSize -Sum).Sum / 1MB
    $pr = ($wv | Measure-Object PrivatePageCount -Sum).Sum / 1MB
    $names = ($wv | Group-Object Name | ForEach-Object { "$($_.Name)x$($_.Count)" }) -join ' '
    $t = ((Get-Date) - $t0).TotalSeconds
    "{0:N1},{1},{2:N1},{3:N1},{4:N1},{5:N1},{6}" -f $t, $wv.Count, $ws, $pr, ($host_.WorkingSetSize / 1MB), ($host_.PrivatePageCount / 1MB), $names | Out-File $Out -Append -Encoding utf8
    Start-Sleep -Milliseconds 1000
}
"exit code $($p.ExitCode)" | Out-File $Out -Append -Encoding utf8
