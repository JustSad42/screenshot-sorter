# Builds the app and installer, and publishes them as a GitHub release.
#   .\release.ps1              build + installer only (dist\)
#   .\release.ps1 -Publish     also tag v<version> and create the GitHub release
# The version comes from rust\Cargo.toml - bump it there first.
param([switch]$Publish, [string]$Notes = "")
$ErrorActionPreference = "Stop"
$root = $PSScriptRoot

$version = (Select-String -Path "$root\rust\Cargo.toml" -Pattern '^version\s*=\s*"([^"]+)"').Matches[0].Groups[1].Value
Write-Host "Version $version"

Push-Location "$root\rust"
cargo build --release
if ($LASTEXITCODE) { throw "cargo build failed" }
Pop-Location

$iscc = @("$env:LOCALAPPDATA\Programs\Inno Setup 6\ISCC.exe", "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe", "$env:ProgramFiles\Inno Setup 6\ISCC.exe") |
    Where-Object { Test-Path $_ } | Select-Object -First 1
if (-not $iscc) { throw "Inno Setup 6 not found (winget install JRSoftware.InnoSetup)" }
& $iscc /Q "/DAppVersion=$version" "$root\installer\setup.iss"
if ($LASTEXITCODE) { throw "installer build failed" }

$setup = "$root\dist\ScreenshotSorter-$version-setup.exe"
$portable = "$root\dist\ScreenshotSorter-$version-portable.exe"
Copy-Item "$root\rust\target\release\screenshot-sorter.exe" $portable -Force
Write-Host "Built $setup"

if ($Publish) {
    if (-not $Notes) { $Notes = "Screenshot Sorter $version" }
    gh release create "v$version" $setup $portable --title "Screenshot Sorter $version" --notes $Notes
    if ($LASTEXITCODE) { throw "gh release create failed" }
}
