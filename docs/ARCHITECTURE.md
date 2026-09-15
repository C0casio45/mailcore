# Architecture

## Le principe : un démon, des coquilles

Le cœur ne vit pas dans l'application graphique. Il vit dans un **démon local sans
interface** qui synchronise, stocke, indexe et expose. Tout le reste — l'UI, un LLM,
une CLI — n'est qu'un client de ce démon.

```
                   ┌──────────────────────────────┐
                   │  maild  (headless, Rust)     │
                   │                              │
   IMAP/JMAP ──────▶  sync ─▶ store ─▶ index      │
                   │            │        │        │
                   │            ▼        ▼        │
                   │      ┌───────────────────┐   │
                   │      │  API locale       │   │
                   │      │  + serveur MCP    │   │
                   │      └─────────┬─────────┘   │
                   └────────────────┼─────────────┘
                                    │
              ┌─────────────────────┼─────────────────────┐
              ▼                     ▼                     ▼
      mail-shell (egui)        LLM via MCP            mail-cli
      coquille native      Claude Code / Desktop    scripts, debug
      (mail-ui / Tauri
       garde le même rôle)
```

Conséquences, et c'est pour ça qu'on le fait :

- **Un LLM accède au mail sans que l'UI tourne.** Le démon est le seul processus
  nécessaire. C'est la vraie réponse à « AI-native » : l'IA n'est pas une fonction
  dans l'app, elle est un client de premier rang, au même titre que l'UI.
- **L'UI démarre instantanément** parce qu'elle ne fait rien : elle ouvre une socket
  et affiche. Plus d'index à charger au lancement.
- Le démon peut tourner sur une machine dédiée, allumée en permanence, et y faire le
  travail lourd — sync continue, indexation, embeddings — pendant que le poste de
  travail n'a qu'à afficher.
- Plusieurs frontends coexistent sans se marcher dessus.

### Deux déploiements, tous les deux de phase 1

**Local.** Démon et UI sur la même machine, socket locale — named pipe sur Windows,
socket Unix ailleurs. C'est le déploiement par défaut d'une installation neuve, et
celui qu'un contributeur obtient sans rien configurer.

**Distant.** Le démon sur une machine dédiée, les clients ailleurs sur le réseau
local. Même protocole, transport différent. Ce n'est **pas** repoussé à une phase
ultérieure : le serveur MCP de la phase 1 doit être joignable depuis un poste qui
n'héberge pas le démon, sinon la validation de l'étape 6 ne vaut que pour un cas
d'usage qu'on n'a pas.

### Le compromis à assumer

En déploiement distant, l'UI dépend du réseau pour **tout**, y compris la simple
navigation dans une liste. Sur un LAN (~1 ms) c'est indolore ; à travers un WAN ça ne
l'est plus. Deux conséquences, et la première n'est pas optionnelle :

- L'UI garde un **cache de lecture local** — les métadonnées de la liste, jamais les
  corps. Il existe pour deux raisons : rester à 60 fps quand chaque page de liste est
  un aller-retour réseau, et rester navigable quand le démon est lent ou injoignable.
  C'est un détail d'implémentation du front, jamais une seconde source de vérité :
  il est jetable, reconstruit depuis le démon, et jamais consulté pour un corps de
  message.
- Les images `cid:` d'un message — les pièces jointes affichées en ligne — sont
  servies par le démon, donc une requête réseau par image. Sans effet sur la garantie
  de `docs/PRIVACY.md` (rien ne sort vers l'extérieur, le démon n'est pas Internet),
  mais avec un effet sur la latence d'ouverture d'un message.

## Workspace

```
mailcore/
├── crates/
│   ├── mailcore/      lib   modèle, store, dédup, index, API de requête
│   ├── mailhtml/      lib   CSP, assainissement, blocs, détection de traceurs
│   ├── mailimport/    lib   mbox Thunderbird → store
│   ├── mailmcp/       lib   serveur MCP exposant mailcore
│   ├── mailapi/       lib   contrat JSON-RPC des clients non-MCP
│   ├── maild/         bin   le démon : store + API + MCP + tâches de fond
│   ├── mail-cli/      bin   client CLI (import, requête, debug)
│   ├── mail-shell/    bin   **la coquille native egui** — l'interface livrée
│   ├── mail-ui/       bin   coquille Tauri — gardée, plus lente au démarrage
│   ├── mailprivacy/   bin   les deux étages du critère 8, message piégé compris
│   ├── mailsync/      lib   IMAP → store : client, moisson, TLS
│   ├── mailfake/      lib   serveur IMAP de test, sait répondre faux
│   └── mailauth/      lib   mots de passe et OAuth2, trousseau   (phase 2)
├── xtask/             bin   mesure des critères d'acceptation
└── docs/
```

Trois sondes jetables — `mail-spike-native`, `mail-spike-software`,
`mail-spike-webview` — ont servi à isoler le plancher de démarrage de chaque pile
graphique. Elles restent dans l'arbre parce qu'un chiffre qu'on ne peut plus refaire n'est
pas un chiffre.

### La coquille écrit deux choses sans passer par l'API — 2026-09-11

Tout ce que `mail-shell` affiche vient de `mailapi`. Deux exceptions, et elles suivent la même
règle : **ce qui ne doit pas traverser une frontière de processus n'a pas de méthode.**

| ce qui est fait localement | ce qui ne doit pas traverser |
|---|---|
| `Link::stage` — ranger une pièce jointe | un **chemin de fichier** : le donner à l'API donnerait la lecture de n'importe quel fichier à qui détient le jeton |
| `crate::settings` — déclarer un compte | un **secret** : une méthode `accounts.add` ferait écrire dans le trousseau de la machine du démon, et transporterait le mot de passe pour y arriver |

La page de paramètres **lit** aussi le store directement, et c'est le même raisonnement pris
dans l'autre sens : `accounts.list` ne rend ni hôte ni port — critère 7 de `docs/PHASE-3.md` —
et l'élargir pour faire marcher un écran local donnerait l'infrastructure de lecture de
quelqu'un à tout client distant.

Conséquence assumée : en mode `--daemon`, la page refuse d'écrire et dit pourquoi. C'est
exactement ce que fait déjà `mail account …` avec le même drapeau.

**Deux coquilles, une seule livrée.** `mail-shell` démarre en 174–180 ms, `mail-ui` en
466–536 ms, et le critère 1 en demande moins de 400. La coquille Tauri est gardée : elle est
le seul endroit où une CSP de moteur de rendu est mise à l'épreuve, et c'est aussi la
porte de sortie si le rendu natif d'un mail très mis en page devenait insuffisant. Les deux
sont clientes du **même** `mailapi`, donc aucune logique n'existe en double.

`mailhtml` existe séparément parce que ses barrières servent deux appelants qui ne se
connaissent pas : `mailcore` compte les traceurs à l'indexation, le front assainit avant
le rendu. Sans crate partagé, la chaîne CSP et la liste blanche `ammonia` existeraient
en deux copies — exactement le point de défaillance unique que `docs/PRIVACY.md` cherche
à écarter.

`mailapi` existe séparément de `mailmcp` pour la même raison inversée : les deux exposent
le même cœur, mais à deux publics qui ne veulent pas les mêmes données. Un modèle veut une
date lisible et se passe des drapeaux ; une interface veut des secondes Unix, des curseurs
de pagination et l'état lu/non-lu. Le protocole est commun, la forme ne l'est pas —
fusionner les deux ferait payer à chaque client les besoins de l'autre. `mailapi` est aussi
la dépendance que les clients tirent : les mêmes types écrivent les réponses côté démon et
les relisent côté UI, donc un champ renommé casse à la compilation.

`xtask` existe parce que les critères de `docs/PHASE-1.md` sont à mesurer, pas à estimer,
et que l'outillage est en Rust comme le reste.

`mailcore` ne dépend d'aucun toolkit UI, d'aucun client réseau, d'aucun serveur.
C'est la règle qui rend tout le reste remplaçable.

## Le store

```
<data_dir>/
├── blobs/<aa>/<bb>/<hash>        message RFC 5322 brut, compressé zstd
├── index.sqlite                  métadonnées, références, fils, comptes
├── search/                       index tantivy
└── tmp/                          temporaires d'écriture, même volume que blobs/
```

### blobs

Clé = **BLAKE3 des octets RFC 5322 bruts**, en hex, shardé sur 2 octets.
Écriture atomique : temporaire hors de l'arborescence des blobs mais **sur le même volume**
(`<root>/tmp/`), `fsync`, `rename`. Le même volume est ce qui rend le `rename` atomique ;
un temporaire dans le répertoire système du même nom ne le serait pas. Un blob n'est jamais
modifié — seulement créé, ou supprimé quand plus aucune référence n'y pointe.

La dédup logique (même `Message-ID` mais en-têtes `Received` différents) se fait au
niveau de l'index, pas du blob. On ne perd jamais d'octet reçu.

Le hash porte sur les octets **tels que stockés** : séparateur mbox exclu,
*From-mangling* dé-échappé, en-têtes propriétaires du client d'origine retirés. Sans
ça, le même message importé depuis deux dossiers donnerait deux blobs et la dédup ne
servirait à rien.

L'accès aux blobs passe par un trait, avec le système de fichiers pour seule
implémentation en phase 1. Un blob immuable adressé par contenu est exactement le
modèle d'accès d'un stockage objet, donc une implémentation S3 reste possible plus
tard — mais elle ne sera jamais le défaut : l'import écrit des centaines de milliers
de petits objets, et le surcoût par requête y est ce qui décide.

### Une seule frontière à ne jamais franchir

Aucun code hors de `mailcore::store` et `mailcore::index` ne sait quel moteur est
dessous. `query.rs` rend des types de `model.rs`, jamais un `rusqlite::Row`, jamais un
document tantivy. C'est ce qui rend le choix du moteur révisable : si une mesure
condamne SQLite, on remplace un module, pas le projet.

### index.sqlite

- `accounts(id, kind, display_name)`
- `folders(id, account_id, path, kind)`
- `messages(id, blob_hash, message_id, thread_id, date, from_addr, from_name,
   subject, size, has_attachments)` — pas de `flags` ici : ils appartiennent à la
   référence, parce que le même contenu peut être lu dans un dossier et non lu dans un
   autre. S'y ajoutent trois drapeaux de suivi — `contacts_counted`, `indexed`,
   `thread_link` — et un fait dérivé, `subject_norm` : ce qui permet aux passes dérivées
   d'avancer après une moisson au lieu de tout refaire.
- `refs(message_id, folder_id, date, flags)` — **la table qui tue la duplication.**
  Un message dans `INBOX` et dans `[Gmail]/Tous les messages` = 1 ligne `messages`,
  2 lignes `refs`.
- `threads(id, root_message_id, subject_norm, last_date, message_count)`
- `message_references(message_id, rfc822_id)` — ce que chaque message cite. Elle existe
  pour la question que les blobs ne savent pas poser sans être tous relus : **qui répond
  à ce message ?** La réponse est dans les en-têtes des *autres*.

`journal_mode=WAL`, `synchronous=NORMAL`. WAL est ce qui permet aux clients de lire
pendant que le démon écrit — prérequis de « l'UI ne bloque jamais ».

### search/

tantivy sur `subject`, `body_text`, `from`, `to`, `folder`, `date`. Le corps est
aplati en texte à l'indexation ; on stocke les postings, pas le contenu — il se
relit depuis le blob.

Phase 4 : index vectoriel à côté, un embedding par message.

## L'API du démon

Protocole : JSON-RPC 2.0, le même que MCP — une seule couche de sérialisation à
maintenir. Contrat dans le crate `mailapi`, transports dans `maild`. Lecture seule en
phase 1 : aucune méthode n'écrit, donc aucun client ne peut abîmer un store.

| Méthode | Ce qu'elle rend |
|---|---|
| `server.hello` | version, protocole, révision, disponibilité de la recherche, nombre de messages |
| `server.methods` | la liste des méthodes servies |
| `folders.list` | les dossiers de tous les comptes, avec compteurs |
| `messages.page` | une page de liste, avec le curseur opaque de la suivante |
| `messages.get` | un message ouvert : en-têtes, corps en texte, pièces jointes listées |
| `messages.thread` | le fil d'un message |
| `messages.source` | les octets RFC 5322 d'un message reçu, rendus lisibles |
| `outbox.source` | les octets **qu'on a composés**, tels qu'ils ont été remis au `DATA` — voir `docs/PRIVACY.md` §6 bis |
| `search.query` | recherche plein texte |
| `store.stats` | l'état du store |
| `store.revision` | la révision courante |
| `store.wait` | attend qu'elle change — l'abonnement |
| `jobs.sources` | les profils que l'opérateur autorise à importer |
| `jobs.start` | met une tâche de fond en file : `import`, `index`, `thread` |
| `jobs.list` / `jobs.get` | les tâches connues, avec leur progression |
| `jobs.cancel` | demande l'arrêt d'une tâche |

Toutes non bloquantes côté client. `server.hello` répond en un aller-retour à tout ce qu'un
client doit savoir pour s'ouvrir : c'est ce que le critère 1 exige quand le démon est au bout
du réseau.

### L'abonnement est un long-poll sur une révision, pas un flux poussé

`store.wait` est la forme que le déploiement a imposée. Un canal interne au démon —
`broadcast`, `notify` — ne se déclencherait jamais : en phase 1 l'import et l'indexation
tournent dans un **autre processus** (`mail import`, `mail index`), invisible du démon.

Ce que le démon peut observer, c'est le store : `PRAGMA data_version`, qui change dès qu'une
autre connexion valide une écriture, plus la date de `meta.json` de l'index tantivy. Les deux
forment un jeton de révision **opaque et sans ordre** — deux révisions se comparent par
égalité, « différente » veut dire « relis ». Le client la présente, le démon répond aussitôt
si elle a bougé, sinon il relève toutes les 250 ms jusqu'au délai demandé.

### Transports et authentification

Deux transports, une seule règle qui les sépare : **le démon ne sert du mail sans
authentification que sur une interface qu'il est seul à pouvoir atteindre.**

| Transport | Authentification | Chiffrement |
|---|---|---|
| stdio | permissions du système de fichiers | sans objet |
| TCP | jeton porteur, obligatoire | TLS, obligatoire hors bouclage |

En TCP, les deux protocoles cohabitent sur le même port — `/mcp` et `/api` — derrière la
**même** couche de jeton. Un seul port ouvert est un seul port à protéger, et une seule
surface d'authentification est une surface qui ne peut pas divergrer. En stdio il faut
choisir (`maild stdio --protocol mcp|api`) : il n'y a qu'un flux d'entrée, et rien dans un
message ne dirait auquel des deux protocoles il appartient.

**Correction : la socket locale nommée n'existe pas.** Ce document décrivait le transport
local comme un *named pipe* sur Windows et une socket Unix ailleurs. stdio couvre le même
besoin sans code spécifique à la plateforme — le client démarre le démon en processus fils et
parle sur les flux qu'il vient d'ouvrir, avec les mêmes garanties. Ce qu'une socket nommée
apporterait de plus est un démon **déjà lancé** auquel plusieurs clients se rattachent : un
vrai besoin, mais de service installé, pas de phase 1.

Le démon **refuse de démarrer** s'il est configuré pour écouter sur une interface non
locale sans jeton. Fail closed : une mauvaise configuration doit empêcher le service
de tourner, pas exposer une boîte mail en clair sur un réseau.

Le jeton est généré par le démon, jamais choisi par l'utilisateur, et stocké côté
client dans le trousseau du système — jamais dans un fichier de configuration ni dans
une variable d'environnement (`docs/PRIVACY.md`, section 7).

Le déploiement recommandé pour l'accès distant reste un tunnel déjà chiffré et
authentifié, monté en dehors de mailcore (WireGuard, tunnel SSH). Il ramène le cas
distant au cas local et réduit à zéro la surface d'attaque que nous écrivons
nous-mêmes. Le TLS du transport TCP est la ceinture pour qui ne veut pas de tunnel,
pas une invitation à exposer le démon sur un réseau hostile.

### Les tâches de fond, et pourquoi elles passent par l'API

Importer, indexer et reconstruire les fils sont des **écritures**, et elles prennent des
minutes. Elles ne peuvent donc ni bloquer une requête, ni exiger un shell sur la machine du
démon — sinon la première action de tout utilisateur, importer son profil, serait
inaccessible dans le déploiement distant.

Un **fil du système dédié**, un job à la fois. SQLite n'a qu'un écrivain ; sérialiser n'est
pas une commodité, c'est la seule façon d'éviter que deux tâches se disputent le verrou ou
qu'une indexation lise un store en mouvement.

La tâche ouvre **sa propre** connexion. Le mode WAL autorise un écrivain et des lecteurs
simultanés, donc l'API et MCP continuent de servir pendant l'import — c'est ce qui rend vraie
la règle « l'UI ne bloque jamais sur un import ». Et parce que l'écriture vient d'une autre
connexion, `PRAGMA data_version` change pour les lecteurs : `store.wait` réveille les clients
sans qu'on ait ajouté de mécanisme.

**Un client ne nomme jamais un chemin.** Les profils importables sont déclarés par l'opérateur
au démarrage (`--profile`, répétable, vide par défaut) ; un client choisit un rang. Sans cette
règle, quiconque détient le jeton ferait lire n'importe quel fichier de la machine du démon,
rangé dans le store puis relisible par `search.query`. Le démon n'a aucun moyen de savoir ce
qui est légitime ; l'opérateur, si.


## Le serveur MCP

Exposé par `maild`, en stdio (client sur la même machine) **et** en HTTP streamable
(client distant). Crate `rmcp`. **Les deux transports sont de phase 1** : un serveur
MCP joignable uniquement en stdio n'est utilisable que par un client qui tourne à côté
du démon, ce qui exclut le déploiement distant décrit plus haut.

Le transport HTTP suit la même règle que l'API : jeton porteur et TLS obligatoires
hors interface locale, refus de démarrer sinon.

Outils exposés : `search_mail`, `get_thread`, `get_message`, `list_folders`,
`get_contact_history`. En lecture seule au départ — l'écriture (répondre, envoyer)
arrive en phase 3 et devra passer par une confirmation explicite de l'utilisateur.

Ce qui part vers un modèle est du **texte assaini** (`mailhtml`), jamais du HTML brut :
un corps de message est du contenu écrit par un inconnu, et les injections de prompt
dans les mails existent. Les pièces jointes sont listées, jamais renvoyées.

**Le serveur MCP est le premier frontend, avant l'UI.** Dès qu'il existe, le mail est
utilisable depuis Claude Code sur le corpus réel. C'est ce qui valide le cœur sans
avoir écrit une ligne d'interface.

## Threading

`jwz` classique : `In-Reply-To` + `References`, repli sur sujet normalisé
(`Re:`, `RE:`, `TR:`, `Fwd:`) quand les en-têtes manquent. Problème résolu depuis
longtemps, l'implémenter fidèlement plutôt que l'inventer.

Deux passes, **une seule règle** — `mailcore::thread::partition`. La reconstruction voit
tout le store ; la passe qui suit une moisson ne voit que le **voisinage** du message qui
arrive, et ce voisinage est fermé : aucune arête du graphe n'en sort, donc y rejouer la
règle donne le même découpage. C'est la différence avec le carnet et l'index, qui se
contentent d'un drapeau par ligne : un fil se calcule à partir de messages qui arrivent
*après* celui qu'on traite, donc « incrémental » y veut dire « recalculer le voisinage
touché », jamais « ne regarder que le nouveau ».

## Dépendances retenues

| Rôle | Crate |
|---|---|
| Parsing MIME | `mail-parser` |
| Construction MIME | `mail-builder` (phase 3) |
| Assainissement HTML | `ammonia` |
| Hash | `blake3` |
| Compression | `zstd` |
| Index métadonnées | `rusqlite` (bundled) |
| Recherche plein texte | `tantivy` |
| MCP | `rmcp` |
| Async | `tokio` |
| CLI | `clap` |
| TLS du transport distant | `rustls` + `tokio-rustls` |
| Trousseau du système | `keyring` |
| Graphe en mémoire | `petgraph` (phase 4) |
| IMAP | `async-imap` (phase 2) |
| SMTP | `lettre` (phase 3) |
| Logs | `tracing` + `tracing-subscriber` |
| Erreurs | `thiserror` (libs) / `anyhow` (bins) |

Pas d'OpenSSL : `rustls` évite une dépendance système à installer sur trois
plateformes, et le démon n'a pas besoin d'interopérer avec un parc existant.

## Le front

### La coquille livrée est native — décidé le 2026-09-02, mesuré le 2026-09-03

**Ce qui suit décrit le choix de Tauri et ses raisons ; elles tenaient, et une mesure les a
tranchées dans l'autre sens.** Le webview coûte à lui seul ~290 ms à créer, avant toute
donnée, sur une machine au repos. Le critère 1 en accorde 400 au démarrage entier. La
coquille Tauri se mesure à 466–536 ms, la coquille native egui à **174–180 ms**.

`crates/mail-shell` est donc l'interface livrée. Elle rend le corps d'un message **sans
moteur de rendu** : `mailhtml::sanitize` puis `mailhtml::blocks`, et ce qui est dessiné est
du texte et des rectangles. La garantie de `docs/PRIVACY.md` devient constructive au lieu
d'être configurée — il n'y a plus de politique de moteur à tenir, parce qu'il n'y a plus de
code capable d'émettre une requête sur le chemin d'un message.

Ce qui est perdu, et c'est réel : la fidélité sur le mail très mis en page. Une newsletter à
tables imbriquées se lit en mode lecture. Ce qui est gagné, à côté du démarrage : aucune
chaîne d'outils JavaScript pour construire l'application.

**Tauri reste dans l'arbre**, et le raisonnement ci-dessous reste valable pour ce qu'il est :
le seul endroit du projet où une CSP de moteur de rendu est mise à l'épreuve — c'est ce qui
a permis de découvrir la fuite `<iframe>` de WebView2 — et la porte de sortie si le rendu
natif s'avérait insuffisant. Les deux coquilles sont clientes du même `mailapi`.

### Le raisonnement d'origine, conservé

**Tauri v2.** Motif : afficher fidèlement le mail HTML quand le mail en a besoin,
ce qu'aucun toolkit Rust natif ne sait faire. Le webview est ici un moteur de rendu
de document, pas un framework applicatif — la logique reste dans le démon.

Bénéfice secondaire, et il est important : le confinement du contenu hostile est
**appliqué par le moteur** via CSP et `sandbox`, pas par du code applicatif qu'on
pourrait oublier de faire tourner. Voir `docs/PRIVACY.md`.

Le front web doit rester utilisable tel quel dans un onglet de navigateur, servi par
le démon. Tauri est un emballage, pas une dépendance de conception.

### Framework front : Solid — décidé le 2026-08-31

Léger et sans VDOM. La liste de messages est virtualisée à la main — c'est le seul morceau
de front qui demande du soin, et c'est lui qui a décidé du choix.

**Solid plutôt que Svelte**, à égalité par ailleurs. Le cadrage habituel « Svelte =
compilateur, Solid = signaux » est périmé : Svelte 5 est aussi à base de signaux. Ce qui
reste différent, et qui tranche ici, c'est le recyclage de lignes. `<Index>` est clé par
**position** : les quarante lignes visibles restent les mêmes nœuds DOM et leur contenu
change par signal. `<For>`, clé par identité, crée et détruit des nœuds en défilant. Le
premier est exactement le motif qu'on veut, et il s'écrit sans contorsion.

Ce qu'on perd : le CSS scopé par composant de Svelte, remplacé par des fichiers CSS
ordinaires — pour un thème aussi dépouillé, ce n'est pas un sacrifice — et un écosystème
plus grand, dont on n'utilise à peu près rien (pas de bibliothèque de composants, pas de
gestion d'état, les signaux suffisent).

**Pas de React**, et la raison n'est pas la mode :

- **Le critère 2.** À 16,7 ms par image, une mise à jour d'état qui ré-exécute des fonctions
  de composant et réconcilie l'ensemble visible travaille contre nous à chaque image. C'est
  faisable, mais le chemin rapide consiste à *sortir React de la boucle* — refs et mutation
  DOM directe — donc à l'adopter pour l'écarter là où le travail est difficile.
- **La granularité des mises à jour.** L'application entière consiste à observer le store et
  à en refléter les changements : un message marqué lu, un tick de progression, un
  `store.wait` qui réveille le client. Avec des signaux, chacun touche ce qu'il concerne et
  rien d'autre. Avec React, chacun est un `setState` qui re-rend un sous-arbre sauf
  discipline de mémoïsation soutenue — et cette discipline se relâche toujours.
- **On n'utiliserait aucune de ses forces.** Server components, SSR en flux, transitions
  concurrentes, écosystème de composants : une fenêtre, trois panneaux, un démon local et un
  thème fait main n'en ont aucun usage. Et React attire un gestionnaire d'état, une couche
  de requêtes, un routeur — ce qui travaille contre « coquille mince et remplaçable ».

Ce qui **n'est pas** un argument, pour être honnête : la taille du bundle. 40 Ko de plus
s'analysent en quelques millisecondes, et le coût dominant du démarrage à froid est
l'initialisation du webview. React ne mettrait pas le critère 1 en danger.

Le coût de la décision, lui, est réel : mailcore est destiné à être publié, et beaucoup plus
de gens savent écrire du React que du Solid. Deux choses l'atténuent — le front est petit et
volontairement remplaçable, et Solid c'est du JSX, donc quelqu'un qui vient de React est
opérationnel vite, moins le piège « les composants ne tournent qu'une fois ».

## Phases

1. **Cœur + MCP + lecteur local** — import mbox, store, index, recherche, serveur MCP
   (stdio **et** HTTP authentifié), puis UI. Aucun réseau *mail* : le seul réseau de la
   phase 1 est celui entre le démon et ses propres clients.
2. **Sync** — IMAP + OAuth2 Google/Microsoft. Le marécage. Lecture d'abord.
3. **Écriture** — rédaction, réponse, SMTP.
4. **IA** — embeddings, recherche sémantique, tri, résumé de fil.
