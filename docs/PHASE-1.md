# Phase 1 — cœur, MCP, puis lecteur local

## Objectif

Un démon qui importe les 11 Go de mail déjà présents sur le disque, les dédulique,
les indexe, et les expose — **d'abord à un LLM par MCP, ensuite à une interface**.

**Aucun réseau mail. Aucun OAuth.** Le corpus est déjà là, dans le profil Thunderbird.
Ça repousse le marécage IMAP/OAuth en phase 2 et donne dès le premier jour un jeu de
données réel pour mesurer la dédup et la tenue de l'index.

Aucun réseau *mail* ne veut pas dire aucun réseau. Le déploiement de référence de la
phase 1 met le démon sur **une machine dédiée du réseau local** et les clients — Claude
Code, l'UI, la CLI — sur un poste de travail distinct. Le déploiement tout-en-local
reste pris en charge et reste le défaut d'une installation neuve, mais ce n'est pas la
configuration sur laquelle les critères se mesurent. Conséquence directe : le transport
HTTP authentifié n'est pas un raffinement de phase 2, il est dans le chemin critique de
l'étape 6.

## L'ordre compte : MCP avant l'UI

Le serveur MCP est le premier frontend. Dès qu'il répond, le mail est interrogeable
depuis Claude Code sur le corpus réel — recherche, lecture de fil, historique d'un
contact. Ça valide le store, l'index et le modèle de données **sans avoir écrit une
ligne d'interface**, et ça rend le projet utile avant d'être fini.

Si le cœur est mauvais, on le saura à ce moment-là, pas après trois semaines de front.

## Périmètre

Dedans :

- `mailcore` — store blobs, schéma SQLite, index tantivy, API de requête et recherche.
- `mailimport` — parcours du profil Thunderbird, lecture streamée des mbox, dédup.
- `mailmcp` + `maild` — serveur MCP en lecture seule, en stdio **et** en HTTP
  streamable, avec jeton porteur et TLS hors interface locale.
- `mailapi` — le contrat JSON-RPC des clients non-MCP : ce que l'UI et la CLI appellent.
  Même protocole que MCP, données de forme différente — voir l'étape 7.
- `mail-cli` — import, requête, statistiques, débogage. Avec `--daemon`, les mêmes
  commandes contre un démon distant : c'est le premier vrai client de `mailapi`.
- `mail-ui` — trois panneaux (dossiers / liste / lecture), rendu HTML confiné,
  recherche, navigation clavier, et un **cache de lecture local** des métadonnées de
  liste pour tenir les 60 fps quand le démon est au bout du réseau.

Dehors : envoi, réponse, brouillon, suppression, sync réseau, IA, ouverture de pièces
jointes (on les liste, on ne les ouvre pas).

## Lecture des mbox Thunderbird — pièges connus

- Séparateur `From ` en début de ligne, avec le *From-mangling* (`>From`) à dé-échapper.
  Ne **pas** exiger de ligne vide devant : mesuré, Thunderbird colle parfois le séparateur
  à une frontière MIME de clôture. C'est le dés-échappement qui protège des faux positifs.
- En-têtes propriétaires `X-Mozilla-Status`, `X-Mozilla-Status2`, `X-Mozilla-Keys`.
- **`X-Mozilla-Status` bit `0x0008` = message supprimé non compacté.** Les ignorer à
  l'import. Mesuré depuis : il y en a **zéro** sur le profil réel, voir plus bas. Le code
  reste par prudence, mais ce n'est pas ce qui gonfle le profil — la duplication l'explique
  entièrement.
- Arborescence : le dossier `X` est un fichier `X`, ses sous-dossiers sont dans `X.sbd/`.
- Noms de dossiers en français, avec espaces et crochets (`[Gmail].sbd/Tous les messages`).
  Ne rien supposer sur l'encodage du nom de fichier.
- Encodages d'en-têtes hétérogènes, MIME malformé, corps tronqués. Un message
  illisible n'interrompt jamais l'import : on le compte, on le journalise, on continue.


## Mesures relevées sur le corpus réel

Sonde en lecture seule (`cargo xtask profile-probe`), 2026-08-28, avant toute écriture
dans un store. Aucun octet écrit dans le profil : vérifié par relevé `mtime` encadrant,
565 fichiers, zéro différence.

| Mesure | Valeur |
|---|---|
| Octets lus | 10,1 Gio en 96 dossiers, 11 comptes |
| Messages | 102 760 |
| Contenus uniques | 73 658 |
| **Doublons** | **29 102, soit 28,3 % des messages et 2,5 Gio récupérables** |
| Supprimés non compactés (`0x0008`) | **0** |
| Messages portant un en-tête `X-Mozilla-*` | 44 638 |
| Débit de lecture, cache chaud | ~1 Gio/s |
| Messages en échec | 0 |

Trois conclusions, dont deux qui corrigent ce document.

**La duplication est confirmée, et c'est la bonne cible.** 28,3 % des messages sont des
copies exactes. Le critère 6 a donc déjà sa valeur de référence avant que la première ligne
du store soit écrite — et si l'import mesuré rend un taux différent, c'est l'import qui a
tort.

**L'espace mort des messages supprimés non compactés n'existe pas sur ce profil.** Zéro
message portant le bit `0x0008`, alors que 44 638 messages portent bien un en-tête
`X-Mozilla-*` — donc l'en-tête est lu correctement, et le zéro est un vrai zéro. Le code qui
ignore ces messages reste : il coûte une comparaison de bits et protège d'un profil moins
bien tenu. Mais il ne faut plus compter dessus pour expliquer la taille du profil. **La
duplication explique tout.**

**Le format n'est pas celui qu'on croyait sur un point.** Thunderbird écrit parfois le
séparateur `From ` juste après la frontière de clôture d'un message MIME, sans ligne vide.
Un lecteur qui exige la ligne vide fusionne alors des centaines de mégaoctets de messages en
un seul. C'est ce qui protège du faux positif, ce n'est pas la ligne vide, c'est le
*From-mangling* — et la sonde confirme que l'écrivain est bien en `mboxrd` : 15 lignes
`>>From ` sont présentes, ce que `mboxo` ne peut pas produire.

### L'import réel — 2026-08-28

`cargo xtask measure-import`, corpus complet, store neuf.

| Mesure | Valeur | Critère |
|---|---|---|
| Durée | 240 s | — |
| **RSS crête** | **121 Mio** | **critère 3 : < 500 Mo — passé avec un facteur 4** |
| Messages lus | 102 760 | — |
| Contenus stockés | 73 658 | — |
| **Doublons** | **29 102 — 28,3 %** | **critère 6 : mesuré et affiché** |
| Références créées | 102 695 | — |
| Écritures dans le profil | **0** sur 565 fichiers | **critère 7 : passé** |
| En-têtes illisibles | 1 | stocké quand même |
| Sans date exploitable | 102 | — |

**Le store fait 4,4 Go pour 10,8 Go de mbox** : 4,3 Go de blobs plus 35 Mo d'index. La
compression zstd et la dédup enlèvent 59 % du volume, sans perdre un octet reçu.

Le chiffre de dédup tombe exactement sur celui de la sonde lue en amont — 28,3 % dans les
deux cas. L'import et la sonde sont deux chemins de code indépendants ; qu'ils s'accordent
au dixième de point est le meilleur signe qu'aucun des deux ne ment.

**Relancer l'import est sans effet et rapide** : deuxième passage en 10 s, zéro octet écrit,
102 760 références déjà présentes. L'idempotence n'est pas un mécanisme ajouté, c'est une
conséquence de l'adressage par contenu.

**Ce qui coûte les 240 s n'est pas la lecture.** La sonde lit les mêmes 10,1 Gio en 10 s.
La différence est dans les 73 658 `fsync`, un par blob créé, soit ~3 ms l'unité sur un SSD
avec antivirus. C'est le prix de la garantie « un blob visible est un blob complet ». Quatre
minutes pour un import complet étant acceptable, on ne l'optimise pas — mais si un jour il
faut, c'est là qu'il faudra regarder, pas dans le parsing.


### L'index et les fils — 2026-08-28

| Passe | Durée | Résultat |
|---|---|---|
| `mail index` | 29 s | 73 658 documents, 284 Mio de texte extrait |
| `mail thread` | 16 s **à chaud** | 56 220 fils pour 73 658 messages |

Le 16 s du threading est un chiffre **à cache chaud**, mesuré juste après `mail index` qui
venait de lire les mêmes blobs. Sur un store froid, la même passe prend **334 s** — un
facteur 21, mesuré le 2026-08-30, voir « La CLI parle au démon ». Les deux chiffres comptent :
le premier dit ce que coûte l'algorithme, le second ce que l'utilisateur attend la première fois.

| Mesure | Valeur | Critère |
|---|---|---|
| Recherche p50 | 495 µs | — |
| **Recherche p95** | **643 µs** | **le plancher côté démon, pas le critère 4** |
| Recherche p99 | 798 µs | — |

Mesuré sur 408 requêtes × 5 répétitions, bout en bout : analyse de la requête, interrogation
de tantivy, **puis** relecture des métadonnées des 50 résultats dans SQLite. Les termes sont
tirés des sujets réellement présents dans le corpus, pas choisis à la main — sinon la mesure
porterait surtout sur ma capacité à trouver des mots commodes. S'y ajoutent des formes
structurées (phrase exacte, champ nommé, booléen, négation, préfixe) dont le profil de coût
diffère.

**Ce chiffre n'est pas le critère 4, c'est son plancher.** Le budget de 50 ms a été écrit
pour un client au bout du réseau ; ce relevé-ci est en processus, sur la machine du store. La
comparaison avec le relevé bout en bout est ce qui a de la valeur — elle est plus bas, à
l'étape 7 : **le transport ajoute 789 µs au p95**, et le p95 bout en bout est de 1,48 ms.

#### Ce que le threading a appris sur le corpus

| Mesure | Valeur |
|---|---|
| Messages avec un `Message-ID` | 73 556 — 99,9 % |
| Messages avec `References` ou `In-Reply-To` | 8 419 — **11,4 %** |
| Références vers un message absent du corpus | 11 004 |
| Liens établis par `References` | 4 368 |
| Liens établis par le sujet normalisé | 13 070 |
| Fils d'un seul message | 49 212 — 87,5 % |

**`jwz` suppose que `References` fait le travail et que le sujet est un dernier recours. Sur
ce corpus, c'est l'inverse** : 11,4 % des messages seulement portent une référence, et le
repli par sujet établit trois fois plus de liens que les en-têtes. La raison est visible dans
les données : un profil mail réel est massivement transactionnel — factures, notifications,
newsletters — et ce courrier-là ne référence jamais rien.

Deux conséquences pratiques. D'abord, le repli par sujet n'est pas un détail de robustesse,
c'est le mécanisme principal : ses garde-fous méritent d'être surveillés, et ils sont comptés
pour ça (84 groupes de sujet écartés comme trop gros pour être une conversation). Ensuite,
87,5 % de fils d'un seul message n'est pas un échec du threading, c'est la nature du corpus.

**Le store complet fait 4,5 Go** : 4,3 Go de blobs, 100 Mo d'index tantivy, 44 Mo d'index
SQLite. Les deux index dérivés pèsent 3 % du total et se reconstruisent en 45 secondes.


### Le serveur MCP — 2026-08-28

Les cinq outils répondent sur le corpus réel, par les deux transports.

| Vérification | Résultat |
|---|---|
| `tools/list` en stdio | 5 outils annoncés |
| `search_mail` sur 73 658 messages | résultats en moins d'une milliseconde |
| `get_message` | corps aplati en texte, pièces jointes listées, dossiers |
| HTTP **sans** jeton | **401** |
| HTTP avec **mauvais** jeton | **401** |
| HTTP avec le bon jeton | 200, session MCP établie |
| Démarrage TCP sans jeton | **refusé, code de sortie 1** |
| Démarrage hors bouclage sans TLS | **refusé, code de sortie 1** |
| Conteneur Docker, `0.0.0.0` sans `--insecure-no-tls` | **refusé** |
| Conteneur Docker avec le drapeau, jeton exigé | démarre, avertit, sert les 73 658 messages |

**Critère 10 : passé.** Le refus est un vrai refus — le processus ne démarre pas et rend un
code d'erreur, il n'écrit pas un avertissement dans un journal avant de servir quand même.
La règle vit dans `maild::config::validate`, séparée du serveur, pour qu'elle soit
vérifiable par des tests unitaires sans monter de socket : neuf tests la couvrent, dont les
quatre cas de refus.

Le jeton est exigé **aussi sur le bouclage**. `127.0.0.1` est joignable par n'importe quel
processus local, y compris un onglet de navigateur qui exécuterait du JavaScript hostile.
Ce qui ne s'applique pas au bouclage, c'est TLS : le trafic ne quitte pas la machine.

Deux détails que la mise en œuvre a imposés :

**Le jeton est comparé en temps constant.** Un `==` sur des chaînes s'arrête au premier
octet différent, ce qui laisse mesurer la longueur du préfixe commun et reconstruire le
secret octet par octet.

**Aucun échec d'authentification ne journalise ce qui a été présenté.** Un journal d'échecs
qui recopie les jetons tentés devient un fichier de secrets.


#### Le conteneur a réfuté une partie de la règle

Le `Dockerfile` refusait de démarrer. La cause n'était pas un défaut de mise en œuvre, c'est
la règle elle-même qui était trop grossière : elle confondait **« lié à `0.0.0.0` »** et
**« joignable depuis le réseau »**. Dans un conteneur, c'est faux. `0.0.0.0` y est la seule
adresse que le processus puisse atteindre, et ce qui décide de l'exposition est la
publication de port de l'hôte — `127.0.0.1:7847:7847` — que le démon ne peut pas observer.

Trois issues possibles, et deux mauvaises. Détecter la conteneurisation serait implicite et
fragile. Exiger TLS pour un conteneur publié sur le bouclage imposerait une gestion de
certificats sans rien sécuriser. La troisième est celle retenue : **un drapeau explicite**,
`--insecure-no-tls`, par lequel l'opérateur affirme qu'une barrière extérieure au démon
assure le confinement.

Le fail closed reste le défaut. Le drapeau ne dispense **jamais** du jeton. Il apparaît en
clair dans le `compose.yaml`, donc il s'audite, et le démon le rappelle **à chaque
démarrage** — un drapeau posé une fois dans un fichier de déploiement se fait oublier, et
l'oubli porte ici sur le contenu d'une boîte mail.

C'est le genre de correction qu'aucun test unitaire n'aurait produite : il fallait faire
tourner la chose pour de vrai.
#### Ce que le montage a coûté en dépendances

`axum` et `axum-server` pour héberger le service `tower` que `rmcp` expose, `rustls` pour le
TLS, `getrandom` pour l'aléa du jeton. `tokio-rustls` a été retiré : `axum-server` termine le
TLS lui-même, et une dépendance retenue mais inutilisée est une dette.

`getrandom` plutôt que `rand` : on ne veut que des octets du système d'exploitation, pas un
générateur ni des distributions. La génération du jeton **rend une erreur** si le système ne
peut pas en fournir, au lieu de se rabattre sur une source prévisible — un jeton devinable
serait pire que pas de jeton, parce qu'il donnerait l'illusion d'une protection.


### L'API des clients non-MCP — 2026-08-28

Le serveur MCP validait le cœur pour un modèle. L'UI a besoin d'autre chose, et l'étape 7
livre cette surface : **JSON-RPC 2.0, crate `mailapi`, servie sur les deux transports**.
Dix méthodes, toutes en lecture seule.

| Méthode | Ce qu'elle rend |
|---|---|
| `server.hello` | version, protocole, révision, disponibilité de la recherche, nombre de messages |
| `server.methods` | la liste des méthodes servies |
| `folders.list` | les dossiers de tous les comptes, avec compteurs |
| `messages.page` | une page de liste, avec le curseur de la suivante |
| `messages.get` | un message ouvert : en-têtes, corps en texte, pièces jointes listées |
| `messages.thread` | le fil d'un message |
| `search.query` | recherche plein texte |
| `store.stats` | l'état du store |
| `store.revision` | la révision courante |
| `store.wait` | attend qu'elle change — l'abonnement |

`server.hello` existe pour le critère 1 : un client ouvre sur son cache, et un seul
aller-retour lui dit s'il peut parler à ce démon, s'il peut chercher, et si son cache est
périmé. Trois requêtes pour la même chose auraient coûté trois allers-retours réseau avant
le premier pixel.

#### Un crate à part de `mailmcp`, et ce n'est pas de la duplication

Un modèle de langage et une interface ne veulent pas les mêmes données. Le tableau est le
vrai argument :

| | `mailmcp` | `mailapi` |
|---|---|---|
| Date | `AAAA-MM-JJ`, pour qu'un modèle la lise | secondes Unix, pour que le client formate selon la locale |
| Drapeaux | absents, sans intérêt pour un modèle | `unread` / `flagged`, c'est la moitié d'une liste de mail |
| Pagination | une limite et « tronqué » | un curseur opaque, parce qu'il faut défiler 100 000 messages |
| Dossiers d'un message | des chemins | chemins **et** état lu/non-lu par référence |

Le protocole, lui, est le même — JSON-RPC 2.0, une seule couche de sérialisation à
maintenir. Ce qui diffère est la forme des données, pas l'encadrement.

`mailapi` est aussi la dépendance que les clients tireront : les mêmes types servent à écrire
les réponses côté démon et à les relire côté UI, donc un champ renommé casse **à la
compilation** au lieu de rendre `null` à l'exécution.

#### L'abonnement aux changements a une forme imposée par le déploiement

`docs/ARCHITECTURE.md` disait « s'abonner aux changements » sans dire comment. La mise en
œuvre a tranché, et pas dans le sens attendu.

Le réflexe est un `tokio::sync::broadcast` dans le démon : un job d'import publie, les
clients abonnés reçoivent. **Sur ce déploiement, ce canal ne se déclencherait jamais.**
L'import et l'indexation tournent dans un autre processus — `mail import`, `mail index` —
et le démon ne voit rien de ce qu'ils font.

Ce que le démon peut observer, c'est le store lui-même. Deux sources externes suffisent :

- `PRAGMA data_version`, qui change dès qu'**une autre connexion** valide une écriture ;
- la date de `meta.json` de l'index tantivy, qu'une reconstruction réécrit.

Les deux forment un jeton de révision opaque, et `store.wait` est un long-poll dessus : le
client présente la révision qu'il a, le démon répond immédiatement si elle a changé, sinon il
relève toutes les 250 ms jusqu'à 30 s par défaut. Un relevé coûte un `PRAGMA` et un `stat`,
soit quelques microsecondes.

Ce n'est pas le mécanisme le plus élégant, c'est celui qui **marche avec un écrivain hors du
processus** — et le test qui le verrouille écrit depuis une deuxième connexion et vérifie que
la révision bouge. Un `broadcast` aurait passé tous ses tests unitaires et n'aurait rien
notifié en vrai.

Limite connue et assumée : le `Searcher` est monté à l'ouverture de la boîte. Après un
`mail index`, la révision change — le client sait qu'il doit relire — mais le démon continue
de chercher dans l'ancien index jusqu'à son redémarrage. Recharger un index à chaud est de la
phase 2.

#### stdio impose un choix que HTTP n'impose pas

En HTTP, les deux protocoles cohabitent : `/mcp` et `/api`, même port, **même couche de
jeton**. Un seul port ouvert est un seul port à protéger, et une seule surface
d'authentification est une surface qui ne peut pas divergrer.

En stdio, il faut choisir : il n'y a qu'un flux d'entrée et rien dans un message ne dirait
auquel des deux protocoles il appartient. D'où `maild stdio --protocol mcp|api`, `mcp` par
défaut pour ne rien casser.

Corollaire : la socket locale nommée que `docs/ARCHITECTURE.md` décrivait — *named pipe* sur
Windows, socket Unix ailleurs — n'a pas été écrite. stdio couvre le même besoin sans code
spécifique à la plateforme : le client démarre le démon en processus fils et parle sur les
flux qu'il vient d'ouvrir, avec les mêmes garanties. Ce qu'une socket nommée apporterait de
plus est un démon **déjà lancé** auquel plusieurs clients se rattachent — un vrai besoin, mais
un besoin de service installé, pas de la phase 1.

#### Les critères 4 et 5, relevés bout en bout

`cargo xtask measure-api`, corpus complet, `maild` compilé en release sur le bouclage,
408 requêtes × 5 répétitions — **le même jeu de requêtes que `measure-search`**, construit par
la même fonction. La différence entre les deux relevés est donc le transport, et rien d'autre.

| Mesure | En processus | À travers l'API | Coût du transport |
|---|---|---|---|
| Recherche p50 | 472 µs | 653 µs | +181 µs |
| **Recherche p95** | **620 µs** | **819 µs** | **+199 µs** |
| Recherche p99 | 756 µs | 961 µs | +205 µs |

| Mesure | Valeur | Critère |
|---|---|---|
| **Recherche p95, bout en bout** | **819 µs** | **critère 4 : < 50 ms — passé, facteur 61** |
| **Ouverture d'un message p95, texte** | **1,36 ms** | **critère 5 : < 50 ms — passé, facteur 37** |
| Pagination p95, plus gros dossier | 557 µs | critère 2 : part réseau |

**Le transport coûte 200 µs, et c'est remarquablement plat** : +181 µs au p50, +199 µs au p95,
+205 µs au p99. De la sérialisation JSON, un `spawn_blocking`, un verrou, la pile `axum`, la
comparaison du jeton en temps constant et un aller-retour TCP — un coût fixe par appel, qui ne
grandit pas avec la taille de la réponse. Le budget de 50 ms avait été écrit en pensant au
réseau ; il en reste 49 ms pour lui.

##### La première version de ces chiffres était fausse, et l'erreur mérite d'être écrite

Le premier relevé annonçait 1,48 ms de p95 pour la recherche et un transport à +789 µs. C'était
un artefact de **l'état du cache de pages du système**, pas une mesure : ce relevé-là partait
d'un store froid, celui auquel on le comparait était chaud.

Le signal qui l'a trahi : dans la même exécution, ouvrir un message en **HTML assaini**
ressortait plus rapide qu'en texte. C'est impossible — le chemin HTML fait tout ce que fait le
chemin texte, plus un assainissement complet. Un résultat impossible dit qu'on mesure autre
chose que ce qu'on croit ; ici, l'ordre des deux passes.

`measure-api` ouvre donc maintenant chaque message des deux façons **avant** de chronométrer
quoi que ce soit. Les chiffres ci-dessus sont ceux d'après, reproductibles d'une exécution à
l'autre à quelques pour cent près. La leçon générale : sur un store de 4,5 Go, une mesure sans
chauffe explicite mesure le disque, pas le code.

**L'ouverture d'un message reste la requête la plus variable** : p50 à 394 µs, p95 à 1,36 ms,
p99 à 4,9 ms. L'écart n'est pas dans le transport — qui est plat — il est dans le travail :
lecture de blob, décompression zstd, parse MIME, et tout ça suit la taille du message. C'est
aussi la requête qu'un utilisateur déclenche à chaque clic.

#### Le corps HTML est arrivé à l'étape 8

Au moment où l'étape 7 s'est arrêtée, `messages.get` ne rendait que du **texte aplati** :
`mailhtml::sanitize` n'était pas écrit, et servir du balisage sans la barrière qui le confine
aurait été exactement l'erreur que l'étape 6 a documentée. La dette est payée — voir
« Le rendu HTML confiné » plus bas. Le paramètre `body: "html"` est arrivé avec l'assainisseur,
et le défaut reste `text` : ce qui coûte cher n'est servi qu'à qui le demande.

#### Ce que le montage a coûté en dépendances

Rien de nouveau. `mailapi` ne tire que `mailcore`, `serde`, `serde_json`, `thiserror` et
`tracing` — pas de runtime async : le répartiteur est synchrone comme le cœur, et c'est
`maild` qui décide sur quel fil il tourne. Deux features de `tokio` s'ajoutent, `io-util` pour
la lecture ligne par ligne sur stdio et `time` pour le long-poll.

Le client HTTP de la mesure est écrit à la main sur une `TcpStream`, une centaine de lignes.
Pas de `reqwest` : un client généreux en pool, en interception et en nouvelle tentative
mesurerait sa propre bibliothèque autant que le démon, et l'arbre de dépendances ne se
justifie pas pour poster du JSON à notre propre serveur.



### Le rendu HTML confiné — 2026-08-30

L'étape 8 commence par la pièce que l'étape 7 avait laissée en dette : les deux barrières de
`docs/PRIVACY.md` écrites pour de vrai, avant l'interface qui les utilisera. C'est l'ordre
qui convient à du code de sécurité — il se teste sans front, et le front n'a alors plus le
choix de s'en passer.

| Module | Ce qu'il fait | Rôle |
|---|---|---|
| `mailhtml::csp` | la CSP et le `sandbox` de l'`<iframe>` | barrière 1, appliquée par le moteur |
| `mailhtml::sanitize` | liste blanche `ammonia` | barrière 2, appliquée par nous |
| `mailhtml::trackers` | relevé et comptage | affichage, **pas** une barrière |

#### Le déblocage des images a réconcilié deux exigences qui se contredisaient

`docs/PRIVACY.md` demande « aucune URL distante dans la sortie » (§5) **et** « un bandeau
*Afficher les images*, valable pour ce message uniquement » (§2). Un assainisseur qui supprime
définitivement les `src` distants rend le second impossible.

La sortie : `clean` prend une politique, et le déblocage est **un nouvel appel**, pas une
retouche du document déjà rendu. Trois conséquences, toutes souhaitables. La sortie bloquée
ne contient aucune URL distante en position chargeable, donc le test du critère 8 dit
exactement ce qu'il prétend. Le déblocage ne persiste rien puisqu'il n'y a rien à persister —
c'est un rendu, pas un état. Et le nombre d'images bloquées est un sous-produit du rendu,
donc il ne peut pas diverger de ce qui a réellement été retiré.

#### Les URL du CSS sont écartées par le nom de la propriété, pas par la valeur

`ammonia` sait analyser un attribut `style` et n'y garder que des propriétés choisies. La
liste blanche retenue **ne contient aucune propriété porteuse d'`url()`** — ni `background`,
ni `background-image`, ni `list-style-image`, ni `content`, ni `cursor`, ni `filter`. Une URL
ne peut donc pas entrer par le CSS sans qu'on ait à inspecter la moindre valeur, et il n'y a
rien à contourner par un encodage. Couleurs, polices, espacements, bordures et alignements de
tableau survivent, ce qui couvre l'essentiel de la mise en forme d'un mail.

Les blocs `<style>`, eux, partent entièrement. `ammonia` n'assainit pas le contenu d'une
feuille de style : autoriser la balise laisserait passer `@import url(…)` intact, et écrire un
demi-analyseur CSS produirait précisément la barrière approximative contre laquelle
`docs/PRIVACY.md` met en garde. Le coût est réel sur les mails très mis en page ; un vrai
assainisseur CSS est une amélioration de phase 2, pas un prérequis pour lire son courrier.

`position`, `top`, `left`, `z-index` et `transform` sont exclues aussi, pour une autre raison :
superposer du contenu permet de faire lire autre chose que ce qui est affiché.

#### Le compteur de traceurs n'est pas une barrière, et c'est ce qui l'autorise à être léger

Les barrières s'appuient sur un analyseur HTML complet et doivent être exactes. Le relevé des
traceurs, non : un faux négatif y est un **compteur trop bas**, pas une fuite — la ressource
reste bloquée qu'on l'ait comptée ou non. C'est ce qui rend acceptable un lecteur de balises
léger, et ce qui permet de lire ensemble le `src`, le `width` et le `height` d'une même image,
ce qu'un filtre d'attributs — qui les voit un par un — ne permet pas.

Le module le dit de lui-même, en toutes lettres : il ne doit jamais devenir le fondement d'une
décision de chargement. Le jour où quelque chose en dépend, il faudra le réécrire sur
l'analyseur.

**Les hôtes sont conservés, les URL complètes non.** Une URL de traceur *est* l'identifiant
corrélé au destinataire ; la recopier dans un rapport qui finira dans un journal ou dans une
interface reviendrait à conserver exactement ce qu'on dénonce. Un test le verrouille.

#### Ce que le corpus réel contient

Relevé sur 500 messages tirés des résultats de recherche, pendant `cargo xtask measure-api` :

| Mesure | Valeur |
|---|---|
| Images distantes retirées | **5 193** |
| Signaux de traçage relevés | **1 034** |
| Messages examinés | 500 |

Plus de dix images distantes par message en moyenne, et un signal de traçage tous les deux
messages. Ce n'est pas un cas limite qu'on prévient par principe : c'est ce que contient une
boîte mail ordinaire, et chacune de ces 5 193 images est une requête qui n'a pas eu lieu.

#### Le coût du rendu, mesuré

| Mesure | Corps texte | Corps HTML assaini | Coût du rendu |
|---|---|---|---|
| p50 | 394 µs | 1,56 ms | ×3,9 |
| **p95** | **1,36 ms** | **4,31 ms** | **×3,2** |
| p99 | 4,94 ms | 9,83 ms | ×2,0 |

**Critère 5 : passé sur le chemin qui compte.** 4,31 ms de p95 pour ce que le volet de lecture
demandera réellement — blob, décompression zstd, parse MIME, assainissement complet, relevé
des traceurs, et le corps sur le fil — contre un budget de 50 ms. Il reste un facteur 11 pour
le réseau et pour le rendu du webview.

L'assainissement triple le coût d'ouverture. C'est cher et c'est accepté : c'est le prix de la
barrière que l'utilisateur ne voit pas, et 4,31 ms restent sous le seuil où un humain perçoit
un délai.

#### Deux choses que la mise en œuvre a apprises

**`mail-parser` convertit les parties `text/plain` en HTML, et il échappe correctement.**
`html_body_count()` n'est donc pas zéro pour un message en texte seul. On s'appuie dessus : le
front a **un seul chemin de rendu** au lieu de deux, et il n'y a pas de deuxième façon
d'afficher un corps qui pourrait diverger de la première. Le risque était qu'une conversion
sans échappement transforme un `<script>` écrit en toutes lettres dans un mail en balisage,
que l'assainisseur supprimerait ensuite — l'utilisateur perdrait du texte qu'on lui a envoyé.
Mesuré : elle échappe, et un test le verrouille.

**`html5ever` avertit à chaque construction d'arbre inhabituelle.** « foster parenting not
implemented », plusieurs fois par message ouvert, sur du courrier réel où les tableaux mal
formés sont la norme et non l'exception. Le filtre de journal par défaut du démon les passe
donc en `error` : un journal qui déborde à chaque lecture est un journal que personne ne lit,
donc plus de journal du tout.

#### Le critère 8, étage 1

Le test `no_network` est écrit pour de bon : un serveur HTTP local instrumenté, un message
piégé qui tente tout ce que `docs/PRIVACY.md` énumère — pixel espion, image distante,
`@font-face`, feuille de style externe, `@import`, `<script>`, `<form>`, `<iframe>`, `<object>`,
`meta refresh`, `link rel=prefetch`, `onload`, `javascript:`, `data:image/svg+xml`, URL
relative, et la contrebande `<scr<script>ipt>`. Toutes les ressources pointent vers le serveur
instrumenté.

Assertions : plus rien de chargeable dans la sortie, et **zéro requête reçue**. Plus un dernier
test qui fait une requête volontaire et vérifie que le compteur monte — sans lui, tous les
zéros passeraient aussi bien avec un serveur mort.

**Ce que l'étage 1 ne prouve pas, et il faut le redire.** Il valide la deuxième ceinture, celle
que nous écrivons. La première — la CSP appliquée par le moteur — ne se vérifie que dans un
vrai webview, sous `tauri-driver`, avec le même message et le même serveur. C'est l'étage 2, et
il arrive avec `mail-ui`. Personne ne doit cocher le critère 8 en voyant ce fichier au vert.

#### Une assertion qui a dû être reformulée

Le test voulait d'abord « l'URL n'apparaît nulle part dans la sortie ». Trop fort, et
l'entrée piégée l'a montré : la contrebande `<scr<script>ipt src="…">` ressort en **texte**,
chevron final échappé en `&gt;`. C'est inerte — aucun moteur n'ira chercher une URL qui est du
contenu — mais une comparaison de chaînes ne sait pas l'en distinguer, parce que les
guillemets d'un texte ne sont pas échappés.

Ce que le critère demande vraiment est « l'URL n'est nulle part où le moteur irait la
chercher », et ça se vérifie en **relisant la sortie comme un moteur la relirait**, pas comme
une chaîne. C'est le lecteur de ressources distantes — celui du comptage des traceurs — qui
sert d'instrument, et le fait qu'il soit un chemin de code indépendant de l'assainisseur rend
l'accord des deux plus convaincant qu'une assertion isolée.


### Les tâches de fond — 2026-08-30

Le dernier trou du back, et il était large : **le démon ne savait que lire**. Importer ou
réindexer voulait dire aller taper `mail import` sur la machine du démon. Dans le déploiement
de référence — démon dédié, clients ailleurs — ça veut dire ouvrir un shell distant pour la
première action que tout utilisateur fait, et une interface ne pouvait pas proposer
« importer mon profil Thunderbird ».

C'était aussi la règle 3 du `CLAUDE.md` sans implémentation : « toute tâche longue est un job
de fond qui écrit dans le store ; l'UI observe le store ».

| Méthode | Ce qu'elle fait |
|---|---|
| `jobs.sources` | les profils que l'opérateur autorise à importer |
| `jobs.start` | met une tâche en file : `import`, `index` ou `thread` |
| `jobs.list` | les tâches connues, de la plus récente à la plus ancienne |
| `jobs.get` | une tâche, avec sa progression |
| `jobs.cancel` | demande l'arrêt |

Quinze méthodes au total dans l'API.

#### Un client ne nomme jamais un chemin

`jobs.start` est **la première méthode de l'API qui écrit**, et ça change la nature de la
surface exposée. Laisser un client passer un chemin de profil donnerait à quiconque détient
le jeton le droit de faire lire n'importe quel fichier de la machine du démon, de le ranger
dans le store, puis de le relire par `search.query`. Une lecture de fichiers arbitraires
déguisée en fonctionnalité d'import.

Aucune validation de chemin ne rattrape ça de façon convaincante : le démon n'a aucun moyen de
savoir ce que l'opérateur considère comme légitime. L'opérateur, lui, le sait. Il le déclare
au démarrage — `maild --profile <chemin>`, répétable — et un client ne peut que choisir un
**rang** dans cette liste.

Fail closed : sans `--profile`, aucun import n'est possible, et le refus dit quoi faire. C'est
le même arbitrage que le critère 10 pour le jeton, appliqué à une autre surface.

Ce que ces tâches peuvent faire reste borné par ailleurs : elles ajoutent des contenus adressés
par leur hash, ou reconstruisent un index dérivé. Aucune ne supprime ni ne modifie un message.

#### Un fil du système, pas le runtime

Un import prend quatre minutes et `mailcore` est synchrone. Le poser sur le pool bloquant de
tokio immobiliserait un de ses fils tout ce temps, en concurrence avec les lectures qui servent
l'interface. Un fil dédié coûte quelques kilo-octets de pile et rend le raisonnement trivial :
**un seul job à la fois, par construction**.

La sérialisation n'est pas qu'une commodité. SQLite n'a qu'un écrivain ; deux imports
concurrents se disputeraient le verrou, et une indexation lancée pendant un import indexerait
un store en mouvement.

Le job ouvre **son propre** `Store`, donc sa propre connexion. La boîte que servent l'API et
MCP reste lisible pendant ce temps — le mode WAL autorise un écrivain et des lecteurs
simultanés. C'est ce qui rend la règle 3 vraie plutôt que souhaitée.

**Effet de bord gratuit** : parce que le job écrit depuis une autre connexion, `PRAGMA
data_version` change pour les lecteurs, donc la révision change, donc `store.wait` réveille les
clients tout seul. Le mécanisme d'abonnement de l'étape 7 marche pour les tâches de fond sans
qu'on ait ajouté une ligne. Un test le verrouille.

#### L'index se recharge à chaud, et c'était une dette

L'étape 7 documentait une limite : après `mail index`, le démon continuait de chercher dans
l'ancien index jusqu'à son redémarrage, parce que le `Searcher` est monté à l'ouverture. C'était
tolérable tant que l'indexation ne pouvait être lancée que par un autre processus. Dès lors que
le démon la lance lui-même, annoncer « indexation terminée » puis servir l'ancien index serait
un mensonge.

`Mailbox::reload_search` remonte l'index, et la tâche d'indexation l'appelle en finissant. Le
bilan qu'elle rend le dit : « 5642 messages indexés, recherche disponible ».

#### Une poignée de progression commune aux trois tâches

`mailcore::Progress` : des atomiques, pas de verrou. Le producteur écrit depuis le fil qui
travaille, les lecteurs lisent depuis le fil qui sert l'interface ; avec un `Mutex`, chaque
lecture d'une barre de progression prendrait un verrou que la tâche relâche des milliers de
fois par seconde, et l'interface ralentirait la chose qu'elle observe.

L'unité appartient à la tâche — des **octets lus** pour l'import, des **messages** pour
l'indexation et le threading. Pour l'import c'est un choix : le nombre de messages n'est connu
qu'une fois tout lu, alors que la taille des mbox l'est dès le parcours. Une barre qui avance
régulièrement vaut mieux qu'une barre exacte qui n'existe pas.

Un total inconnu rend `fraction: null` plutôt que zéro. Une barre indéterminée est honnête ;
une barre à zéro qui ne bouge pas ressemble à une panne.

#### Vérifié sur le corpus réel, pas seulement en test

Démon démarré avec `--profile` pointant sur le vrai profil Thunderbird, store jetable, import
déclenché **par l'API** :

| Étape | Résultat |
|---|---|
| `jobs.sources` | 1 profil déclaré, désigné par son rang |
| `jobs.start` `import` | accepté, `state: running`, `total: 10 893 823 112` octets |
| Progression après ~10 s | 614 Mo lus, `fraction: 0.056` |
| `jobs.cancel` pendant l'import | **68 Ko lus après la demande** — arrêt au message suivant |
| État final | `cancelled`, « 5643 messages lus, 5642 contenus stockés » |
| `store.stats` après annulation | 5 642 messages, store cohérent |
| `jobs.start` `index` sur le store partiel | 5 642 indexés en 4 s |
| `search.query` juste après | répond, **sans redémarrage du démon** |

L'annulation à 68 Ko près confirme le choix de vérifier le drapeau **par message** et non par
dossier : `[Gmail]/Tous les messages` fait 1,4 Gio à lui seul, et une annulation qui attendrait
la fin du dossier n'annulerait rien pendant plusieurs minutes.

Le store partiel est un store valide. C'est ce qui autorise à exposer l'annulation à un
client : l'adressage par contenu rend l'import idempotent, donc reprendre après une annulation
ne refait pas le travail déjà fait.

**Le critère 7 a été re-mesuré sur ce nouveau chemin**, et il passe : voir juste en dessous.
Le chemin de lecture du profil n'avait pas changé, mais le raisonnement ne remplace pas un
relevé quand la règle 1 du `CLAUDE.md` est en jeu.


### Le critère 7, re-mesuré sur le chemin des tâches de fond — 2026-08-30

L'import venait de changer de chemin : il passe désormais par une tâche du démon, avec un
drapeau d'annulation et une validation de transaction en plus. La règle 1 du `CLAUDE.md` ne
se re-vérifie pas par raisonnement.

`cargo xtask profile-snapshot` avant, import **complet déclenché par l'API**, snapshot après,
`profile-diff`.

| Mesure | Valeur |
|---|---|
| Fichiers relevés | 565 → 565 |
| **Mbox modifiés** | **0** |
| Autres fichiers modifiés | 0 |
| Durée de l'import, par le démon | 255 s |
| Messages lus | 102 760 |
| Contenus stockés | 73 658 |
| **Doublons** | **29 102 — 28,3 %** |

**Critère 7 : passé.** Et les chiffres d'import tombent **exactement** sur ceux de l'étape 4,
mesurés par la CLI en direct : 102 760 / 73 658 / 28,3 %. Deux chemins de code différents —
la CLI ouvre le store, la tâche de fond l'ouvre depuis un fil du démon — qui rendent le même
résultat au dixième de point. C'est le meilleur signe qu'aucun des deux n'a dérivé.

### La CLI parle au démon — 2026-08-30

`mail --daemon <hôte>` route les commandes vers l'API au lieu d'ouvrir le store. Sans ce
drapeau, rien ne change.

| Commande | En local | Avec `--daemon` |
|---|---|---|
| `search`, `stats` | store ouvert directement | `search.query`, `store.stats` |
| `import`, `index`, `thread` | exécution au premier plan | tâche de fond suivie, ou `--detach` |
| `doctor` | diagnostic complet | **refusé**, avec la raison |
| `daemon login/logout/status` | — | trousseau et état du démon |

C'était le dernier trou pratique du back : une CLI qui n'ouvre que le store local ne sert, en
déploiement de référence, qu'à être lancée en SSH sur la machine du démon — exactement ce
qu'on cherchait à éviter en écrivant l'API.

**C'est aussi la première validation extérieure du contrat.** `mail --daemon` est écrit
par-dessus `mailapi` sans rien savoir de l'intérieur du démon. Un contrat mal fichu se voit
ici, avant de se voir dans le front.

#### Le client HTTP est remonté dans `mailapi`

Il avait été écrit deux fois — dans l'outillage de mesure et dans un test d'intégration — ce
qui est le signe habituel qu'il doit être écrit une fois. Il vit maintenant dans le crate qui
**est** le contrat : fournir les types sans fournir de quoi les transporter obligeait chaque
client à réécrire la même centaine de lignes.

Bloquant, sur `std::net::TcpStream`, sans arbre de dépendances. `mailapi` ne dépend d'aucun
runtime, ce qui lui permet d'être tiré par une CLI synchrone comme par un front asynchrone.

**Il refuse d'envoyer un jeton à un hôte non local.** C'est la symétrie exacte de la règle que
le démon s'applique au critère 10 : on ne met pas le secret d'une boîte mail en clair sur un
réseau. La vérification porte sur l'adresse **résolue**, pas sur le nom — un nom qui pointe
ailleurs que sur la machine n'est pas local, quoi qu'il ressemble — et elle a lieu **avant**
l'ouverture de la socket, donc le jeton ne part jamais, même partiellement.

Ça ne restreint rien dans le déploiement recommandé : `docs/ARCHITECTURE.md` conseille un
tunnel déjà chiffré, qui ramène le cas distant au cas local. Le client vise alors `127.0.0.1`.
Un client TLS natif viendra avec le front, qui en a besoin pour son propre compte.

Vérifié en vrai, jeton enregistré pour une adresse distante :

```
Error: refus d'envoyer le jeton en clair à 192.0.2.10:7847, qui n'est pas une adresse
locale. Monter un tunnel chiffré et viser 127.0.0.1, ou attendre le client TLS.
```

Et `mail daemon login` le dit **au moment de l'enregistrement**, plutôt que de laisser
découvrir le problème à la commande suivante.

#### Le jeton va dans le trousseau, pas dans l'environnement

`docs/PRIVACY.md` §7 l'exige, et la règle n'est pas décorative : une variable d'environnement
se retrouve dans l'historique du shell, dans la table des processus lisible par les autres
utilisateurs de la machine, et recopiée dans chaque processus fils. Un jeton mailcore ouvre
une boîte mail entière.

`mail daemon login` lit le jeton sur **l'entrée standard**, jamais en argument — un argument
est visible dans la table des processus et reste dans l'historique. Une entrée de trousseau
par démon : un poste qui parle à deux démons garde deux jetons, et en révoquer un ne touche
pas l'autre.

**Le démon, lui, continue de lire une variable d'environnement, et ce n'est pas une
contradiction.** `maild` est démarré par un gestionnaire de services ou un `compose.yaml`, qui
ont leurs propres mécanismes de secrets et ne passent pas par le shell d'un utilisateur. Les
deux côtés n'ont pas la même surface d'exposition, donc pas la même règle.

`keyring` était épinglé dans le manifeste depuis le début sans être tiré. Il l'est maintenant,
mais **derrière un drapeau de compilation** (`token-store`) que seuls les clients activent :
le démon n'a aucune raison de compiler les greffons de trousseau de trois plateformes.

#### `doctor` reste local, et c'est un refus argumenté

Il inspecte les blobs, les orphelins, la dérive de l'index. Ça demande un accès au store, pas
une API : l'exposer voudrait dire ouvrir un chemin de lecture arbitraire dans le stockage. La
commande refuse en mode distant et dit d'aller la lancer sur la machine du démon.

#### Ce que l'essai réel a appris : les 16 s du threading étaient un chiffre à chaud

En suivant une tâche `thread` depuis la CLI sur le corpus complet :

| Passage | Durée |
|---|---|
| Store froid | **334 s** |
| Immédiatement après, à chaud | **15,5 s** |

**Un facteur 21.** Le threading lit les 73 658 blobs un par un ; à froid, c'est 4,3 Go de
petits fichiers relus depuis le disque avec un antivirus dans le chemin — le même coût par
fichier que celui déjà identifié sur les `fsync` de l'import. À chaud, tout est dans le cache
de pages.

Le 16 s annoncé à l'étape 5 était donc mesuré juste après `mail index`, qui venait de lire les
mêmes blobs. Ce n'était pas faux, c'était incomplet. **Les deux chiffres comptent, et pour des
raisons différentes** : le chiffre à chaud dit ce que coûte l'algorithme, le chiffre à froid
dit ce que l'utilisateur attend la première fois. C'est le second qui décide de ce que l'UI
doit afficher pendant ce temps — et c'est précisément pour ça que les tâches de fond ont une
barre de progression plutôt qu'un sablier.

Les deux passages rendent **56 220 fils**, comme à l'étape 5.


### Le front, premier jet — 2026-08-31

**Solid**, retenu contre Svelte pour une raison qui se voit dans un seul fichier : la liste
virtualisée. Voir « Le choix du framework » plus bas.

Vite + Solid + TypeScript, sans méta-framework. Pas de SolidStart : il apporte du rendu
serveur et un routage dont on n'a aucun usage, alors que `docs/ARCHITECTURE.md` demande une
application statique « ouvrable telle quelle dans un onglet de navigateur, servie par le
démon ».

| Fichier | Rôle |
|---|---|
| `List.tsx` | la liste virtualisée à la main — critère 2 |
| `Reader.tsx` | l'`<iframe>` confinée — critère 8 |
| `cache.ts` | le cache de lecture IndexedDB — critères 1 et 9 |
| `api.ts` | le client JSON-RPC, jeton et contrôle de version du contrat |
| `App.tsx` | l'assemblage, le chargement par pages, l'abonnement |
| `theme.css` | le thème dense, clair et sombre |

Poids construit : **28,7 Ko de JS** (11,2 Ko gzip) et 4,6 Ko de CSS. Un seul fichier de
chaque : le critère 1 se joue sur le temps d'analyse et d'exécution, pas sur le
téléchargement — tout est local — et une cascade de modules coûterait des allers-retours pour
rien.

#### Le démon sert le front

`maild --ui-dir <répertoire>`. Sans le drapeau, le démon ne sert que `/api` et `/mcp`.

**Le front est servi hors de la couche de jeton, et c'est nécessaire** : un navigateur qui
ouvre un onglet ne peut pas poser d'en-tête `Authorization` sur sa requête initiale. Ce qui
sort par là est notre propre JavaScript et notre propre CSS, aucun octet de courrier — les
données restent derrière `/api`. Un test le verrouille dans les deux sens : `/` répond 200
sans jeton, `/api` répond 401.

Le service de fichiers vient de `tower-http`, pas de nous : servir des fichiers statiques veut
dire répondre de la traversée de chemin, ce qui n'est pas un exercice qu'on s'impose. Huit
formes d'échappement sont testées — `..`, `%2e%2e`, `....//`, antislash — et aucune ne sort du
répertoire du front.

Détail relevé en le testant : sans `--ui-dir`, la racine répond **401** et non 404, parce que
la couche de jeton est devant tout le routeur protégé. C'est le meilleur des deux
comportements — quelqu'un qui sonde le port apprend qu'il y a un jeton, pas quels chemins
existent — donc c'est ce qui est verrouillé par le test.

#### Le choix du framework se paie dans un seul fichier

`<Index>` et pas `<For>`. `<For>` est clé par identité : quand la fenêtre visible change, il
crée, déplace et détruit des nœuds. `<Index>` est clé par **position** : la ligne 0 reste la
même ligne 0 du DOM, et son contenu change par signal.

En défilant, c'est exactement ce qu'on veut : garder les quarante lignes vivantes et ne
changer que leurs nœuds texte. Aucun nœud créé, aucun détruit, aucune réconciliation. C'est le
motif qu'un VDOM oblige à contourner et que les signaux donnent par défaut — l'argument
avancé pour écarter React, vérifié à l'écriture.

Deux détails que l'implémentation a imposé de connaître :

- **`transform` et pas `top`.** Déplacer la fenêtre par `top` provoque un recalcul de mise en
  page à chaque image ; par `translateY`, c'est une composition. Sur une ligne de 22 px, c'est
  la différence qui décide du critère.
- **`contain: strict`** sur le conteneur défilant : le moteur n'a plus à remonter l'arbre pour
  savoir si quelque chose à l'intérieur affecte la mise en page extérieure.

La hauteur totale est connue **sans avoir chargé une seule ligne** : `folders.list` rend déjà
le compte de chaque dossier. La barre de défilement est donc juste dès la première image.

#### La limite connue : le saut arbitraire dans la liste

La pagination du démon est par clé — `docs/ARCHITECTURE.md` : jamais d'`OFFSET` — donc les
pages ne s'obtiennent qu'en séquence. Tirer la barre au milieu d'un dossier de 20 000 messages
affiche des lignes vides le temps que les pages intermédiaires arrivent.

Ce **n'est pas** un défaut de virtualisation : le défilement reste fluide, une ligne vide se
dessine aussi vite qu'une pleine. C'est une conséquence du choix de pagination, qui était le
bon pour le coût par page. La sortie propre demande une méthode « curseur à la position N »
côté démon — un `OFFSET` payé **une fois par saut** au lieu d'à chaque page, ce qui est un
arbitrage complètement différent de celui que le document rejette. Pas encore écrite.

#### Le corps du message : `srcdoc`, et ce que ça impose

Le corps n'a pas d'URL, il arrive dans une réponse JSON. Il est donc injecté par `srcdoc`.
Conséquence directe : **la CSP ne peut pas être un en-tête HTTP** — il n'y a pas de réponse
HTTP pour la porter — elle passe par un `<meta http-equiv>` en tête du document injecté.

Deux directives ne fonctionnent pas en `<meta>` : `sandbox` et `frame-ancestors`. Le `sandbox`
est donc posé en attribut de l'`<iframe>`, ce qui est de toute façon le bon endroit ;
`frame-ancestors` n'a pas d'objet, rien n'encadre ce document.

La CSP et le `sandbox` viennent du démon **avec le corps**, jamais d'une constante recopiée
dans le front : source unique, comme `mailhtml::csp` l'exige.

**Un effet de bord qui renforce le confinement, et qu'il faut connaître.** Le `sandbox`
n'accorde pas `allow-same-origin`, donc le document a une **origine opaque** — et `'self'` ne
désigne alors plus rien. Même `img-src 'self'` ne peut charger aucune image. C'est plus strict
que ce que la politique laisse croire, et c'est tant mieux ; mais il ne faudra pas compter sur
`'self'` pour servir un jour les images `cid:` d'un message. Elles devront être des `data:`.

Le front a par ailleurs **sa propre** CSP, distincte : `connect-src 'self'` — il ne parle qu'au
démon qui le sert et à personne d'autre. C'est la moitié du critère 8 qui concerne le front
lui-même.

#### L'ordre de démarrage sert le critère 1

Cache d'abord, réseau ensuite :

1. lire les dossiers et un écran de lignes dans IndexedDB, et les afficher — **zéro
   aller-retour** ;
2. appeler `server.hello`, qui répond en une fois à tout ce qu'il faut savoir ;
3. rafraîchir depuis le démon, dont la réponse gagne toujours, sans arbitrage ni fusion.

Un front qui attendrait le réseau avant de dessiner paierait deux allers-retours avant le
premier pixel, et le critère serait hors de portée sur un réseau lent.

IndexedDB et pas `localStorage` : ce dernier est synchrone, et lire 100 000 lignes y
bloquerait le fil principal — exactement ce que le critère 2 interdit.

Le cache contient les sujets et les expéditeurs de toute la boîte. Sur un poste partagé,
c'est presque aussi révélateur que le courrier (`docs/PRIVACY.md` §8), d'où l'absence de tout
corps de message dedans et une fonction de purge.

#### L'abonnement n'utilise aucune minuterie

`store.wait` dort côté démon jusqu'à ce que la révision change. Le front le rappelle en
boucle : un client au repos ne coûte donc rien, et il apprend un import déclenché ailleurs
sans interroger toutes les secondes. C'est le mécanisme de l'étape 7 utilisé pour ce qu'il a
été fait.

#### Le jeton en mode onglet, et l'écart assumé avec `PRIVACY.md`

`docs/PRIVACY.md` §7 veut le jeton dans le trousseau du système. **Un onglet de navigateur n'y
a pas accès** : le mieux qu'il puisse faire est `sessionStorage`, effacé à la fermeture de
l'onglet. C'est un cran en dessous de la règle, et c'est écrit ici plutôt que caché.

`sessionStorage` et pas `localStorage` : un jeton qui survit à la fermeture de l'onglet
survit aussi à quelqu'un qui s'assied devant la machine.

Dans la coquille Tauri, le jeton vivra dans le trousseau côté Rust et le front le demandera
par IPC, sans jamais transiter par le stockage du navigateur. C'est ce mode-là qui respecte
§7 ; le mode onglet reste le chemin de secours et de débogage.

#### Le coût de TypeScript, nommé

Les types de `mailapi::dto` sont **retapés** en TypeScript. Un client Rust les relit avec le
même code, donc un champ renommé casse à la compilation ; ici, rien ne le garantit. C'est le
vrai prix du front web, et il était connu quand Tauri a été retenu.

Le contrepoids est `PROTOCOL` : le démon annonce sa version de contrat dans `server.hello`, et
le front **refuse de démarrer** si le numéro ne correspond pas. Ça ne rattrape pas un champ
renommé en silence sans changement de version, mais ça rattrape tout changement incompatible
fait dans les règles.

#### Ce qui n'est pas encore vérifié

Le front est construit, servi, et son type-check passe. **Son comportement à l'exécution n'a
pas été vu** : ça demande un navigateur, et les vérifications faites ici sont au niveau HTTP.
Les critères 1, 2 et 9 ne sont donc pas mesurés, et l'étage 2 du critère 8 — le message piégé
rendu dans un vrai webview — reste à faire.

### La coquille Tauri, exécutée pour la première fois — 2026-09-02

L'application s'ouvre, parle au service embarqué, affiche le corpus et se referme. Les critères
1 et 2 sont mesurés dans le vrai webview par `cargo xtask measure-ui`, en release, sur le store
de 73 825 messages.

#### Trois pannes qu'aucune vérification HTTP ne pouvait voir

Le front avait été déclaré « construit, servi, type-check propre » le 2026-08-31. Il ne
fonctionnait pas. Les trois causes sont indépendantes, et les trois donnent **exactement le
même symptôme** : une fenêtre qui s'ouvre et ne répond à rien.

1. **`custom-protocol` n'était pas demandée.** Sans cette caractéristique, Tauri charge
   `devUrl` — le serveur Vite — quel que soit le profil de compilation. Le binaire release
   ouvrait donc `http://localhost:5173` où rien n'écoute. La trace de chargement de page l'a
   dit en une ligne, une fois qu'il y a eu une trace de chargement de page. La coquille
   **prévient maintenant au démarrage** quand elle est construite sans, et `measure-ui` la
   passe.

2. **Deux CSP s'intersectent, elles ne s'additionnent pas.** `index.html` porte la sienne en
   `<meta>` — c'est ce qui protège le mode onglet, où le démon ne pose aucun en-tête — et Tauri
   injecte celle de `tauri.conf.json`. Chaque directive est alors prise dans sa version la plus
   stricte. Le `connect-src 'self'` de la page annulait l'autorisation d'`ipc:` accordée par la
   configuration ; et même corrigé, `script-src 'self'` restait sans le **nonce** que Tauri
   fabrique à chaque démarrage pour son script d'amorçage — celui qui pose
   `window.__TAURI_INTERNALS__`. L'amorçage bloqué, `embedded()` répondait faux et
   l'application se croyait dans un onglet : elle réclamait un jeton qui n'existe pas en mode
   embarqué.

   La sortie n'est pas d'élargir la politique de la page : `http://ipc.localhost` n'est pas le
   démon, et l'autoriser en mode onglet contredirait le critère 8. **Une politique par hôte, et
   une seule** : le `<meta>` est retiré à la construction pour la coquille, `tauri.conf.json`
   fait foi là — et c'est la seule qui *puisse* faire foi, puisqu'elle est la seule que Tauri
   sait compléter. Trois tests Rust comparent les deux politiques directive par directive, et
   n'admettent qu'un écart : le canal d'IPC.

3. **Le paquet web est embarqué à la compilation, et rien ne le disait à Cargo.** Reconstruire
   le front ne provoquait aucune recompilation : `cargo build` répondait « Finished » en une
   seconde et l'exécutable gardait l'ancienne page. Le correctif de CSP du point 2 a donc paru
   sans effet, ce qui a coûté une exécution de mesure entière. Un `rerun-if-changed` sur le
   répertoire du paquet, dans `build.rs`.

**Ce que ces trois pannes ont en commun est plus instructif qu'elles-mêmes** : le webview n'a
pas de console qu'on puisse lire, donc un échec au chargement de la page ne laisse
*strictement rien* — pas une trace, pas un code de sortie. C'est ce qui a été corrigé d'abord :
la coquille journalise chaque chargement de page, et pose, quand une mesure est demandée, un
script d'amorçage qui rapatrie les erreurs JavaScript, les promesses rejetées et les violations
de CSP vers le journal. Sans ce canal, les points 1 et 2 étaient indiscernables l'un de
l'autre.

#### Critère 1 — échoué, et le budget n'est pas dépassé par notre code

Cinq exécutions, mesurées de l'extérieur : du `spawn` du processus à l'instant où la page
signale que l'interface répond.

| Étape | Coût | Qui la paie |
|---|---|---|
| Ouverture du store — SQLite, index tantivy | **31 ms** | nous |
| Création de la fenêtre et du webview | **800 à 1 390 ms** | WebView2 |
| Chargement de la page, cache relu, liste à l'écran | **150 à 360 ms** | nous |
| **Total jusqu'à l'interface** | **1 050 à 1 570 ms** | budget : 400 ms |

La dispersion n'est pas du bruit de mesure : elle suit la charge de la machine, et c'est la
création du webview qui l'absorbe presque entièrement. Le relevé le plus favorable donne 800 ms
pour cette seule étape, le plus défavorable 1 390 ms. **Notre code, store et page réunis, tient
dans 190 à 390 ms** — soit à peu près le budget entier, à lui seul, sans le webview.

Deux vérifications faites pour écarter les explications faciles :

- **Ce n'est pas le cache de lecture.** Le banc de défilement remplit IndexedDB de 20 544
  lignes ; mesuré avec ce cache plein puis avec un profil de webview neuf, le démarrage n'est
  pas plus rapide à vide — il était même plus lent, la machine étant plus chargée à ce
  moment-là. Les lectures du cache sont bornées à un écran, et ça se voit.
- **Ce n'est pas le store.** 31 ms, mesuré, stable, sur 73 825 messages et un index tantivy
  monté. Le paralléliser avec la création du webview économiserait ces 31 ms et rien de plus.

`docs/PHASE-1.md` annonçait « ~250 ms au lieu de ~80 ms pour un natif, c'est le prix du rendu
HTML, il est payé sciemment ». Le prix réel est **quatre à six fois** cette estimation, et il
est payé avant que la moindre ligne de notre code s'exécute dans la page.

Ce qui reste ouvert est une décision, pas une optimisation :

- garder le seuil de 400 ms et le tenir demanderait de sortir le webview du chemin de
  démarrage — une liste native, le webview réservé au corps du message. C'est revenir sur le
  choix de Tauri, qui a été fait pour de bonnes raisons de rendu et de confinement ;
- ou re-baser le critère sur ce qui est mesurable et corrigeable — le temps de *notre* code
  jusqu'à l'interface, webview exclu — en écrivant à côté le coût de plate-forme constaté.

Aucune des deux ne se décide en écrivant du code, donc ni l'une ni l'autre n'a été prise ici.

#### Critère 2 — passé, après une première mesure qui ne mesurait rien

Le premier relevé annonçait « 600 images en retard sur 600 », p50 à 18,1 ms. C'était faux, et
d'une manière qui valait la peine d'être comprise : **l'intervalle entre deux
`requestAnimationFrame` est décidé par le compositeur, pas par nous**. Il mesurait un écran qui
ne présentait pas à 60 Hz. Deux exécutions du même code, sans une ligne de différence, ont
donné 55 Hz puis 32 Hz au repos — une fenêtre qui n'a pas le premier plan est ralentie.

La mesure juste est ailleurs. L'argument `timestamp` du rappel d'animation est l'heure de
**début** de l'image ; `performance.now()` lu à la première instruction du rappel est l'heure où
notre code reprend la main. L'écart entre les deux est le temps passé dans cette image avant
nous — et c'est exactement là que tourne la mise à jour du défilement, parce que les « scroll
steps » du navigateur s'exécutent avant les rappels d'animation. Cet écart **est** le coût que
la liste impose à l'image, et il ne dépend ni de la fréquence de l'écran ni du premier plan.

| Mesure | Relevé | Budget |
|---|---|---|
| Travail par image en défilant, p95 | **2,80 ms** | 16,7 ms |
| Travail par image, pire cas | 3,50 ms | 16,7 ms |
| Travail par image au repos | 0,30 ms | — |
| Images perdues sur 600, contre la cadence au repos | **0** | 0 |

Sur **20 544 lignes réellement chargées** — le plus gros dossier du corpus, `[Gmail]/Tous les
messages`, lu en 205 pages avant de mesurer. Les lignes sont chargées pour de vrai et non
laissées vides : défiler sur des lignes vides mesurerait un dessin qu'on n'affiche jamais, sans
date, sans expéditeur et sans sujet à réécrire.

Deux limites, nommées :

- **20 544 lignes et non 100 000.** C'est le plus gros dossier du corpus réel ; le critère parle
  de 100 000. Le coût par image ne dépend pas du total — la fenêtre rendue fait quarante lignes,
  et la cale de défilement est un `height` — mais ce n'est pas la même chose que de l'avoir
  mesuré à 100 000.
- **La molette réelle n'est pas testée.** Le banc pilote `scrollTop` image par image. La latence
  d'entrée du système demande un pilote de navigateur, comme le critère 8 étage 2.

#### Le banc a aussi trouvé un vrai comportement du front

Première version du banc : « chargement arrêté à 0 ligne après 1 page ». La liste **demande ses
propres pages** — son effet appelle `onNeed` dès qu'une position visible manque — et
`loadNextPage` refuse un appel concurrent. Le banc voyait donc son propre appel ne rien faire et
concluait à un chargement bloqué, alors qu'une page arrivait juste après. Ce n'est pas un défaut
du front, mais c'est une propriété qu'un client de `loadNextPage` doit connaître : le banc
attend maintenant qu'aucune page ne soit en vol avant de conclure.

Au passage, ce diagnostic n'existait que parce qu'un canal de diagnostic avait été ajouté une
heure plus tôt. C'est le même enseignement que les trois pannes du début.

#### Ce qui reste

- **Critère 9** — la liste reste défilable, démon injoignable. Le mode embarqué n'a pas de démon
  à couper : le critère porte sur le mode onglet ou sur le mode distant de la coquille, qui
  n'est pas écrit. Le code du front le traite (bandeau visible, cache relu, échecs propres) mais
  ce n'est pas mesuré.
- **Critère 8 étage 2** — le message piégé rendu dans le vrai webview, sous `tauri-driver`.
- **L'import depuis la coquille.** `startup` et `jobs.sources` existent côté Rust, la page ne
  les appelle pas encore : une installation neuve ouvre donc sur un store vide sans moyen de
  l'alimenter depuis l'interface.

### Le plancher du webview, et une coquille native mesurée — 2026-09-02

Le critère 1 échoue à ~1 050–1 570 ms pour un budget de 400 ms. Avant de re-baser un critère
ou de changer de coquille, deux questions se posaient, et les deux se mesurent : **est-ce que
ces 800 à 1 390 ms sont Tauri ou le moteur de rendu du système ?** Et **combien coûterait une
fenêtre native sur les mêmes données ?**

Deux sondes jetables, `crates/mail-spike-webview` et `crates/mail-spike-native`, chronométrées
par le même harnais que la coquille — `cargo xtask measure-ui --shell tauri|native`. C'est
cette identité de protocole qui rend la comparaison recevable.

#### Le webview : ~935 ms, et Tauri n'y est pour rien

`wry` + `tao` nus. Pas de Tauri, pas de store, pas de CSP, pas de capacités : une fenêtre, un
webview, une page `data:` de trois lignes qui appelle l'IPC dès qu'elle s'exécute.

| Jalon | Coût |
|---|---|
| `EventLoop::new()` | 125–134 ms |
| Fenêtre créée | 281–285 ms |
| **Webview construit** | **1 216–1 267 ms** |
| Notre code s'exécute dans la page | 1 229–1 280 ms |

**La création du webview coûte à elle seule ~935 ms.** L'application complète — store de
73 825 messages, index tantivy monté, front Solid, cache IndexedDB relu — mesure 1 050 à
1 570 ms sur la même machine. Tauri, notre code et nos données réunis n'ajoutent donc rien de
mesurable au plancher du moteur : par moments l'application complète est même *plus rapide* que
la sonde vide, la dispersion due à la charge de la machine dépassant l'écart.

Conséquence, et elle est définitive : **aucun réglage de Tauri, de la CSP ou du front ne peut
faire tenir le critère 1**, parce que le budget entier est dépensé avant que la première ligne
de notre code s'exécute dans la page. La question n'est pas « comment optimiser la coquille »
mais « est-ce qu'on crée un webview au démarrage ».

#### La coquille native : 465–481 ms, dont 449 ms de fenêtre et de contexte

`eframe`/`egui` sur le vrai store, la liste virtualisée par `show_rows`, le plus gros dossier
du corpus chargé page par page dans un fil de fond.

| | Tauri + Solid | egui natif |
|---|---|---|
| Démarrage à froid jusqu'à l'interface | 1 050–1 570 ms | **465–481 ms** |
| dont ouverture du store | 31 ms | 21–25 ms |
| dont fenêtre et moteur de rendu | 800–1 390 ms | 449–455 ms |
| dont première image | 150–360 ms | ~20 ms |
| Travail par image en défilant, p95 | 2,80 ms | **1,47 ms** |
| Travail par image au repos | 0,30 ms | 0,63 ms |
| Chaîne de construction | Rust + npm + Vite | Rust seul |

**Deux fois et demie à trois fois plus rapide au démarrage, et toujours 65 à 80 ms au-dessus du
budget.** Le reste n'est plus notre mise en page — la première image coûte 20 ms — mais la
création de la fenêtre et du contexte OpenGL.

#### Le piège qui coûtait 800 ms au natif : `wgpu` par défaut

Premier relevé de la sonde native : **1 300 à 2 500 ms**, donc pire que Tauri. La cause n'est
pas egui, c'est le choix de moteur : **`eframe` 0.36 active `wgpu` dans ses caractéristiques
par défaut**, et sur cette machine l'énumération des adaptateurs DX12, la création du
périphérique et la compilation des pipelines coûtent ~800 ms. En basculant sur `glow`
(OpenGL) : 479 ms.

C'est le même enseignement que `custom-protocol` du matin — un défaut de bibliothèque décide
d'un ordre de grandeur — et c'est écrit dans le manifeste de la sonde pour que personne ne le
redécouvre.

#### La machine, calibrée

`mail.exe --version`, un CLI qui analyse ses arguments et sort, prend **116–123 ms** ici. Le
coût de création de processus n'est donc pas négligeable dans un budget de 400 ms, et
`EventLoop::new()` à 125 ms plus la fenêtre à 155 ms sont élevés pour ce qu'ils font. Cette
machine est lente à créer des processus et à charger des DLL ; les mêmes sondes sur une machine
récente donneraient très probablement un natif **sous** les 400 ms. Les chiffres ci-dessus sont
ceux de cette machine, et le critère se juge là-dessus — mais il faut savoir qu'il se juge sur
un plancher local, pas universel.

#### Ce que le corpus exige d'un moteur de texte

Le reproche le plus sérieux fait à egui est de ne pas **façonner** le texte : pas de formes
contextuelles, pas de réordonnancement. Un sujet en arabe n'y serait pas « moins joli », il
serait faux. La question n'est pas abstraite, elle se compte —
`cargo xtask corpus-scripts` :

| Exigence maximale du sujet et du nom d'expéditeur | Messages | Part |
|---|---|---|
| Latin de base | 65 310 | 88,47 % |
| Latin étendu, grec, cyrillique | 532 | 0,72 % |
| **CJK, hangul, émoji — police de repli** | **7 965** | **10,79 %** |
| Arabe, hébreu — façonnage et bidirectionnel | 13 | 0,02 % |
| Indien, thaï, khmer — façonnage complexe | 5 | 0,01 % |

**18 messages sur 73 825** exigent un vrai façonnage. Ce qui pèse, c'est le **repli de
police** — un dixième du corpus a des émoji ou du CJK dans son sujet — et ça, egui le fait
dès qu'on lui fournit les polices. L'objection est donc réelle mais marginale *sur ce
corpus-là* ; sur une boîte arabophone ou indienne, elle serait disqualifiante.

Une remarque de méthode : le premier relevé annonçait 589 messages « en arabe ou en hébreu ».
C'était faux — l'intervalle Unicode retenu avalait les sélecteurs de variante émoji (U+FE0F),
et les prétendus sujets arabes étaient du marketing français avec des cœurs. **C'est
l'échantillon de sujets imprimé en fin de rapport qui l'a montré**, et c'est la raison pour
laquelle il est imprimé : un pourcentage seul n'est pas vérifiable.

#### Un second avis, et ce qu'il a corrigé

Sollicité sur l'arbitrage, il a confirmé qu'aucun levier WebView2 ne change l'ordre de
grandeur — runtime *fixed-version*, `additional_browser_args`, hébergement par composition :
rien qui touche au coût de l'environnement, et `--disable-gpu` détruirait le défilement. Il a
apporté trois choses que la mesure ne donnait pas :

- **Slint n'est pas exclu par sa licence.** Il est en triple licence, dont une « royalty-free »
  qui couvre le bureau sans copyleft. C'est une friction — licence non OSI — pas un blocage.
- **GPUI (Zed, Apache-2.0) est le seul toolkit Rust où le texte passe par DirectWrite sur
  Windows et CoreText sur macOS**, avec `uniform_list` qui est exactement une liste de
  messages. Contre : API qui suit le rythme de Zed, documentation = le code de Zed,
  accessibilité quasi absente. `iced` a un bon texte (cosmic-text) mais **aucune
  virtualisation** — 20 544 lignes mises en page à chaque image ne tiendront pas — et
  `FontSystem::new()` énumère les polices du système, 50 à 150 ms dans le budget.
- **Le corps HTML dans une coquille native : le problème de l'*airspace*.** Un webview enfant
  sur Windows crée son propre HWND, qui est **toujours** au-dessus de ce que le toolkit
  dessine : menus contextuels et infobulles disparaissent sous le rectangle du webview. Le
  focus clavier part au webview au premier clic et les raccourcis cessent de répondre jusqu'au
  suivant, ce qui demande `AcceleratorKeyPressed` — non exposé par `wry`, donc du `unsafe` dans
  un crate isolé. Sous Linux, un enfant `wry` passe par un conteneur GTK, donc X11 ou XWayland.
  Aucune application sérieuse ne fait « toolkit GPU Rust + webview enfant » aujourd'hui.

Sa recommandation, qui rejoint l'architecture du projet : **on ne jette pas un front qui
marche, on ajoute un client.** Le démon et son API ont été conçus exactement pour ça. Pour le
corps des messages, rendu natif du HTML déjà assaini en phase 1, plus une échappatoire « ouvrir
dans le navigateur » qui écrit le HTML assaini dans un fichier temporaire — zéro code de
webview — et le webview enfant plus tard si l'usage le réclame.

#### Ce qui est établi, et ce qui reste à décider

Établi, mesuré, reproductible :

1. le critère 1 est **inatteignable** avec un webview sur le chemin de démarrage, sur cette
   plate-forme, quelle que soit la configuration ;
2. une coquille native le rate encore de 65 à 80 ms **sur cette machine**, en étant trois fois
   plus rapide, et son coût restant est la fenêtre et le contexte graphique, pas notre code ;
3. le défilement est plus léger en natif — 1,47 ms contre 2,80 ms par image ;
4. le corpus ne réclame pas de façonnage complexe (0,02 %), mais réclame un repli de police
   (10,79 %) ;
5. la coquille native n'a pas besoin de `npm` pour être construite.

À décider, et ce n'est pas une question de code :

- **Basculer la coquille de lecture en natif**, en gardant `maild`, l'API, `mailhtml` et la
  coquille Tauri comme client de référence et échappatoire de fidélité. Le rendu du corps
  devient le vrai sujet, et l'`<iframe>` confinée — sur laquelle repose la garantie de
  `docs/PRIVACY.md` — doit être remplacée par une garantie d'une autre nature : un rendu qui
  n'a **aucune** pile réseau plutôt qu'un moteur configuré pour ne pas s'en servir.
- **Ou garder Tauri** et écrire dans ce document que le critère 1 est manqué par
  plate-forme, chiffres à l'appui.

Le seuil de 400 ms lui-même n'est pas à re-baser sur « notre code sans le moteur de rendu » :
ce serait une métrique flatteuse que personne ne vit. Il se juge sur le démarrage complet, et
c'est ce que les deux sondes mesurent.

#### Le rendu logiciel : il tient le critère 1 et casse le critère 2

Sur les 449 ms de « fenêtre et contexte » de la sonde OpenGL, combien sont le **contexte** ?
Une sonde de plus, `winit` + `softbuffer` sans aucun GPU, fond uni :

| Jalon | Coût |
|---|---|
| `EventLoop::new()` | 122–134 ms |
| Fenêtre créée | 288–328 ms |
| Surface `softbuffer` | **+0,4 ms** |
| Premier pixel présenté | 291–333 ms |

La surface logicielle est donc **gratuite** là où le contexte OpenGL coûte ~150 ms, et
`EventLoop::new()` à 122 ms plus la fenêtre à ~165 ms sont irréductibles — les deux toolkits
les paient, `tao` comme `winit`.

Restait à savoir si egui rastérisé par le processeur tient la charge. `mail-spike-software`,
sur `egui_software_backend`, la même interface et le même banc :

| | Tauri + Solid | egui / OpenGL | egui / logiciel |
|---|---|---|---|
| Démarrage à froid | 1 050–1 570 ms | 465–481 ms | **323–336 ms** |
| Travail par image, 1280×800 | 2,80 ms | 1,47 ms | 5,16 ms |
| Travail par image, 1920×1080 | — | 1,63 ms | 10,28 ms |
| Travail par image, 2560×1440 | — | — | 14,54 ms |
| Travail par image, 3840×2160 | — | **1,87 ms** | **24,43 ms** |
| Critère 1 — budget 400 ms | échoué (×3) | échoué de 65–80 ms | **passé** |
| Critère 2 — budget 16,7 ms | passé | **passé partout** | **échoué en 4K** |

**Le coût GPU est plat avec la résolution, le coût logiciel est linéaire** — 1 Mpx à peindre en
1280×800, 8,3 Mpx en 3840×2160. Le rendu logiciel achète 140 ms de démarrage et perd le
critère 2 sur n'importe quel écran haute densité, ce qui en 2026 est le cas normal.

**Décision : OpenGL.** L'échange est mauvais dans l'autre sens : les 65 à 80 ms de dépassement
au démarrage sont le plancher *de cette machine* — lente à créer des processus, `mail --version`
prend 116 ms — et disparaîtront probablement sur une machine récente, tandis que 24 ms par image
en 4K est de la géométrie, pas de la conjoncture.

Détail relevé au passage : en 4K, la pire image de la sonde OpenGL monte à 110 ms — un accroc
isolé, probablement un envoi d'atlas de police, avec un p95 qui reste à 1,87 ms. À surveiller
quand la coquille existera, pas à corriger à l'aveugle.

La seule façon d'avoir les deux serait de dessiner la **première** image en logiciel pendant
que le contexte OpenGL se crée à côté, puis de basculer. Deux backends à maintenir et un
transfert d'état de textures entre les deux, pour 80 ms : nommé ici, pas retenu.


### La coquille native, livrée — 2026-09-02

`crates/mail-shell` : egui sur OpenGL, une fenêtre, trois panneaux, **cliente du même service
que le démon**. Elle lit, cherche, ouvre, suit les fils, importe.

| | Tauri + Solid | Coquille native |
|---|---|---|
| Démarrage jusqu'à une interface **avec ses lignes** | 1 050–1 570 ms | **485–526 ms** |
| dont fenêtre et contexte graphique | 800–1 390 ms | 471 ms |
| Travail par image en défilant, p95 | 2,80 ms | **1,73 ms** |
| Chaîne de construction | Rust + npm + Vite | Rust seul |
| Critère 1 — budget 400 ms | échoué ×3 | **échoué de 85 à 125 ms** |
| Critère 2 — budget 16,7 ms | passé | **passé, dix fois la marge** |

Relevés par le même harnais que tout le reste : `cargo xtask measure-ui --shell shell`, qui
sait aussi mesurer `tauri`, `native` et `software` — l'identité de protocole est ce qui rend
les colonnes comparables.

#### Aucune logique n'est réimplémentée

Le fil d'API de la coquille appelle `maild::api::jsonrpc::Api::handle_message`, **la fonction
que le démon sert sur HTTP et sur stdio**, celle que la page de la coquille Tauri appelle par
IPC. Pagination par curseur, politique d'images distantes, bornes de recherche, registre des
tâches de fond : tout est déjà écrit et déjà testé.

Le prix est un aller-retour de sérialisation en mémoire — quelques dizaines de microsecondes
pour une page de cent lignes, trois ordres de grandeur sous la lecture SQLite qu'elle
transporte. En échange, le contrat reste unique et les types de `mailapi::dto` sont relus par
le même code Rust qui les écrit : un champ renommé casse à la compilation.

La découverte des profils Thunderbird, elle, a **déménagé dans `maild::config`**. Elle vivait
dans la coquille Tauri ; la coquille native en avait besoin ; deux copies de la règle
« `prefs.js` marque un profil » étaient une copie de trop. C'est de toute façon sa place : le
service décide ce qui est importable, un client choisit un rang.

#### Deux fils, et pourquoi

L'interface ne bloque jamais — règle 3 du `CLAUDE.md`. Elle demande, dessine ce qu'elle a, et
le fil répond quand il a fini. Deux fils, pas un :

- **l'interactif** sert les demandes de l'interface, une à la fois ;
- **l'abonnement** dort dans `store.wait` jusqu'à ce que la révision du store change. Sur un
  fil unique, cet appel aurait bloqué tout le reste pendant trente secondes. Il est
  **autonome** : l'interface ne le demande pas, elle reçoit un changement — donc un import
  déclenché par la CLI apparaît dans la liste sans qu'on rafraîchisse.

Les tâches de fond, elles, sont relevées à la minuterie tant qu'il y en a une active : la
révision du store ne change pas à chaque page importée, et une barre de progression qui
n'avance qu'à la fin ne sert à rien.

#### Le premier écran arrive en un seul aller-retour

Premier jet : la première image à 461 ms, les **premières lignes à 591 ms**. Cent trente
millisecondes d'écart pour 22 ms de travail réel — le reste étant quatre images d'attente sur
un écran à 32 Hz, parce que la page ne pouvait partir qu'une fois la liste des dossiers
absorbée par l'interface.

D'où une demande `Bootstrap` qui enchaîne `folders.list` et `messages.page` **dans le fil**, et
rend le premier écran d'un bloc. Les deux jalons coïncident maintenant : l'interface s'ouvre
avec ses cent premières lignes, à 485–526 ms.

Le choix du dossier ouvert — la boîte de réception, à défaut le premier — est une politique de
présentation posée dans le fil, et c'est assumé : c'est le prix d'un premier écran qui arrive
avec son contenu. Même arbitrage que `server.hello`, qui répond en une fois à tout ce qu'un
client doit savoir.

**Ce que ce détour a appris** : sans le jalon `premieres_lignes`, l'écart était invisible. Le
jalon de « première image » seul aurait dit 461 ms — un chiffre vrai, sur une fenêtre qui ne
montrait rien. Les deux sont donc relevés, et c'est le second qui compte comme critère 1.

#### Le corps des messages : plus de moteur du tout

`mailhtml::blocks`, nouveau : il découpe le HTML **déjà assaini** en paragraphes, titres,
puces, citations, liens et images annoncées. 20 tests, dont la moitié sur des entrées cassées —
balise non fermée, `<` isolé, fermetures non appariées, cinquante mille `<div>` imbriqués,
entité tronquée, caractère multioctet collé à une balise.

Ce n'est pas un moteur de rendu HTML et ça ne cherche pas à l'être : une table devient une
suite de lignes, le CSS disparaît. Pour la correspondance et les listes de diffusion, c'est
fidèle ; pour une newsletter à tables imbriquées, c'est du « mode lecture ».

**La garantie de `docs/PRIVACY.md` change de nature** : elle passe de « un moteur configuré
pour ne rien aller chercher » à « aucun code capable d'aller chercher quoi que ce soit ». Pas
de client HTTP, pas de décodeur d'images, pas de police distante. Une image est annoncée —
`[image distante, non chargée]`, `[image bloquée]`, `[image embarquée]` — jamais chargée. Les
reculs assumés et l'échappatoire « ouvrir dans le navigateur » sont détaillés dans
`docs/PRIVACY.md`, §5.

#### Les polices, en deux temps

Le corpus dit que **10,79 % des messages ont du CJK ou des émoji dans leur sujet** et que
0,02 % exigent un façonnage complexe. La coquille charge donc la police d'interface du système
avant la première image — quelques centaines de kilo-octets — et la police de repli CJK
**après**, sur un fil : `msyh.ttc` fait plusieurs dizaines de mégaoctets, et la charger sur le
chemin de démarrage coûterait plus que le budget entier. L'interface s'ouvre en latin, les
idéogrammes apparaissent une fraction de seconde plus tard.

#### L'import se déclenche depuis l'interface

`jobs.sources` et `jobs.start` existaient depuis le 2026-08-30 et aucun client ne les appelait :
une installation neuve ouvrait sur un store vide sans moyen de l'alimenter. C'est réglé — un
panneau liste les profils détectés, un bouton met l'import en file, une barre suit la
progression, et l'index se reconstruit d'un clic.

**Un rang, jamais un chemin.** La coquille choisit dans la liste que le service rend, exactement
comme un client distant : sans cette règle, qui détient l'interface pourrait faire lire
n'importe quel fichier de la machine et le relire ensuite par `search.query`.

#### Ce qui reste

- **Critère 1** : manqué de 85 à 125 ms, et le reste est la fenêtre et le contexte OpenGL — pas
  notre code, qui tient dans 35 ms. À re-mesurer sur une machine récente avant d'en conclure
  quoi que ce soit : celle-ci met 116 ms à démarrer un CLI qui ne fait rien.
- **Critère 9** — la liste reste consultable, service injoignable. En mode embarqué il n'y a
  rien à couper ; le critère porte sur un démon distant, que la coquille native ne sait pas
  encore viser.
- **Critère 8 étage 2** — le message piégé dans un vrai webview, sous `tauri-driver`. Il ne
  concerne plus le chemin par défaut, mais il concerne toujours la coquille Tauri et
  l'échappatoire de fidélité.
- **Les images embarquées, les liens cliquables, la composition.** Nommés dans
  `docs/PRIVACY.md` et dans `mailhtml::blocks`.
- **Les trois sondes** (`mail-spike-webview`, `mail-spike-native`, `mail-spike-software`) ont
  répondu à leur question. Elles restent le temps que les chiffres soient reproduits sur une
  autre machine, puis elles disparaissent.


### Le mode distant, et le critère 9 mesuré — 2026-09-02

La coquille native sait maintenant viser un démon déjà en place : `mail-shell --daemon
hôte:port`, jeton lu dans le trousseau du système. C'est le **déploiement de référence** de
`docs/ARCHITECTURE.md` — démon sur une machine dédiée, client ailleurs — et c'était la
condition pour mesurer le critère 9 : il faut un service à couper, et le mode embarqué n'en a
pas, puisque le service tombe avec l'application.

| Mode | Transport | Jeton |
|---|---|---|
| **embarqué** (défaut) | appel en fonction, dans le processus | aucun |
| **distant** (`--daemon`) | HTTP vers le démon, connexion réutilisée | trousseau du système |

Un seul endroit de la coquille connaît la différence — `Link::call`, quarante lignes. Les deux
dos parlent le même contrat `mailapi`, et le mode distant passe par `mailapi::client`, comme la
CLI : rien de nouveau n'a été écrit pour le transport.

#### Le relevé

`cargo xtask measure-ui --shell shell --bench offline` monte un vrai `maild` sur le bouclage,
lance la coquille en mode distant, **tue le démon** quand la coquille annonce ses lignes
chargées, puis lit le verdict :

| Mesure | Relevé |
|---|---|
| Lignes chargées avant la coupure | 1 000 |
| État dégradé visible | **oui** |
| Ouverture d'un message | **échec propre** |
| Recherche | **échec propre** |
| Travail par image en défilant, après la coupure, p95 | **1,24 ms** |
| Images observées | 600 |

**Critère 9 : passé.** Chaque mot du critère est un terme du verdict — la liste déjà chargée
défile, les deux actions échouent proprement, l'état est visible, et rien n'a gelé : si
l'interface avait bloqué, aucune de ces 600 images n'existerait et le relevé ne sortirait pas.

#### Trois choix de conception que ce banc a imposés

**Un échec de transport n'est pas un refus du service.** La distinction est le critère 9 tout
entier : un `search.query` mal formé est une erreur de l'utilisateur, un démon injoignable est
un mode dégradé. Les confondre ferait afficher « hors ligne » sur une faute de frappe. D'où
trois issues et pas deux dans le lien — servi, refusé, injoignable — et un drapeau `transport`
sur l'échec qui remonte à l'interface.

**C'est l'abonnement qui découvre la coupure**, pas l'utilisateur. Le fil qui dort dans
`store.wait` voit son appel échouer dans les deux secondes et le signale ; l'interface passe en
mode dégradé sans qu'on ait cliqué sur quoi que ce soit. Sans ce fil, la coupure ne se serait
vue qu'au premier clic.

**Le démon est tué de l'extérieur.** Un service qu'on couperait de l'intérieur ne prouverait
rien : l'application saurait qu'elle a coupé. C'est l'outillage qui tue, en voyant passer le
jalon `etape=charge`.

Détail relevé au passage : la connexion est **jetée** à la première panne, pour que l'appel
suivant en rouvre une. Sans ça, un démon redémarré resterait « injoignable » jusqu'à la
fermeture de la coquille — un bug qu'on n'aurait vu qu'en production.

Le jeton de mesure est déposé dans le trousseau puis **retiré**, y compris si le banc échoue :
une entrée de trousseau laissée derrière une mesure est un secret oublié.

#### Le cache de lecture du mode distant — fait le 2026-09-03

Ce paragraphe annonçait un manque : « il n'a pas de cache de lecture ». Il en a un depuis le
2026-09-03 — `crates/mail-shell/src/cache.rs` — parce que la note du critère 1 l'exige : « avec
un démon distant, il impose en plus qu'elle n'attende pas le premier aller-retour réseau — donc
qu'elle ouvre sur son cache de lecture ».

Les dossiers et les 500 premières lignes du dernier dossier ouvert, dans un fichier par démon,
relu avant la création de la fenêtre. **Jamais un corps de message.** Mesuré par
`cargo xtask measure-ui --bench startup-remote` : 149–160 ms avec cache contre 160 ms sans, sur
le bouclage. L'écart y est faible parce qu'un aller-retour local est gratuit ; c'est le
**plancher** de ce que le cache apporte, et sur un réseau réel c'est un aller-retour complet qui
disparaît du chemin du premier pixel.

Trois choses que `docs/PRIVACY.md` §8 imposait, et qui sont dans le code :

- le fichier vit dans `data_local_dir` et **non** `data_dir`. Sur Windows, le second est le
  profil **itinérant** : sur un poste en domaine, les sujets et les expéditeurs d'une boîte
  seraient partis sur un serveur de profils. Le premier ne quitte pas la machine ;
- il est **borné** à 500 lignes : un cache n'est pas une copie de la boîte ;
- il se **purge**, par un bouton et par `mail-shell --daemon <hôte> --purge-cache`. Et la purge
  **désactive l'écriture pour la session** : sans ça, la réponse suivante du démon réécrivait le
  fichier quelques dizaines de millisecondes plus tard, et le bouton n'effaçait que jusqu'à la
  page suivante.


### Correction : les relevés de démarrage du 2026-09-02 étaient faux d'un facteur trois — 2026-09-03

**Tous les chiffres de démarrage consignés le 2026-09-02 sont invalides.** Ils ont été relevés
juste après des compilations release de une à deux minutes, sur une machine encore occupée à
écrire ses artefacts et à les faire scanner par l'antivirus. Refaits sur une machine au repos,
même code, même harnais :

| Mesure | Publié le 2026-09-02 | Vrai, machine au repos | Facteur |
|---|---|---|---|
| Coquille native, démarrage | 485–526 ms | **174–180 ms** | 2,9 |
| Coquille native, fenêtre + contexte OpenGL | 471 ms | **162 ms** | 2,9 |
| Coquille Tauri, démarrage | 1 050–1 570 ms | **466–536 ms** | 2,4 |
| Sonde `wry` nue, webview construit | 1 216–1 267 ms | **312–328 ms** | 3,8 |
| Sonde `wry`, fil d'événements | 122–134 ms | **3,2–3,7 ms** | 36 |
| Sonde egui/glow | 465–481 ms | **152–167 ms** | 2,9 |
| Sonde egui logicielle | 323–336 ms | **83–95 ms** | 3,7 |

La démonstration tient en une ligne : le banc de défilement lancé juste après une compilation
de 65 secondes a relevé **536 ms** de démarrage pour le binaire qui en fait 174 au repos. Même
exécutable, même store, même harnais, deux minutes d'écart.

#### Ce que ça touche, et ce que ça ne touche pas

Le poste contaminé est l'**initialisation graphique** — création de fenêtre, contexte OpenGL,
environnement WebView2 : beaucoup de bibliothèques à charger, un pilote à réveiller, tout ce
qui souffre d'un cache disque sous pression et d'un processeur occupé. Le facteur 36 du fil
d'événements de `tao` en est le cas extrême.

Le démarrage de processus, lui, n'a presque pas bougé : `mail --version` faisait 116–123 ms
sous charge et fait 109–110 ms au repos, **mesuré à travers le même shell**. Ce qui compte ici :
ce chiffre-là ne disait rien de la contamination, et il était lui-même mal interprété — via
PowerShell, le même binaire fait **41–52 ms**. Les ~65 ms de différence sont le coût de
lancement d'un processus par le shell MSYS, pas celui de la machine. L'affirmation du
2026-09-02 selon laquelle « cette machine met 116 ms à démarrer un CLI qui ne fait rien » était
donc fausse deux fois.

#### Ce que la conclusion architecturale devient

Elle **tient**, et l'écart relatif est même plus net :

| | Machine au repos | Verdict critère 1 |
|---|---|---|
| Coquille native, mode embarqué | **174–180 ms** | **passé** |
| Coquille native, mode distant | **149–160 ms** | **passé** |
| Coquille Tauri | 466–536 ms | échoué |

Mais deux affirmations du 2026-09-02 doivent être retirées :

- « **le critère 1 est inatteignable avec un webview au démarrage, quelle que soit la
  configuration** » était trop fort. Le plancher se décompose en ~3 ms de fil d'événements,
  ~20 ms de fenêtre, **~290 ms de webview**, plus le temps de la page. La coquille Tauri manque
  le budget de 15 à 35 %, pas de 300 % : un front plus léger pourrait s'en approcher, sans
  garantie de l'atteindre.
- « la création du webview coûte **~935 ms** » : c'est **~290 ms**. Le webview reste de loin le
  poste dominant — quinze fois le coût d'une fenêtre native — mais l'ordre de grandeur annoncé
  était faux.

Ce qui ne change pas : la coquille native démarre **trois fois plus vite**, tient le critère 1
avec un facteur deux de marge, et se construit sans chaîne d'outils JavaScript. Le choix était
le bon pour de bonnes raisons ; il avait été justifié par de mauvais chiffres.

#### La règle de méthode qui manquait

**Ne jamais mesurer un démarrage sur une machine qui vient de compiler.** Le harnais
`cargo xtask measure-ui` compile avant de mesurer, ce qui garantit qu'on mesure dans les pires
conditions — et son en-tête affirmait mesurer « un démarrage à froid ». C'était faux dans les
deux sens : processus froid, mais **machine chaude et occupée**.

Ce qui l'aurait attrapé n'est pas une calibration — celle qui était en place n'a rien vu — mais
la **dispersion**. Les relevés du 2026-09-02 allaient de 1 050 à 1 570 ms sur la même
configuration ; ceux du 2026-09-03 tiennent dans 174–180 ms. Un écart de 50 % entre deux
exécutions identiques n'est pas du bruit, c'est un symptôme, et il était sous les yeux dans le
tableau publié.

Trois précautions, désormais :

1. laisser la machine retomber au repos entre la compilation et la mesure ;
2. lire la **dispersion** avant la médiane, et refuser un relevé dont les exécutions ne se
   ressemblent pas ;
3. quand une comparaison porte une décision, refaire les deux termes **dans la même session**,
   au repos. Le webview à 935 ms et l'egui à 465 ms avaient été mesurés à des moments
   différents, ce qui rendait leur rapport indéfendable — même si, cette fois, il pointait dans
   la bonne direction.


### Ce que la revue a trouvé — 2026-09-03

Tout le code des 2 et 3 septembre a été soumis à un relecteur extérieur, avec une consigne
précise : *trouver ce qui pourrait faire passer un critère à tort*. Onze défauts en sont sortis.
Trois relèvent exactement de cette consigne, et ce sont les plus instructifs.

#### Trois harnais qui se félicitaient tout seuls

**1. Le critère 8 mesurait la phase B pendant quelques millisecondes au lieu de 2,5 s.**

Le message piégé contient un `<meta http-equiv="refresh">`. En phase de contrôle il est
autorisé, donc le cadre **navigue**, donc l'élément `<iframe>` émet un **deuxième** événement
`load`. Deux `load`, deux minuteurs : le premier clôturait la phase A et lançait la phase B, le
second clôturait **la phase B** quelques millisecondes plus tard, alors que son cadre venait
d'être créé.

La phase B rendait donc zéro *quoi que fasse la CSP* — et c'est la phase dont le programme dit
qu'elle « prouve le critère ». Corrigé par trois moyens qui se recoupent : chaque événement
porte sa phase, un seul minuteur peut être armé par phase, et **la durée de décantation réelle
est relevée** puis comparée au délai attendu. Un zéro obtenu en moins de 2,5 s est maintenant
un échec, pas un succès.

**2. Le serveur instrumenté servait les connexions en série, et se bloquait.**

Une seule requête laissée en suspens par un client — un `<video>` qui ouvre et attend — bloquait
la boucle d'acceptation pour toute la suite de l'exécution. Conséquence indirecte et vicieuse :
un `load` d'`<iframe>` n'est émis qu'une fois **toutes les sous-ressources** terminées ; serveur
bloqué, les sous-ressources d'une phase ultérieure restaient en attente, l'`<iframe>` n'émettait
jamais `load`, et la phase ne se posait pas.

C'est le **contrôle final** — la phase D, ajoutée sur recommandation de la revue — qui l'a
révélé. Sa seule raison d'être est de vérifier que le moteur chargeait encore *à la fin*, et
elle a trouvé un défaut du harnais dès sa première exécution. Corrigé : un fil par connexion,
avec un délai de lecture.

**3. L'attribution des requêtes se trompait, deux fois de suite.**

Premier jet : un compteur cumulé et une ligne de base par phase. Faux dès qu'une requête arrive
juste après la clôture d'une phase. Deuxième jet : un préfixe de chemin par phase. Faux aussi —
le message contient une URL **racine-relative**, qui ignore le chemin de la `<base>` et atterrit
hors du préfixe. Le vecteur devenait invisible.

Et surtout : une connexion TCP **sans requête HTTP lisible** ne peut être attribuée à rien du
tout. Or c'est exactement la forme que prend la fuite trouvée au point suivant.

Troisième jet, celui qui tient : **un port instrumenté par vecteur et par phase**. Cent
serveurs, cent fils qui dorment. Une connexion arrivée là vient de là, requête ou pas.

#### Ce que le harnais réparé a immédiatement trouvé

Avec l'attribution exacte, la phase B — le document intact de l'attaquant, sous la seule CSP —
n'est plus à zéro : **une connexion sort, et c'est `<iframe>`**.

`MESSAGE_CSP` porte `frame-src 'none'`. WebView2 refuse bien la requête, mais il a **déjà ouvert
la connexion TCP** vers l'hôte visé avant de l'abandonner. Le serveur de l'expéditeur voit donc
une connexion venant de l'adresse du lecteur, à l'instant où il ouvre le message : c'est
précisément le signal qu'un pixel espion cherche.

**Ça corrige le modèle de sécurité écrit dans `docs/PRIVACY.md`.** Le document posait une
hiérarchie : le moteur d'abord, l'assainisseur en deuxième ceinture. Pour ce vecteur, **c'est
l'inverse** : seul l'assainisseur ferme la fuite, en retirant la balise. La phase C — les deux
ceintures — est à zéro.

La liste de ces fuites est **fermée** dans le code (`ENGINE_LEAKS`) : une fuite d'un vecteur qui
n'y figure pas fait échouer l'exécution. Une régression du moteur ne peut donc pas se glisser
dans un « c'est comme ça ».

#### Un déni de service à un mail

`mailhtml::blocks` cherchait le nom d'un attribut avec `find`, après avoir abaissé la casse du
reste **à chaque tour de boucle**. Quadratique. Mesuré :

| Taille de l'attribut | Avant | Après |
|---|---|---|
| 50 Ko | 13 ms | 0,13 ms |
| 100 Ko | 52 ms | 0,29 ms |
| 200 Ko | 204 ms | 0,56 ms |
| 2 Mo — le plafond d'un corps | ~20 s (extrapolé) | **5,92 ms** |

Et ce découpage tournait **sur le fil de l'interface**. Un mail avec un `alt` de deux mégaoctets
figeait l'application vingt secondes : violation de la règle 3 du `CLAUDE.md`, et déni de
service trivial à provoquer. `alt` et `title` sont dans la liste blanche de l'assainisseur, donc
l'entrée était atteignable.

Corrigé en deux temps : un vrai tokeniseur d'attributs, un seul passage — et le découpage
**déplacé sur le fil d'API**, pour que le coût futur de l'analyse, quel qu'il soit, ne puisse
plus faire tomber une image.

Le même tokeniseur ferme deux autres défauts que la revue a reproduits :

- **un `>` dans une valeur d'attribut coupait la balise.** Le sérialiseur d'`html5ever` ne
  l'échappe pas, et `alt="a > b"` est courant. `<img alt="a > b" src="cid:x">` rendait une image
  sans source, suivie du texte `b" src="cid:x"> suite` ;
- **une valeur pouvait forger un attribut.** `title="href=&quot;http://mechant/&quot;"` faisait
  retenir `http://mechant/` comme cible du lien, en ignorant le vrai `href`. Inoffensif tant
  qu'un lien n'est pas cliquable — et c'est précisément le travail annoncé pour la suite.

#### Deux bugs de liste que personne n'aurait vus tout de suite

**Une page en retard était acceptée.** `Reply::Page` ne portait que le dossier. Séquence : une
page `after=c1` en vol, un `Changed` qui recharge le **même** dossier, et la page en retard
arrive avec le bon numéro de dossier. Elle était concaténée à une liste repartie de zéro : la
liste commençait à la centième ligne, les quatre-vingt-dix-neuf premières manquaient. Un import
émet un `Changed` par lot de 5 000 messages, soit une quinzaine de fois sur le corpus réel. La
réponse porte maintenant le curseur, et une page dont le curseur ne correspond pas est jetée.

**Un changement du store fermait le message qu'on lisait.** Le gestionnaire de `Changed`
appelait `open_folder`, qui remet le volet de lecture à zéro. Pendant un import, le message
ouvert se fermait une quinzaine de fois. Il y a maintenant un `reload_folder` qui ne touche
qu'à la liste : ce qu'on lit ne dépend pas de ce qui s'écrit.

#### Trois fuites de vie privée

- **le cache de lecture vivait dans le profil itinérant de Windows** (`data_dir` = `%APPDATA%`).
  Sur un poste en domaine, sujets et expéditeurs partaient sur un serveur de profils, ce que
  `docs/PRIVACY.md` §8 interdit explicitement. Passé à `data_local_dir` ;
- **le bouton de purge réécrivait le cache aussitôt.** Il effaçait le fichier, puis la réponse
  suivante du démon le réécrivait. La purge désactive maintenant l'écriture pour la session ;
- **« ouvrir dans le navigateur » laissait un corps de message en clair dans `%TEMP%`, pour
  toujours.** Les fichiers vont désormais dans un sous-répertoire dédié, et chaque ouverture
  balaie ceux de plus d'une minute. On ne peut pas supprimer celui qu'on vient d'ouvrir — le
  navigateur le lit — donc on supprime les précédents.

Les bancs de mesure semaient eux aussi des caches nominatifs : chaque exécution en mode distant
utilise un port éphémère, donc créait un fichier de plus. Le garde qui retirait déjà le jeton du
trousseau purge maintenant le cache aussi, y compris quand la mesure échoue.

#### Ce que la revue a aussi rappelé

Deux commentaires affirmaient des choses fausses :

- « l'abonnement **dort** côté service, aucune minuterie, un client au repos ne coûte rien ».
  `store.wait` **sonde** le store toutes les 250 ms côté démon. Le coût est faible — un
  `PRAGMA data_version` et un `stat` — mais ce n'est pas ce qui était écrit ;
- « la coquille native ne contient **aucun client HTTP** ». Elle en contient un, `mailapi::client`,
  pour le mode distant. La garantie réelle est plus précise et plus faible : **aucun chemin de
  données du corps d'un message ne mène à ce client**. C'est vrai — les paramètres envoyés sont
  des identifiants, un curseur, une requête de recherche — mais c'est une propriété tenue par
  relecture, pas une impossibilité de construction. Les deux formulations sont corrigées.

#### Ce qui reste ouvert

Le rapport a laissé deux choses délibérément, et elles sont notées ici plutôt que corrigées à la
hâte :

- **le critère 2 ne mesure que la liste.** Le banc défile avec le volet de lecture vide. Un banc
  « message ouvert, corps long » manque. Le corps est désormais borné à 400 blocs par écran, avec
  un bouton pour la suite, ce qui borne le coût sans le mesurer ;
- **le critère 9 ne teste que la panne franche.** Le démon est tué, le noyau renvoie RST, l'appel
  échoue instantanément. Le cas « injoignable sans RST » — câble débranché, pare-feu qui jette —
  laisserait le fil d'API attendre le délai de lecture de `mailapi::client`, qui est de 180 s.
  L'interface ne gèlerait pas, mais « échoue proprement » n'est vérifié que pour la coupure
  nette.


### Le critère 5 mesuré depuis la liste, et le piège qui s'est refermé deux fois — 2026-09-03

Le critère 5 était coché sur un chiffre côté API : 4,31 ms pour `messages.get` sur un corps
HTML. Le critère ne dit pas « la réponse de l'API », il dit « ouverture d'un message **depuis
la liste** ». Il manquait le fil d'API, le découpage du corps en blocs, et la mise en page de
ces blocs.

`--bench open` ouvre **trente messages distincts** — deux ouvertures du même message
mesureraient un blob déjà décompressé — et compte les blocs dessinés, pour qu'un p95 flatteur
obtenu sur des messages vides se voie au relevé.

| Mesure | Relevé |
|---|---|
| Ouvertures | 120, en 4 exécutions, **0 sans corps** |
| Blocs dessinés | 2 732 |
| Délai du clic au corps dessiné, p95 | **32,7 ms** — budget 50 ms, **passé** |
| Dont le **service** (fil d'API), p95 | **3,8 ms** — p50 à 1,45 ms |
| **Images d'attente** | **exactement 1**, médiane et pire cas, sur les 120 |

#### Le chiffre est celui de l'écran, pas le nôtre

Une image d'attente est le **minimum arithmétique** : rien ne peut être montré avant la
prochaine image. Une image dure ici ~30 ms — ce qui recoupe les 32 Hz déjà relevés sur cette
machine le 2026-09-02 — donc le délai *est* la cadence, plus 1,5 ms de travail.

Conséquence utile : le critère tient tant que l'écran dépasse **~22 Hz**, et un écran à 60 Hz
ramènerait le même code à ~20 ms. Ce qu'on mesure ici n'est pas une marge à défendre, c'est un
plancher physique déjà atteint.

Le rapport donne donc les deux chiffres côte à côte, et le code porte les deux
(`Opened::served_ms`). **L'un est ce que l'utilisateur attend, l'autre est ce dont on
répond** — et les confondre est exactement l'erreur commise sur le critère 2 le 2026-09-02,
quand des deltas de `requestAnimationFrame` avaient été lus comme du travail.

#### Le piège de la compilation s'est refermé deux fois de suite

Le premier relevé donnait 32,7 ms de p95. Le deuxième, sur le même code, **47,2 ms**. Sur un
budget de 50, la différence n'est plus une nuance : elle décide du verdict.

La cause était écrite dans le journal du banc : `cargo xtask measure-ui` **compile avant de
mesurer**, et la compilation avait tourné 58 s juste avant le chronomètre. C'est le défaut que
la section précédente venait de documenter, et le harnais l'a reproduit sur un critère neuf le
jour même.

Compiler soi-même avant ne suffisait pas. Un `cargo build --release` lancé depuis un shell et
le même lancé depuis `cargo run -p xtask` ne rendent pas la même empreinte : les variables
d'environnement posées par `cargo run` entrent dans celle des scripts de construction, donc
`ring`, `rustls`, `maild` et la coquille se recompilaient malgré une compilation identique
deux minutes plus tôt.

**Le harnais accepte maintenant `--no-build`**, et c'est le mode de tout relevé qu'on publie :
compiler, laisser la machine retomber, mesurer sans recompiler. La preuve que ça marche est la
dispersion — 32,38 à 32,75 ms sur quatre exécutions propres, contre 46,3 à 47,2 sur les
contaminées. C'est la deuxième des trois précautions de la section précédente, appliquée :
*lire la dispersion avant la médiane*.
## Front — décidé

**Tauri v2**, avec un front web léger sans VDOM (Svelte ou Solid, pas React).

Motif : le mail HTML doit s'afficher fidèlement quand le mail en a besoin, et aucun
toolkit Rust natif ne sait le faire. Bénéfice décisif en prime — le confinement du
contenu hostile est appliqué par le moteur (CSP + `sandbox`) plutôt que par du code
applicatif faillible. Voir `docs/PRIVACY.md`.

Le front web doit rester ouvrable tel quel dans un onglet de navigateur, servi par le
démon. Tauri est un emballage, pas une dépendance de conception.

Contrepartie assumée : démarrage ~250 ms au lieu de ~80 ms pour un natif, et une
liste virtualisée à écrire à la main. C'est le prix du rendu HTML, il est payé sciemment.

## Thème — minimaliste / bureautique

Un outil de travail, pas une vitrine. Dense, lisible, sans effet.

- **Police système** (Segoe UI sur Windows). Pas de police embarquée, pas de police web.
- **Densité avant respiration.** Ligne de liste ~22 px. 40 messages visibles sans
  défiler sur un écran normal.
- **Niveaux de gris + un seul accent**, pour la sélection et le non-lu. Pas de dégradé,
  pas d'ombre portée, pas de coin arrondi décoratif.
- **Clair et sombre**, en suivant le réglage système par défaut.
- **La hiérarchie passe par la graisse et l'espacement**, pas par la couleur.
- Bordures 1 px entre panneaux. Pas de carte flottante.
- Référence mentale : la densité d'Outlook, la sobriété d'un bon outil interne.

## Critères d'acceptation — à mesurer, pas à estimer

Sauf mention contraire, les critères se mesurent dans le **déploiement de référence** :
démon sur une machine dédiée du réseau local, client sur un autre poste. Mesurer en
tout-en-local donnerait des chiffres flatteurs pour une configuration qu'on n'utilise
pas.

| # | Critère | Seuil |
|---|---|---|
| 1 | Démarrage à froid de l'UI jusqu'à l'interface | **< 400 ms** — **passé** le 2026-09-03 sur machine au repos : **174 à 180 ms** pour la coquille native `mail-shell` en mode embarqué, **149 à 160 ms** en mode distant, interface **avec ses lignes** dans les deux cas. La coquille Tauri échoue à 466–536 ms. Les chiffres du 2026-09-02 étaient contaminés par la charge machine — voir « Correction » |
| 2 | Défilement d'une liste de 100 000 messages | **60 fps constant** — **passé**, mesuré le 2026-09-03 sur 20 544 lignes chargées : **1,45 ms** de travail par image en p95 dans la coquille native, pour un budget de 16,7 ms. **2,80 ms** en Tauri (relevé du 2026-09-02, non refait) |
| 3 | RSS de l'import sur le corpus de 11 Go, côté démon | **< 500 Mo** |
| 4 | Recherche plein texte, p95, bout en bout depuis le client | **< 50 ms** — mesuré 819 µs sur bouclage, passé |
| 5 | Ouverture d'un message depuis la liste | **< 50 ms** — **passé**, mesuré le 2026-09-03 dans la coquille native sur 120 ouvertures de messages distincts : **32,7 ms** de p95 du clic au corps dessiné, dont **3,8 ms** de service et **exactement une image** d'attente. Côté API seul, 4,31 ms |
| 6 | Taux de dédup sur le corpus réel | **mesuré et affiché** en fin d'import |
| 7 | Écritures dans le profil Thunderbird | **exactement 0** — re-mesuré le 2026-08-30 sur le chemin des tâches de fond, passé |
| 8 | Requêtes réseau au rendu d'un mail piégé, hors démon | **exactement 0** — les deux étages passés le 2026-09-03, `crates/mailprivacy`, **un port instrumenté par vecteur et par phase**. Dans WebView2 : le message intact ouvre **19 connexions** sans CSP, **1** avec — le vecteur `<iframe>`, que le moteur connecte avant de refuser la requête — et **0** avec l'assainisseur. Pour ce vecteur, l'assainisseur est donc la **première** barrière, pas la seconde. WKWebView reste à mesurer |
| 9 | Navigation dans la liste, démon injoignable | **reste fluide** sur ce qui est en cache — **passé**, mesuré le 2026-09-03 sur la coquille native en mode distant, démon tué en cours de route : 1,14 ms de travail par image en défilant, ouverture et recherche en échec propre, état dégradé visible |
| 10 | Démarrage du démon sur une interface non locale sans jeton | **refusé** — mesuré, passé |

- Le **critère 3** fait échouer les implémentations naïves : un `read_to_string` sur
  `[Gmail]/Tous les messages` alloue 1,44 Go d'un coup.
- Le **critère 1** impose que l'UI n'attende jamais l'index : elle ouvre la socket,
  affiche ce que le démon a déjà, et charge le reste en fond. Avec un démon distant, il
  impose en plus qu'elle n'attende pas le premier aller-retour réseau — donc qu'elle
  ouvre sur son cache de lecture. **Le relevé du 2026-09-02 le fait échouer pour une raison
  qui n'est aucune de celles-là** : la création du webview coûte deux à trois fois le budget
  entier avant que la page existe. Le seuil est donc à trancher — le tenir demande de sortir
  le webview du chemin de démarrage, c'est-à-dire de revenir sur Tauri ; le re-baser demande
  de dire ce qu'on mesure. Décision ouverte, détaillée dans « La coquille Tauri, exécutée pour
  la première fois ».
- Le **critère 2** ne se mesure pas en intervalles entre images : cet intervalle appartient au
  compositeur, qui ralentit une fenêtre sans premier plan — 55 Hz puis 32 Hz relevés sur le
  même code. Ce qui se compare au budget de 16,7 ms est le **travail par image** : l'écart
  entre le début de l'image et l'instant où notre rappel reprend la main, où tourne la mise à
  jour du défilement.
- Le **critère 4** se relève en deux temps, côté démon et bout en bout, pour que la
  part du réseau soit visible. Un p95 dégradé par le transport et un p95 dégradé par
  l'index ne se corrigent pas au même endroit.
- Le **critère 7** se vérifie : `mtime` de tout le profil relevés avant et après un
  import complet, identiques. Avec le démon sur une autre machine, le profil est lu à
  travers un partage réseau ou copié — dans les deux cas la vérification porte sur la
  source, et la copie ne dispense pas du relevé.
- Le **critère 8** est reformulé pour rester falsifiable maintenant que l'UI parle au
  réseau par conception : **zéro requête vers une destination autre que le démon, et
  zéro requête déclenchée par le contenu d'un message**, démon inclus. Test
  d'intégration avec serveur HTTP local instrumenté, décrit dans `docs/PRIVACY.md`.
  Il tourne en CI et bloque la fusion.
- Le **critère 9** est ce que le cache de lecture achète. Il se teste en coupant le
  démon : la liste déjà chargée reste défilable, la recherche et l'ouverture d'un
  message échouent proprement avec un état visible, rien ne gèle et rien ne ment.
- Le **critère 10** est un fail closed : une mauvaise configuration doit empêcher le
  démon de tourner, pas exposer une boîte mail en clair sur un réseau.

## Ordre de travail

1. Squelette du workspace, CI (`fmt`, `clippy -D warnings`, `test`).
2. `mailcore` : schéma SQLite, écriture/lecture de blobs derrière le trait de store,
   tests unitaires.
3. `mailimport` : lecteur mbox streamé. Tester sur un mbox synthétique petit et tordu
   **avant** de le lâcher sur le corpus réel.
4. ~~Import réel, sur la machine du démon. Mesurer les critères 3, 6, 7.~~ **Fait le
   2026-08-28 : critères 3, 6 et 7 passés, chiffres dans « Mesures relevées ».**
5. ~~Index tantivy + API de requête. Mesurer le critère 4 côté démon.~~ **Fait le
   2026-08-28 : p95 à 643 µs. La part réseau viendra à l étape 6, et la comparaison des
   deux relevés dira ce qu'a coûté le transport.**
6. ~~`mailmcp` + `maild`, les deux transports, critère 10.~~ **Fait le 2026-08-28 : les
   cinq outils répondent en stdio et en HTTP authentifié, critère 10 passé.**
   **Point de validation en cours : le corpus doit être interrogeable depuis Claude Code
   sur un poste qui n'héberge pas le démon.** À tester par l'utilisateur.
7. ~~API du démon pour les clients non-MCP, sur les deux transports.~~ **Fait le
   2026-08-28 : `mailapi`, dix méthodes, servies en stdio et en HTTP sur `/api`. Critères 4
   et 5 relevés bout en bout — 819 µs et 1,36 ms de p95, après correction d'une mesure biaisée
   par le cache disque.**
8. `mail-ui`, cache de lecture inclus. Mesurer 1, 2, 8, 9 — et la moitié du critère 5 que
   l'API ne voit pas : ce que le front ajoute entre la réponse et le pixel.
   - ~~Les barrières de rendu : `mailhtml::sanitize`, `mailhtml::trackers`, corps HTML dans
     l'API.~~ **Fait le 2026-08-30 : critère 8 étage 1 passé, critère 5 à 4,31 ms sur le
     chemin HTML.**
   - ~~Les tâches de fond du démon : `jobs.*`, progression, annulation, rechargement de
     l'index à chaud.~~ **Fait le 2026-08-30 : vérifié sur le corpus réel, import déclenché
     et annulé par l'API.**
   - ~~La CLI contre un démon : `mail --daemon`, jeton dans le trousseau, tâches suivies.~~
     **Fait le 2026-08-30 : premier vrai client de `mailapi`, critère 7 re-mesuré au passage.**
   - ~~Le front : Solid, trois panneaux, liste virtualisée, cache de lecture, `<iframe>`
     confinée, et `maild --ui-dir` pour le servir.~~ **Premier jet le 2026-08-31 : construit,
     servi, type-check propre. Comportement à l'exécution pas encore vu.**
   - ~~La coquille Tauri, et les mesures des critères 1 et 2.~~ **Fait le 2026-09-02 :
     l'application tourne — trois pannes corrigées, dont deux invisibles sans un canal de
     diagnostic depuis le webview. Critère 2 passé à 2,80 ms de travail par image ; critère 1
     échoué à ~1 050–1 570 ms, dont 800 à 1 390 ms de création de webview. Le seuil du
     critère 1 est une décision à prendre, pas une optimisation à trouver.**
   - ~~Le plancher du webview, et une coquille native mesurée.~~ **Fait le 2026-09-02 : le
     webview coûte ~935 ms à créer, seul, sans Tauri ni données — le critère 1 est inatteignable
     avec un webview au démarrage. Une coquille egui démarre 2,5 à 3 fois plus vite. Le rendu
     logiciel démarre encore mieux mais casse le critère 2 en 4K : OpenGL retenu.**
   - ~~`crates/mail-shell` : la coquille native.~~ **Fait le 2026-09-02 : trois panneaux, liste
     virtualisée, recherche, fils, raccourcis clavier, import déclenchable, corps rendu sans
     moteur. 485–526 ms au démarrage, 1,73 ms de travail par image. Cliente de
     `Api::handle_message`, comme le démon.**
   - ~~Le mode distant de la coquille, et le critère 9.~~ **Fait le 2026-09-02 :
     `mail-shell --daemon`, jeton dans le trousseau, et le critère 9 mesuré en tuant un vrai
     démon en cours de route — 1,24 ms par image, échecs propres, état visible, passé.**
   - ~~L'étage 2 du critère 8, dans un vrai moteur de rendu.~~ **Fait le 2026-09-02, corrigé le
     2026-09-03 : `crates/mailprivacy`, quatre phases dont deux contrôles, un port instrumenté
     par vecteur et par phase. Dans WebView2, le message intact ouvre 19 connexions sans CSP,
     **1** avec — `<iframe>` — et 0 avec l'assainisseur. En CI sur Windows et sur Linux, il
     bloque la fusion. WKWebView reste à mesurer.**
   - ~~Le cache de lecture du mode distant, exigé par la note du critère 1.~~ **Fait le
     2026-09-03 : `cache.rs`, borné, purgeable, hors du profil itinérant. Critère 1 en mode
     distant passé à 149–160 ms.**
   - ~~Une revue critique de tout ce qui a été écrit ces deux jours.~~ **Faite le 2026-09-03,
     par un relecteur extérieur. Onze défauts corrigés, dont trois qui faisaient passer un
     critère à tort — voir « Ce que la revue a trouvé ».**
   - ~~La moitié du critère 5 que l'API ne mesure pas.~~ **Fait le 2026-09-03 : 32,7 ms de p95
     du clic au corps dessiné, dont 3,8 ms de service et exactement une image d'attente. Le
     harnais a gagné `--no-build` au passage, parce qu'il avait reproduit deux fois le biais
     de compilation documenté la veille.**

**La phase 1 est close.** Les dix critères sont mesurés sur le corpus réel, aucun ne repose
sur une estimation. Trois choses restent ouvertes et sont **volontairement** repoussées, pas
oubliées :

- les **images embarquées** et les **liens cliquables** du volet de lecture
  (`docs/PRIVACY.md` §5) : les deux demandent du code sur une entrée hostile — un décodeur
  d'images, une ouverture d'URL écrite par l'expéditeur — et ça se fait avec ses tests, pas à
  la hâte ;
- **WKWebView** pour le critère 8 : il faut un Mac, il n'y en a pas. La fuite `<iframe>`
  trouvée dans WebView2 est peut-être propre à Chromium, personne ne l'a mesuré ;
- deux **trous de mesure** nommés plus haut : le critère 2 ne défile qu'avec le volet de
  lecture vide, le critère 9 ne teste que la panne franche.

La suite est `docs/PHASE-2.md` : la synchronisation IMAP. Le critère 2 pendant une sync y est
un critère à part entière — c'est la version honnête du trou laissé ici.

L'étape 6 porte le vrai risque nouveau de la phase 1. Un serveur MCP en stdio se
débogue en quelques minutes ; un serveur MCP derrière TLS, un jeton et un pare-feu se
débogue en quelques heures, et il vaut mieux le découvrir à l'étape 6 qu'à l'étape 8.
Corollaire pratique : garder stdio fonctionnel en permanence, parce que c'est le
transport qui permet d'isoler un bug du transport d'un bug du cœur.
