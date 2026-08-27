# mailcore

Un client mail natif, rapide et AI-native, écrit en Rust.

> Statut : phase 1, développement initial. Rien n'est utilisable pour l'instant.

## Pourquoi

Les clients mail existants stockent un fichier plat par dossier. Sur un compte Gmail
réel, ça veut dire que le même message est stocké jusqu'à quatre fois — dans `INBOX`,
`Tous les messages`, `Messages envoyés` et `Important` — et que compacter un dossier
signifie réécrire un fichier de plusieurs gigaoctets en synchrone.

mailcore traite un message comme une donnée immuable adressée par son contenu. Les
dossiers et labels sont des références. La duplication devient impossible par
construction, et le compactage n'existe plus.

Voir [`docs/VISION.md`](docs/VISION.md).

## Architecture

Le cœur est une bibliothèque sans dépendance à un toolkit UI ni à un client réseau.
Voir [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md).

## Licence

À décider avant la première publication. Par défaut : MIT OR Apache-2.0.
