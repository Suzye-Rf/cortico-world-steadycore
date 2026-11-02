# Install cortico-world-steadycore: copy this folder into the host's extensions/node_modules
# and register it as a file: dependency.
# ASCII only (PowerShell 5.1 reads .ps1 as GBK, non-ASCII would be mangled).
$src = $PSScriptRoot
$ext = Join-Path $env:LOCALAPPDATA 'Programs\Coopanion\data\extensions'
$dst = Join-Path $ext 'node_modules\cortico-world-steadycore'

Write-Output ('SRC: ' + $src)
Write-Output ('DST: ' + $dst)
if (-not (Test-Path $ext)) {
  Write-Output ('FAILED: host extensions dir not found: ' + $ext)
  exit 1
}

if (Test-Path $dst) { Remove-Item $dst -Recurse -Force }
New-Item -ItemType Directory -Path $dst -Force | Out-Null
Copy-Item (Join-Path $src 'src') $dst -Recurse -Force
foreach ($f in @('describe.json', 'package.json', 'README.md')) {
  $p = Join-Path $src $f
  if (Test-Path $p) { Copy-Item $p $dst -Force }
}
Write-Output 'COPIED'
Get-ChildItem $dst -Recurse | ForEach-Object { '  ' + $_.FullName.Substring($dst.Length) }

$pkgPath = Join-Path $ext 'package.json'
Copy-Item $pkgPath ($pkgPath + '.bak') -Force
$pkg = Get-Content $pkgPath -Raw | ConvertFrom-Json
if (-not $pkg.dependencies) {
  $pkg | Add-Member -NotePropertyName dependencies -NotePropertyValue (New-Object PSObject)
}
$dep = 'file:' + $src
if ($pkg.dependencies.PSObject.Properties.Name -contains 'cortico-world-steadycore') {
  $pkg.dependencies.'cortico-world-steadycore' = $dep
} else {
  $pkg.dependencies | Add-Member -NotePropertyName 'cortico-world-steadycore' -NotePropertyValue $dep
}
$json = $pkg | ConvertTo-Json -Depth 20
[System.IO.File]::WriteAllText($pkgPath, $json, (New-Object System.Text.UTF8Encoding($false)))
Write-Output 'DEPENDENCIES NOW:'
$pkg.dependencies.PSObject.Properties | ForEach-Object { '  ' + $_.Name + ' = ' + $_.Value }
Write-Output 'OK'
