# Tampon mpv du shell Windows (LAST-146)

## Résumé

- Défaut (`src/laststream_app/laststream_player/player.rs`) : `demuxer-max-bytes` 120 MiB (125829120), `demuxer-max-back-bytes` 30 MiB (31457280), `cache-secs` et `demuxer-readahead-secs` 3600 s. Seule la borne en octets limite la mémoire du tampon.
- Ancien défaut : 512 MiB, 64 MiB, 3600 s. Le défaut intermédiaire 200 MiB, 50 MiB, 67 s n'est pas retenu (4K : pic de 662 Mo).
- 1080p, tampon plein : pic de 278,2 Mo (WorkingSet) et 279,7 Mo (privé). La cible de 300 Mo est tenue.
- 4K : pic de 562,4 Mo (WorkingSet) et 572,7 Mo (privé). La base du décodeur 4K pèse environ 415 Mo.

## Critère

Le défaut tient la cible de 300 Mo en 1080p, tampon plein, mesuré. En 4K, le pic ≈ base du décodeur (~415 Mo, incompressible sans dégrader le décodage) + 150 MiB de tampon ; la part réglable (tampon) reste sous 300 Mo dans les deux cas.

## Méthode

- Le dépôt ne contient pas de `mpv.exe`, seulement `libmpv-2.dll` (extrait par `build.rs`). La lecture est faite par `examples/mpv_cache_probe.rs` : libmpv 0.41 avec `vo=null`, `ao=null`, `force-window=no`, la même DLL que le shell, et les options de tampon passées en arguments.
- Les processus sont lancés cachés (`-WindowStyle Hidden`) : aucune fenêtre visible. `scripts/throttled-server.mjs` sert le fichier sur 127.0.0.1 au débit donné (Mo/s, support de `Range`).
- `scripts/measure-mpv-memory.ps1` relève `WorkingSet64` et `PrivateMemorySize64` chaque seconde, puis arrête la sonde et le serveur. Les sorties vont dans `target/mpv-measure/` (non versionné).
- Fichiers, générés avec `bin/ffmpeg.exe` (4.4.1-Jellyfin, source `testsrc2`, `libx264 -preset veryfast -movflags +faststart`) :
  - 1080p long : `-s 1920x1080 -t 300 -b:v 8M`, 286,2 MiB, servi à 8 Mo/s, lu 180 s.
  - 4K : `-s 3840x2160 -t 120 -b:v 25M`, 358,0 MiB, servi à 12 Mo/s, lu 120 s.
  - `+faststart` place l'index en tête du fichier. Sans lui, mpv lirait la fin du fichier avant de commencer et le tampon ne se remplirait pas en lecture séquentielle.
- Journalisation : `cache-used` n'existe pas dans libmpv 0.41 (erreur `PROPERTY_NOT_FOUND`), la sonde ne pouvait donc pas la lire. Elle lit maintenant `demuxer-cache-state` : `fw-bytes` (octets en avance, ce que borne `demuxer-max-bytes`) et `total-bytes` (avance et retour). Les runs « ancien » ont été faits avant ce correctif : leur log affiche `n/a`, et leurs pics viennent de la sonde précédente (seule la journalisation a changé).
- Commande type :
  `pwsh -NoProfile -File scripts\measure-mpv-memory.ps1 -Media target\mpv-measure\testsrc-1080p-8M-300s.mp4 -RateMBps 8 -DurationSec 180 -Label new-1080p-long`
  Options par défaut : celles du nouveau défaut. Pour l'ancien : `-Options "cache=yes,cache-secs=3600,demuxer-readahead-secs=3600,demuxer-max-bytes=536870912,demuxer-max-back-bytes=67108864,cache-pause-wait=5"`.

## Mesures (MiB, pic sur 1 s)

| Run | Fichier | Tampon / retour / secondes | Durée | WorkingSet | Privé |
|---|---|---|---|---|---|
| ancien 1080p long | 286,2 | 512 / 64 / 3600 s | 180 s | 421,0 | 422,8 |
| nouveau 1080p long | 286,2 | 120 / 30 / 3600 s | 180 s | 278,2 | 279,7 |
| ancien 4K | 358,0 | 512 / 64 / 3600 s | 120 s | 772,7 | 782,9 |
| nouveau 4K | 358,0 | 120 / 30 / 3600 s | 120 s | 562,4 | 572,7 |
| base décodeur 4K (diagnostic) | 358,0 | 1 / 1 / 1 s, threads automatiques | 60 s | 415,1 | 424,7 |
| 4K hwdec (diagnostic) | 358,0 | 120 / 30 / 3600 s, `hwdec=d3d11va-copy` | 120 s | 448,9 | 832,7 |

La base décodeur est une mesure de diagnostic faite lors d'une session précédente : tampon minimal, décodage à threads automatiques, 60 s.

Tampon plein (`fw-bytes` et `total-bytes`, sonde finale) :

- 1080p nouveau : `fw-bytes` à 120,0 MiB (au paquet près) de t=20 s à t=170 s. `total-bytes` entre 144 et 148 MiB de t=60 s à t=170 s.
- 4K nouveau : `fw-bytes` à 120,1 MiB de t=20 s à t=60 s. Après t=100 s, le tampon se vide : le flux de 120 s est lu en entier. `total-bytes` plafonne à 129 MiB, sous le maximum de 150 MiB : le retour ne se remplit pas entièrement dans ce run.

## Lecture des mesures

- **1080p.** Le fichier (286 Mo) dépasse la borne, donc le tampon est plein pendant la mesure. Le pic de 278 à 280 Mo est la base du processus plus 145 à 148 MiB de tampon. L'ancien défaut garde tout le fichier en mémoire (421 Mo). Marge sur la cible : environ 20 Mo.
- **4K.** Le pic de 562 Mo se compose d'environ 415 Mo de base (diagnostic) et de 129 MiB de tampon au plateau. L'écart d'une quinzaine de Mo avec la base diagnostique n'est pas analysé. L'ancien défaut garde tout le fichier (773 Mo).
- **Diagnostic hwdec.** Sur un seul passage, `hwdec=d3d11va-copy` abaisse le WorkingSet de 562 à 449 Mo mais porte le privé de 573 à 833 Mo. Une baisse de la base n'est donc pas établie sur les deux compteurs.
- La mesure 1080p de 114,5 Mo, faite précédemment, tenait entièrement sous la borne : elle ne discriminait rien et est retirée.

## Limites

- Flux synthétique `testsrc2` : son débit et sa compressibilité ne ressemblent pas à des films ou séries.
- Serveur local : pas de latence ni de pertes réseau.
- `vo=null` : le rendu GPU n'est pas mesuré. En production, `hwdec=auto` peut changer la base 4K ; le seul diagnostic (`d3d11va-copy`) montre un effet contraire sur les deux compteurs.
- Une passe par run. Le pic est le maximum d'un échantillon par seconde.
- Mesure de la sonde libmpv, pas du processus `laststream-shell-ng.exe` : WebView2 et le serveur ne sont pas compris.

## Décisions

1. **Tampon 120 MiB + 30 MiB de retour** (`DEMUXER_MAX_BYTES`, `DEMUXER_MAX_BACK_BYTES`). La cible 1080p est tenue (279,7 Mo en privé).
2. **Surcharge « Mémoire tampon » de l'UI inactive.** Le réglage de l'UI (`app-221/src/player/bufferLevel.ts`, `mpvBufferProps` : standard 150 MiB, large 512 MiB, max 1 GiB, `cache-secs` 3600), envoyé par `engines.js:359` via `mpv-set-prop`, est refusé côté shell : `PropKey` (`laststream_player/communication.rs`) ne liste aucune de ces propriétés. Constat fait à la lecture du code, non testé à l'exécution. Suivi : issue séparée LAST-181. Si on les ouvre, plafonner via `buffer_options(max_bytes)` (plafond 1 GiB, déjà codé et testé) et aligner `bufferLevel.ts` (repo UI, hors périmètre).
3. **Secondes à 3600.** `cache-secs` et `demuxer-readahead-secs` ne sont plus dérivées de la borne en octets. À 25 Mbit/s, 67 s coupaient les faibles débits avant la borne. Seule `demuxer-max-bytes` limite la mémoire.
4. **Délai réseau 30 s + reconnexion en fin de flux (LAST-187).** `network-timeout=30` et `reconnect_at_eof=1` (dans `stream-lavf-o`) : une connexion figée, ou fermée par le CDN après inactivité quand le tampon est plein, ne produit aucune erreur, et sans ces réglages mpv restait bloqué.

Piste non retenue : `vd-lavc-threads=1` (diagnostic antérieur : 384 Mo en 4K avec le tampon 200 MiB). Il change le décodage 4K ; à mesurer séparément avant toute adoption.
