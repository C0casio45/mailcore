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
        mail-ui (Tauri)        LLM via MCP            mail-cli
        coquille de rendu   Claude Code / Desktop    scripts, debug
```

Conséquences, et c'est pour ça qu'on le fait :

- **Un LLM accède au mail sans que l'UI tourne.** Le démon est le seul processus
  nécessaire. C'est la vraie réponse à « AI-native » : l'IA n'est pas une fonction
  dans l'app, elle est un client de premier rang, au même titre que l'UI.
- **L'UI démarre instantanément** parce qu'elle ne fait rien : elle ouvre une socket
  et affiche. Plus d'index à charger au lancement.
- Le démon peut tourner sur une autre machine (le Lenovo, 24/7) et faire le travail
  lourd — sync permanente, indexation, embeddings — pendant que le poste de travail
  n'a qu'à afficher.
- Plusieurs frontends coexistent sans se marcher dessus.

### Le compromis à assumer

Si le démon est sur une autre machine, l'UI dépend du réseau pour **tout**, y compris
la simple navigation. Sur un LAN (~1 ms) c'est indolore ; à travers un WAN ça ne l'est
plus. Donc :

- Démon et UI sur la même machine par défaut, socket locale.
- Démon distant possible, même protocole, mais l'UI garde alors un **cache de lecture
  local** (les métadonnées de la liste, pas les corps) pour rester navigable si le
  démon est lent ou injoignable. Ce cache est un détail d'implémentation du front,
  jamais une seconde source de vérité.

## Workspace

```
mailcore/
├── crates/
│   ├── mailcore/      lib   modèle, store, dédup, index, API de requête
│   ├── mailimport/    lib   mbox Thunderbird → store            (phase 1)
│   ├── mailsync/      lib   IMAP / JMAP → store                 (phase 2)
│   ├── mailmcp/       lib   serveur MCP exposant mailcore       (phase 1)
│   ├── maild/         bin   le démon : sync + API + MCP
│   ├── mail-cli/      bin   client CLI (import, requête, debug)
│   └── mail-ui/       bin   coquille Tauri
└── docs/
```

`mailcore` ne dépend d'aucun toolkit UI, d'aucun client réseau, d'aucun serveur.
C'est la règle qui rend tout le reste remplaçable.

## Le store

```
<data_dir>/
├── blobs/<aa>/<bb>/<hash>        message RFC 5322 brut, compressé zstd
├── index.sqlite                  métadonnées, références, fils, comptes
└── search/                       index tantivy
```

### blobs

Clé = **BLAKE3 des octets RFC 5322 bruts**, en hex, shardé sur 2 octets.
Écriture atomique : temporaire hors store, `fsync`, rename. Un blob n'est jamais
modifié — seulement créé, ou supprimé quand plus aucune référence n'y pointe.

La dédup logique (même `Message-ID` mais en-têtes `Received` différents) se fait au
niveau de l'index, pas du blob. On ne perd jamais d'octet reçu.

### index.sqlite

- `accounts(id, kind, display_name)`
- `folders(id, account_id, path, kind)`
- `messages(id, blob_hash, message_id, thread_id, date, from_addr, from_name,
   subject, size, has_attachments, flags)`
- `refs(message_id, folder_id, flags)` — **la table qui tue la duplication.**
  Un message dans `INBOX` et dans `[Gmail]/Tous les messages` = 1 ligne `messages`,
  2 lignes `refs`.
- `threads(id, root_message_id, subject_norm, last_date, message_count)`

`journal_mode=WAL`, `synchronous=NORMAL`. WAL est ce qui permet aux clients de lire
pendant que le démon écrit — prérequis de « l'UI ne bloque jamais ».

### search/

tantivy sur `subject`, `body_text`, `from`, `to`, `folder`, `date`. Le corps est
aplati en texte à l'indexation ; on stocke les postings, pas le contenu — il se
relit depuis le blob.

Phase 4 : index vectoriel à côté, un embedding par message.

## L'API du démon

Socket locale par défaut (named pipe sur Windows), TCP + token si démon distant.
Protocole : JSON-RPC, le même que MCP — une seule couche de sérialisation à maintenir.

Opérations : lister dossiers, paginer une liste de messages, lire un message,
rechercher, s'abonner aux changements. Toutes non bloquantes côté client.

## Le serveur MCP

Exposé par `maild`, en stdio (client local type Claude Code) **et** en HTTP
streamable (client distant). Crate `rmcp`.

Outils exposés : `search_mail`, `get_thread`, `get_message`, `list_folders`,
`get_contact_history`. En lecture seule au départ — l'écriture (répondre, envoyer)
arrive en phase 3 et devra passer par une confirmation explicite de l'utilisateur.

**Le serveur MCP est le premier frontend, avant l'UI.** Dès qu'il existe, le mail est
utilisable depuis Claude Code sur le corpus réel. C'est ce qui valide le cœur sans
avoir écrit une ligne d'interface.

## Threading

`jwz` classique : `In-Reply-To` + `References`, repli sur sujet normalisé
(`Re:`, `RE:`, `TR:`, `Fwd:`) quand les en-têtes manquent. Problème résolu depuis
longtemps, l'implémenter fidèlement plutôt que l'inventer.

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
| IMAP | `async-imap` (phase 2) |
| SMTP | `lettre` (phase 3) |
| Logs | `tracing` + `tracing-subscriber` |
| Erreurs | `thiserror` (libs) / `anyhow` (bins) |

## Le front

**Tauri v2.** Motif : afficher fidèlement le mail HTML quand le mail en a besoin,
ce qu'aucun toolkit Rust natif ne sait faire. Le webview est ici un moteur de rendu
de document, pas un framework applicatif — la logique reste dans le démon.

Bénéfice secondaire, et il est important : le confinement du contenu hostile est
**appliqué par le moteur** via CSP et `sandbox`, pas par du code applicatif qu'on
pourrait oublier de faire tourner. Voir `docs/PRIVACY.md`.

Le front web doit rester utilisable tel quel dans un onglet de navigateur, servi par
le démon. Tauri est un emballage, pas une dépendance de conception.

Framework front : léger et sans VDOM (Svelte ou Solid). Pas de React. La liste de
messages est virtualisée à la main — c'est le seul morceau de front qui demande du soin.

## Phases

1. **Cœur + MCP + lecteur local** — import mbox, store, index, recherche, serveur MCP,
   puis UI. Aucun réseau mail.
2. **Sync** — IMAP + OAuth2 Google/Microsoft. Le marécage. Lecture d'abord.
3. **Écriture** — rédaction, réponse, SMTP.
4. **IA** — embeddings, recherche sémantique, tri, résumé de fil.
