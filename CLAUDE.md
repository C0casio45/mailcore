# mailcore — instructions de travail

Client mail open source, natif, AI-native, écrit en Rust.

Lis `docs/VISION.md` (le pourquoi), `docs/ARCHITECTURE.md` (le comment),
`docs/PRIVACY.md` (non négociable) et
`docs/PHASE-1.md` (ce qu'il faut livrer maintenant) avant d'écrire du code.

## Règles absolues

1. **Ne jamais écrire dans le profil Thunderbird.** Il est en production, l'utilisateur
   s'en sert tous les jours en parallèle. On lit ses fichiers mbox, on n'y touche
   jamais. Pas de rename, pas de troncature, pas de fichier temporaire déposé
   dedans. Ouvrir en lecture seule explicitement.
   Chemin : `C:\Users\<vous>\AppData\Roaming\Thunderbird\Profiles\<profil>`

2. **Rust partout.** Y compris l'outillage, les scripts de bench et les utilitaires.
   Pas de Python ni de shell script sauf contrainte de runtime impossible à contourner,
   et dans ce cas, dire explicitement laquelle.

3. **L'UI ne bloque jamais sur le réseau ou sur un import.** Le store local est
   l'unique source de vérité pour l'affichage. Toute tâche longue est un job de fond
   qui écrit dans le store ; l'UI observe le store. Si la sync met 20 minutes,
   l'interface doit rester à 60 fps pendant tout ce temps.

4. **Rien ne charge un mbox entier en mémoire.** Certains font 1,4 Go. Tout est
   streamé, y compris à l'import et à l'indexation.

5. **Aucune requête réseau déclenchée par le contenu d'un mail.** Ni image distante,
   ni police, ni accusé de réception, tant que l'utilisateur n'a pas cliqué. C'est une
   règle de conception, pas une préférence : voir `docs/PRIVACY.md` et le test
   d'intégration qui la verrouille en CI.

## Style de code

- Édition 2024, `#![forbid(unsafe_code)]` dans chaque crate sauf justification écrite.
- `thiserror` pour les erreurs de bibliothèque, `anyhow` uniquement dans les binaires.
- `tracing` pour les logs, jamais `println!` en dehors d'une sortie CLI destinée à l'utilisateur.
- Pas de `unwrap()` / `expect()` hors tests et hors invariants prouvés localement
  (et dans ce cas, un commentaire qui dit pourquoi c'est un invariant).
- Tout ce qui touche au parsing d'entrée hostile (MIME, en-têtes, HTML) doit avoir
  des tests sur des cas malformés, pas seulement sur des cas propres.

## Décisions déjà prises

- Dédup par contenu adressé (BLAKE3 sur les octets RFC 5322 bruts).
- Index métadonnées en SQLite, recherche plein texte en tantivy.
- Parsing MIME avec `mail-parser`.
- Le cœur est une bibliothèque (`mailcore`) ; l'UI est un shell mince et remplaçable.
- Architecture démon + coquilles : `maild` headless fait tout, l'UI et les LLM sont des clients.
- Front en Tauri v2, pour le rendu HTML confiné. Voir `docs/PRIVACY.md`.
- Le serveur MCP est le premier frontend, avant l'UI.

## Vérification

Avant de dire qu'une étape est finie : `cargo fmt --check`, `cargo clippy -- -D warnings`,
`cargo test`, et les critères d'acceptation chiffrés de `docs/PHASE-1.md` réellement
mesurés sur le corpus réel — pas estimés.
