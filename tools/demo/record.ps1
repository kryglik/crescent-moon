# Records the feature page's animations: builds cm and the recorder, maps the made-up world to a
# drive letter (so no real path shows), and for each scene in scenes\ rebuilds the world, plays
# the scene with examples\record.rs and renders it into site\gifs with render.py.
#
#   tools\demo\record.ps1                 # every scene
#   tools\demo\record.ps1 -Only tour,git  # some
param(
    [string[]]$Only,
    [string]$Base = (Join-Path ([IO.Path]::GetTempPath()) 'cm-demo'),
    [string]$Drive = 'M:',
    [switch]$NoBuild
)
$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
$here = $PSScriptRoot
if (-not $NoBuild) {
    cargo build --release --manifest-path "$repo\Cargo.toml" --bin cm --example record 2>&1 | Select-Object -Last 1
    if ($LASTEXITCODE) { throw 'build failed' }
}
$env:CM = "$repo\target\release\cm.exe"
$recorder = "$repo\target\release\examples\record.exe"

if (Test-Path "$Drive\") { throw "$Drive is taken: pass -Drive with a free letter" }
New-Item -ItemType Directory -Force "$Base\world", "$Base\out" | Out-Null
subst $Drive "$Base\world"
try {
    # What cm and git see: settings and git's config of the made-up world, colours on.
    $env:APPDATA = "$Base\appdata"
    $env:GIT_CONFIG_GLOBAL = "$Base\gitconfig"
    $env:GIT_CONFIG_NOSYSTEM = '1'
    Remove-Item Env:NO_COLOR, Env:CM_PID, Env:RMC_PID -ErrorAction SilentlyContinue
    $scenes = Get-ChildItem "$here\scenes\*.txt" | Where-Object { -not $Only -or $Only -contains $_.BaseName }
    foreach ($scene in $scenes) {
        $name = $scene.BaseName
        python -I "$here\make_world.py" $Base "$Drive\" | Out-Null
        if ($LASTEXITCODE) { throw "making the world failed before $name" }
        & $recorder $scene.FullName "$Base\out\$name.json"
        if ($LASTEXITCODE) { Write-Warning "$name failed"; continue }
        # Nothing of this computer may show: another drive, a share, the user, where the world is.
        $letter = [int][char]$Drive.ToLower()[0]
        $forbid = @(
            "(?i)\b[a-$([char]($letter - 1))$([char]($letter + 1))-z]:\\",
            '\\\\[^\\\s]+\\',
            "(?i)$([regex]::Escape($env:USERNAME))",
            "(?i)$([regex]::Escape((Split-Path -Leaf $Base)))"
        )
        $forbidArgs = $forbid | ForEach-Object { '--forbid'; $_ }
        python -I "$here\render.py" "$Base\out\$name.json" "$repo\site\gifs\$name.gif" --png "$Base\out\$name.png" @forbidArgs
        if ($LASTEXITCODE) { Write-Warning "$name shows something of this computer: not used" }
    }
} finally {
    subst $Drive /d
}
