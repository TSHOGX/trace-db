param(
    [Parameter(Mandatory = $true)][string]$Target,
    [Parameter(Mandatory = $true)][string]$Version,
    [Parameter(Mandatory = $true)][string]$OutputDirectory
)

$ErrorActionPreference = "Stop"
$Root = Split-Path -Parent $PSScriptRoot
$Package = "trace-db-$Version-$Target"
$StageRoot = Join-Path ([System.IO.Path]::GetTempPath()) ([System.Guid]::NewGuid())
$Stage = Join-Path $StageRoot $Package

try {
    New-Item -ItemType Directory -Path (Join-Path $Stage "proto/tracedb/v1") -Force | Out-Null
    New-Item -ItemType Directory -Path $OutputDirectory -Force | Out-Null
    Copy-Item (Join-Path $Root "target/$Target/release/trace-db.exe") (Join-Path $Stage "trace-db.exe")
    Copy-Item (Join-Path $Root "target/$Target/release/fts5jieba.dll") $Stage
    Copy-Item (Join-Path $Root "README.md") $Stage
    Copy-Item (Join-Path $Root "LICENSE") $Stage
    Copy-Item (Join-Path $Root "proto/tracedb/v1/tracedb.proto") (Join-Path $Stage "proto/tracedb/v1/tracedb.proto")
    Compress-Archive -Path $Stage -DestinationPath (Join-Path $OutputDirectory "$Package.zip")
}
finally {
    Remove-Item -Recurse -Force $StageRoot -ErrorAction SilentlyContinue
}
