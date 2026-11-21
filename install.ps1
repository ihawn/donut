# Install or upgrade Donut from the latest GitHub release. Settings are kept.
# DONUT_INSTALL_DIR picks the folder for the executables (default %LOCALAPPDATA%\Programs\donut\bin).
# DONUT_INSTALL_SOURCE installs a local release archive instead of downloading one.
$ErrorActionPreference = 'Stop'
Write-Host 'Installing or upgrading Donut...'
$binDir = $env:DONUT_INSTALL_DIR
if (-not $binDir) { $binDir = Join-Path $env:LOCALAPPDATA 'Programs\donut\bin' }

$target = 'x86_64-pc-windows-msvc'  # Windows on ARM runs it through emulation.

# Remove the earlier Python install first, so its launcher does not hide this one.
if (Get-Command uv -ErrorAction SilentlyContinue) {
    if ((& uv tool list 2>$null) -match '^donut-cli ') { & uv tool uninstall donut-cli *> $null }
}

$work = Join-Path ([IO.Path]::GetTempPath()) ([Guid]::NewGuid())
New-Item -ItemType Directory -Path $work | Out-Null
try {
    $source = $env:DONUT_INSTALL_SOURCE
    if (-not $source) {
        $base = 'https://github.com/ihawn/Donut/releases/latest/download'
        $source = Join-Path $work 'donut.zip'
        Invoke-WebRequest "$base/donut-$target.zip" -OutFile $source -UseBasicParsing
        $sums = (Invoke-WebRequest "$base/SHA256SUMS" -UseBasicParsing).Content
        $line = ($sums -split "`n") | Where-Object { $_ -match " donut-$target\.zip\s*$" } | Select-Object -First 1
        if (-not $line) { throw "The release has no checksum for donut-$target.zip. Nothing was installed." }
        $expected = ($line -split ' ')[0].Trim().ToLower()
        $actual = (Get-FileHash $source -Algorithm SHA256).Hash.ToLower()
        if ($expected -ne $actual) { throw 'The downloaded archive does not match its checksum. Nothing was installed.' }
    }
    $unpacked = Join-Path $work 'unpacked'
    Expand-Archive -Path $source -DestinationPath $unpacked
    New-Item -ItemType Directory -Force -Path $binDir | Out-Null
    foreach ($name in 'donut.exe', 'qwen-build-next.exe') {
        $found = Get-ChildItem -Path $unpacked -Recurse -Filter $name | Select-Object -First 1
        if (-not $found) { throw "The archive has no $name. Nothing was installed." }
        $dest = Join-Path $binDir $name
        # A running exe cannot be overwritten, but it can be renamed out of the way.
        if (Test-Path $dest) { Move-Item -Force $dest "$dest.old" }
        Copy-Item $found.FullName $dest
        Remove-Item -Force "$dest.old" -ErrorAction SilentlyContinue
    }
} finally {
    Remove-Item -Recurse -Force $work -ErrorAction SilentlyContinue
}

if ($env:CI -ne 'true') {
    $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    if (-not (($userPath -split ';') -contains $binDir)) {
        [Environment]::SetEnvironmentVariable('Path', "$binDir;$userPath", 'User')
        Write-Host "Added $binDir to your PATH. Open a new terminal to use it."
    }
}
$donut = Join-Path $binDir 'donut.exe'
& $donut welcome
if ($LASTEXITCODE -ne 0) { throw 'Donut installed but could not start.' }
