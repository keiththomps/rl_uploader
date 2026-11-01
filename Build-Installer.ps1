$ErrorActionPreference = 'Stop'
Set-Location -LiteralPath $PSScriptRoot
$env:DOTNET_CLI_TELEMETRY_OPTOUT = '1'
$env:DOTNET_ROLL_FORWARD = 'LatestMajor'
$wix = Join-Path $PSScriptRoot '.tools\wix\wix.exe'
if (-not (Test-Path -LiteralPath $wix)) {
    throw 'Install the build-only packager: dotnet tool install wix --tool-path .tools\wix --version 4.0.6 --allow-roll-forward'
}
$manifest = Get-Content -LiteralPath (Join-Path $PSScriptRoot 'Cargo.toml') -Raw
$version = [regex]::Match($manifest, '(?m)^version = "([^"]+)"').Groups[1].Value
if (-not $version) { throw 'App version is unavailable.' }
$app = Join-Path $PSScriptRoot 'target\release\rl-uploader.exe'
if (-not (Test-Path -LiteralPath $app)) { throw 'Build the release executable first.' }
$output = Join-Path $PSScriptRoot 'dist\RLUploader-Setup.msi'
New-Item -ItemType Directory -Path (Split-Path $output) -Force | Out-Null
& $wix build (Join-Path $PSScriptRoot 'installer\Package.wxs') -arch x64 -d "AppVersion=$version" -d "AppExe=$app" -d "ProjectRoot=$PSScriptRoot" -o $output -intermediateFolder (Join-Path $PSScriptRoot 'target\wix')
if ($LASTEXITCODE -ne 0) { throw 'Windows Installer packaging failed.' }
Write-Output ('Installer: ' + $output)
