param([switch]$SkipBuild, [string]$Executable, [string]$NsisCompiler, [string]$OutputDirectory)
$ErrorActionPreference = 'Stop'
$projectRoot = Split-Path $PSScriptRoot -Parent
Push-Location $projectRoot
try {
    $version = [regex]::Match((Get-Content Cargo.toml -Raw), '(?m)^version = "([^"]+)"').Groups[1].Value
    # 发布版不嵌入构建机器的用户目录和项目路径：依赖源码路径换成中性前缀，PDB 只记文件名。
    # 用 CARGO_ENCODED_RUSTFLAGS（\x1f 分隔）避免路径里的空格被拆开；它会覆盖 .cargo/config.toml 的 rustflags，所以带上 crt-static。
    $cargoHome = if ($env:CARGO_HOME) { $env:CARGO_HOME } else { Join-Path $env:USERPROFILE '.cargo' }
    $env:CARGO_ENCODED_RUSTFLAGS = @('-C', 'target-feature=+crt-static', "--remap-path-prefix=$cargoHome=cargo", "--remap-path-prefix=$projectRoot=daozai", '-C', 'link-arg=/PDBALTPATH:%_PDB%') -join [char]0x1f
    if (-not $SkipBuild) {
        $env:CARGO_BUILD_JOBS = '1'
        cargo +stable-x86_64-pc-windows-msvc build --locked --release --bin daozai --target x86_64-pc-windows-msvc
        if ($LASTEXITCODE -ne 0) { throw 'Release build failed' }
    }
    if (-not $Executable) {
        $targetDirectory = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { Join-Path $projectRoot 'target' }
        $Executable = Join-Path $targetDirectory 'x86_64-pc-windows-msvc\release\daozai.exe'
    }
    $binary = Get-Item -LiteralPath $Executable
    & (Join-Path $PSScriptRoot 'check-runtime.ps1') -Executable $binary.FullName
    if ($binary.VersionInfo.FileVersion -ne $version -and $binary.VersionInfo.FileVersion -ne "$version.0") { throw 'Executable version mismatch' }
    $content = [Text.Encoding]::UTF8.GetString([IO.File]::ReadAllBytes($binary.FullName))
    foreach ($private in @($env:USERPROFILE, $projectRoot)) {
        if ($private -and $content.Contains($private)) { throw "Executable embeds a build-machine path: $private" }
    }
    if (-not $NsisCompiler) {
        $compiler = Get-Command makensis.exe -ErrorAction SilentlyContinue
        if ($compiler) { $NsisCompiler = $compiler.Source }
        elseif ($env:NSIS_HOME) { $NsisCompiler = Join-Path $env:NSIS_HOME 'makensis.exe' }
        else { $NsisCompiler = Join-Path ${env:ProgramFiles(x86)} 'NSIS\makensis.exe' }
    }
    if (-not (Test-Path -LiteralPath $NsisCompiler)) { throw 'NSIS compiler not found; install NSIS or pass -NsisCompiler' }
    $packageName = "Daozai-v$version-windows-x64"
    if (-not $OutputDirectory) { $OutputDirectory = Join-Path $projectRoot 'dist' }
    $outputDirectory = [IO.Path]::GetFullPath($OutputDirectory)
    $stagingDirectory = Join-Path $outputDirectory $packageName
    if (Test-Path -LiteralPath $stagingDirectory) { throw 'Package directory already exists' }
    New-Item -ItemType Directory -Path $stagingDirectory -Force | Out-Null
    Copy-Item -LiteralPath $binary.FullName -Destination (Join-Path $stagingDirectory 'daozai.exe')
    # ZIP 保留给免安装使用和应用内更新；普通用户只需下载安装 EXE。
    @('LICENSE','THIRD_PARTY_NOTICES.txt') | ForEach-Object {
        Copy-Item -LiteralPath (Join-Path $projectRoot $_) -Destination $stagingDirectory -Recurse
    }
    $voiceFile = Join-Path $projectRoot 'voice\daozai_voice.py'
    if (Test-Path -LiteralPath $voiceFile) {
        New-Item -ItemType Directory -Path (Join-Path $stagingDirectory 'voice') -Force | Out-Null
        Copy-Item -LiteralPath $voiceFile -Destination (Join-Path $stagingDirectory 'voice\daozai_voice.py')
    }
    $setup = Join-Path $outputDirectory "Daozai-Setup-v$version-x64.exe"
    & $NsisCompiler /V2 /WX "/DAPP_VERSION=$version" "/DPROJECT_DIR=$projectRoot" "/DSOURCE_EXE=$($binary.FullName)" "/DOUTPUT_FILE=$setup" (Join-Path $projectRoot 'installer\setup.nsi')
    if ($LASTEXITCODE -ne 0) { throw 'Installer build failed' }
    $archive = Join-Path $outputDirectory "$packageName.zip"
    Compress-Archive -LiteralPath $stagingDirectory -DestinationPath $archive
    @($setup, $archive) | ForEach-Object {
        $checksum = (Get-FileHash -LiteralPath $_ -Algorithm SHA256).Hash.ToLowerInvariant()
        "$checksum  $([IO.Path]::GetFileName($_))"
    } | Set-Content -LiteralPath (Join-Path $outputDirectory 'SHA256SUMS.txt') -Encoding ascii
    Write-Output $setup
    Write-Output $archive
} finally { Pop-Location }
