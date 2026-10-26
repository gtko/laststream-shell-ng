#requires -Version 7.0
<#
.SYNOPSIS
  Mesure le pic mémoire d'une lecture mpv avec un tampon donné, sur un flux local à débit limité (LAST-146).
.DESCRIPTION
  Le dépôt ne contient pas de mpv.exe (seules les DLL libmpv sont fournies) : la lecture est faite par
  target\debug\examples\mpv_cache_probe.exe, libmpv sans fenêtre (vo=null, ao=null, force-window=no), avec les
  options de tampon passées en paramètre. Un serveur HTTP local à débit limité (scripts\throttled-server.mjs)
  sert le fichier. WorkingSet64 et PrivateMemorySize64 sont relevés chaque seconde jusqu'à la fin de la sonde.
  Tout processus lancé est arrêté à la fin. Sorties sous target\mpv-measure\ (non versionné).
.EXAMPLE
  pwsh -NoProfile -File scripts\measure-mpv-memory.ps1 -Media target\mpv-measure\testsrc-1080p-8M-120s.mp4 -RateMBps 8 -DurationSec 120 -Label new-1080p
#>
param(
    [Parameter(Mandatory)][string]$Media,
    [Parameter(Mandatory)][double]$RateMBps,
    [Parameter(Mandatory)][int]$DurationSec,
    [string]$Label = 'run',
    [string[]]$Options = @(
        'cache=yes', 'cache-secs=67', 'demuxer-readahead-secs=67',
        'demuxer-max-bytes=209715200', 'demuxer-max-back-bytes=52428800', 'cache-pause-wait=5'
    )
)
$ErrorActionPreference = 'Stop'
$Options = @($Options -split '[\s,]+' | Where-Object { $_ })
$invariant = [System.Globalization.CultureInfo]::InvariantCulture
$repo = Split-Path -Parent $PSScriptRoot
$outDir = Join-Path $repo 'target\mpv-measure'
New-Item -ItemType Directory -Force -Path $outDir | Out-Null
$probe = Join-Path $repo 'target\debug\examples\mpv_cache_probe.exe'
if (-not (Test-Path $probe)) { throw "sonde absente : cargo build --example mpv_cache_probe" }
if (-not (Test-Path (Join-Path $repo 'libmpv-2.dll'))) { throw "libmpv-2.dll absent : cargo build l'extrait (build.rs)" }
$mediaPath = (Resolve-Path $Media).Path
$port = Get-Random -Minimum 30000 -Maximum 60000
$url = "http://127.0.0.1:$port/media"
$server = $null
$proc = $null
$samples = [System.Collections.Generic.List[object]]::new()
$log = Join-Path $outDir "$Label.probe.log"
try {
    $server = Start-Process -FilePath 'node' -WindowStyle Hidden -PassThru -ArgumentList @(
        (Join-Path $repo 'scripts\throttled-server.mjs'), $mediaPath,
        $RateMBps.ToString($invariant), $port.ToString($invariant))
    $ready = $false
    for ($i = 0; $i -lt 50 -and -not $ready; $i++) {
        try { $client = [System.Net.Sockets.TcpClient]::new('127.0.0.1', $port); $client.Dispose(); $ready = $true }
        catch { Start-Sleep -Milliseconds 200 }
    }
    if (-not $ready) { throw "serveur à débit limité non joignable sur le port $port" }

    $env:PATH = "$repo;$env:PATH"
    $proc = Start-Process -FilePath $probe -WindowStyle Hidden -PassThru `
        -RedirectStandardOutput $log -RedirectStandardError "$log.err" `
        -ArgumentList (@($url, $DurationSec.ToString($invariant)) + $Options)
    $t0 = Get-Date
    while (-not $proc.HasExited) {
        $p = Get-Process -Id $proc.Id -ErrorAction SilentlyContinue
        if (-not $p) { break }
        $samples.Add([pscustomobject]@{
            t = [math]::Round(((Get-Date) - $t0).TotalSeconds, 1)
            workingSet = $p.WorkingSet64
            privateMemory = $p.PrivateMemorySize64
        })
        Start-Sleep -Seconds 1
    }
    $proc.WaitForExit()
    if ($proc.ExitCode -ne 0) { throw "sonde en échec (code $($proc.ExitCode)) : $log" }
}
finally {
    foreach ($child in @($proc, $server)) {
        if ($child -and -not $child.HasExited) { Stop-Process -Id $child.Id -Force -ErrorAction SilentlyContinue }
    }
}

$peakWs = ($samples | Measure-Object -Property workingSet -Maximum).Maximum
$peakPriv = ($samples | Measure-Object -Property privateMemory -Maximum).Maximum
$summary = [pscustomobject]@{
    label = $Label
    media = Split-Path $mediaPath -Leaf
    mediaMiB = [math]::Round((Get-Item $mediaPath).Length / 1MB, 1)
    rateMBps = $RateMBps
    durationSec = $DurationSec
    options = $Options
    samples = $samples.Count
    peakWorkingSetMiB = [math]::Round($peakWs / 1MB, 1)
    peakPrivateMiB = [math]::Round($peakPriv / 1MB, 1)
}
$samples | Export-Csv -NoTypeInformation -Path (Join-Path $outDir "$Label.csv")
$summary | ConvertTo-Json -Depth 4 | Set-Content -Path (Join-Path $outDir "$Label.json")
$summary | ConvertTo-Json -Depth 4 -Compress
