# Phase 1 — cœur, MCP, puis lecteur local

## Objectif

Un démon qui importe les 11 Go de mail déjà présents sur le disque, les dédulique,
les indexe, et les expose — **d'abord à un LLM par MCP, ensuite à une interface**.

**Aucun réseau mail. Aucun OAuth.** Le corpus est déjà là, dans le profil Thunderbird.
Ça repousse le marécage IMAP/OAuth en phase 2 et donne dès le premier jour un jeu de
données réel pour mesurer la dédup et la tenue de l'index.

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
- `mailmcp` + `maild` — serveur MCP en lecture seule, en stdio.
- `mail-cli` — import, requête, statistiques, débogage.
- `mail-ui` — trois panneaux (dossiers / liste / lecture), rendu HTML confiné,
  recherche, navigation clavier.

Dehors : envoi, réponse, brouillon, suppression, sync réseau, IA, ouverture de pièces
jointes (on les liste, on ne les ouvre pas).

## Lecture des mbox Thunderbird — pièges connus

- Séparateur `From ` en début de ligne, avec le *From-mangling* (`>From`) à dé-échapper.
- En-têtes propriétaires `X-Mozilla-Status`, `X-Mozilla-Status2`, `X-Mozilla-Keys`.
- **`X-Mozilla-Status` bit `0x0008` = message supprimé non compacté.** Les ignorer à
  l'import : c'est précisément l'espace mort qui gonfle les 11 Go.
- Arborescence : le dossier `X` est un fichier `X`, ses sous-dossiers sont dans `X.sbd/`.
- Noms de dossiers en français, avec espaces et crochets (`[Gmail].sbd/Tous les messages`).
  Ne rien supposer sur l'encodage du nom de fichier.
- Encodages d'en-têtes hétérogènes, MIME malformé, corps tronqués. Un message
  illisible n'interrompt jamais l'import : on le compte, on le journalise, on continue.

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

| # | Critère | Seuil |
|---|---|---|
| 1 | Démarrage à froid de `mail-ui` jusqu'à l'UI interactive | **< 400 ms** |
| 2 | Défilement d'une liste de 100 000 messages | **60 fps constant** |
| 3 | RSS de l'import sur le corpus de 11 Go | **< 500 Mo** |
| 4 | Recherche plein texte, p95 | **< 50 ms** |
| 5 | Ouverture d'un message depuis la liste | **< 50 ms** |
| 6 | Taux de dédup sur le corpus réel | **mesuré et affiché** en fin d'import |
| 7 | Écritures dans le profil Thunderbird | **exactement 0** |
| 8 | Requêtes réseau émises au rendu d'un mail piégé | **exactement 0** |

- Le **critère 3** fait échouer les implémentations naïves : un `read_to_string` sur
  `[Gmail]/Tous les messages` alloue 1,44 Go d'un coup.
- Le **critère 1** impose que l'UI n'attende jamais l'index : elle ouvre la socket,
  affiche ce que le démon a déjà, et charge le reste en fond.
- Le **critère 7** se vérifie : `mtime` de tout le profil relevés avant et après un
  import complet, identiques.
- Le **critère 8** est un test d'intégration avec serveur HTTP local instrumenté,
  décrit dans `docs/PRIVACY.md`. Il tourne en CI et bloque la fusion.

## Ordre de travail

1. Squelette du workspace, CI (`fmt`, `clippy -D warnings`, `test`).
2. `mailcore` : schéma SQLite, écriture/lecture de blobs, tests unitaires.
3. `mailimport` : lecteur mbox streamé. Tester sur un mbox synthétique petit et tordu
   **avant** de le lâcher sur le corpus réel.
4. Import réel. Mesurer les critères 3, 6, 7. **Ne pas avancer tant qu'ils ne passent pas.**
5. Index tantivy + API de requête. Mesurer le critère 4.
6. `mailmcp` + `maild` en stdio. **Point de validation : le corpus est interrogeable
   depuis Claude Code.** S'arrêter ici et faire tester à l'utilisateur.
7. API locale du démon pour les clients non-MCP.
8. `mail-ui`. Mesurer 1, 2, 5, 8.
