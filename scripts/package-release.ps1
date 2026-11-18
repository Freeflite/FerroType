# Packages a built ferrotype.exe for a GitHub release, into dist\:
#   ferrotype.exe                        the program itself
#   ferrotype-<version>-windows-x64.zip  the program with its license notices
#   SHA256SUMS.txt                       checksums, in `sha256sum -c` format
#
# Usage: pwsh scripts\package-release.ps1 [-Tag v1.2.3] [-Exe <path>] [-OutDir <path>]
# With -Tag, refuses to package unless the tag, Cargo.toml and the exe all agree on the version.
param([string]$Tag, [string]$Exe, [string]$OutDir)
$ErrorActionPreference = 'Stop'

$repo = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
if (-not $Exe) { $Exe = Join-Path $repo 'target\release\ferrotype.exe' }
if (-not $OutDir) { $OutDir = Join-Path $repo 'dist' }

$manifest = Get-Content (Join-Path $repo 'Cargo.toml') -Raw
if ($manifest -notmatch '(?m)^version = "([0-9]+\.[0-9]+\.[0-9]+)"') { throw 'No workspace version in Cargo.toml' }
$version = $Matches[1]
$built = (Get-Item $Exe).VersionInfo.ProductVersion
if ($built -ne $version) { throw "ferrotype.exe reports version $built, Cargo.toml says $version" }
if ($Tag -and $Tag -ne "v$version") { throw "Tag $Tag doesn't match version $version" }

if (Test-Path $OutDir) { Remove-Item $OutDir -Recurse -Force }
$name = "ferrotype-$version-windows-x64"
$staging = Join-Path $OutDir $name
New-Item -ItemType Directory $staging | Out-Null
Copy-Item $Exe (Join-Path $staging 'ferrotype.exe')
Copy-Item (Join-Path $repo 'LICENSE') (Join-Path $staging 'LICENSE.txt')
Copy-Item (Join-Path $repo 'THIRD-PARTY-NOTICES.md') $staging
Compress-Archive -Path (Join-Path $staging '*') -DestinationPath (Join-Path $OutDir "$name.zip")
Remove-Item $staging -Recurse
Copy-Item $Exe (Join-Path $OutDir 'ferrotype.exe')

# LF line endings, so `sha256sum -c SHA256SUMS.txt` works as well as Get-FileHash.
$sums = Get-ChildItem $OutDir -File | Sort-Object Name | ForEach-Object {
    '{0}  {1}' -f (Get-FileHash $_.FullName -Algorithm SHA256).Hash.ToLowerInvariant(), $_.Name
}
[IO.File]::WriteAllText((Join-Path $OutDir 'SHA256SUMS.txt'), (($sums -join "`n") + "`n"))

"Packaged FerroType $version into ${OutDir}:"
Get-ChildItem $OutDir | ForEach-Object { '  {0,-36} {1,8:N1} MB' -f $_.Name, ($_.Length / 1MB) }
