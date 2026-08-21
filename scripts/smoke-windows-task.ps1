param(
    [string]$TraceDb = "target/debug/trace-db.exe"
)

$ErrorActionPreference = "Stop"
$TraceDb = (Resolve-Path $TraceDb).Path
$Workspace = Join-Path $env:TEMP ("tracedb-task-smoke-" + [System.Guid]::NewGuid())
$NativeRoot = Join-Path $Workspace "native"
$Database = Join-Path $Workspace "trace.db"
New-Item -ItemType Directory -Path $NativeRoot -Force | Out-Null
'{"type":"session_meta","payload":{"id":"windows-daemon-smoke","cwd":"C:\\tracedb-smoke"}}' |
    Set-Content -Encoding utf8 (Join-Path $NativeRoot "rollout-smoke.jsonl")

try {
    & $TraceDb --db $Database daemon install --interval 60 --agent codex --root $NativeRoot
    if ($LASTEXITCODE -ne 0) { throw "daemon install failed" }

    $TaskXml = schtasks /Query /TN TraceDB-Watch /XML
    if ($LASTEXITCODE -ne 0 -or -not (($TaskXml -join "`n") -match "RestartOnFailure")) {
        throw "registered task is missing restart policy"
    }

    $Ingested = $false
    for ($Attempt = 0; $Attempt -lt 30; $Attempt++) {
        $Stats = & $TraceDb --db $Database stats --json | ConvertFrom-Json
        if ($Stats.totalSessions -eq 1) {
            $Ingested = $true
            break
        }
        Start-Sleep -Seconds 1
    }
    if (-not $Ingested) { throw "daemon startup ingest did not complete" }

    & $TraceDb --db $Database daemon status
    if ($LASTEXITCODE -ne 0) { throw "daemon status failed" }
    & $TraceDb --db $Database daemon stop
    if ($LASTEXITCODE -ne 0) { throw "daemon stop failed" }
    & $TraceDb --db $Database daemon start
    if ($LASTEXITCODE -ne 0) { throw "daemon start failed" }
    Write-Output "Windows Task Scheduler daemon smoke: ok"
}
finally {
    & $TraceDb --db $Database daemon uninstall
    Remove-Item -Recurse -Force $Workspace -ErrorAction SilentlyContinue
}
