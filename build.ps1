cargo build --release --target x86_64-pc-windows-msvc
# libVLC (moteur secondaire) dans le bundle que setup\Laststream.iss embarque (meme dossier que son defaut).
& (Join-Path $PSScriptRoot 'scripts\fetch-libvlc.ps1') -Destination (Join-Path $PSScriptRoot '..\dist\laststream-desktop\vlc')
$iscc = @("C:\Program Files (x86)\Inno Setup 6\ISCC.exe", "$env:LOCALAPPDATA\Programs\Inno Setup 6\ISCC.exe") | Where-Object { Test-Path $_ } | Select-Object -First 1
if (-not $iscc) { throw "ISCC.exe introuvable : installer Inno Setup 6" }
& $iscc "setup\Laststream.iss"
