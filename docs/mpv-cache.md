# Tampon mpv du shell Windows (LAST-146)

## Résumé

- Nouveau défaut (`src/laststream_app/laststream_player/player.rs`) : `demuxer-max-bytes` 200 MiB, `demuxer-max-back-bytes` 50 MiB, `cache-secs` et `demuxer-readahead-secs` 67 s. Les secondes valent la borne en octets divisée par 25 Mbit/s, débit le plus élevé visé : elles ne promettent jamais plus que la borne ne permet.
- Ancien défaut : 512 MiB, 64 MiB, 3600 s.
- Cible « pic ≤ 300 Mo » (MiB, tels qu'affichés par Windows) : **non atteinte en 4K**. Le pic 4K reste à 662 Mo avec le nouveau défaut (773 Mo avant).
- La cause n'est pas le tampon seul : la base du décodeur 4K (threads automatiques) pèse seule environ 415 Mo. Aucun réglage de tampon ne tient donc la cible en 4K. Décision à prendre (section « Décisions en attente »).

## Méthode

- Le dépôt ne contient pas de `mpv.exe`, seulement `libmpv-2.dll` (extrait par `build.rs`) et les fichiers d'import. La lecture est donc faite par `examples/mpv_cache_probe.rs` : libmpv2 avec `vo=null`, `ao=null`, `force-window=no`, la même DLL que le shell, et les options de tampon passées en arguments.
- `scripts/throttled-server.mjs` sert le fichier sur 127.0.0.1 au débit donné (Mo/s, support de `Range`).
- `scripts/measure-mpv-memory.ps1` lance le serveur et la sonde en processus cachés, relève `WorkingSet64` et `PrivateMemorySize64` chaque seconde, puis arrête les deux processus. Les sorties vont dans `target/mpv-measure/` (non versionné).
- Fichiers : `bin/ffmpeg.exe` du dépôt (4.4.1-Jellyfin), source `testsrc2`, `libx264 -preset veryfast`, 120 s.
  - 1080p : `-s 1920x1080 -b:v 8M`, 114,5 MiB, servi à 8 Mo/s.
  - 4K : `-s 3840x2160 -b:v 25M`, 358,0 MiB, servi à 12 Mo/s.
- Commande type :
  `pwsh -NoProfile -File scripts\measure-mpv-memory.ps1 -Media target\mpv-measure\testsrc-4k-25M-120s.mp4 -RateMBps 12 -DurationSec 120 -Label new-4k`
  Options par défaut : celles du nouveau défaut. Pour l'ancien : `-Options "cache=yes,cache-secs=3600,demuxer-readahead-secs=3600,demuxer-max-bytes=536870912,demuxer-max-back-bytes=67108864,cache-pause-wait=5"`.

## Mesures (MiB, pic sur 1 s)

| Run | Fichier | Options | Durée | WorkingSet | Privé |
|---|---|---|---|---|---|
| ancien 1080p | 114,5 | 512 MiB / 64 MiB / 3600 s | 120 s | 242,2 | 243,6 |
| nouveau 1080p | 114,5 | 200 MiB / 50 MiB / 67 s | 120 s | 246,3 | 246,7 |
| ancien 4K | 358,0 | 512 MiB / 64 MiB / 3600 s | 120 s | 772,7 | 782,9 |
| nouveau 4K | 358,0 | 200 MiB / 50 MiB / 67 s | 120 s | 662,4 | 673,6 |

Diagnostics 4K (60 s, non retenus pour le critère) :

| Run | Options | WorkingSet | Privé |
|---|---|---|---|
| tampon minimal (1 MiB / 1 MiB / 1 s) | threads auto | 415,1 | 424,7 |
| nouveau défaut + `vd-lavc-threads=1` | 200 MiB / 50 MiB / 67 s | 383,6 | 382,3 |

## Lecture des mesures

- **1080p non discriminant.** Le fichier (114,5 MiB) tient entier sous la borne de 200 MiB : l'ancien et le nouveau défaut donnent le même pic, qui est la taille du fichier plus la base. Pour exercer la borne en 1080p, il faudrait un fichier de plus de 200 MiB (environ 200 s à 8 Mbit/s). Non fait.
- **4K : la base domine.** Avec un tampon minimal, le pic est déjà de 415 Mo. Le nouveau défaut ajoute environ 247 Mo (662 - 415), ce qui correspond à 200 + 50 MiB. L'ancien défaut ajoute tout le fichier (358 MiB), d'où 773 Mo.
- **La durée ne décide pas du pic.** La borne en octets plafonne le tampon quelle que soit la durée de lecture. Estimation : à 12 Mo/s pour un flux de 25 Mbit/s, le tampon avant se remplit en environ 25 s et le tampon arrière en environ 16 s. 120 s suffisent donc à atteindre le plateau. Ce calcul est une estimation, non mesurée séparément.

## Limites

- Flux synthétique `testsrc2` : son débit réel et sa compressibilité ne ressemblent pas à des films ou séries.
- Serveur local : pas de latence ni de pertes réseau.
- `vo=null` : le rendu GPU et le décodage matériel (`hwdec=auto` sans présentation) ne sont pas mesurés. En production, les images décodées en matériel peuvent changer le pic.
- 120 s pour les mesures retenues ; les diagnostics durent 60 s.
- Mesure de la sonde libmpv, pas du processus `laststream-shell-ng.exe` : les autres mémoires du shell (WebView2, serveur) ne sont pas comprises.

## Décisions en attente

1. **Cible 4K (300 Mo) non atteinte.** Options :
   - a) Limiter les threads de décodage (`vd-lavc-threads`, réglage absent aujourd'hui, donc automatique). Avec un thread, le pic 4K passe à 383,6 Mo avec le nouveau tampon. Pour descendre sous 300 Mo, il faudrait un tampon total d'environ 150 Mo (estimation, non mesurée). Ce réglage change la performance de décodage 4K : à mesurer avant de l'adopter.
   - b) Accepter une cible plus haute en 4K (662 Mo au nouveau défaut).
   - c) Autre levier à définir.
2. **Surcharge par l'UI.** Le réglage « Mémoire tampon » de l'UI (`app-221/src/player/bufferLevel.ts`, `mpvBufferProps` : standard 150 MiB, large 512 MiB, max 1 GiB, `cache-secs` 3600) est envoyé par `engines.js:359` via `mpv-set-prop`. Côté shell, `PropKey` (`laststream_player/communication.rs`) ne liste ni `demuxer-max-bytes`, ni `demuxer-max-back-bytes`, ni `cache-secs`, ni `demuxer-readahead-secs`, ni `cache-pause-wait`. Le message est donc refusé (`cannot parse InMsg`, `player.rs`). Constat fait à la lecture du code ; non testé à l'exécution.
   - Ouvrir ces propriétés réactiverait les valeurs de l'UI : 512 MiB au niveau par défaut, donc le pic de l'ancien défaut. Non fait.
   - Si on les ouvre : plafonner via `buffer_options(max_bytes)` (plafond 1 GiB, déjà codé et testé) et aligner `bufferLevel.ts` (repo UI, hors périmètre).
3. **Secondes réduites.** Avec la règle 25 Mbit/s, un flux à 8 Mbit/s est limité à 67 s de tampon au lieu de 200 MiB. Le commentaire de l'UI (`bufferLevel.ts`) juge que les secondes ne doivent pas plafonner les faibles débits. Le changement est appliqué ici ; à valider.
