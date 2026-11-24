# Installs (or updates) puppy from the latest GitHub release.
#
#   irm https://raw.githubusercontent.com/lavavrik/puppy/main/scripts/install.ps1 | iex
#
# Picks the archive for this machine, checks it against its published SHA-256,
# installs puppy.exe to %LOCALAPPDATA%\Programs\puppy and adds that folder to the
# user PATH. The unpacked archive, with the browser extension and service files,
# is kept in %LOCALAPPDATA%\puppy-install. Set $env:PUPPY_VERSION = '0.1.0' to
# pick a version.
& {
    $ErrorActionPreference = 'Stop'
    $ProgressPreference = 'SilentlyContinue'   # Invoke-WebRequest is very slow with it
    $repo = 'lavavrik/puppy'

    $version = $env:PUPPY_VERSION
    if (-not $version) {
        $version = (Invoke-RestMethod "https://api.github.com/repos/$repo/releases/latest").tag_name.TrimStart('v')
    }
    $arch = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture
    $target = if ("$arch" -eq 'Arm64') { 'aarch64-pc-windows-msvc' } else { 'x86_64-pc-windows-msvc' }
    $name = "puppy-$version-$target"
    $work = Join-Path $env:LOCALAPPDATA 'puppy-install'
    $url = "https://github.com/$repo/releases/download/v$version/$name.zip"

    Write-Host "Downloading puppy $version for $target"
    New-Item -ItemType Directory -Force $work | Out-Null
    $zip = Join-Path $work "$name.zip"
    Invoke-WebRequest $url -OutFile $zip -UseBasicParsing
    Invoke-WebRequest "$url.sha256" -OutFile "$zip.sha256" -UseBasicParsing
    $expected = (Get-Content "$zip.sha256" -Raw).Trim().Split(' ')[0]
    if ((Get-FileHash $zip -Algorithm SHA256).Hash -ne $expected) { throw "checksum mismatch for $name.zip" }
    $unpacked = Join-Path $work $name
    if (Test-Path $unpacked) { Remove-Item $unpacked -Recurse -Force }
    Expand-Archive $zip $work -Force

    $dir = Join-Path $env:LOCALAPPDATA 'Programs\puppy'
    New-Item -ItemType Directory -Force $dir | Out-Null
    Copy-Item (Join-Path $unpacked 'puppy.exe') $dir -Force
    $path = [Environment]::GetEnvironmentVariable('Path', 'User')
    if (";$path;" -notlike "*;$dir;*") {
        [Environment]::SetEnvironmentVariable('Path', (($path, $dir) -ne '' -join ';'), 'User')
        Write-Host "Added $dir to your PATH; open a new terminal to use it."
    }
    Write-Host "Installed $(& (Join-Path $dir 'puppy.exe') --version) to $dir"
    Write-Host "Browser extension and service files: $unpacked"
}
