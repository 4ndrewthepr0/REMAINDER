param([switch]$SkipModel, [switch]$SkipRuntime, [string]$RuntimeSource)
$ErrorActionPreference = 'Stop'
$dataDir = Join-Path $env:LOCALAPPDATA 'REMAINDER'
$modelDir = Join-Path $dataDir 'models\all-MiniLM-L6-v2'
$runtimeDir = Join-Path $dataDir 'runtime'
if (-not $SkipRuntime -and ([Environment]::OSVersion.Platform -ne [PlatformID]::Win32NT -or $env:PROCESSOR_ARCHITECTURE -ne 'AMD64')) {
    throw 'This runtime installer supports Windows x64. Use -SkipRuntime and provision the appropriate native runtime on other platforms.'
}

if (-not $SkipModel -and (-not (Test-Path "$modelDir\model.onnx") -or -not (Test-Path "$modelDir\tokenizer.json"))) {
    $cachedModel = Join-Path $env:USERPROFILE '.cache\chroma\onnx_models\all-MiniLM-L6-v2\onnx'
    if ((Test-Path "$cachedModel\model.onnx") -and (Test-Path "$cachedModel\tokenizer.json")) {
        $modelSource = $cachedModel
    } else {
        $modelStage = Join-Path ([IO.Path]::GetTempPath()) ('remainder-model-' + [guid]::NewGuid().ToString('N'))
        New-Item -ItemType Directory -Path $modelStage | Out-Null
        $archive = Join-Path $modelStage 'onnx.tar.gz'
        Invoke-WebRequest 'https://chroma-onnx-models.s3.amazonaws.com/all-MiniLM-L6-v2/onnx.tar.gz' -OutFile $archive -UseBasicParsing
        if ((Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash -ne '913d7300ceae3b2dbc2c50d1de4baacab4be7b9380491c27fab7418616a16ec3') {
            throw "MiniLM archive checksum failed: $archive"
        }
        & tar -xzf $archive -C $modelStage
        if ($LASTEXITCODE -ne 0) { throw 'MiniLM archive extraction failed.' }
        $modelSource = Join-Path $modelStage 'onnx'
    }
    New-Item -ItemType Directory -Path $modelDir -Force | Out-Null
    foreach ($name in @('model.onnx', 'tokenizer.json')) {
        Copy-Item -LiteralPath (Join-Path $modelSource $name) -Destination (Join-Path $modelDir $name) -Force
    }
    Write-Host "MiniLM installed: $modelDir"
}
if (-not $SkipModel -and (Test-Path -LiteralPath $modelDir)) {
    foreach ($name in @('Apache-2.0.txt', 'MiniLM-NOTICE.txt')) {
        Copy-Item -LiteralPath (Join-Path $PSScriptRoot "..\third_party\$name") -Destination $modelDir -Force
    }
}

if (-not $SkipRuntime -and ($RuntimeSource -or -not (Test-Path "$runtimeDir\onnxruntime.dll"))) {
    if ($RuntimeSource) {
        if (-not (Test-Path -LiteralPath $RuntimeSource -PathType Leaf)) { throw "Runtime missing: $RuntimeSource" }
        $runtimeSourceDir = Split-Path -Parent $RuntimeSource
    } else {
        $runtimeStage = Join-Path ([IO.Path]::GetTempPath()) ('remainder-runtime-' + [guid]::NewGuid().ToString('N'))
        New-Item -ItemType Directory -Path $runtimeStage | Out-Null
        $archive = Join-Path $runtimeStage 'runtime.zip'
        Invoke-WebRequest 'https://github.com/microsoft/onnxruntime/releases/download/v1.24.4/onnxruntime-win-x64-1.24.4.zip' -OutFile $archive -UseBasicParsing
        if ((Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash -ne 'd2319fddfb6ea4db99ccc4b60c85c517bcd855721f5daa6a06d40d7cb2ee2357') {
            throw "ONNX Runtime archive checksum failed: $archive"
        }
        Expand-Archive -LiteralPath $archive -DestinationPath $runtimeStage
        $runtimeSourceDir = Join-Path $runtimeStage 'onnxruntime-win-x64-1.24.4\lib'
        New-Item -ItemType Directory -Path $runtimeDir -Force | Out-Null
        foreach ($name in @('LICENSE', 'ThirdPartyNotices.txt')) {
            $notice = Join-Path $runtimeStage "onnxruntime-win-x64-1.24.4\$name"
            if (Test-Path -LiteralPath $notice) { Copy-Item -LiteralPath $notice -Destination $runtimeDir -Force }
        }
    }
    New-Item -ItemType Directory -Path $runtimeDir -Force | Out-Null
    $runtimeDll = if ($RuntimeSource) { $RuntimeSource } else { Join-Path $runtimeSourceDir 'onnxruntime.dll' }
    Copy-Item -LiteralPath $runtimeDll -Destination (Join-Path $runtimeDir 'onnxruntime.dll') -Force
    $providers = Join-Path $runtimeSourceDir 'onnxruntime_providers_shared.dll'
    if (Test-Path -LiteralPath $providers) { Copy-Item -LiteralPath $providers -Destination $runtimeDir -Force }
    foreach ($name in @('LICENSE', 'ThirdPartyNotices.txt')) {
        foreach ($directory in @($runtimeSourceDir, (Split-Path -Parent $runtimeSourceDir))) {
            $notice = Join-Path $directory $name
            if (Test-Path -LiteralPath $notice) { Copy-Item -LiteralPath $notice -Destination $runtimeDir -Force; break }
        }
    }
    Write-Host "ONNX Runtime installed: $runtimeDir"
}
