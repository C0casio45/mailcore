# mailcore — instructions de travail

Client mail open source, natif, AI-native, écrit en Rust.

Lis `docs/VISION.md` (le pourquoi), `docs/ARCHITECTURE.md` (le comment),
`docs/PRIVACY.md` (non négociable),
`docs/PHASE-3.md` (ce qu'il faut livrer maintenant),
`docs/PHASE-2.md` et `docs/PHASE-1.md` (ce qui est fait, mesuré, et ce que les mesures ont
appris)
avant d'écrire du code.

**La phase 1 est close depuis le 2026-09-03** et **la phase 2 depuis le 2026-09-09** : leurs dix
critères respectifs sont mesurés sur le corpus réel. La phase 3 est l'envoi, puis le carnet
d'adresses, la signature riche et les invitations reçues.

**L'envoi est le premier morceau irréparable du projet.** Un bug de lecture affiche faux ; un bug
d'envoi expédie un message à quelqu'un. Les critères 1, 2 et 8 de `docs/PHASE-3.md` sont là pour
ça, et l'ordre de travail met la file d'envoi avant l'interface.

**L'envoi marche depuis le 2026-09-09**, et deux règles en sortent, à ne pas défaire :

- **un message en `committing` ne se remet jamais automatiquement.** C'est le critère 2. La règle
  vit à deux endroits — la clause `WHERE` de `Store::deliverable` et `SendState::is_deliverable` —
  et un test vérifie qu'ils sont d'accord. L'ordre des écritures qui la rend vraie est écrit à un
  seul endroit, `mailsmtp::queue::deliver_one` ;
- **le serveur de soumission n'est jamais déduit du serveur IMAP.** `mail account list` suggère,
  `mail account submission` écrit. Une configuration à moitié écrite est refusée, parce que
  deviner le mode de chiffrement rétrograderait le chiffrement à l'insu de l'utilisateur.

Et une leçon sur les types qui portent un secret : `#[derive(Debug)]` y est le **défaut
dangereux**. Il affichait le mot de passe dans `Credential`, des deux côtés. Les deux ont
maintenant un `Debug` écrit à la main et un test qui le vérifie.

**L'interface n'envoie rien elle-même**, depuis le même jour. Un clic écrit une ligne dans la
file et rend la main ; `maild::outbox::Postman` parle au serveur. C'est la règle 3 ci-dessous, et
son corollaire est que le message survit à la fermeture de la fenêtre. La décision — doute,
recul, abandon — reste dans `mailsmtp::queue::deliver_one`, qui est aussi ce qu'appelle
`mail send` : **un seul endroit décide**, deux moteurs l'appliquent.

**Le drapeau `\Seen` est poussé depuis le 2026-09-09**, et deux choses en dépendent :

- `refs.flags` est **dérivé** de `remote_uids.flags`. Y écrire directement est effacé au
  prochain recalcul : toute marque locale va dans les copies, et `flag_pushes` retient ce qu'il
  reste à dire au serveur. L'ordre est **pousser, puis relire** ;
- le raccourci `LIST-STATUS` ne regarde que ce que le serveur annonce, donc il évitait les
  dossiers qui avaient une poussée en attente et la marque ne partait jamais. `up_to_date` a un
  refus en tête pour ça — **c'est la seule raison locale de ne pas sauter un dossier**, et si un
  autre état local devient poussable, il faut passer par là.

Et la moisson ouvre en `EXAMINE`, en lecture seule. Un dossier n'est rouvert en `SELECT` que
s'il a une poussée en attente : le droit d'écrire est demandé au dernier moment et pour un
dossier à la fois.

**Les pièces jointes sont streamées de bout en bout depuis le 2026-09-09**, et la chaîne se casse
au premier maillon qu'on remet en tampon : `Draft::write_to` → `BlobStore::put_writer` →
`Client::finish_data_from` → `Stuffing`. Trois règles en sortent :

- **une pièce jointe est désignée par son contenu**, jamais par un chemin. Un chemin dans une
  demande de client donnerait la lecture de n'importe quel fichier à quiconque détient le jeton.
  Mettre un fichier au magasin est une opération **locale** ;
- **`Stuffing` porte deux états entre deux écritures** — `at_line_start` et `pending_cr` — et
  chacun a besoin de son contrôle négatif. Vérifier qu'un point *est* doublé ne prouve rien :
  il faut aussi vérifier qu'un point de milieu de ligne ne l'est **pas** ;
- **une mesure qui écrit crée son propre store jetable.** `measure-attachment` prenait un
  `--store`, elle a été visée sur les données de production, et elle y a laissé 533 Mo. Un
  paramètre qui peut désigner le store réel le désignera.

Deux pièges trouvés en écrivant le carnet d'adresses (`mailcore::contacts`), et ils resserviront :

- **`lower()` de SQLite ne replie que l'ASCII.** `instr(lower(name), 'éloïse')` ne trouve jamais
  « Éloïse ». Sur un corpus français, c'est la plupart des noms. Toute recherche insensible à la
  casse se fait donc sur une colonne repliée **par Rust**, dont `to_lowercase` est Unicode ;
- **un classement documenté doit avoir un test de propriété.** Le plafond des réceptions était
  au-dessus du poids des envois, ce qui contredisait la propriété que le module affirmait. Ce
  n'était pas un bogue de calcul — c'était un écart entre l'intention écrite et le code, et
  seul un test qui vérifie la propriété l'attrape.

**L'éditeur de signature marche depuis le 2026-09-10**, et il laisse trois leçons :

- **le HTML n'est pas un format de rangement.** `mailhtml::blocks` jette les blocs entièrement
  blancs — la bonne règle pour *afficher* du courrier — donc une ligne blanche ne survit pas à
  un aller-retour HTML. Or une signature commence par « Cordialement, » et une ligne blanche.
  La signature est donc rangée comme **document sérialisé** (`SCHEMA_V8`) ; le HTML est ce qui
  **part**, et il se recalcule à l'envoi ;
- **une signature rejoint un corps à un seul endroit** : `mailsmtp::compose::Draft::sign_with`,
  appelée par `mail send --signature` et par `outbox.send`. Le drapeau dit **qui compose** :
  sans lui, un client qui a déjà mis la signature dans son texte la verrait doublée chez le
  destinataire. C'est la règle de `queue::stage`, appliquée à la composition ;
- **un contrôle négatif qui cherche une sous-chaîne dans un message entier finit par tomber sur
  de l'aléa.** `!message.contains("bcc")` échouait une fois sur cent cinquante : un
  `Message-ID` est de l'hexadécimal, et `b`, `c`, `c` en sont des chiffres. Le contrôle porte
  maintenant sur le **bloc d'en-têtes, ligne par ligne** — et il a son propre contrôle inverse,
  qui vérifie qu'un vrai `Bcc:` serait vu.

**Les invitations reçues marchent depuis le 2026-09-10** (`mailcal`), et elles laissent trois
leçons dont deux ne parlent pas de calendrier :

- **`PRAGMA data_version` ne voit pas les écritures de sa propre connexion.** La coquille sert
  son propre store en mode embarqué : ses écritures lui étaient donc invisibles, et un message
  marqué lu restait affiché « non lu ». `Mailbox::revision` combine maintenant `data_version`
  — qui voit les autres — et `Connection::total_changes` — qui voit soi. Toute nouvelle source
  de changement doit se demander **laquelle des deux la verrait** ;
- **un test peut verrouiller plus que sa raison écrite.** `a_message_without_a_server_copy_…`
  interdisait toute marque locale, avec pour motif « ne pas promettre une poussée au serveur ».
  Le motif ne couvrait que la moitié : le résultat était qu'aucun message importé d'un mbox ne
  devenait jamais lu. Quand un test gêne, **relire sa justification** avant de le contourner —
  et si elle ne couvre pas le cas, c'est le test qui bouge, en écrivant pourquoi ;
- **un fuseau d'invitation se lit dans le fichier, jamais dans une base.** Les `TZID` du corpus
  sont des noms Windows — « Romance Standard Time » — qu'aucune base IANA ne connaît, et
  **toutes** ces pièces embarquent leur `VTIMEZONE`. Appliquer les décalages que le fichier
  porte lui-même est à la fois suffisant et plus fiable : ils viennent du producteur de l'heure.
  Corollaire : `mailcal` n'a aucune dépendance, donc « zéro requête réseau » y est vrai par
  construction.

**Le banc du critère 1 (`cargo xtask measure-replies`) a trouvé trois défauts réels le
2026-09-10**, et les trois étaient invisibles en test unitaire :

- **citer et encoder ne sont pas le même métier.** Un nom d'affichage est un `phrase` dans un
  champ **structuré** — il faut le citer ; un `Subject` est un champ **non structuré** (RFC 5322
  §3.6.5) — le citer y ajoute des guillemets que le destinataire lit. Une seule fonction servait
  les deux, et **toute réponse** partait en `Subject: "Re: …"`. Trois emplois existent en
  réalité : `encoded_word`, `display_name`, et l'échappement d'un paramètre MIME ;
- **un `msg-id` s'écrit entre chevrons, et `mail-parser` les retire.** Les identifiants arrivent
  donc des deux formes selon leur source. La remise en forme est faite **au point d'écriture**,
  jamais chez l'appelant : c'est une propriété de l'en-tête, et une barrière par défaut vaut
  mieux que trois appelants vigilants ;
- **une valeur sans arobase n'est pas une adresse.** Un `From:` mal formé la fait rendre par
  `address()` ; la ranger dans `from_addr` affiche une fausse adresse et fabrique un
  « Répondre » que tout serveur refuse. Elle va dans le nom, aux **deux** portes d'entrée
  (`mailimport::sender`, `mailsync::headers`).

Et la leçon de méthode, qui vaut plus que les trois : **le premier relevé d'un banc neuf accuse
le banc.** Celui-ci a annoncé 0 % de conformité, puis « 0 chaîne à prolonger sur 2 000 messages
réels » — un chiffre impossible, donc un symptôme, et le premier suspect est l'instrument. Un
banc qui réimplémente ce qu'il vérifie ne vérifie rien : la règle de fil a d'abord été sortie de
la coquille (`mailsmtp::compose::reply_threading`) pour que les deux appellent la même fonction.

**La source du message est lisible depuis le 2026-09-11** (`mailcore::source`), et elle laisse
deux leçons :

- **un engagement sans vérification n'est pas une propriété.** « Rien n'est ajouté au message en
  secret » reposait sur un argument — `Draft` n'a pas de champ `X-Mailer`, `SendParams` n'a pas
  de champ `headers` — vrai, mais qui demande de lire `mailsmtp::compose`. Un argument se périme
  au premier en-tête ajouté par commodité, sans que personne ne s'en aperçoive. `outbox.source`
  lit **le blob remis au `DATA`**, jamais une recomposition, et un test compare les noms
  d'en-têtes à une liste close : un en-tête de plus doit être ajouté à la liste, donc
  consciemment ;
- **un afficheur d'octets hostiles fait du parsing d'entrée hostile.** Un en-tête qui porte
  `ESC[2J` efface l'écran où il s'affiche — donc les en-têtes qu'on venait vérifier. Les
  caractères de contrôle sont neutralisés **au point de rendu**, pas chez chacun des trois
  clients, et le contrôle négatif est que le `\r` d'un `\r\n` ne l'est **pas** : sans lui, la
  vue mettrait un `\x0d` au bout de chaque ligne de tout message conforme.

**La page de paramètres existe depuis le 2026-09-11** (`mail-shell::settings`), et elle laisse
trois leçons :

- **le trousseau survit au store.** Le store de production s'est retrouvé vide avec ses cinq
  entrées de Credential Manager intactes : ce sont deux magasins indépendants. Redéclarer un
  compte OAuth2 refaisait alors un consentement complet — navigateur, identifiant client, écran
  du fournisseur — pour aboutir au jeton déjà rangé à côté. `mailauth::session::has_secret`
  demande **avant** de demander à l'utilisateur, et `--renew` est le chemin de celui qui veut
  vraiment remplacer. Elle ne devine pas le mécanisme : un jeton OAuth2 n'est pas un mot de
  passe, et le test le vérifie dans les deux sens ;
- **ce qui ne doit pas traverser une frontière de processus n'a pas de méthode d'API.** C'est la
  règle de `Link::stage` — un chemin de fichier — étendue à un secret, où elle est plus forte :
  une méthode `accounts.add` ferait écrire dans le trousseau de la **machine du démon**, et y
  transporterait le mot de passe. Corollaire dans l'autre sens : `accounts.list` ne rend ni hôte
  ni port, et l'élargir pour faire marcher un écran **local** donnerait l'infrastructure de
  lecture de quelqu'un à tout client distant. La page lit le store directement ;
- **une page de configuration qui laisse son travail à moitié renvoie à l'outil qu'elle
  remplace.** Déclarer un compte n'affichait rien : il fallait un terminal pour `mail sync`. Le
  bouton « Synchroniser » est donc sur chaque compte — et grisé avec sa raison quand il ne
  marcherait pas, ce qui est la règle du critère 8 appliquée à un écran de réglages. Même
  raisonnement pour le consentement OAuth2, ajouté dans la foulée ;
- **la règle 3 vaut aussi pour le fil du service, pas seulement pour l'interface.** Un
  consentement OAuth2 attend un humain jusqu'à cinq minutes. Le servir sur le fil sériel
  n'aurait pas figé le dessin — mais plus une seule page, plus un seul message ne serait arrivé
  pendant ce temps, ce qui revient au même pour qui regarde. Il a son propre fil, détaché, qui
  parle par le canal des réponses et n'a **pas** de `Link` : il écrit dans le trousseau et rien
  d'autre, parce qu'un consentement réussi suivi d'une écriture de store ratée annoncerait un
  compte enregistré qui ne l'est pas.

Et une correction de méthode du même jour : **« intestable » était faux, on n'avait pas
cherché.** Le journal a affirmé qu'aucun banc de la coquille ne tourne sans ouvrir une fenêtre,
donc que le dessin n'est pas couvrable. Vrai des bancs, faux de la bibliothèque :
`egui::__run_test_ui` monte un contexte sans police et exécute le code de dessin. Quatre cents
lignes n'avaient aucune couverture pour cette seule raison. Ce qu'il vérifie utilement : que
chaque état se dessine sans paniquer, et qu'**une page que personne n'a cliquée ne demande
rien** — sans quoi un formulaire écrirait un compte à chaque image. Ce qu'il ne fait pas : il ne
clique pas, donc un bouton grisé n'est pas vérifié comme tel.

**Le carnet suit la moisson depuis le 2026-09-11** (`SCHEMA_V11`, `contacts::update`), et il
laisse trois leçons :

- **un filigrane sur un identifiant suppose que les identifiants ne sont jamais réutilisés.**
  `messages.id` est un `INTEGER PRIMARY KEY` **sans `AUTOINCREMENT`**, donc un alias de `rowid`,
  et SQLite réutilise un `rowid` libéré. Rien ne supprime de ligne `messages` aujourd'hui ; le
  jour où quelque chose le fera, un message inséré sous le filigrane serait sauté pour toujours,
  en silence. D'où un **drapeau par ligne** : « compté » est un fait rangé dans la ligne, pas une
  déduction sur son identifiant ;
- **une boucle de fond qui ne finit jamais est pire qu'un résultat faux.** Le contrôle négatif —
  retirer le marquage — n'a fait tomber aucun test : il les a fait **pendre**, parce que la
  requête rendait indéfiniment les mêmes lignes. Dans le démon, ç'aurait été un job consommant un
  cœur pour toujours, affiché « en cours », donc invisible. La passe est maintenant **bornée par
  ce qui était en attente à son début** ; le même contrôle fait tomber trois tests en 0,16 s ;
- **deux chemins vers la même donnée doivent partager leur code, pas leur intention.** `rebuild`
  et `update` appellent tous deux `advance` ; la seule différence est que le premier vide avant.
  Deux boucles auraient divergé, et un carnet reconstruit qui ne donne pas le même classement
  qu'un carnet tenu à jour ne se verrait qu'à l'usage. Même règle pour le mappeur de lignes
  d'indexation, partagé par `all_for_indexing` et `unindexed`.

**L'index plein texte suit la moisson depuis le même jour** (`SCHEMA_V12`, `index::update`), et
il ajoute deux leçons que le carnet ne pouvait pas donner :

- **quand le drapeau et la donnée vivent dans deux magasins, ils peuvent se contredire.** Le
  drapeau est dans SQLite, les documents dans tantivy. Un index recréé — répertoire supprimé,
  schéma illisible — est vide alors que les drapeaux disent « indexé » : la recherche resterait
  vide **pour toujours**, sans que rien ne le signale. Le symptôme se lit en une requête, et le
  contrôle vit au point d'**usage** (`index::update`), jamais dans `open_or_create` — qui est
  appelée sur des chemins de lecture et ne doit pas écrire ;
- **rendre l'écriture idempotente est ce qui rend l'ordre sûr.** Chaque indexation retire d'abord
  le document de même identifiant (`delete_term`), donc réindexer ne fait rien. C'est ce qui
  permet de valider l'index **puis** de marquer : une coupure entre les deux fait réindexer sans
  conséquence, là où l'ordre inverse rendrait un message introuvable pour toujours.

**Le banc `measure-followup` existe depuis le 2026-09-11**, et il a payé son écriture en une
heure — deux leçons de méthode :

- **« ça ne coûte rien » est un argument tant que ce n'est pas un chiffre.** Une passe d'index à
  vide coûtait **11 à 15 ms**, payées à chaque moisson, parce qu'elle ouvrait l'index tantivy
  avant de demander s'il y avait du travail. La question la moins chère passe en premier : un
  `COUNT` sur colonne indexée, et on tombe à **19–23 µs**. Six cents fois moins, pour une ligne
  — et personne ne l'aurait vu sans le mesurer ;
- **la dispersion dit ce que la médiane cache.** Sur trois exécutions, la première s'écartait de
  50 % des deux autres, qui s'accordaient : la machine n'était pas retombée au repos après le
  `cargo build`, malgré quarante secondes de pause. C'est le biais du 2026-09-02, pour la
  troisième fois. Lire la dispersion **avant** la médiane l'attrape à chaque coup.

**Les fils suivent la moisson depuis le 2026-09-13** (`SCHEMA_V13`, `thread::update`), et c'est
le cas dérivé qui ne rentrait pas dans le moule des deux autres — trois leçons :

- **« incrémental » ne veut pas dire la même chose pour une donnée qui regarde en arrière.** Le
  carnet et l'index avancent sur un drapeau par ligne ; un fil, non, parce qu'il se calcule à
  partir de messages qui arrivent **après** celui qu'on traite. Ce qui marche à la place est de
  recalculer un **voisinage**, et la propriété à établir est qu'il est **fermé** : aucune arête
  du graphe global n'en sort. Les trois fermetures sont les trois façons d'être voisin — une
  référence dans les deux sens, un fil déjà écrit, un groupe de sujet — et la troisième dépend de
  la première, donc elle se réexamine à chaque tour au lieu d'être mémorisée. Ce qui rend la
  passe locale possible n'est pas l'algorithme mais les **faits rangés** : la relecture des blobs
  était le coût (334 s à froid), pas le partitionnement (quelques millisecondes) ;
- **un garde-fou à seuil se franchit dans les deux sens.** Le plafond du repli par sujet refusait
  bien de fusionner le vingt-sixième message, et laissait les vingt-cinq précédents réunis — là
  où une reconstruction les aurait tous séparés. Un refus doit donc savoir **défaire ce qu'il
  avait accepté**, et la question à se poser sur tout seuil est : « qu'est-ce qui a été écrit
  quand j'étais en dessous ? » ;
- **un identifiant réutilisé pour la stabilité se réclame une seule fois.** Garder l'identifiant
  d'un fil est ce qui permet à une interface de rester où elle est quand une réponse arrive. Mais
  un fil qui se **sépare** donne deux composantes dont tous les membres désignent l'ancien
  identifiant : le réutiliser deux fois transforme la séparation en fusion. Même forme que le
  doublon de `In-Reply-To` et `References`, corrigé le même jour — deux sources qui nomment la
  même chose doivent être réduites, au point d'écriture.

## Règles absolues

1. **Ne jamais écrire dans le profil Thunderbird.** Il est en production, l'utilisateur
   s'en sert tous les jours en parallèle. On lit ses fichiers mbox, on n'y touche
   jamais. Pas de rename, pas de troncature, pas de fichier temporaire déposé
   dedans. Ouvrir en lecture seule explicitement.
   Chemin : `C:\Users\<vous>\AppData\Roaming\Thunderbird\Profiles\<profil>`

   Le chemin réel, les cinq comptes du corpus et les noms de serveurs vivent **hors du
   dépôt** depuis sa publication le 2026-09-15 : les journaux de phase les nomment
   `compte-a`, `contact@perso.invalid`, `mail.perso.invalid`. Une mesure nouvelle se
   pseudonymise de la même façon avant d'être écrite dans `docs/`, sinon la publication
   rouvre ce qui a été refermé.

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
- **Coquille native egui (`mail-shell`)** : l'interface livrée, 174–180 ms au démarrage. Le
  corps d'un message est rendu sans moteur de rendu — texte et rectangles. La coquille Tauri
  (`mail-ui`) est gardée : elle est le seul endroit où une CSP de moteur est mise à l'épreuve.
- Le serveur MCP est le premier frontend, avant l'UI.
- Phase 2 : pas d'écriture côté serveur IMAP, sauf le drapeau `\Seen`. Voir `docs/PHASE-2.md`.
- **Éditeur riche = `egui::TextEdit` plus un `layouter`.** Le champ édite une `String`, donc
  curseur, sélection, collage et annulation viennent gratuitement ; le layouter fait le gras et
  l'italique ; `mailhtml::rich::Document::reconcile` rattrape le document sur le tampon à chaque
  image. Aucun modèle de document maison, aucun widget à écrire.
- **Lecture d'invitations dans `mailcal`, sans aucune dépendance.** RFC 5545 : dépliage,
  propriétés, dates, `VTIMEZONE`. Il ne répond pas à une invitation et ne développe pas les
  répétitions — la `RRULE` est affichée telle qu'écrite. Phase 3 : pas de CalDAV.
- **Les brouillons ont leur table (`drafts`), jamais un état de `outbox`.** La file décide ce
  qui part chez quelqu'un ; y ajouter un état pour ranger des brouillons toucherait la règle du
  critère 2 pour une commodité. Et rien n'est validé à l'enregistrement d'un brouillon : les
  adresses sont gardées **telles que tapées**, sinon rouvrir perd la frappe en cours.
- **Un envoi `failed` se renvoie, un envoi `committing` se tranche.** Deux sorties distinctes —
  `Store::retry_outgoing` / `outbox.retry` / `mail outbox --retry` d'un côté,
  `Store::resolve_doubt` / `outbox.decide` / `--resend` de l'autre — parce que les risques sont
  opposés : un serveur qui a refusé n'a rien pris, un serveur qui n'a pas répondu a peut-être
  tout. Chaque fonction refuse l'état de l'autre. Et le renvoi remet `attempts` à zéro, sinon
  une ligne épuisée échouerait au premier refus passager.

**Les refus d'envoi sont provoqués depuis le 2026-09-10**, et trois leçons en sortent :

- **une phrase qui parle de l'avenir se compose après la décision, pas avant.** « L'envoi est
  réessayé automatiquement ; rien à faire » était écrit avant que `settle` ne sache s'il
  restait des tentatives : les six épuisées, la ligne passait à `failed` en gardant la promesse.
  L'utilisateur attendait un message mort. Une fausse promesse est pire qu'un code numérique —
  un code fait chercher, une promesse fait attendre ;
- **dire quoi faire oblige à rendre le geste possible.** « Renvoyez le message plus tard » sur
  une ligne dont la seule autre sortie jetait le message était le critère 8 à moitié. Chaque
  fois qu'une phrase nomme une action, vérifier qu'un chemin la réalise ;
- **ce qu'un client doit décider se dérive une fois, du bon côté, et se range à côté de la
  phrase.** `outbox.resendable` est un booléen et non la famille du refus : la famille se lit
  sur un code et une étape SMTP, et la ranger dans le store forcerait `mailcore` à connaître
  SMTP — ou chaque client à deviner en cherchant des mots dans une phrase française. Même
  raisonnement que `Outgoing::doubtful`.

## Vérification

Avant de dire qu'une étape est finie : `cargo fmt --check`, `cargo clippy -- -D warnings`,
`cargo test`, et les critères d'acceptation chiffrés de la phase en cours réellement
mesurés sur le corpus réel — pas estimés.

**Ne jamais mesurer un démarrage ou une latence sur une machine qui vient de compiler.**
Compiler, laisser la machine retomber au repos, puis mesurer avec `--no-build`. Ce biais a
faussé les relevés du 2026-09-02 d'un facteur trois, et il s'est reproduit sur le critère 5
le lendemain. Et **lire la dispersion avant la médiane** : deux exécutions identiques qui
s'écartent de 40 % ne sont pas du bruit, c'est un symptôme.

**Une pause ne suffit pas, un relevé jetable si.** Quarante secondes d'attente n'ont pas suffi
le 2026-09-11, quatre minutes pas davantage le 2026-09-13 — première exécution à 45,4 µs, les
deux suivantes à 22,7 et 20,8. Quatrième récidive. La parade qui marche n'est pas une pause plus
longue mais **trois exécutions dont on jette la première**, et de préférence sans rien faire
tourner d'autre entre-temps : lancer des tests pendant qu'une machine « retombe au repos »
annule l'attente.
