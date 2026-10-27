<#
.SYNOPSIS
  Telecharge libVLC 3.0.21 (VideoLAN), verifie son SHA-256 epingle et l'installe dans le bundle de la coque.

.DESCRIPTION
  Moteur secondaire de la coque Windows (LAST-504). Source officielle :
  https://get.videolan.org/vlc/3.0.21/win64/vlc-3.0.21-win64.zip
  Le SHA-256 epingle ci-dessous est celui que VideoLAN publie dans vlc-3.0.21-win64.zip.sha256.

  1. Archive dans le cache (target\vlc-cache par defaut, ignore par git). Une archive en cache dont le SHA-256
     ne correspond pas est retelechargee. Une archive telechargee dont le SHA-256 ne correspond pas est rejetee.
  2. Extraction ciblee, sans l'arbre complet de l'archive, vers -Destination :
       libvlc.dll, libvlccore.dll,
       COPYING-libvlc.txt (COPYING.txt de VLC, GPL v2),
       COPYING.LGPL-2.1.txt (licenses\LGPL-2.1.txt du depot : l'archive ne contient pas le texte LGPL),
       plugins\<categorie>\*.dll pour toutes les categories sauf $ExcludedCategories
       et sauf les plugins $ExcludedPlugins (encodeurs, capture, disques, tuner),
       plugins\plugins.dat regenere avec le vlc-cache-gen.exe de l'archive (evite le scan de tous les plugins
       au premier libvlc_new ; vlc-cache-gen.exe lui-meme n'est pas livre).
  3. Un -Destination non vide qui ne contient pas COPYING-libvlc.txt n'est jamais ecrase. Un -Destination
     issu d'une execution precedente de ce script est remplace.

  Compatible Windows PowerShell 5.1 et PowerShell 7. Fichier en ASCII uniquement (5.1 lit sans BOM en ANSI).

.PARAMETER Destination
  Dossier cible. Defaut : <depot>\dist\laststream-desktop\vlc (le bundle que setup\Laststream.iss embarque).
  Le dossier parent doit deja exister : le bundle est assemble avant ce script.

.PARAMETER CacheDir
  Dossier de l'archive. Defaut : <depot>\target\vlc-cache.

.EXAMPLE
  .\scripts\fetch-libvlc.ps1 -Destination C:\temp\vlc-test
#>
[CmdletBinding()]
param(
    [string]$Destination,
    [string]$CacheDir
)

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
if ($PSVersionTable.PSVersion.Major -lt 6) {
    [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
}

$Version = '3.0.21'
$ZipName = "vlc-$Version-win64.zip"
$Url = "https://get.videolan.org/vlc/$Version/win64/$ZipName"
$Sha256Pinned = 'a0b7ec02b50adf6417eed014fb8df50af39690505a4225b85b3dc2ed17d14843'
$LicenseName = 'COPYING-libvlc.txt'
$LgplName = 'COPYING.LGPL-2.1.txt'
# Categories de plugins (dossiers plugins\<categorie>\ de l'archive) non livrees : hors lecture reseau.
# mux et access_output ne servent qu'a la sortie de flux (stream_out, deja exclue).
$ExcludedCategories = @('gui', 'lua', 'control', 'services_discovery', 'stream_out', 'visualization', 'video_splitter', 'keystore', 'mux', 'access_output')
# Plugins exclus dans les categories livrees : "categorie\motif" (joker -like sur le nom du fichier).
$ExcludedPlugins = @(
    'codec\libx264*', 'codec\libx26410b*', 'codec\libx265*', 'codec\libtwolame*', 'codec\libshine*',
    'access\libdshow*', 'access\libscreen*', 'access\libdtv*', 'access\libdvdnav*', 'access\libdvdread*',
    'access\liblibbluray*', 'access\libbluray*'
)

function Test-ExcludedPlugin([string]$Category, [string]$File) {
    foreach ($pattern in $ExcludedPlugins) {
        $pair = $pattern.Split('\', 2)
        if ($pair[0] -eq $Category -and $File -like $pair[1]) { return $true }
    }
    return $false
}

$repoRoot = Split-Path -Parent $PSScriptRoot
if (-not $Destination) { $Destination = Join-Path $repoRoot 'dist\laststream-desktop\vlc' }
if (-not $CacheDir) { $CacheDir = Join-Path $repoRoot 'target\vlc-cache' }
$Destination = [IO.Path]::GetFullPath($Destination)
$CacheDir = [IO.Path]::GetFullPath($CacheDir)

function Get-Sha256Lower([string]$Path) {
    (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant()
}

# 0. Le dossier parent (le bundle) doit exister : sinon ce script creerait un dist\ partiel qu'ISCC empaqueterait sans erreur.
$parent = Split-Path -Parent $Destination
if (-not (Test-Path -LiteralPath $parent -PathType Container)) {
    throw "Dossier parent absent : $parent. Assembler dist\laststream-desktop avant ce script."
}

# 1. Archive : cache verifie, sinon telechargement verifie.
New-Item -ItemType Directory -Force -Path $CacheDir | Out-Null
$zipPath = Join-Path $CacheDir $ZipName
if ((Test-Path -LiteralPath $zipPath) -and ((Get-Sha256Lower $zipPath) -ne $Sha256Pinned)) {
    Write-Warning "Archive en cache au SHA-256 inattendu : retelechargee."
    Remove-Item -LiteralPath $zipPath -Force
}
if (-not (Test-Path -LiteralPath $zipPath)) {
    $part = "$zipPath.part"
    Write-Host "Telechargement : $Url"
    # curl.exe et non Invoke-WebRequest : get.videolan.org renvoie a PowerShell une page HTML "Downloads"
    # (200, 29 Ko) au lieu de l'archive ; curl.exe suit la redirection vers un miroir officiel VideoLAN.
    # L'integrite ne repose pas sur le miroir : le SHA-256 epingle est verifie ci-dessous.
    $curl = @(Get-Command -Name 'curl.exe' -CommandType Application -ErrorAction SilentlyContinue) | Select-Object -First 1
    if (-not $curl) { throw 'curl.exe introuvable (Windows 10 1803 ou plus recent requis).' }
    & $curl.Source --location --fail --silent --show-error --output $part $Url
    if ($LASTEXITCODE -ne 0) {
        Remove-Item -LiteralPath $part -Force -ErrorAction SilentlyContinue
        throw "Telechargement de $Url echoue (curl.exe, code $LASTEXITCODE)."
    }
    $actual = Get-Sha256Lower $part
    if ($actual -ne $Sha256Pinned) {
        Remove-Item -LiteralPath $part -Force
        throw "SHA-256 de $ZipName different du SHA-256 epingle (attendu $Sha256Pinned, obtenu $actual) : archive rejetee."
    }
    Move-Item -LiteralPath $part -Destination $zipPath -Force
}
Write-Host "Archive verifiee (SHA-256 epingle) : $zipPath"

# 2. Destination : un dossier non vide qui n'est pas le sien n'est jamais ecrase.
if (Test-Path -LiteralPath $Destination) {
    $existing = @(Get-ChildItem -LiteralPath $Destination -Force)
    if ($existing.Count -gt 0 -and -not (Test-Path -LiteralPath (Join-Path $Destination $LicenseName))) {
        throw "Destination non vide sans $LicenseName : refus de l'ecraser ($Destination)."
    }
    Remove-Item -LiteralPath $Destination -Recurse -Force
}
New-Item -ItemType Directory -Force -Path $Destination | Out-Null

# 3. Extraction ciblee : seules les entrees retenues sont lues et ecrites.
Add-Type -AssemblyName System.IO.Compression
Add-Type -AssemblyName System.IO.Compression.FileSystem
$prefix = "vlc-$Version/"
$selected = New-Object System.Collections.Generic.List[object]
$cacheGenDir = Join-Path $CacheDir 'cache-gen'
$cacheGenExe = Join-Path $cacheGenDir 'vlc-cache-gen.exe'
$zip = [IO.Compression.ZipFile]::OpenRead($zipPath)
try {
    foreach ($entry in $zip.Entries) {
        if (-not $entry.FullName.StartsWith($prefix, [StringComparison]::Ordinal)) { continue }
        $rel = $entry.FullName.Substring($prefix.Length)
        $parts = $rel.Split('/')
        if ($rel -eq 'libvlc.dll' -or $rel -eq 'libvlccore.dll') {
            $target = $rel
        } elseif ($rel -eq 'COPYING.txt') {
            $target = $LicenseName
        } elseif ($parts.Count -eq 3 -and $parts[0] -eq 'plugins' -and $parts[2].EndsWith('.dll', [StringComparison]::OrdinalIgnoreCase) -and ($ExcludedCategories -notcontains $parts[1]) -and -not (Test-ExcludedPlugin $parts[1] $parts[2])) {
            $target = $rel.Replace('/', '\')
        } else {
            continue
        }
        $selected.Add([pscustomobject]@{ Entry = $entry; Path = (Join-Path $Destination $target) })
    }
    foreach ($item in $selected) {
        New-Item -ItemType Directory -Force -Path (Split-Path -Parent $item.Path) | Out-Null
        [IO.Compression.ZipFileExtensions]::ExtractToFile($item.Entry, $item.Path, $true)
    }
    # vlc-cache-gen.exe n'est pas livre : il sert ici a produire plugins\plugins.dat (dossier temporaire du cache).
    $cacheGenEntry = $zip.GetEntry($prefix + 'vlc-cache-gen.exe')
    if ($cacheGenEntry) {
        if (Test-Path -LiteralPath $cacheGenDir) { Remove-Item -LiteralPath $cacheGenDir -Recurse -Force }
        New-Item -ItemType Directory -Force -Path $cacheGenDir | Out-Null
        [IO.Compression.ZipFileExtensions]::ExtractToFile($cacheGenEntry, $cacheGenExe, $true)
        foreach ($dll in 'libvlc.dll', 'libvlccore.dll') { Copy-Item -LiteralPath (Join-Path $Destination $dll) -Destination $cacheGenDir }
    }
} finally {
    $zip.Dispose()
}

# Texte LGPL 2.1 : celui du depot (l'archive VLC ne contient que la GPL). Le COPYING de l'archive reste livre.
$lgplSource = Join-Path $repoRoot 'licenses\LGPL-2.1.txt'
if (-not (Test-Path -LiteralPath $lgplSource -PathType Leaf)) { throw "Texte LGPL absent du depot : $lgplSource" }
Copy-Item -LiteralPath $lgplSource -Destination (Join-Path $Destination $LgplName)

foreach ($required in 'libvlc.dll', 'libvlccore.dll', $LicenseName, $LgplName) {
    if (-not (Test-Path -LiteralPath (Join-Path $Destination $required) -PathType Leaf)) {
        throw "Fichier attendu absent de l'archive : $required"
    }
}
$pluginsDir = Join-Path $Destination 'plugins'
$plugins = @(Get-ChildItem -LiteralPath $pluginsDir -Recurse -Filter '*.dll' -ErrorAction SilentlyContinue)
if ($plugins.Count -eq 0) { throw 'Aucun plugin copie : filtre de categories a verifier.' }

# Cache des plugins : evite le scan de tous les plugins au premier libvlc_new (un echec n'est pas bloquant).
if (Test-Path -LiteralPath $cacheGenExe -PathType Leaf) {
    & $cacheGenExe $pluginsDir | Out-Null
    if (-not (Test-Path -LiteralPath (Join-Path $pluginsDir 'plugins.dat') -PathType Leaf)) {
        Write-Warning 'plugins.dat non genere : libVLC scannera les plugins au premier lancement.'
    }
} else {
    Write-Warning 'vlc-cache-gen.exe absent de l archive : plugins.dat non genere.'
}

# 4. Bilan.
$files = @(Get-ChildItem -LiteralPath $Destination -Recurse -File)
$bytes = ($files | Measure-Object -Property Length -Sum).Sum
Write-Host ("libVLC {0} : {1} fichiers, {2:N1} Mo dont {3} plugins, dans {4}" -f $Version, $files.Count, ($bytes / 1MB), $plugins.Count, $Destination)
Write-Host ("Categories de plugins exclues : {0}" -f ($ExcludedCategories -join ', '))
Write-Host ("Plugins exclus : {0}" -f ($ExcludedPlugins -join ', '))
