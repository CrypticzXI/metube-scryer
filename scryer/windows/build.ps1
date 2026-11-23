param()
$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $true
$repoRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '../..'))
$bundle = Join-Path $repoRoot 'dist/metube-scryer-windows-x64'
if (Test-Path -LiteralPath $bundle) { throw "Build folder already exists: $bundle" }
New-Item -ItemType Directory -Path $bundle -Force | Out-Null
$runtime = Join-Path $bundle 'runtime'
$stage = Join-Path $repoRoot 'target/windows-package'
New-Item -ItemType Directory -Path $runtime,$stage,(Join-Path $bundle 'bin'),(Join-Path $bundle 'plugins') -Force | Out-Null
$env:CARGO_TARGET_DIR = Join-Path $repoRoot 'target'

function Invoke-Checked([string]$Program, [string[]]$Arguments) {
    & $Program @Arguments
    if ($LASTEXITCODE -ne 0) { throw "$Program failed ($LASTEXITCODE)" }
}
function Download-Verified([string]$Url, [string]$Path, [string]$Sha256) {
    Invoke-WebRequest -Uri $Url -OutFile $Path
    if ((Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash -ne $Sha256) {
        throw "Publisher checksum mismatch: $Url"
    }
}

Push-Location -LiteralPath $repoRoot
try {
    Invoke-Checked cargo @('build','--manifest-path','scryer/service/Cargo.toml','--release','--locked','--bin','scryer-youtube-library-service')
    Copy-Item -LiteralPath 'target/release/scryer-youtube-library-service.exe' -Destination (Join-Path $bundle 'bin')
    Invoke-Checked python @('scryer/indexer/package.py','--output',(Join-Path $bundle 'plugins/youtube-library.wasm'))
    Invoke-Checked python @('scryer/indexer/package.py','--component','download-client','--output',(Join-Path $bundle 'plugins/metube.wasm'))
    Push-Location ui
    try {
        Invoke-Checked pnpm @('install','--frozen-lockfile')
        Invoke-Checked pnpm @('run','build')
    } finally { Pop-Location }

    # Official embeddable CPython keeps the package independent of installed Python.
    $pythonVersion = (& python -c 'import platform; print(platform.python_version())').Trim()
    if ($LASTEXITCODE -ne 0) { throw 'Cannot determine Python version' }
    $pythonZip = Join-Path $stage 'python.zip'
    Invoke-WebRequest -Uri "https://www.python.org/ftp/python/$pythonVersion/python-$pythonVersion-embed-amd64.zip" -OutFile $pythonZip
    Expand-Archive -LiteralPath $pythonZip -DestinationPath $runtime
    $pth = @(Get-ChildItem -LiteralPath $runtime -Filter 'python*._pth')
    if ($pth.Count -ne 1) { throw 'Unexpected embeddable Python layout' }
    $pyZip = (Get-ChildItem -LiteralPath $runtime -Filter 'python*.zip' | Select-Object -First 1).Name
    @($pyZip,'.','Lib/site-packages','../app','import site') | Set-Content -LiteralPath $pth[0].FullName -Encoding ascii
    $requirements = Join-Path $stage 'requirements.txt'
    & uv export --frozen --no-dev --no-emit-project --format requirements-txt --output-file $requirements | Out-Null
    if ($LASTEXITCODE -ne 0) { throw 'Dependency export failed' }
    Invoke-Checked python @('-m','pip','install','--require-hashes','--only-binary=:all:','--target',(Join-Path $runtime 'Lib/site-packages'),'-r',$requirements)

    Download-Verified 'https://github.com/yt-dlp/yt-dlp/releases/download/2026.10.26/yt-dlp.exe' (Join-Path $runtime 'yt-dlp.exe') 'f5389d3698ecde4ef06f4e6b1ce0294fe9ac1396228c8990b8d3416d6d177a7d'
    $denoZip = Join-Path $stage 'deno.zip'
    Download-Verified 'https://github.com/denoland/deno/releases/download/v2.9.8/deno-x86_64-pc-windows-msvc.zip' $denoZip 'cba84b26e0df5ae7f74fc66b86a95b866872a86292ebd84fa1e959927edd93bf'
    Expand-Archive -LiteralPath $denoZip -DestinationPath $runtime -Force
    Invoke-WebRequest 'https://raw.githubusercontent.com/yt-dlp/yt-dlp/2026.10.26/LICENSE' -OutFile (Join-Path $runtime 'yt-dlp-LICENSE')
    Invoke-WebRequest 'https://raw.githubusercontent.com/denoland/deno/v2.9.8/LICENSE.md' -OutFile (Join-Path $runtime 'deno-LICENSE')
    $ffmpegUrl = 'https://www.gyan.dev/ffmpeg/builds/ffmpeg-release-essentials.zip'
    $ffmpegSha = ((Invoke-WebRequest ($ffmpegUrl + '.sha256')).Content.Trim() -split '\s+')[0]
    if ($ffmpegSha -notmatch '^[a-fA-F0-9]{64}$') { throw 'Invalid FFmpeg publisher checksum' }
    $ffmpegZip = Join-Path $stage 'ffmpeg.zip'
    Download-Verified $ffmpegUrl $ffmpegZip $ffmpegSha
    $ffmpegStage = Join-Path $stage 'ffmpeg'
    Expand-Archive -LiteralPath $ffmpegZip -DestinationPath $ffmpegStage
    $ffmpegRoot = @(Get-ChildItem -LiteralPath $ffmpegStage -Directory)
    if ($ffmpegRoot.Count -ne 1) { throw 'Unexpected FFmpeg layout' }
    Copy-Item -LiteralPath $ffmpegRoot[0].FullName -Destination (Join-Path $runtime 'ffmpeg') -Recurse

    Copy-Item -LiteralPath app -Destination (Join-Path $bundle 'app') -Recurse
    New-Item -ItemType Directory -Path (Join-Path $bundle 'ui/dist') -Force | Out-Null
    Copy-Item -LiteralPath 'ui/dist/metube' -Destination (Join-Path $bundle 'ui/dist/metube') -Recurse
    New-Item -ItemType Directory -Path (Join-Path $bundle 'scryer/windows') -Force | Out-Null
    Copy-Item -LiteralPath 'scryer/windows/launcher.py' -Destination (Join-Path $bundle 'scryer/windows/launcher.py')
    Copy-Item -LiteralPath LICENSE -Destination (Join-Path $bundle 'LICENSE')
    Copy-Item -LiteralPath 'scryer/indexer/LICENSE' -Destination (Join-Path $bundle 'LICENSE-Scryer-plugins')
    Copy-Item -LiteralPath 'scryer/README.md' -Destination (Join-Path $bundle 'SCRYER-SETUP.md')
    Copy-Item -LiteralPath 'scryer/windows/README.md' -Destination (Join-Path $bundle 'README.md')
    @('@echo off','cd /d "%~dp0"','"runtime\python.exe" "scryer\windows\launcher.py" %*','if errorlevel 1 pause') | Set-Content -LiteralPath (Join-Path $bundle 'Start-MeTube.cmd') -Encoding ascii
    (& git rev-parse HEAD) | Set-Content -LiteralPath (Join-Path $bundle 'SOURCE-COMMIT.txt') -Encoding ascii
    $manifest = Get-ChildItem -LiteralPath $bundle -Recurse -File | Sort-Object FullName | ForEach-Object {
        (Get-FileHash -LiteralPath $_.FullName -Algorithm SHA256).Hash.ToLowerInvariant() + '  ' + [IO.Path]::GetRelativePath($bundle,$_.FullName).Replace('\','/')
    }
    $manifest | Set-Content -LiteralPath (Join-Path $bundle 'SHA256SUMS') -Encoding utf8NoBOM
    $archive = Join-Path $repoRoot 'dist/metube-scryer-windows-x64.zip'
    Compress-Archive -LiteralPath $bundle -DestinationPath $archive
    ((Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash.ToLowerInvariant() + '  metube-scryer-windows-x64.zip') | Set-Content -LiteralPath ($archive + '.sha256') -Encoding ascii
    Write-Output "Windows package: $archive"
} finally { Pop-Location }
