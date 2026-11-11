# Exits once the build is gone and total CPU load stays under $Max% for $Need consecutive 10 s samples.
param([int]$Max = 30, [int]$Need = 3)
$ok = 0
while ($ok -lt $Need) {
    Start-Sleep -Seconds 10
    $building = @(Get-Process cargo, rustc -ErrorAction SilentlyContinue).Count
    $load = (Get-CimInstance Win32_Processor | Measure-Object LoadPercentage -Average).Average
    if ($building -eq 0 -and $load -lt $Max) { $ok++ } else { $ok = 0 }
    "$(Get-Date -Format HH:mm:ss) load=$load% build_procs=$building streak=$ok"
}
"IDLE"
