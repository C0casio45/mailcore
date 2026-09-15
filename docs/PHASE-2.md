# Phase 2 — la synchronisation réseau

## Objectif

Le store cesse d'être un instantané. Les dix comptes IMAP se synchronisent dans
`mailcore`, en **lecture d'abord**, et le pari de la phase 1 — un message est un contenu
immuable, un dossier n'est qu'une référence — se met à l'épreuve sur ce qui l'a motivé :
la duplication Gmail.

**Aucun envoi.** Recevoir et envoyer sont deux mécanismes distincts, et mélanger les deux
dans la même phase, c'est se retrouver à déboguer un SMTP pendant qu'un IMAP ment.

## Pourquoi la synchronisation avant l'envoi et avant l'IA

Trois raisons, dans cet ordre.

**Le corpus est daté du 2026-08-28.** Chaque jour qui passe, l'application est un peu moins
utile qu'un Thunderbird ouvert à côté. Ce n'est pas une gêne théorique : c'est la seule
raison pour laquelle l'utilisateur ne peut pas encore s'en servir.

**C'est le risque restant.** `docs/PHASE-1.md` a repoussé « le marécage IMAP/OAuth » en
phase 2, et le mot était juste. Dix comptes, quatre fournisseurs, OAuth2 chez Google, des
serveurs qui mentent sur leurs capacités, des `UIDVALIDITY` qui changent sans prévenir. Tout
le reste de la phase 2 est prévisible ; ça ne l'est pas. La méthode de la phase 1 était
d'attaquer l'inconnu tôt et de le mesurer — l'étape 6 avait été placée avant l'UI pour cette
raison.

**Envoyer sans recevoir n'a pas de sens.** Un client qui poste un message dont la réponse
n'arrive jamais n'est pas à moitié fini, il est inutilisable. Et l'IA de `docs/VISION.md` —
recherche sémantique, tri à l'arrivée, résumé de fil — a besoin d'une **arrivée** pour avoir
un sens. « Tri automatique à l'arrivée » suppose qu'il arrive quelque chose.

## Périmètre

Dedans :

- `mailsync` — la bibliothèque de synchronisation IMAP. Découverte des dossiers, moisson
  incrémentale, application des drapeaux, reprise après coupure.
- `mailauth` — les identifiants : mot de passe applicatif, OAuth2 `XOAUTH2` pour Google,
  rafraîchissement des jetons. **Rien en clair sur le disque**, jamais.
- Un **compte** dans le modèle de données : `mailcore` connaît aujourd'hui des dossiers et
  des références, pas des comptes. C'est un changement de schéma, pas un ajout d'écran.
- Les tâches de fond du démon étendues à la sync : progression par compte et par dossier,
  annulation, reprise. Le mécanisme de `jobs.*` existe déjà et a été mesuré à l'import.
- `mailfake` — un **serveur IMAP de test, en Rust**, capable de répondre faux. Voir plus bas.
- L'affichage du réseau dans la coquille : par compte, l'état, la dernière sync, l'erreur
  s'il y en a une. Rien de modal, rien qui bloque.

Dehors :

- **l'envoi, la réponse, le brouillon** — phase 3 ;
- **JMAP** — l'architecture le prévoit, aucun des dix comptes ne le parle ;
- **l'IA** — phase 3, et elle a besoin de cette phase pour avoir une entrée ;
- **la suppression et le déplacement côté serveur.** Lire, oui ; écrire dans la boîte d'un
  fournisseur, non. Un bug de sync qui supprime du courrier chez Gmail n'est pas
  rattrapable, et cette phase est précisément celle où les bugs de sync se trouvent. Seule
  exception discutée plus bas : le drapeau `\Seen`.

## Ce que la phase 1 a déjà payé, et qu'il faut vérifier

Le diagnostic de `docs/VISION.md` mesurait, sur **un seul** compte Gmail, 3,3 Go stockés pour
1,4 Go de contenu réel : les mêmes messages dans `INBOX`, `[Gmail]/Tous les messages`,
`Messages envoyés` et `Important`.

L'adressage par contenu rend cette duplication **impossible par construction**. Ce n'est pas
une optimisation à écrire en phase 2, c'est une conséquence de la phase 1 : quatre dossiers
qui voient le même message, c'est un blob et quatre références.

**Donc ça se mesure, et c'est le premier critère de la phase.** Si la sync des quatre
comptes Gmail fait grossir le store de plus que le volume de contenu distinct reçu, le pari
de la phase 1 est faux quelque part, et il vaut mieux le savoir sur le premier compte que sur
le dixième.

## Les pièges connus, avant d'écrire une ligne

**`UIDVALIDITY` peut changer.** Quand il change, tous les UID d'un dossier sont invalides et
il faut resynchroniser. Un client naïf redescend alors le dossier entier et le stocke deux
fois. Ici, l'adressage par contenu absorbe la resynchronisation sans dupliquer un octet — mais
il faut que le code **détecte** le changement et refasse le lien, sinon les références
pointent dans le vide.

**Les capacités annoncées ne sont pas les capacités réelles.** `CONDSTORE` et `QRESYNC`
rendent la sync incrémentale triviale quand ils marchent. Il faut un chemin de repli par
plages d'UID, et il faut que ce chemin soit **exercé en test**, pas seulement écrit — un
repli jamais exécuté est du code mort qui s'exécutera un jour de panne.

**Gmail n'est pas un IMAP ordinaire.** Les labels sont exposés en dossiers, `Tous les
messages` contient tout, et `X-GM-EXT-1` donne un identifiant de message et de fil propres à
Google. Utiliser `X-GM-MSGID` serait tentant pour dédupliquer entre dossiers ; ce serait une
erreur, parce que ça ne marche que chez Gmail et qu'on a déjà un identifiant qui marche
partout : le contenu.

**OAuth2 demande un navigateur, une fois.** Le flux `installed app` ouvre une page, reçoit un
code sur un `127.0.0.1` éphémère, l'échange contre un jeton de rafraîchissement. C'est la
seule fois où l'application ouvre un navigateur, et c'est une action explicite de
l'utilisateur — ça ne contredit pas `docs/PRIVACY.md`, qui interdit une requête déclenchée
par **le contenu d'un mail**.

**Un dossier de 1,4 Go ne se moissonne pas d'un bloc.** La règle 4 du `CLAUDE.md` s'applique
au réseau exactement comme au mbox : on récupère par lots, on écrit au fur et à mesure, et
une coupure au milieu laisse un store cohérent.

## Ce que la vie privée exige en plus, cette fois

La phase 1 avait une règle simple : rien ne sort. La phase 2 fait sortir des paquets, et la
règle doit devenir précise au lieu de disparaître.

**Vers qui.** L'application se connecte aux serveurs que l'utilisateur a configurés, et à
rien d'autre. Pas de découverte automatique par un service tiers, pas de télémétrie, pas de
vérification de mise à jour. La formulation testable : *aucune connexion sortante vers un
hôte qui ne figure pas dans la configuration des comptes* — et c'est un critère, mesuré par
un harnais du même genre que `mailprivacy`.

**Les identifiants.** Trousseau du système, comme le jeton du démon l'est déjà. Le store ne
contient pas de mot de passe, les journaux n'en contiennent pas, un plantage n'en écrit pas.
Testable : on cherche le secret dans le store, dans les journaux et dans les fichiers
temporaires, et on doit ne rien trouver.

**Le drapeau `\Seen` est une décision, pas un détail.** Le propager, c'est dire au
fournisseur quand un message a été lu — un accusé de réception, exactement ce que §4 de
`docs/PRIVACY.md` refuse d'envoyer à un expéditeur. Ne pas le propager, c'est que Thunderbird
et mailcore se contredisent en permanence, alors que les deux tournent en parallèle pendant
toute cette montée en puissance.

**Décision : on le propage, et on le dit.** Le fournisseur héberge déjà le message et voit
déjà les connexions IMAP ; l'information qu'on lui donne en plus est marginale, alors que la
divergence entre deux clients est un vrai coût quotidien. C'est une préférence, désactivable,
mais son défaut est « propager ». Ce qui reste interdit sans changement : un accusé vers
l'**expéditeur**.

## Critères d'acceptation — à mesurer, pas à estimer

Sur les dix comptes réels, pas sur un compte de test.

| # | Critère | Seuil |
|---|---|---|
| 1 | Croissance du store à la sync des 4 comptes Gmail | **≤ le volume de contenu distinct reçu**, et le taux de dédup inter-comptes affiché — **passé** le 2026-09-08 sur cinq comptes réels dont quatre Gmail : 105 508 références pour 48 529 contenus distincts, soit **54 % de doublons**. Par compte à la première moisson : 65 %, 58,7 %, 57,6 %, 26,5 % ; les taux montent avec l'ordre de passage, ce qui *est* la dédup inter-comptes |
| 2 | RSS du démon pendant la sync des 10 comptes | **< 500 Mo** — **passé** le 2026-09-08 sur cinq comptes réels, 80 dossiers, 48 529 messages en base : **27 Mo** de crête pendant une synchronisation incrémentale de 59 s, contre 17 Mo au repos. Et **144 Mo** pendant une réindexation complète, qui est le job le plus gourmand du démon. ~~Ce qui **n'est pas** mesuré : la crête d'une moisson complète sur un vrai compte~~ — **mesurée le 2026-09-09** dans un store jetable, ce qui contourne le correctif de reprise sans rien casser : **169,6 Mio** sur un compte **entier** — 2 842 contenus, 1,5 Gio de RFC 5322 — et **96,7 Mio** sur un autre de 3 477 contenus, contre 10 Mio au repos. L'écart entre les deux est l'information : la borne réelle est `max(32 Mio, le plus gros message)`, et ce compte-là en a un de 48 Mio. Ce n'est pas le nombre de messages qui décide. **Et lever la réserve a montré que sa propre justification était fausse** : « le lot de cent messages borne la mémoire par construction » bornait le *nombre*, pas les octets. Le premier relevé donnait **266,5 Mio** ; le lot est maintenant borné aux deux, et le pire cas théorique passe de 12,8 Gio à 32 Mio. Voir le journal |
| 3 | Sync incrémentale sans rien de nouveau | **< 5 s** pour les 10 comptes, et **0 octet** écrit — **passé** le 2026-09-08 sur les cinq comptes réels, médiane de cinq passes : **0,31 s / 0,41 s / 0,87 s / 1,22 s / 1,33 s**, et `reflagged: 0` partout. Les cinq comptes ensemble tiennent en **5,1 s**, quand un seul en coûtait 9,4 s le matin même. `LIST-STATUS` (RFC 5819) rend l'état des 80 dossiers en une commande, et 80 dossiers sur 80 sont reconnus à jour sans `EXAMINE`. Mesuré avant correctifs : jusqu'à **67 s** et 11 900 lignes réécrites pour rien |
| 4 | Travail par image dans la coquille **pendant** une sync complète | **< 16,7 ms** en p95 — **passé** le 2026-09-03 sur `mailfake` : **0,99 ms** de p95, 2,27 ms au pire, en défilant 9 400 lignes pendant qu'une moisson de 60 000 messages écrivait dans le même store depuis un autre processus. **58 changements du store reçus pendant les 600 images**, ce qui est le contrôle du recouvrement. **Refait sur les vrais serveurs** le 2026-09-09 : **2,50 ms** de p95, 5,10 ms au pire, pendant que **les cinq comptes réels moissonnent en parallèle** — vraies connexions TLS, OAuth2, corps réels — dans le store que la coquille lit. 3 à 5 changements reçus pendant les 600 images. Le chiffre est trois fois moins bon que celui de `mailfake` et reste six fois sous le budget. **Réserve** : la fenêtre de défilement du banc dure ~3,3 s, donc le recouvrement se compte en unités et non en dizaines ; c'est ce banc qui a trouvé deux défauts réels, voir le journal |
| 5 | Connexions sortantes vers un hôte non configuré | ~~**exactement 0**~~ — **le seuil était faux, et c'est mesuré.** La vérification de révocation du système ouvre une connexion vers l'autorité de certification à chaque poignée de main TLS. Le critère devient : **exactement 0 par notre code**, vérifié le 2026-09-04, et la connexion du système est nommée plutôt que niée. Voir la correction plus bas |
| 6 | Identifiants en clair dans le store, les journaux, `%TEMP%` | **exactement 0** — **passé** le 2026-09-03 sur `mailfake` : un mot de passe sentinelle passe par le vrai `LOGIN`, la découverte et la moisson de trois messages, puis est cherché octet par octet dans tout le store, dans les journaux capturés au niveau `TRACE`, et dans le répertoire temporaire du système. Deux contrôles positifs. **Refait sur les vrais comptes** le 2026-09-08 : **17 secrets réels** — quatre jetons de rafraîchissement, quatre secrets client, cinq jetons d'accès dont quatre rafraîchis pendant la passe, un mot de passe applicatif — cherchés octet par octet dans les 3,5 Gio du store réel, dans les journaux `TRACE` d'une vraie synchronisation des cinq comptes, et dans `%TEMP%`. **Zéro.** Le contrôle positif porte cette fois sur **les octets réels** de chaque secret, retrouvés dans un tampon en mémoire — aucun secret n'est jamais écrit sur le disque par le harnais |
| 7 | Écritures dans le profil Thunderbird | **exactement 0** — **passé** le 2026-09-08 : relevé de 502 fichiers avant et après une synchronisation des cinq comptes et une ouverture de la coquille, **0 mbox modifié, 0 autre fichier modifié**. Avec son contrôle positif : le comparateur, nourri d'un relevé où un seul octet de taille a été changé, signale bien le mbox et sort en échec. **Réserve** : Thunderbird ne tournait pas pendant la fenêtre de mesure, donc le « il tourne toujours à côté » du critère n'est pas éprouvé — seul notre côté l'est |
| 8 | Coupure réseau au milieu d'une sync | **reprise sans perte et sans doublon**, mesurée en coupant vraiment — **subie** le 2026-09-08 : Gmail a coupé deux dossiers de `compte-b` sans `close_notify`. Sans perte ni doublon, mais la reprise a d'abord coûté **25 028 corps redemandés pour 2 messages nouveaux**. Corrigé — l'effacement est réservé au `UIDVALIDITY` changé, et aucun corps déjà possédé n'est redemandé — et verrouillé par un test à coupure réelle |
| 9 | `UIDVALIDITY` qui change sur un dossier de 1 Go | **détecté**, resynchronisé, **0 blob dupliqué** — **passé** le 2026-09-08 sur `mailfake`, à l'échelle : 2 000 messages, 7,63 Mio de RFC 5322, resservis sous une numérotation disjointe. **2 000 corps retéléchargés, 0 contenu nouveau, 2 000 doublons reconnus, 2 000 copies refaites**, aucun ancien UID survivant, et le répertoire des blobs inchangé — 2 000 fichiers, 676 836 octets, avant comme après. Avec son contrôle inverse : un corps réellement modifié pendant la renumérotation, lui, est écrit. Reste l'échelle du gigaoctet, qui ne se reproduit pas dans une suite de tests : ce qui est mesuré est le **rapport** entre ce qui redescend et ce qui se réécrit |
| 10 | Chemin de repli sans `CONDSTORE` | **exercé en test**, et il rend le même résultat que le chemin rapide — **passé** : exercé en test le 2026-09-03, puis **en vrai** le 2026-09-04, le premier compte réel n'annonçant pas `CONDSTORE`. C'est ce chemin-là qui a révélé deux bugs de correction, dont un que le test manquant — deuxième passage **sans** `CONDSTORE` — couvre maintenant. **Correction du 2026-09-08 au soir** : ce compte annonçait `CONDSTORE` depuis toujours, on le lui demandait avant le `LOGIN`. Le repli a donc bien tourné en vrai, mais pour une mauvaise raison ; il n'est plus exercé que par les tests, et c'est à eux qu'il faut le demander |

Le critère 4 est celui qui ferme un trou reconnu de la phase 1 : le critère 2 n'avait été
mesuré que sur une liste au repos, jamais pendant qu'une tâche de fond écrivait. Une sync est
le cas réel, et c'est la règle 3 du `CLAUDE.md` mise à l'épreuve pour de bon.

Le critère 10 est là parce qu'un repli qu'on n'exécute jamais n'existe pas. C'est ce que
`mailfake` sert à produire.

## `mailfake` — pourquoi un serveur IMAP écrit ici

Un Dovecot en conteneur donne un IMAP correct. C'est utile pour l'interopérabilité, et ça ne
suffit pas : la règle du `CLAUDE.md` sur le parsing d'entrée hostile demande des tests **sur
des cas malformés**, et un serveur correct ne sait pas répondre de travers.

Ce qu'il faut pouvoir provoquer à volonté :

- une réponse tronquée au milieu d'un littéral ;
- un `UIDVALIDITY` qui change entre deux `SELECT` ;
- des capacités annoncées puis refusées à l'usage ;
- un `FETCH` qui rend un corps dont la longueur ne correspond pas au littéral annoncé ;
- une connexion coupée à un octet de la fin ;
- un dossier dont le nom est de l'UTF-7 modifié invalide.

Aucune de ces situations n'est théorique — ce sont les pannes qu'on trouve chez un
fournisseur, un jour, sans reproduction possible. En Rust, dans le dépôt, elles deviennent
des tests. C'est aussi la raison qui décide de la question de la règle 2 : ce n'est pas un
script d'outillage qu'on pourrait écrire dans n'importe quel langage, c'est une partie du
harnais de test, et elle a les mêmes exigences que le reste.

Dovecot reste utilisé, en dernier ressort, pour vérifier qu'on parle bien à un vrai serveur.
C'est le seul endroit de la phase où un conteneur est nécessaire.

## Ordre de travail

1. **Le schéma des comptes**, dans `mailcore`. Un compte, ses dossiers distants, la liaison
   `(compte, dossier, UIDVALIDITY, UID) → référence`. Migration testée sur une copie du
   store réel de 4,3 Go, jamais sur l'original.
2. **`mailfake`** avant `mailsync`. Écrire le client contre un serveur qu'on maîtrise, y
   compris quand il ment. Sans ça, chaque bug est ambigu : le client ou le serveur ?
3. **`mailsync` en lecture, un dossier, un compte, mot de passe applicatif.** Le compte le
   plus simple des dix, pas Gmail. Objectif : un dossier qui arrive dans le store et qui
   s'affiche dans la coquille.
4. **La moisson incrémentale**, les deux chemins : `CONDSTORE` quand il existe, plages d'UID
   sinon. Critère 10 ici.
5. **`mailauth` et OAuth2**, puis le premier compte Gmail. Critère 1 ici : c'est le moment
   où le pari de la phase 1 se vérifie ou s'effondre.
6. **Les dix comptes.** Critères 1, 2, 3, 7. Une sync complète chronométrée, RSS relevé.
7. **Les pannes** : coupure au milieu, `UIDVALIDITY` changé, serveur qui refuse. Critères 8
   et 9.
8. **Le harnais réseau** : critères 5 et 6. Du même genre que `mailprivacy` — un observateur
   qui voit les connexions, et des contrôles positifs pour que les zéros veuillent dire
   quelque chose. La leçon de la phase 1 est explicite là-dessus : un zéro sans contrôle ne
   prouve rien.
9. **`IDLE`**, pour que le courrier arrive sans qu'on demande. En dernier : c'est du confort,
   et une sync périodique correcte le remplace.
10. **Le critère 4**, mesuré une fois que tout marche : la coquille qui défile pendant que
    dix comptes se synchronisent.

L'étape 5 porte le vrai risque. Un IMAP avec mot de passe se débogue en une soirée ; un
OAuth2 Google avec rafraîchissement de jeton, un consentement à renouveler et des messages
d'erreur qui ne disent pas ce qui manque, ça se débogue en plusieurs. Mieux vaut le découvrir
à l'étape 5, sur un compte, qu'à l'étape 6 sur quatre.

## Journal

### Étape 1 : le schéma, et ce que le cas des doublons a imposé — 2026-09-03

Schéma v2. La migration tient en `ALTER TABLE ADD COLUMN` sur `accounts` et `folders`, plus une
table neuve — et **c'est cette table qui est la décision de l'étape**.

#### `remote_uids`, et pourquoi ce n'est pas une colonne de `refs`

La solution évidente était d'ajouter `uid` à `refs`, dont la clé est `(message_id, folder_id)`.
Elle est fausse, et le cas n'est pas exotique : **deux UID d'un même dossier peuvent porter un
contenu identique.** Une double remise, un `APPEND` répété, un message copié sur lui-même — et
le serveur a deux copies, deux jeux de drapeaux, deux existences.

Avec un `uid` dans `refs`, la deuxième ne rentre pas : la clé est déjà prise. On en perdrait
une, et **la suppression de l'autre retirerait la référence** alors que le serveur en a encore
une. La liste afficherait un message de moins que la boîte, sans rien dans les journaux.

Deux tables, deux rôles : `refs` pagine l'affichage, `remote_uids` porte les copies du serveur.
Une référence existe tant qu'au moins un UID lui répond. `Writer::forget_copy` rend ce
booléen-là plutôt que de le laisser deviner à l'appelant, ce qui est ce qui empêche de
réintroduire le bug plus tard.

#### Les drapeaux ont deux règles, pas une

Quand deux copies ne s'accordent pas, il faut choisir. La réponse n'est pas la même pour tous
les drapeaux :

| Drapeaux | Règle | Pourquoi |
|---|---|---|
| `\Seen`, `\Flagged`, `\Answered` | **une copie suffit** | le contenu *a été* lu, marqué, répondu |
| `\Draft`, `\Deleted` | **il les faut toutes** | `\Deleted` veut dire « marqué pour la purge », pas « parti » : un OU ferait disparaître de la liste un message que le serveur a toujours |

La réduction existe **deux fois** — en Rust (`MessageFlags::reduce`) pour la logique de sync,
en SQL (`Writer::refresh_ref_flags`) pour recalculer un dossier entier en une instruction. Deux
implémentations du même contrat dérivent toujours, donc un test croise les **1 024
combinaisons** de drapeaux et exige qu'elles disent la même chose.

#### Le critère 6 est devenu une propriété du schéma

« Zéro identifiant en clair dans le store » n'est pas une discipline à tenir : `accounts` n'a
**aucune colonne où en ranger un**. Hôte, port, identifiant, mécanisme — pas de secret. Un test
fige la liste des colonnes, donc ajouter `password` ou `token` fait échouer la suite.

Trois autres refus plutôt que des replis silencieux, tous pour la même raison — un repli
choisit à la place de l'utilisateur quelque chose qui touche à un secret ou à du courrier :

- `Security` n'a **pas de variante en clair**. Un IMAP non chiffré transporte le mot de passe
  sur le réseau ; il n'y a pas de configuration où on l'accepterait, donc pas de valeur pour le
  dire ;
- un mécanisme d'authentification inconnu **refuse** le compte. Se tromper de mécanisme, c'est
  envoyer un secret dans un champ qui ne l'attend pas ;
- un compte `imap` sans hôte **refuse** au lieu de rendre un `Server` vide. Un hôte vide
  finirait dans un résolveur, et « échec de connexion » est un diagnostic bien moins utile que
  « ce compte est incohérent en base ».

Un `MODSEQ` au-delà des 63 bits de la RFC 7162 est refusé aussi, et là c'est un piège de
correction : le tronquer ferait **rater des changements pour toujours**, la moisson croyant
avoir déjà vu ce qui arrive après.

#### Ce que « jamais synchronisé » veut dire

`uidvalidity`, `uidnext`, `highest_modseq`, `synced_at` sont nullables. `NULL` est « jamais
synchronisé », et c'est différent de « le serveur a répondu zéro » : un `NOT NULL DEFAULT 0`
rendrait les deux indistinguables, et une resynchronisation complète se déclencherait — ou
pas — sur un zéro qui ne veut rien dire. `Writer::set_sync_state` écrit **tous** les champs,
`None` compris, parce qu'un `UPDATE` partiel laisserait un `uidnext` périmé à côté d'un
`uidvalidity` neuf et la moisson suivante croirait n'avoir rien à faire.

#### La migration, mesurée sur l'index réel

`cargo xtask schema-check` copie l'index, migre la copie, et compare. L'original n'est jamais
ouvert en écriture.

| Mesure | Relevé |
|---|---|
| Index | 41,2 Mo, **102 894 références**, 73 825 messages, 96 dossiers, 11 comptes |
| Durée de la migration | **2,4 ms** |
| Lignes perdues | **0** sur les cinq tables témoins |
| Références orphelines | **0** (`PRAGMA foreign_key_check`) |
| Intégrité SQLite | `ok` |

`ALTER TABLE ADD COLUMN` est bien en temps constant : 2,4 ms sur 41 Mo. Ce n'est plus « censé
l'être », c'est mesuré — et la commande reste, donc la v3 se vérifiera pareil.

**Ce que l'étape 1 ne fait pas** : rien ne remplit encore ces colonnes. Aucun compte IMAP
n'existe, aucune copie n'est enregistrée. C'est l'étape 3 ; l'étape 2 est le serveur de test,
et elle vient d'abord pour que chaque bug de l'étape 3 ait une cause et une seule.

### Étape 2 : `mailfake`, un serveur IMAP qui sait répondre faux — 2026-09-03

`crates/mailfake`, 51 tests. **Aucune dépendance IMAP**, et c'est le point : une bibliothèque
IMAP refuserait d'émettre ce qu'on veut émettre. Un littéral dont la longueur annoncée ne
correspond pas aux octets qui suivent est exactement ce qu'une bibliothèque correcte empêche
d'écrire.

Pas d'async non plus : `std::net`, un fil par connexion. Un serveur de test qui demande un
exécuteur pour démarrer est un serveur de test qu'on n'utilise pas.

#### Les huit pannes, et ce que chacune attrape

| Panne | Ce qu'elle reproduit | Le bug de client qu'elle révèle |
|---|---|---|
| `TruncatedLiteral` | `{n}` annoncé, moins d'octets envoyés, puis raccroché | un client qui lit « jusqu'à la parenthèse » au lieu de compter les octets **attend pour toujours** |
| `LiteralLengthMismatch` | longueur annoncée fausse, dans les deux sens | trop grand : blocage ; trop petit : la fin du corps est prise pour de la syntaxe IMAP |
| `UidvalidityChangesOnSelect` | une restauration de sauvegarde | tous les UID connus deviennent faux, et un client qui ne le voit pas croit avoir déjà tout |
| `AdvertisesCondstoreThenRefuses` | l'annonce est vraie, la promesse ne l'est pas | **le chemin de repli**, celui qui ne s'exécuterait qu'un jour de panne |
| `ClosesAfter` | le câble débranché, au milieu d'un littéral | une reprise qui laisse le store incohérent |
| `RefusesLogin` | un mot de passe applicatif révoqué | un client qui continue comme si de rien n'était |
| `ImpossibleSequenceNumber` | un numéro de séquence hors de la boîte | un client qui indexe par numéro de séquence écrit le message **au mauvais endroit** |
| `DuplicateUid` | un serveur sous charge qui répète un UID | un client qui n'est pas idempotent |

`ClosesAfter` est un **budget d'octets écrits**, pas un compteur de réponses. C'est ce qui met
la coupure au milieu d'un littéral, d'un nom de boîte, d'une accolade ouvrante — et pas
seulement aux frontières propres, c'est-à-dire là où un client s'en sort trop facilement.

#### Ce que les tests d'intégration ont d'obligatoire

Les 24 premiers tests portent sur des fonctions pures — découper une commande, développer un
ensemble d'UID. Ils ne disaient **rien du serveur, qui n'avait jamais reçu un octet**. Pour un
harnais, c'est la seule chose qui compte : *une panne qui ne se déclenche pas est pire que pas
de panne*, parce qu'elle ferait passer le client de l'étape 3 pour correct alors que rien ne
l'a éprouvé.

27 tests parlent donc au serveur sur une vraie connexion TCP, avec un client de test qui **lit
les littéraux en comptant les octets**, jamais par délimiteur. Sans ça, il ne verrait pas la
différence entre un littéral correct et un littéral tronqué — donc il ne pourrait pas prouver
que la panne fonctionne.

Chaque panne a son test, et deux ont leur **contrôle négatif** :
`uidvalidity_is_stable_without_the_fault` et `a_server_without_condstore_omits_highest_modseq`.
La leçon du critère 8 s'applique ici mot pour mot : sans contrôle, un serveur qui changerait
*toujours* d'`UIDVALIDITY` passerait pour correct.

Un test vérifie aussi que **deux clients sont servis en même temps** — le défaut qui avait
bloqué le harnais du critère 8 pendant une exécution entière.

#### Deux détails qui auraient mordu plus tard

**`1:*` ne se développe pas jusqu'à `u32::MAX`.** `*` veut dire « le plus grand UID de la
boîte ». Le développer littéralement produirait quatre milliards d'itérations dans un serveur
de test : le genre de détail qui fait passer un test de trois millisecondes à jamais.

**Les noms de boîte voyagent en octets, de bout en bout.** `Mailbox::name` est un `Vec<u8>`, et
la réponse `LIST` est assemblée en octets. Le faire passer par une `String` remplacerait les
octets invalides par des caractères de remplacement — ce qui désamorcerait précisément le cas
qu'on veut servir, celui du nom qui n'est pas de l'UTF-7 modifié valide. Un test le vérifie
octet pour octet.

**Ce que le serveur ne parle pas** : ni `APPEND`, ni `STORE`, ni `EXPUNGE`, ni TLS. La phase 2
n'écrit pas côté serveur, et un serveur de test qui accepte des commandes que le client
n'émet jamais est du code non couvert qui prétend l'être. Le chiffrement se vérifie sur un
vrai serveur, à l'étape 3 — monter une autorité de certification jetable n'éprouverait que
`rustls`.

L'analyseur de commandes ne gère pas les **littéraux en entrée** (`{12}` suivi d'octets), pour
la même raison : aucune commande d'une synchronisation en lecture n'en contient. Le jour où
`APPEND` arrive, il faudra l'écrire, et il aura ses tests.

### Étape 3 : le client IMAP et la moisson — 2026-09-03

`crates/mailsync`, 88 tests. Le corpus arrive dans le store et s'affiche : c'est l'objectif de
l'étape. Ce qui manque est la validation sur un **vrai** serveur, qui demande des identifiants.

#### Deux bugs que les tests ont attrapés, dont un qui cassait tout

**`find(" UID ")` ne trouve jamais `(UID 7`.** Le premier élément d'une réponse `FETCH` est
collé à la parenthèse ouvrante. Cette version du client n'analysait donc **aucune** réponse
réelle — pas un cas limite, toutes. Corrigé par une recherche à frontière de mot, qui refuse
aussi `X-UID` et `UIDPLUS` là où on cherche `UID`.

**Le nom de boîte était lu depuis la fin.** Chercher le dernier guillemet et remonter jusqu'au
précédent casse sur `"un\"nom"` : le guillemet trouvé est celui du déguisement, et le nom
ressort tronqué. Corrigé par une analyse vers l'avant, qui suit la forme
`* LIST (attributs) séparateur nom` au lieu de la deviner.

Les deux sont dans le module qui lit de l'entrée hostile, et les deux ont été trouvés par des
tests sur des cas malformés — pas par un test sur un cas propre.

#### Trois défenses, et ce qu'elles empêchent

| Défense | Sans elle |
|---|---|
| Plafond de littéral **vérifié avant l'allocation** | `{4294967295}` fait réserver quatre gigaoctets avant qu'on s'aperçoive que rien ne suit |
| Plafond de ligne à 64 Kio | un serveur qui n'envoie jamais de fin de ligne remplit la mémoire disponible |
| Un `FETCH` sans UID est une **erreur** | il n'y a aucun endroit sûr où ranger le message : le numéro de séquence change dès qu'un message est purgé |

La longueur annoncée est lue en `u64` et comparée au plafond **avant** conversion. Convertir
d'abord perdrait l'information sur une plateforme 32 bits, où `{5000000000}` deviendrait un
petit nombre plausible.

#### La lecture est streamante, et ce n'était pas le premier jet

`command()` accumulait toutes les réponses dans un `Vec` avant de rendre. Sur un
`UID FETCH 1:* (BODY[])` d'un dossier de 100 000 messages, c'est 4 Go en mémoire — la règle 4
du `CLAUDE.md` violée, et le plafond par message n'y change rien : il protège d'un seul
message démesuré, pas de leur somme.

`command_each` et `uid_fetch_each` passent chaque réponse à une fermeture. Un corps est en
mémoire à la fois, comme le lecteur de mbox de la phase 1.

#### Ce que `plan` sépare, et pourquoi

`sync::plan` est une **fonction pure** : état local, état distant, et la réponse à « que
faut-il demander ? ». Aucun réseau, aucun store. Les cas qui décident de la correction d'une
synchronisation s'y testent exhaustivement.

| Situation | Plan |
|---|---|
| jamais synchronisé | complet |
| `UIDVALIDITY` changé | complet, **après** avoir oublié les copies |
| le serveur n'annonce pas d'`UIDVALIDITY` | complet, à chaque fois — il ne promet rien |
| `CONDSTORE` actif, rien de neuf | rien à faire |
| `CONDSTORE` actif, du changement | les drapeaux depuis `MODSEQ`, les corps au-delà d'`UIDNEXT` |
| pas de `CONDSTORE` | **tous** les drapeaux, les corps au-delà d'`UIDNEXT` |

**Il n'y a pas de « rien à faire » sans `CONDSTORE`**, et c'est nommé plutôt que caché : un
drapeau modifié ne bouge ni `UIDNEXT` ni le nombre de messages. Le critère 3 est alors atteint
parce que la comparaison ne trouve rien à écrire, pas parce qu'on a sauté la demande.

Le paramètre est le **résultat de l'`ENABLE`**, pas la capacité annoncée. Un serveur qui
annonce `CONDSTORE` puis le refuse doit emprunter le repli, et c'est exactement la panne que
`mailfake` sait provoquer.

#### Les suppressions demandent un balayage, et c'est une dette assumée

`CONDSTORE` seul ne signale pas les purges. Il faudrait `QRESYNC` et sa réponse `VANISHED`,
qu'aucun des dix comptes n'est garanti d'offrir. La moisson fait donc un `UID FETCH 1:* (UID)`
à chaque passage : une réponse, pas de corps, et la liste exacte de ce qui existe encore.

C'est ce qui fait qu'une copie disparue est **détectée** au lieu d'être oubliée. Le jour où
`QRESYNC` arrivera, il le remplacera.

#### L'ordre des écritures est ce qui rend une coupure sans perte

L'état de synchronisation est écrit **en dernier**. Il est le témoin de ce qui est fait :
l'écrire avant rendrait un dossier à moitié moissonné indistinguable d'un dossier à jour. Un
test le vérifie sur la panne du littéral tronqué — après l'échec, `uidvalidity` est encore
`None`, donc le passage suivant recommence.

Les corps arrivent par lots de 100, un lot par transaction. Un lot par message ferait 100 000
allers-retours sur le plus gros dossier ; tout en un seul ferait perdre le dossier entier sur
une coupure.

#### Ce que le chiffrement refuse

Trois refus, et aucun n'a d'échappatoire :

- **aucune option pour ignorer un certificat.** Une option « accepter quand même » finit
  toujours par être activée « juste pour tester », et ne se désactive jamais ;
- **aucun repli en clair.** Un repli silencieux enverrait le mot de passe en clair —
  l'accident exact que `Security` sans variante en clair sert à rendre impossible ;
- **aucun `STARTTLS` optionnel.** Un serveur qui ne l'annonce pas fait échouer la connexion.
  Continuer, c'est se laisser rétrograder par un attaquant qui a retiré l'annonce.

**Le test le plus important du crate pointe le connecteur TLS sur `mailfake`**, qui parle en
clair, et exige un échec. Il n'y a aucun autre moyen de le vérifier : un connecteur qui n'a
jamais rencontré de serveur en clair ne prouve rien sur ce qu'il ferait.

Le mot de passe est **cité et déguisé** dans la commande `LOGIN`. Les mots de passe
applicatifs de Google contiennent des espaces ; et sans déguisement, un mot de passe
contenant un guillemet coupe la commande en deux — le même trou qu'une injection, sur un
protocole plus vieux. Un test le vérifie avec `x" LOGOUT"`.

#### Une fuite mémoire bornée, et pourquoi elle est assumée

`NewMessage<'a>` emprunte ses champs, ce qui est le bon choix pour l'import mbox : les
tranches pointent dans un tampon qui vit plus longtemps que l'insertion. La moisson IMAP
**fabrique** ces chaînes en décodant des en-têtes, et elles n'ont nulle part à vivre.

Trois options : rendre `NewMessage` possédant, et faire payer une allocation par champ aux
100 000 messages de l'import qui n'en a pas besoin ; le rendre générique sur la propriété, et
compliquer un type public pour un seul appelant ; ou fuiter quatre chaînes courtes par message
moissonné. La troisième est retenue, **et sa condition de validité est écrite** : un message
n'est moissonné qu'une fois, la deuxième synchronisation le trouve déjà connu par son UID. Le
test `a_second_harvest_writes_nothing` est ce qui garde cette hypothèse honnête — le jour où il
tombe, la fuite devient une fuite au sens propre.

#### Les huit pannes de `mailfake`, toutes exercées

Chacune a son test dans `tests/moisson.rs`, et le résultat est vérifié **dans le store** :

- littéral tronqué et longueur fausse → la moisson **échoue** au lieu de bloquer, et l'état de
  synchronisation n'a pas avancé ;
- `UIDVALIDITY` changé → moisson complète, **zéro blob dupliqué** (critère 9) ;
- `CONDSTORE` annoncé puis refusé → repli emprunté, et un test compare les deux chemins
  message par message (critère 10) ;
- numéro de séquence impossible → sans effet, le client indexe par UID ;
- UID doublé → absorbé, rien n'est dédoublé dans le store ;
- login refusé → `AuthRefused`, **non réessayable** : réessayer sur un mot de passe faux fait
  bloquer le compte chez le fournisseur ;
- connexion coupée → échec réessayable, rien d'écrit.

Et le pari de la phase 1 est vérifié sur le chemin réseau : le même message dans deux boîtes
donne **un blob et deux références**.

#### Ce qui reste de l'étape 3

**Le connecteur TLS n'a jamais parlé à un vrai serveur.** Ce qu'on sait de lui : il refuse un
serveur en clair, il refuse un `STARTTLS` non annoncé, il refuse un nom d'hôte invalide, et il
lit le magasin de confiance du système. Ce qu'on ne sait pas : s'il synchronise réellement un
compte. C'est la validation qui demande des identifiants, et elle appartient à l'utilisateur.

Rien ne déclenche encore une moisson : ni commande CLI, ni tâche de fond du démon. Le
trousseau et OAuth2 sont l'étape 5.

### Étapes 4 et 5, la partie qui ne demande pas de compte réel — 2026-09-03

Le décodage des noms, la découverte des dossiers, le trousseau, et les deux commandes. 630 tests
au total sur le dépôt.

#### `mailsync::mutf7` — décoder un nom de boîte

La RFC 3501 §5.1.3 : de l'ASCII tel quel, le reste en Base64 d'UTF-16BE entre `&` et `-`, avec
`,` à la place de `/`. `R&AOk-glages` est `Réglages`.

**Il n'y a pas d'encodeur, et c'est délibéré.** On n'envoie jamais un nom qu'on n'a pas reçu :
`folders.remote_name` garde les octets du serveur, et ce sont eux qui repartent. Un encodeur
créerait un deuxième chemin vers le nom d'une boîte, donc une deuxième occasion de se tromper.

**Le décodage ne peut pas échouer**, et c'est un choix : `path` est un libellé d'affichage, pas
une clé. Un nom mal encodé doit produire quelque chose de lisible plutôt que faire échouer la
découverte — un dossier affiché de travers reste ouvrable, un dossier qu'on refuse de lister
est du courrier perdu de vue. Ce qui protège la correction, c'est que le décodage ne sert à
**rien d'autre** que l'affichage.

Trois écarts réels traités : des noms en UTF-8 brut malgré la RFC, des séquences non terminées,
du Base64 tronqué. Et trois refus, parce que décoder à moitié donnerait un nom silencieusement
faux : bourrage non nul, nombre impair d'octets, substitut non apparié.

**Un test m'a corrigé.** J'avais écrit que `&Ti0FDA-` valait `決算`. Il vaut `中Ԍ` — le décodeur
avait raison, ma mémoire de l'exemple de la RFC non. Les encodages du fichier sont maintenant
calculés à la main : `&bHp7lw-` pour `決算`, et `&A,A-` pour U+03F0, qui est le seul moyen de
faire apparaître le `,` de l'alphabet modifié. Un exemple recopié de mémoire teste une mémoire,
pas un décodeur.

#### `mailsync::discover` — quels dossiers existent

`LIST` puis un `upsert_folder` par boîte. Le rôle vient de l'attribut `SPECIAL-USE` de la
RFC 6154 quand le serveur en donne un, du nom sinon — **l'attribut gagne**, parce qu'une
corbeille renommée `Poubelle` n'est reconnue par aucune heuristique et que `\Trash` la désigne
sans ambiguïté.

L'heuristique de nom a déménagé de `mailimport` vers `mailcore` : deux portes d'entrée doivent
ranger la même `Corbeille` pareil, sinon le rôle d'un dossier dépend de la façon dont il est
arrivé. Les 29 tests d'origine passent sur la fonction déplacée.

Trois pièges, tous avec leur test :

- **`\Noselect`** — le `[Gmail]` de Gmail est un nœud de hiérarchie sans contenu. Un `EXAMINE`
  dessus rendrait un `NO` et ferait échouer une synchronisation par ailleurs correcte. Écarté,
  compté, journalisé — jamais en silence ;
- **le séparateur n'est pas toujours `/`** — Courier utilise `.`. Sans normalisation,
  `INBOX.Corbeille` n'est pas reconnu comme une corbeille et l'arbre s'affiche à plat. Le
  chemin affiché est normalisé, `remote_name` ne l'est jamais ;
- **la découverte ne doit pas remettre l'état à zéro.** Passer par `set_sync_state` effacerait
  `uidvalidity`, donc déclencherait une moisson complète à chaque passage. D'où
  `set_remote_name`, qui n'écrit que le nom.

Et un refus assumé : **un dossier disparu du serveur reste**. Le supprimer retirerait ses
références, donc ferait disparaître du courrier, sur la foi d'un `LIST` qui peut être incomplet
parce que le serveur redémarrait. Le nettoyage sera une action explicite.

#### `mailauth` — le secret, et rien que le secret

Un crate à part de `mailsync`, pour une raison de dépendances : OAuth2 apportera un client HTTP
et l'ouverture d'un navigateur, et un compte à mot de passe applicatif n'a pas à tirer ça.

**La clé est `(hôte, identifiant)`, pas l'`AccountId`.** Un `AccountId` est un `rowid` SQLite,
et le store *est* reconstructible — c'est une propriété du projet. Un secret rangé sous un
`rowid` deviendrait orphelin au premier store neuf, et l'utilisateur retaperait ses dix mots de
passe sans comprendre pourquoi.

Deux refus : un **secret vide** est rejeté à l'écriture, parce qu'il réussit l'enregistrement et
échoue à l'authentification — l'utilisateur croirait avoir configuré son compte et le
diagnostic arriverait une synchronisation plus tard. Et il n'y a **aucune fonction qui rende
tous les secrets** : un inventaire des secrets n'a qu'un usage.

#### Une suite de tests qui salissait la machine

`cmdkey /list` le 2026-09-03 : **34 entrées de test** dans le Credential Manager de
l'utilisateur, accumulées sur plusieurs jours, la plupart venues de `mailapi::token`.

La cause est structurelle. Ces tests écrivent dans le **vrai** trousseau — il n'y a pas de
trousseau en mémoire à substituer, et en simuler un ne testerait plus le trousseau. Le `forget`
en dernière ligne est donc sauté dès qu'une assertion tombe.

Les 34 ont été supprimées, et les deux crates ont maintenant un garde `Drop`, qui tourne en
panique comme en succès. Un test vérifie **le garde lui-même**, en paniquant volontairement :
sans lui, on ne saurait pas que le nettoyage marche sur le chemin qui compte. Zéro résidu après
une exécution complète.

*Une suite de tests qui salit la machine de son utilisateur est un bug de la suite de tests*,
et celui-là était passé sous le radar de dix relectures parce qu'il ne fait échouer aucun test.

#### `mail account` et `mail sync`

**Le secret ne passe jamais par un argument** — pas même une option cachée « pour scripter ».
Il se lit sur l'entrée standard, sans écho quand c'est un terminal.

Le premier jet se bloquait pour toujours en mode scripté. `rpassword` ouvre la console
(`CONIN$`) sur Windows au lieu de lire l'entrée standard : il **ignore un tube**, et
`mail account add … < secret.txt` attendait une frappe qui ne viendrait jamais. Corrigé en
détectant le terminal **avant** l'appel, avec `IsTerminal` de la bibliothèque standard — la
vraie question est « y a-t-il quelqu'un pour taper ? », pas « est-ce que `rpassword` échoue ? ».

**`--daemon` est refusé sur les deux commandes**, avec la raison : c'est le démon qui se
connecte au serveur IMAP, donc c'est **son** trousseau qui doit porter le secret. Déclarer le
compte ici et synchroniser là-bas donnerait un secret introuvable au moment de s'en servir.

Deux choix de robustesse dans `sync` :

- **un compte qui échoue n'arrête pas les autres.** Dix comptes, quatre fournisseurs : il y en
  aura toujours un qui ne répond pas, et la synchronisation d'ensemble ne doit pas être aussi
  fiable que son maillon le plus faible. Idem un cran plus bas : un dossier qui échoue n'arrête
  pas le compte ;
- **le code de sortie porte le verdict.** Un planificateur qui relance toutes les dix minutes
  n'a que ça pour savoir qu'il s'est passé quelque chose.

Et le bilan distingue « à réessayer » de « à corriger ». Ce n'est pas cosmétique : insister sur
un mot de passe refusé fait bloquer le compte chez le fournisseur.

Un mensonge attrapé en essayant la commande : le message « `mail account add` pour le
réactiver » était faux, parce que `upsert_imap_account` ne touche pas à `enabled`. Corrigé dans
le code plutôt que dans le message — redéclarer un compte veut dire qu'on veut qu'il remarche.

#### Ce qui reste, et ce qui appartient à l'utilisateur

La chaîne complète a été exercée bout en bout contre un hôte inexistant : compte déclaré,
secret au trousseau, découverte tentée, échec réseau annoncé comme réessayable, code de sortie
non nul. **Ce qui n'a jamais eu lieu, c'est la même chaîne contre un vrai serveur** — et ça
demande des identifiants.

```
mail account add --host <hôte> --username <identifiant> --security tls
mail sync
```

Pas Gmail pour le premier : `AuthKind::OAuth2` existe dans le modèle mais rien ne le sert, et
un compte déclaré en OAuth2 s'enregistrerait sans jamais se synchroniser. C'est l'étape 5, et
elle porte le vrai risque de la phase.

### Critère 6 mesuré, et un test qui passait à vide — 2026-09-03

`crates/mailsync/tests/secret.rs`. Un mot de passe **sentinelle** traverse tout le chemin réel
— connexion, `LOGIN`, découverte des dossiers, moisson de trois messages — puis on le cherche
dans tout ce que le processus a écrit :

| Où | Comment |
|---|---|
| le store | tous les fichiers, récursivement, **octet par octet** — pages libérées de SQLite et fragments d'index tantivy compris |
| les journaux | capturés au niveau **`TRACE`**, pas au niveau par défaut |
| le répertoire temporaire du système | non récursif, plafonné à 8 Mio par fichier |

Ce n'est pas une relecture de code. Une relecture dit « je ne vois pas où le secret sortirait » ;
ceci dit « il n'est pas sorti ». La différence compte parce que le secret traverse une pile
qu'on n'a pas écrite : `rustls`, `rusqlite`, `tracing`, et un jour une bibliothèque OAuth2.

#### Le contrôle du tampon de journaux a servi immédiatement

Le harnais a deux contrôles positifs, dans l'esprit du critère 8. Le premier vérifie que le
chercheur trouve une sentinelle qu'on vient d'écrire. Le second vérifie que **le tampon de
journaux n'est pas vide** — et c'est celui-là qui a attrapé quelque chose, à la première
exécution.

Le mécanisme vaut d'être écrit, parce qu'il piégera quelqu'un d'autre : **`tracing` met en
cache l'intérêt de chaque site d'appel globalement au processus.** Un `set_default` ne vaut que
pour son fil. Quand un autre fil du même binaire n'a aucun abonné, le site d'appel qu'il touche
le premier est mis en cache comme « personne ne s'y intéresse », et les événements du fil qui
écoute sont **sautés**.

Observé exactement : le tampon ne contenait que deux lignes — celles des sites d'appel touchés
d'abord par le bon fil — et l'assertion « aucun secret dans les journaux » passait sur des
journaux quasi vides. **Le test aurait été vert en ne vérifiant rien.**

Corrigé par un `set_global_default` et **un seul test dans le fichier**. La contrainte est
structurelle, pas cosmétique : ajouter un deuxième `#[test]` ici recasserait la capture.

#### Une deuxième version fausse, du balayage temporaire

Le premier jet comparait la liste des fichiers du répertoire temporaire avant et après, et
échouait sur le répertoire temporaire de l'autre test. C'est faux par construction : n'importe
quelle application de la machine y écrit pendant la fenêtre de mesure.

Chercher **la sentinelle** n'a pas ce défaut, et c'est ce qui justifie qu'elle soit une chaîne
improbable : aucun autre processus ne la connaît. La trouver est une fuite d'ici ; ne pas la
trouver ne dépend de personne d'autre.

#### Ce que le test affirme aussi en creux

L'identifiant du compte **est** journalisé, et le test l'exige. Savoir pour quel compte une
synchronisation a tourné est nécessaire au diagnostic ; c'est la ligne exacte entre une trace
utile et une fuite, et l'affirmer la rend visible plutôt que subie.

**Ce que ça ne prouve pas** : rien sur un vrai serveur, ni sur un vrai mot de passe applicatif.
Le chemin testé est celui de `mailfake` en clair, parce que le connecteur TLS refuse — à juste
titre — un serveur non chiffré. Le secret passe quand même par le vrai `Client::login`, donc
par le vrai code de citation et d'écriture sur la socket.

### Critère 4 mesuré, et la liste qui s'effondrait — 2026-09-03

`cargo xtask measure-ui --bench sync`. La coquille défile dans son processus, la moisson écrit
depuis celui du banc. **Deux processus, et ce n'est pas une commodité** : en déploiement de
référence, le démon écrit et la coquille lit. Les faire dans un seul mesurerait une contention
de verrou qui n'existe pas en production.

| Mesure | Relevé |
|---|---|
| Lignes défilées | **9 400**, sur un dossier de 60 000 en cours de remplissage |
| Changements du store **pendant** les 600 images | **58** |
| Travail par image, p95 | **0,99 ms** — budget 16,7 ms, **passé** |
| Pire image | 2,27 ms |

Ça ferme le trou reconnu de la phase 1 : le critère 2 défilait sur une liste au repos, volet de
lecture vide, rien qui écrivait. La règle 3 du `CLAUDE.md` dit *l'UI ne bloque jamais sur le
réseau ou sur un import*, et une synchronisation est les deux à la fois.

Le critère 2 a été repassé après coup sur le corpus réel : **1,59 ms** de p95 sur 20 544
lignes, `changements=0`. Le changement de comportement décrit ci-dessous ne l'a pas abîmé.

#### Trois relevés de suite, dont deux qui ne mesuraient rien

**Premier : 0,71 ms.** Le contrôle était « la moisson a-t-elle écrit pendant que la coquille
tournait ? », et il passait. Mais la coquille avait chargé les 20 000 lignes, donc la moisson
était **finie avant que le défilement commence**. Le contrôle était trop large : il couvrait
toute la vie du processus, pagination comprise.

Corrigé en déplaçant le contrôle **dans la coquille** : elle compte les `Changed` reçus pendant
la phase de défilement, et le banc refuse un zéro. C'est le seul endroit d'où le recouvrement
est observable.

**Deuxième : 0,60 ms, et `changements=1`.** Le contrôle a fait son travail — un seul changement
en 600 images est un recouvrement symbolique. La cause est intéressante : **chaque `Changed`
relançait la pagination depuis la première page**, donc la coquille n'atteignait « tout
chargé » qu'une fois les écritures arrêtées. Le banc attendait cet état, donc il défilait
toujours après la moisson.

Mais ce relevé avait un deuxième défaut, que le premier contrôle ne voyait pas : `lignes=200`.
La coquille défilait sur **deux cents lignes** pendant que l'offset balayait la hauteur de
60 000. La plupart des images dessinaient une liste vide, et le chiffre était bas parce qu'il
n'y avait rien à dessiner.

**Troisième : 0,99 ms, 58 changements, 9 400 lignes.** Celui-là mesure.

#### Le défaut que le banc a trouvé

`Changed` **vidait la liste** et repaginait. Correct au sens où les lignes reviennent, et
inacceptable au sens de l'usage : l'utilisateur était à la cinq-millième ligne, il se retrouve
avec cent lignes et une position de défilement qui pointe au-delà de la fin. **La liste
s'effondre sous lui**, une soixantaine de fois pendant une synchronisation.

C'est le frère du bug corrigé le matin même — `Changed` fermait le message ouvert. La relecture
avait trouvé celui-là et pas celui-ci, parce qu'aucun test ne défilait pendant qu'on écrivait.

La correction tient à la forme de la liste : elle est triée par `(date DESC, id DESC)`, donc un
message qui arrive va **en tête**. `merge_head` recharge la première page et la fusionne
par-dessus les lignes déjà chargées, en dédupliquant par identifiant. Rien n'est perdu, le
nouveau courrier apparaît, la position de défilement tient.

Ce que la fusion ne rattrape pas, et qui est écrit dans le code : un message **supprimé** au
milieu de ce qui est déjà chargé, et un message ancien qui arriverait après coup — un import
qui remplit le passé. Les deux se corrigent au prochain vrai rechargement, et aucun des deux ne
justifie de faire s'effondrer la liste à chaque lot.

#### Une borne de patience, et pourquoi elle est sans effet sur le critère 2

Le banc de défilement attendait « tout chargé » avant de mesurer. Sur un dossier qu'on remplit,
cet état n'arrive jamais. La coquille se contente donc maintenant de ce qu'elle a chargé au
bout de 25 s, et le dit dans son relevé.

Sur un store au repos — le cas du critère 2 — la pagination des 20 544 lignes du corpus finit
en quelques secondes et la borne ne se déclenche pas. Le relevé du critère 2 refait après coup
le confirme : 20 544 lignes chargées, comme avant.

#### Une note sur le chiffre de critère 1 de ce relevé

Le banc `scroll` rapporte aussi le critère 1, et il affichait **1 523 ms** — contre 174 à
180 ms au repos. Ce chiffre est à ignorer : il a été pris juste après deux moissons de 60 000
messages, donc sur un disque saturé. C'est le biais documenté dans `docs/PHASE-1.md`, et il
vaut la peine d'être noté ici comme démonstration : un facteur **huit et demi** sur la même
machine et le même exécutable. Le critère 1 se mesure avec le banc `startup`, sur une machine
au repos, avec `--no-build`.

### La synchronisation devient une tâche de fond du démon — 2026-09-03

`mail sync --daemon` marche. C'est ce qui débloque le déploiement de référence : démon sur une
machine dédiée, clients ailleurs, et la synchronisation déclenchable depuis n'importe lequel
d'entre eux.

#### La boucle de compte est partagée, et il fallait la déplacer

`mail-cli` avait sa propre boucle « pour chaque dossier, moissonner ». La recopier dans le job
du démon aurait donné **deux versions du même comportement**, qui auraient divergé — et la
version locale aurait fini par traiter un compte autrement que la version distante, sur le même
compte.

Elle vit donc dans `mailsync`, en deux couches, pour la même raison que `Client::greet` et
`connect` :

- `sync_account_over(client, …)` prend une connexion **déjà authentifiée**. C'est celle que les
  tests exercent contre `mailfake`, qui parle en clair ;
- `sync_account(store, account, secret, …)` chiffre, s'authentifie, puis appelle la première.

Ce qui reste dans la CLI est ce qui appartient à la CLI : lire le secret, présenter le bilan.

#### L'annulation est vérifiée entre les lots, pas seulement entre les dossiers

Entre les dossiers ne suffirait pas : le plus gros dossier du corpus réel fait 1,4 Go, et une
annulation qui attendrait la fin de celui-là n'en serait pas une. `harvest_watched` consulte
donc la progression **à chaque lot de cent messages**.

Ce qui rend l'interruption sûre est l'ordre d'écriture décidé à l'étape 3 : l'état de
synchronisation est écrit **en dernier**. Une moisson interrompue laisse donc les lots validés
en place et le témoin inchangé, et le passage suivant redemande ce qui manque au lieu de croire
le dossier à jour. Un test le vérifie sur 250 messages, en annulant après le premier lot :
l'`uidvalidity` est encore `None`, et le passage suivant finit le travail.

Ce que l'annulation ne fait pas : interrompre un `UID FETCH` déjà émis. Un lot arrive en entier
ou pas du tout.

#### Le total de progression est découvert, pas inventé

Comme celui de l'import, qui compte des octets découverts fichier par fichier. Le total du
`sync` augmente à mesure que les dossiers sont examinés. Un total annoncé au départ serait un
chiffre qu'on ne sait pas, présenté comme un chiffre qu'on sait.

#### Une contrainte de déploiement à connaître

**Le secret vient du trousseau de la machine du démon.** C'est lui qui se connecte au serveur
IMAP, donc c'est son trousseau qui doit le porter. `mail account add` refuse `--daemon` pour
cette raison, avec le message qui le dit — déclarer le compte depuis un poste client rangerait
le secret dans le mauvais trousseau, et la synchronisation échouerait côté démon sur un secret
introuvable.

Conséquence : **un démon sur une machine dédiée a besoin d'un trousseau accessible sur cette
machine.** Une session Linux sans Secret Service fait échouer le job, avec un message qui le
dit plutôt qu'un « rien ne s'est passé ».

Ce n'est pas une limitation à contourner. `docs/PRIVACY.md` §7 interdit l'alternative — un
secret dans un fichier de configuration ou une variable d'environnement — et le jeton du démon
lui-même est traité autrement pour une raison écrite là-bas : il est posé par un gestionnaire
de services, qui a ses propres mécanismes de secrets. Un mot de passe de messagerie appartient
à l'utilisateur, pas au service.

#### Ce qu'un client peut demander, et pourquoi ça n'ouvre rien

`jobs.start` accepte `kind: "sync"` avec un `account` optionnel. L'identifiant d'un compte est
**déjà connu du client** : `folders.list` le porte pour chaque dossier. Ce paramètre n'ouvre
donc aucune capacité nouvelle, contrairement au chemin d'un profil que l'import refuse au
profit d'un rang.

Le job échoue quand **tous** les comptes ont échoué, et réussit avec un bilan nuancé sinon —
« neuf comptes sur dix » est un résultat, pas une erreur. Les noms d'hôte et les identifiants
restent dans le journal du démon ; le client reçoit de quoi agir sans recevoir de quoi profiler
(`docs/PRIVACY.md` §8).

### Le premier compte réel, et trois bugs qu'aucun test ne voyait — 2026-09-04

`contact@perso.invalid`, un serveur Dovecot sur `mail.perso.invalid`, en `STARTTLS` sur 143. La
configuration a été relue dans le `prefs.js` de Thunderbird — en lecture seule, règle 1 — et le
mot de passe est venu de l'utilisateur : celui de Thunderbird est dans `logins.json` chiffré par
NSS, et le magasin de secrets d'une autre application ne se déchiffre pas.

| Mesure | Relevé |
|---|---|
| Moisson complète | 15 dossiers, **2 839 messages**, 1,5 Gio de RFC 5322, **2 min 19 s** |
| Deuxième passage, rien de neuf | **2 s, 0 corps téléchargé, 0 écriture** — critère 3, **passé** |
| `CONDSTORE` | **absent** : tout est passé par le chemin de repli, critère 10 sur un vrai serveur |
| Index plein texte | 2 839 messages, 7,6 Mio de texte, 4,4 s |
| Fils | 1 499 fils, 980 liens `References`, 360 par sujet, 2,3 s |

**Une connexion réelle a trouvé en une tentative ce que 41 tests d'intégration ne voyaient
pas.** Trois bugs, dont deux de correction.

#### Après un `STARTTLS`, un serveur n'envoie pas de second salut

Mon code en attendait un, jusqu'au délai de lecture de 120 s, rapporté en « erreur réseau ».
La RFC 2595 décrit la suite : commande `STARTTLS`, réponse `OK`, poignée de main, puis le
client **redemande** `CAPABILITY`. Il n'y a pas de deuxième `* OK`.

**Aucun test ne pouvait l'attraper** : `mailfake` ne parle pas TLS, donc le chemin `STARTTLS`
n'existait que contre un vrai serveur. C'est la limite du harnais, et elle est écrite dans
`Client::over` — le constructeur qui prend un flux sans attendre de salut, et qui repart avec
une liste de capacités **vide**, comme la RFC l'exige : celles annoncées en clair ont pu être
modifiées par quelqu'un qui voulait faire disparaître `STARTTLS` de la liste.

#### `{uidnext}:*` ne veut pas dire « rien »

Un ensemble d'UID n'est pas ordonné, dit la RFC 3501. `1000:*` sur une boîte dont le plus
grand UID est 999 se lit donc `999:1000`, et le serveur rend **le dernier message**.

Chaque passage incrémental retéléchargeait le dernier message de chaque dossier : 13 corps
pour rien, un par dossier non vide. Avec une pièce jointe de 25 Mo, c'est 25 Mo par dossier et
par passage. `UIDNEXT` distant répond exactement à la question, et la garde tient en une
condition.

**Le test qui manquait était précis** : un *deuxième* passage **sans** `CONDSTORE`.
`a_second_harvest_writes_nothing` passait, mais sur un serveur *avec* `CONDSTORE`, où le plan
est `UpToDate` et où aucun corps n'est demandé. Les deux tests du chemin de repli ne faisaient
qu'un seul passage. La combinaison — repli **et** deuxième passage — n'existait nulle part.

#### Réécrire une ligne identique est une écriture

Sans `CONDSTORE`, la moisson redemande **tous** les drapeaux à chaque passage. Elle
réenregistrait donc les 2 840 copies avec les mêmes valeurs : SQLite met la ligne à jour, salit
une page du WAL, et le bilan annonçait 2 840 « copies » qui n'avaient rien produit.

**Le critère 3 était faux pour cette seule raison.** L'`ON CONFLICT` porte maintenant une
clause `WHERE`, `record_copy` rend « quelque chose a changé », et le deuxième passage tombe à
2 s et zéro franc.

`IS NOT` et non `<>` dans cette clause : le `MODSEQ` est nullable, et `NULL <> NULL` vaut
`NULL` — la comparaison naïve n'aurait jamais rien écrit.

### Correction du critère 5 : le seuil était faux — 2026-09-04

Le critère disait « aucune connexion sortante vers un hôte qui ne figure pas dans la
configuration des comptes, **exactement 0** ». C'est faux, et pas d'un cheveu.

#### Ce qui a été mesuré

`rustls-platform-verifier` passe `CERT_CHAIN_REVOCATION_CHECK_END_CERT` à l'API de Windows :
la **vérification de révocation avec récupération réseau** est active. Le certificat de
`mail.perso.invalid` est un Let's Encrypt sans point OCSP — ils l'ont retiré — mais avec un
point de distribution de CRL : `http://ye1.c.lencr.org/115.crl`.

La mesure, en trois pas :

1. cette entrée précise a été retirée du cache d'URL de Windows (`certutil -urlcache … delete`,
   réversible : le système la retélécharge au besoin) ;
2. `mail sync` a été lancé ;
3. **l'entrée est revenue.**

Donc une synchronisation fait ouvrir une connexion vers `ye1.c.lencr.org`, un hôte qui n'est
pas dans la configuration du compte. Le cache est partagé par toute la machine, ce qui rendait
la première observation ambiguë ; la suppression ciblée puis le retour lèvent l'ambiguïté.

#### Pourquoi ce n'est pas un défaut à corriger

C'est un **arbitrage entre vie privée et sécurité**, et il penche du bon côté. Sans
vérification de révocation, un certificat volé puis révoqué resterait accepté jusqu'à son
expiration. Ce que l'autorité de certification apprend en échange est faible et indirect :
qu'une machine s'intéresse à un certificat qu'elle a émis, à cet instant — pas quel serveur,
pas quel compte, pas quel message.

La désactiver serait possible et serait une **erreur** : on échangerait une fuite marginale
vers une autorité de certification contre l'acceptation silencieuse d'un certificat révoqué,
sur le canal qui transporte un mot de passe de messagerie.

#### Le critère devient

> **Aucune connexion sortante vers un hôte non configuré, par le code de mailcore.** Les
> connexions que le système d'exploitation initie pour valider un certificat sont nommées, pas
> niées.

C'est le même mouvement que la fuite `<iframe>` du critère 8 : la plateforme fait quelque chose
que notre code ne fait pas, et le document le disait autrement. Dans les deux cas, le seuil
« exactement 0 » était une promesse qu'on ne tenait pas — et dans les deux cas, c'est un
harnais qui l'a montré, pas une relecture.

#### Ce que le harnais vérifie, et ce qu'il ne vérifie pas

Observer **toutes** les connexions d'un processus demande une capture au niveau du système :
privilèges, pilote de filtrage, dépendance à la plateforme. `xtask/tests/reseau.rs` fait autre
chose, et le dit :

- **deux fichiers livrés, et deux seulement, ouvrent une connexion sortante** :
  `mailapi/src/client.rs` — adresse venue de `--daemon` — et `mailsync/src/tls.rs` — adresse
  venue de la configuration du compte. Aucun des deux ne choisit son hôte ;
- **aucun crate livré ne dépend d'un client HTTP ni d'un SDK de télémétrie.**

Deux contrôles positifs, dans l'esprit du critère 8 : le lecteur de manifestes doit trouver une
dépendance qui est là, et l'analyse de source doit trouver les deux fichiers autorisés. Un
chercheur aveugle rendrait « aucun coupable » sur un dépôt qui en serait plein.

Et une correction du premier jet, qui vaut d'être notée : le test échouait sur la **phrase**
`//! Pas de reqwest : un client généreux en pool…`, un commentaire qui dit exactement le
contraire de ce que le test cherchait. Les lignes sont maintenant débarrassées de leur
commentaire avant l'analyse, et un test vérifie ce nettoyage — parce qu'un chercheur qui saute
les lignes entières raterait un vrai appel suivi d'un commentaire.

#### Ce qui reste découvert sur ce critère

**Les autres plateformes.** `CERT_CHAIN_REVOCATION_CHECK_END_CERT` est le chemin Windows.
macOS et Linux ont leurs propres politiques de révocation, et personne ne les a mesurées.

**Le DNS.** Chaque connexion résout un nom, donc parle à un résolveur. C'est une connexion vers
un hôte non configuré, par construction, et aucune application n'y échappe. Ça n'a pas été
mesuré parce que ça ne se corrige pas.

### OAuth2 : le consentement, le rafraîchissement, et deux fuites de la suite de tests — 2026-09-04

Ce qui manquait pour qu'un compte Google fonctionne sans mot de passe applicatif. Trois
morceaux, dans cet ordre : le connecteur TLS vers le point de terminaison de jeton, l'écouteur
de bouclage qui reçoit le code d'autorisation, et le stockage du jeton de rafraîchissement avec
son rafraîchissement automatique.

#### Le connecteur de jeton a fait tomber le test du critère 5, et c'était son travail

`mailauth::http::post_form_tls` est la troisième surface réseau d'un crate livré.
`xtask/tests/reseau.rs` a échoué à la compilation suivante, en nommant le fichier et en
demandant qu'il soit déclaré. Il l'est, avec sa raison écrite : **l'hôte ne vient pas d'une
donnée, il vient d'une constante du code** (`oauth::Provider`). Un point de terminaison de jeton
configurable serait le moyen le plus simple de faire envoyer un jeton de rafraîchissement
ailleurs.

#### L'écouteur de bouclage, et le piège de Windows sur l'ouverture du navigateur

`mailauth::consent::Loopback` ouvre un port **éphémère** sur `127.0.0.1` — littéral, pas
`localhost` : une résolution de nom est une dépendance de trop sur le chemin d'un code
d'autorisation. Il sert au plus dix requêtes, en accepte une seule utile, puis ferme.

Trois protections, dont aucune ne suffirait seule : le port éphémère, l'état anti-rejeu vérifié
**avant** de regarder le code, et PKCE qui rend le code inutilisable sans le vérificateur.

L'ouverture du navigateur se fait par `rundll32.exe url.dll,FileProtocolHandler` : un appel
direct à un exécutable, sans shell.

> **Ce paragraphe contenait deux affirmations fausses, corrigées le soir même par la sonde
> `cargo xtask browser-probe`. Elles sont laissées ici barrées plutôt qu'effacées, parce que la
> façon dont elles se sont installées est plus instructive que leur contenu.**
>
> ~~« `cmd` développe les variables d'environnement, donc `%2F%2F` disparaît et l'URL arrive
> tronquée. »~~ **Faux pour `cmd /c`** : l'effacement des variables non définies est le
> comportement des *fichiers de commandes*, pas de la ligne de commande. Mesuré : `cmd /d /c
> start "" "<url>"` reçoit l'URL intacte, `%2F%2F` compris. C'était un raisonnement plausible
> que personne n'avait éprouvé.
>
> ~~« `rundll32` rend 0 sans rien ouvrir au-delà de ~400 caractères. »~~ **Non reproduit** :
> mesuré identique à 40, 420, 1 000 et 2 048 caractères, onglet ouvert et URL intacte à chaque
> fois. Cette affirmation venait d'une déduction — un essai dont personne n'avait confirmé le
> résultat — écrite ensuite comme un fait mesuré. C'est exactement la faute que le reste de ce
> document prétend éviter.

Ce qui **était** vrai, et que la sonde a établi : `explorer.exe <url>` n'ouvre jamais le
navigateur — il ouvre l'explorateur de fichiers — et rend 1. Placé en tête de la liste des
lanceurs, il empêchait `rundll32` d'être essayé du tout, puisque `open_browser` s'arrête au
premier processus **démarré** et non au premier qui aboutit. C'était le vrai défaut.

Reste une observation inexpliquée : le tout premier consentement, avec `rundll32` seul, n'a rien
ouvert. La sonde ne la reproduit pas. Deux causes possibles, aucune vérifiée — l'appel venait
d'une tâche de fond dont le processus fils a pu être tué, ou le schéma `https://` vers un hôte
distant se comporte autrement que le `http://127.0.0.1` que la sonde peut tester sans sortir de
la machine.

Et un navigateur qui ne s'ouvre pas **n'est pas** un échec du consentement : l'URL vient d'être
affichée, l'utilisateur peut la coller. Échouer là casserait le cas d'une session distante.

#### Un bug de mon premier jet : le délai portait sur la mauvaise attente

Le délai de cinq minutes était posé sur le socket **accepté**. Dans le cas qui arrive vraiment —
l'utilisateur ferme l'onglet sans rien valider — personne ne se connecte, `accept` bloque pour
toujours, et le CLI reste pendu sans un mot. `std` n'a pas d'`accept` avec délai : l'écouteur est
donc non bloquant et repasse toutes les 50 ms.

Le test qui le verrouille existe parce que l'échéance a été rendue injectable. Vérifier que cinq
minutes expirent demanderait un test de cinq minutes, donc personne ne l'écrirait, donc le
chemin qui compte ne serait jamais exécuté. Il porte son contrôle inverse : l'attente doit avoir
**duré**, sinon une fonction qui rendrait l'erreur tout de suite passerait l'assertion sans rien
prouver.

Un deuxième défaut de la même famille, dans le test cette fois : une requête de plus que le
plafond de l'écouteur laisse un client attendre son propre délai, ce qui faisait durer la suite
huit secondes. Le test s'arrête au plafond exact, et le client a désormais un délai de lecture —
un test qui peut se pendre est un test qu'on finira par désactiver.

#### Le jeton : une seule entrée de trousseau, et pourquoi

Quatre choses vont ensemble — identifiant client, secret client, jeton de rafraîchissement,
jeton d'accès en cache — et elles sont écrites comme **un** objet JSON sous **une** entrée.

La raison est l'atomicité. Un rafraîchissement remplace le jeton d'accès et parfois le jeton de
rafraîchissement ; en deux entrées, une interruption entre les deux écritures laisse un compte
dont l'un des deux jetons ne correspond plus à l'autre. Le trousseau ne connaît pas la
transaction : la seule façon d'être atomique est de n'écrire qu'une fois.

Deux décisions qui vont contre l'intuition, et leurs raisons :

- **le jeton d'accès est mis en cache**, au même endroit que le jeton de rafraîchissement. Qui
  peut lire l'un peut lire l'autre, et l'autre est bien plus puissant : un jeton d'accès périme
  en une heure, un jeton de rafraîchissement dure jusqu'à révocation. Ranger l'un ailleurs — le
  store, un fichier — serait le vrai relâchement. Sans cache, chaque `mail sync` ferait un
  aller-retour HTTPS avant de commencer ;
- **le jeton de rafraîchissement n'est remplacé que s'il en vient un nouveau.** Google n'en
  renvoie pas au rafraîchissement ; le remplacer par « rien » effacerait le seul secret qui
  vaille, et le compte redemanderait un consentement complet à la passe suivante.

Le nouveau jeton est **réécrit avant d'être rendu**. Si l'écriture échoue, l'appel échoue au lieu
de rendre le jeton quand même : un jeton rendu mais pas enregistré marche une fois, puis la
synchronisation suivante en redemande un — le compte fonctionne en apparence tout en consommant
un rafraîchissement par passe, et le quota du fournisseur finit par répondre à la place du
diagnostic.

#### Le branchement mot de passe / jeton est écrit une fois de chaque côté

Un jeton d'accès envoyé dans un `LOGIN` **part dans le champ mot de passe**, et un serveur
journalise les échecs de `LOGIN` avec l'identifiant : un jeton porteur finirait écrit sur le
disque de quelqu'un d'autre. Avec une seule `&str` pour les deux secrets, cette confusion est
une ligne à écrire de travers.

Elle ne compile plus : `mailsync::Credential` a deux variantes, `sync_account` **refuse** un
compte déclaré `oauth2` avec un mot de passe en main, et le branchement n'existe qu'à deux
endroits — `Credential::for_auth` côté protocole, `mailauth::session::secret_for` côté trousseau.
Les deux appelants, le CLI et le job de fond du démon, appellent les deux sans rebrancher.

`secret_for` prend une **étiquette** et pas un type, et ça mérite sa raison : prendre un type
demanderait à `mailauth` de dépendre de `mailcore`, donc de tirer SQLite, tantivy et l'analyseur
MIME dans l'arbre de dépendances du crate qui garde les secrets. L'étiquette vient de
`AuthKind::as_str`, une fonction totale sur une énumération déjà validée ; tout ce qui n'est pas
`password` ou `oauth2` est refusé, sans repli.

#### Le rafraîchissement a lieu dans le trousseau du démon

Quand la synchronisation est un job de fond, c'est le démon qui lit le secret, qui rafraîchit, et
qui réécrit. Un client MCP distant ne voit ni le jeton ni le rafraîchissement — la même propriété
que l'étape 6 de la phase 1, appliquée à OAuth2.

#### Deux fuites de la suite de tests, dont une que `Drop` ne pouvait pas attraper

**La première.** `xtask/tests/reseau.rs` signalait `mailauth/src/consent.rs` comme nouvelle
surface réseau. Le fichier n'ouvre aucune connexion : ce sont ses **tests** qui se connectent à
son propre écouteur pour le mettre à l'épreuve, et ils sont dans le même fichier sous
`#[cfg(test)]`. Le choix était de déclarer un fichier qui ne se connecte pas en production — ce
qui aurait rendu la déclaration sans valeur — ou d'arrêter de lire ce qui ne s'installe pas.
L'analyse coupe maintenant au module de tests, et un test vérifie que la coupe ne mange pas le
code réel.

**La seconde, et c'est la plus instructive.** Le 2026-09-03, 34 entrées de test traînaient dans
le Credential Manager de la machine, et des gardes `Drop` ont été ajoutés. Le 2026-09-04, il y en
avait 106 de plus — **alors que les gardes tournaient**.

Mesuré : 100 tests en parallèle laissent deux à six entrées derrière eux ; les mêmes 100 tests
avec `--test-threads=1` n'en laissent aucune. Le Credential Manager de Windows perd une
suppression émise pendant qu'un autre fil écrit. C'est aussi ce qui a fait échouer
`mailapi::token::two_daemons_keep_separate_tokens` une fois sur une exécution complète du
workspace, puis passer à la reprise — un test intermittent dont la cause n'était pas dans le code
testé.

Les épreuves de trousseau des deux crates se sérialisent maintenant sur un verrou de processus,
partagé entre les modules de test — un verrou par module laisserait `session` et `lib` se marcher
dessus. Trois exécutions consécutives ne laissent plus rien. Les 106 entrées ont été supprimées.

Ce n'est pas un défaut du code livré : rien dans mailcore n'écrit vingt secrets à la fois. Mais
une suite de tests qui salit la machine de son utilisateur est un bug de la suite de tests, et la
deuxième récidive montre qu'un `Drop` correct ne prouve pas un nettoyage effectif — il fallait
**compter les entrées avant et après**, ce qui n'avait pas été fait la première fois.

#### Ce qui reste à faire sur OAuth2

**Aucun compte Google n'a encore été synchronisé.** Tout ce qui précède est vérifié contre
`mailfake`, contre des réponses de jeton en dur, et sur le découpage. Le premier vrai
consentement demande un identifiant client, donc une console de fournisseur, donc l'utilisateur.

Et **le secret client n'est pas dans le dépôt et n'y sera pas** : mailcore sera publié, et un
identifiant client dans un dépôt public est un identifiant client révoqué. Chaque utilisateur
déclare le sien ; `mail account add --auth oauth2` sans `--client-id` affiche les étapes de la
console, pour Google comme pour Microsoft.

### Cinq comptes réels, et le critère 3 qui échouait en silence — 2026-09-08

La première journée avec un corpus réel branché : cinq comptes IMAP, dont quatre en OAuth2 chez
Google. C'est elle qui a montré ce que trois semaines de tests contre `mailfake` n'avaient pas pu
montrer.

#### Ce que le corpus contient maintenant

| | |
|---|---|
| Comptes | 5 — un mot de passe, quatre OAuth2 |
| Dossiers | 80 |
| Contenus distincts | 48 529 |
| Références | 105 508 |
| Octets RFC 5322 | 6,3 Gio |

**Le critère 1 est mesuré : 54 % des références sont des doublons.** Sur 105 508 emplacements de
messages, 48 529 contenus distincts. Un client qui range par dossier écrirait les 105 508.

Par compte, à la première moisson : `moi@perso2.invalid` 65 %, `compte-c` 58,7 %,
`compte-a` 57,6 %, `compte-b` 26,5 %. Les taux montent avec l'ordre de passage, parce que chaque
compte profite de ce que les précédents ont déjà déposé — c'est la dédup **entre** comptes, celle
qu'un compte isolé ne peut pas montrer.

#### Le critère 3 échouait, et sur ses deux moitiés

Mesuré sur les cinq comptes, sur un passage où rien n'avait changé :

| Compte | Références | Avant |
|---|---:|---:|
| `contact@perso.invalid` | 2 847 | 1,1 s |
| `moi@perso2.invalid` | 10 926 | 6,4 s |
| `compte-a@gmail.invalid` | 11 900 | 6,9 s |
| `compte-c@gmail.invalid` | 28 324 | 31,2 s |
| `compte-b@gmail.invalid` | 51 496 | **67,0 s** |

Seuil : 5 s. Un compte sur cinq passait, celui de 2 800 messages. Et le coût suivait le nombre de
**messages**, pas de dossiers — 29 dossiers et 11 000 messages coûtaient 6,4 s quand 10 dossiers
et 51 000 messages coûtaient 67 s.

La deuxième moitié du critère échouait aussi, et personne ne l'avait vue parce que le CLI
n'affiche pas ce compteur. Les rapports par dossier, eux, le disaient :

```
folder=44  fetched: 0, stored: 0, copies: 0, vanished: 0, reflagged: 5008
folder=51  fetched: 0, stored: 0, copies: 0, vanished: 0, reflagged: 5048
```

**11 900 lignes réécrites pour zéro changement** — exactement le nombre de références du compte.

#### Quatre causes, quatre correctifs

**1. `refresh_ref_flags` réécrivait tout.** La même erreur que celle corrigée cinq jours plus tôt
sur `record_copy` : un `UPDATE` inconditionnel compte une ligne inchangée comme une écriture. Une
clause `WHERE refs.flags IS NOT (…)` suffit. `IS NOT` et non `<>`, parce que la sous-requête peut
rendre `NULL` et que `NULL <> x` vaut faux.

La leçon avait été retenue trop étroitement la première fois : il fallait aller relire **les
autres écritures de la même passe**, pas seulement celle qui avait été signalée.

**2. Et elle relisait tout, même sans rien écrire.** La requête corrigée n'écrivait plus rien,
mais faisait toujours une sous-requête corrélée par référence — 9 671 lignes pour une seule
boîte, ~0,8 s par dossier. Les drapeaux d'une référence sont *dérivés* des copies : si aucune
copie n'a bougé dans le passage, la valeur calculée est forcément celle en place. La passe est
donc sautée quand rien n'a été écrit.

**3. Le balayage était en O(n²).** `sweep` faisait `present.contains(uid)` sur un `Vec` : 2,6
milliards de comparaisons sur le plus gros compte. Un `HashSet` a fait tomber ce compte de 67 s à
38,8 s — sans rien changer aux autres, ce qui a montré que le carré n'était pas le seul problème.

**4. Le balayage lui-même, une ligne par message et par passage.** Deux réponses, dans cet
ordre.

`QRESYNC` (RFC 7162) est la bonne : l'`EXAMINE` rend directement `VANISHED (EARLIER)` et les
drapeaux modifiés depuis un `MODSEQ`. Implémenté, avec l'analyse des ensembles d'UID, sept tests
d'intégration contre un `mailfake` qui sait désormais purger et rejouer ses purges — et
**inutile sur ce corpus** : ni Gmail ni le Dovecot de `mail.perso.invalid` ne l'annoncent. C'est
du code correct qui attend un serveur.

La réponse qui a payé est plus bête. `EXISTS` est déjà dans la réponse à l'`EXAMINE`, gratuit :
si le plan dit qu'aucun UID n'est apparu **et** que le serveur compte autant de messages que
nous, aucune purge n'a pu avoir lieu. Une purge ferait baisser `EXISTS`, et rien ne peut la
compenser puisque rien n'est arrivé. Le raisonnement tombe dès qu'un UID apparaît — un message
purgé et un message reçu laissent le compte inchangé — d'où la double condition.

#### Ce que ça donne

Cinq passes par compte, binaire de débogage, dispersion lue avant la médiane :

| Compte | Passes (ms) | Médiane | Avant |
|---|---|---:|---:|
| `contact@perso.invalid` | 1299 914 933 917 929 | **929** | 1 132 |
| `moi@perso2.invalid` | 2417 1358 1361 1256 1479 | **1 361** | 6 376 |
| `compte-a@gmail.invalid` | 8287 1856 1645 1719 1920 | **1 856** | 6 939 |
| `compte-b@gmail.invalid` | 9135 2335 9166 2310 2520 | **2 520** | 67 042 |
| `compte-c@gmail.invalid` | 9209 9288 9390 9965 9762 | **9 390** | 31 232 |

**Quatre comptes sur cinq passent les 5 s. Le cinquième non**, et il faut dire pourquoi plutôt
que de le contourner : ses 18 dossiers coûtent chacun un aller-retour `EXAMINE`, mesuré à ~0,46 s
sur cette connexion — **y compris les dossiers vides**, ce qui écarte toute cause locale. 18 ×
0,46 s, plus l'établissement de la connexion, font les 9 s. Le seul chemin en dessous est le
pipelinage des commandes, que le client ne sait pas faire : il est strictement question-réponse.

La bimodalité de `compte-b` et `compte-a` — 9 s ou 2 s selon les passes, sans rien changer — vient
de Gmail, pas de nous. C'est la raison de mesurer cinq fois et de lire la dispersion avant la
médiane.

Et la moitié « 0 octet écrit » est atteinte : `reflagged: 0` sur les huit dossiers d'un compte où
rien n'a changé, contre 11 900 avant.

#### La reprise après coupure

Gmail a coupé deux dossiers de `compte-b` en pleine moisson — `peer closed connection without
sending TLS close_notify`, sur le plus gros dossier, 18 300 messages. L'isolement par dossier a
tenu : les huit autres ont moissonné 19 450 messages.

Mais la reprise a coûté **25 028 corps retéléchargés pour 2 messages nouveaux**. Correct, la
dédup n'a rien stocké deux fois, mais la bande passante était repayée en entier.

La cause : un dossier interrompu n'écrit pas son état, donc la passe suivante le planifie en
`Full`, et `Full` commençait par `forget_folder` — il **effaçait les UID connus** avant de tout
redemander depuis 1. Cet effacement est juste pour la seule raison qui l'a motivé, un
`UIDVALIDITY` changé ; il ne l'est pas pour les deux autres cas qui produisent le même plan.

Trois changements :

- l'effacement est réservé à `UidvalidityChanged` et `NoUidvalidity` ;
- un **témoin de reprise** — l'`UIDVALIDITY` seul, sans `UIDNEXT` ni `MODSEQ` — est écrit *avant*
  le premier téléchargement, pour qu'un dossier coupé sache contre quelle numérotation ses copies
  partielles ont été enregistrées ;
- **aucun corps déjà possédé n'est redemandé**, quel que soit le plan. Une ligne de `remote_uids`
  n'existe que si le corps a été écrit dans la même transaction : elle est la preuve qu'on l'a.

Avec un piège que le premier jet aurait laissé passer : une moisson complète prend les drapeaux
*avec* les corps, donc les messages dont le corps n'est pas redemandé n'auraient vu passer aucun
changement de drapeau survenu pendant la coupure. Une passe de drapeaux est ajoutée dès qu'une
moisson complète en saute.

Le test qui verrouille tout ça utilise une **vraie coupure** — `Fault::ClosesAfter`, le serveur
raccroche au milieu d'un lot — et pas une simulation. Avec son contrôle inverse : un
`UIDVALIDITY` changé doit toujours tout effacer, sinon on lierait des messages à des numéros qui
désignent autre chose.

#### Deux bugs de mon propre code, trouvés par `mailfake`

**La liste `ENABLED`.** Je cherchais la sous-chaîne `"ENABLED QRESYNC"`. Un serveur qui active les
deux extensions d'un coup répond `* ENABLED CONDSTORE QRESYNC` — la RFC 7162 §3.2.3 l'y encourage
— et la sous-chaîne ne s'y trouve pas. Le client concluait « refusé » et repartait sur le
balayage, sans que rien ne le signale. `mailfake` l'a attrapé parce qu'il répond comme la RFC le
recommande plutôt que comme le client l'attendait.

**`ENABLE` après `EXAMINE`.** La négociation se faisait par dossier, après avoir sélectionné une
boîte. `ENABLE` n'est valide qu'en état authentifié (RFC 5161 §3.1) : Gmail le tolérait, ce qui
est exactement le genre de tolérance sur laquelle on ne peut pas compter. Elle a lieu une fois
par connexion, avant le premier `EXAMINE`.

#### Ce qui reste ouvert

**Le pipelinage.** C'est le seul chemin sous les 5 s pour un compte à beaucoup de dossiers sur
une connexion lente. Le client est strictement question-réponse.

**`QRESYNC` n'a jamais tourné contre un vrai serveur.** Il est couvert par sept tests
d'intégration, et par rien d'autre.

**Trois comptes du profil ne sont pas branchés** : Free et SFR — mots de passe applicatifs à
générer chez eux — et `etudiant@ecole.invalid` sur Office 365, qui exercerait le chemin OAuth2 de
Microsoft, écrit mais jamais éprouvé contre un vrai serveur lui non plus.

### Critères 2 et 7 mesurés, et l'échappatoire « ouvrir dans le navigateur » retirée — 2026-09-08

#### Critère 2 : la mémoire du démon

Le démon ouvert sur le store réel — cinq comptes, 80 dossiers, 48 529 messages — puis mis au
travail par l'API, pendant que sa RSS est échantillonnée depuis l'extérieur :

| | RSS crête |
|---|---:|
| Au repos, store et index ouverts | 17 Mo |
| Synchronisation incrémentale des cinq comptes (59 s, 80 dossiers) | **27 Mo** |
| Réindexation complète des 48 529 messages | **144 Mo** |

Seuil : 500 Mo. La réindexation est le job le plus gourmand du démon, et c'est attendu :
`all_for_indexing` rend un `Vec` de toutes les lignes, choix documenté et assumé — ~200 octets
la ligne, donc ~10 Mo de métadonnées pour ce corpus, le reste étant tantivy qui écrit.

**Ce qui n'est pas mesuré, et il faut le dire :** la crête d'une **moisson complète** sur un vrai
compte. Il faudrait retélécharger les 6,3 Gio, et le correctif de reprise du même jour rend ce
retéléchargement impossible à provoquer sans casser le store — aucun corps déjà possédé n'est
redemandé. C'est une bonne propriété du produit qui coûte une mesure. Le lot de cent messages
borne la mémoire par construction ; ça reste un argument de conception, pas un relevé.

#### Critère 7 : le profil Thunderbird

502 fichiers relevés avant, 502 après une synchronisation des cinq comptes et une ouverture de la
coquille. **0 mbox modifié, 0 autre fichier modifié.**

Avec son contrôle positif, sans lequel un « 0 » ne prouve rien : le comparateur nourri d'un relevé
dont une seule taille a été incrémentée d'un octet signale le mbox, l'affiche, et sort en code 1.

**Réserve** : Thunderbird ne tournait pas pendant la fenêtre. Le critère dit « il tourne toujours
à côté », et cette moitié-là n'est pas éprouvée — on a montré que *nous* n'écrivons pas, pas que
la cohabitation se passe bien.

#### « Ouvrir dans le navigateur » : retiré

La coquille avait un bouton qui écrivait le corps assaini dans `%TEMP%\mailcore-lecture\` et
l'ouvrait avec le navigateur du système. Sa défense, écrite dans le code, portait sur le bon
axe — la CSP de `mailhtml` était posée en `<meta>`, donc le document ne pouvait pas aller
chercher plus de ressources que la coquille — et passait à côté de trois choses.

**Le fichier reste.** Le nettoyage n'efface que les ouvertures précédentes : on ne peut pas
supprimer celui qu'un navigateur est en train de lire. Le corps du **dernier message lu** reste
donc en clair, indéfiniment, après la fermeture de l'application. Ce n'est pas théorique : au
moment de retirer le bouton, `%TEMP%\mailcore-lecture\` contenait encore un corps écrit deux
heures plus tôt. `docs/PRIVACY.md` §8 s'inquiète du cache de lecture ; un corps de message est
plus sensible que lui.

**Une CSP en `<meta>` n'est pas une CSP d'en-tête.** Plusieurs directives y sont ignorées, et
surtout le document part vers un moteur qu'on ne contrôle pas — alors que le critère 8 repose sur
deux ceintures dont l'assainisseur est la première.

**C'était le contournement de deux manques, pas une fonctionnalité.** Les manques sont réels : le
lecteur natif ne sait pas afficher les images intégrées, et les liens n'y sont pas cliquables. La
bonne réponse est de les combler — un clic qui montre l'URL et demande confirmation — pas de
livrer une porte de sortie qui pose le message en clair sur le disque. C'est écrit à l'endroit du
code qui invoquait cette échappatoire, pour que le manque reste visible.

La dépendance `opener` part avec le bouton.

### Le critère 3 passe, et la capacité qu'on demandait trop tôt — 2026-09-08

Le critère 3 échouait sur un compte, et la cause était écrite la veille : dix-huit dossiers, un
aller-retour `EXAMINE` chacun. La conclusion l'était aussi — « le seul chemin en dessous est le
pipelinage » — et elle était fausse. Il y a plus simple, et le trouver a demandé de mesurer au
lieu de se souvenir.

#### `mail account probe`, parce qu'on ne savait pas ce que les serveurs offraient

`LIST-STATUS` (RFC 5819) rend l'état de **toutes** les boîtes en une commande : `LIST "" "*"
RETURN (STATUS (MESSAGES UIDNEXT UIDVALIDITY HIGHESTMODSEQ))`. Un aller-retour au lieu de dix-huit,
à condition que le serveur l'annonce.

Rien dans le dépôt ne savait répondre à « est-ce que Gmail l'annonce ? », et répondre de mémoire
est exactement ce que le `CLAUDE.md` interdit. D'où une commande, en lecture seule, qui interroge
les comptes configurés et affiche ce qu'ils savent faire :

| Compte | `LIST-STATUS` | `EXAMINE` un par un | `LIST-STATUS` d'un coup |
|---|---|---:|---:|
| `contact@perso.invalid` | annoncé | 15 boîtes, 194 ms | **13 ms** |
| `moi@perso2.invalid` | annoncé | 29 boîtes, 741 ms | **39 ms** |
| `compte-a@gmail.invalid` | annoncé | 8 boîtes, 1 100 ms | **183 ms** |
| `compte-b@gmail.invalid` | annoncé | 10 boîtes, 1 457 ms | **139 ms** |
| `compte-c@gmail.invalid` | annoncé | 18 boîtes, 4 775 ms | **280 ms** |

Les cinq l'annoncent. Le compte qui échouait dépensait 4,8 s à poser dix-huit fois la même
question, à laquelle une seule réponse suffisait.

La commande reste : la question se reposera au compte suivant, et un fournisseur peut retirer une
extension aussi bien qu'en ajouter une.

#### Le raccourci ne décide rien que la moisson ne déciderait

C'est la seule chose qui compte, et elle tient en une phrase : les nombres du `STATUS` groupé
passent par **la même fonction `plan`** que ceux de l'`EXAMINE`, et le dossier n'est sauté que
là où elle répond `UpToDate`. Plus la condition sur le compte de messages, qui est mot pour mot
le raisonnement déjà tenu sur `EXISTS` la veille : `UpToDate` dit « rien n'est arrivé », il ne
dit pas « rien n'a disparu », et une purge ferait baisser le compte sans que rien puisse la
compenser.

Autrement dit : le raccourci ne connaît aucune règle de synchronisation que le chemin normal
ignore. Il obtient les mêmes quatre nombres plus tôt et moins cher.

**Ce qu'il ne fait pas non plus, et il faut le dire :** sans `CONDSTORE`, `plan` ne peut jamais
répondre `UpToDate` — les drapeaux d'un message déjà connu peuvent changer sans que `UIDNEXT` ni
le compte ne bougent, et rien ne le dirait. Le raccourci reste donc fermé sur un serveur sans
`CONDSTORE`. L'économie ne vaut pas un message lu qui s'afficherait non lu pour toujours.

#### Trois façons de ne rien obtenir, toutes sans conséquence

`LIST-STATUS` est envoyé **à part** du `LIST` de la découverte, alors que le fondre dedans aurait
économisé un aller-retour de plus. La raison est qu'un `LIST-STATUS` refusé emporterait la
découverte des dossiers avec lui, et un dossier non découvert est du courrier qui n'arrive pas.
Un aller-retour contre l'impossibilité de casser la découverte : le change est bon.

Les trois refus possibles rendent tous une carte vide, donc le chemin d'avant :

- le serveur n'annonce pas `LIST-STATUS` — `Config::without_list_status` ;
- il l'annonce et refuse la commande — `Fault::AdvertisesListStatusThenRefuses` ;
- il répond pour une partie des boîtes seulement, ce que la **RFC 5819 §2 l'autorise à faire** —
  `Fault::PartialListStatus`.

Les trois sont des pannes de `mailfake`, et chacune a son test. La troisième est la plus vicieuse :
un client qui lirait « pas de `STATUS` » comme « rien à faire » cesserait de moissonner ces
boîtes-là, en silence.

`HIGHESTMODSEQ` n'est demandé que si `CONDSTORE` a été **activé**, pas seulement annoncé. Un
serveur qui applique la RFC 7162 à la lettre répond `BAD` autrement — et ce `BAD` emporterait
l'état de toutes les autres boîtes, pas seulement celui de la donnée en trop. `mailfake` refuse
comme lui, et un test le vérifie.

#### Le vrai bug était ailleurs, et il durait depuis le début

Après le correctif, quatre comptes sur cinq sautaient tous leurs dossiers. Le cinquième —
`contact@perso.invalid`, le seul Dovecot — n'en sautait aucun, alors que la sonde venait de dire
qu'il annonçait `CONDSTORE`, `QRESYNC` **et** `LIST-STATUS`.

La sonde et la synchronisation ne voyaient pas le même serveur. La différence : la sonde
redemandait les capacités **après** authentification, la synchronisation non.

`tls::connect` redemande bien `CAPABILITY` après la poignée de main — la RFC 2595 l'exige, et le
commentaire à cet endroit dit même que c'est « ce qui fait découvrir `CONDSTORE` chez les serveurs
qui ne l'annoncent qu'authentifiés ». Sauf que c'est fait **avant le `LOGIN`**, et que la phrase
décrit précisément le cas qui n'était pas couvert. Dovecot n'annonce ni `CONDSTORE`, ni `QRESYNC`,
ni `LIST-STATUS` avant de savoir à qui il parle.

Et il les annonce, comme la RFC 3501 §6.2.3 le recommande, dans le `OK` étiqueté du `LOGIN` :
`a3 OK [CAPABILITY … CONDSTORE QRESYNC LIST-STATUS …] Logged in`. Précisément pour épargner un
aller-retour. `command_each` jetait cette ligne : elle n'absorbait les capacités que des réponses
**non étiquetées**, et rendait sur la réponse étiquetée avant de la lire.

Ce compte s'est donc synchronisé sans aucune extension, sur le chemin de repli, depuis le premier
jour — en croyant que le serveur n'avait rien de mieux à offrir. Il était le compte « sans
`CONDSTORE` » du 2026-09-04, celui qui a servi à valider le critère 10. Le repli reste exercé,
et il l'est maintenant par des tests plutôt que par un malentendu.

Le correctif est en deux morceaux, et le premier est gratuit :

1. **absorber les capacités de la réponse étiquetée**, `LOGIN` et `AUTHENTICATE` compris. Aucun
   aller-retour ajouté ;
2. `refresh_capabilities_if_silent`, appelé une fois par compte, qui redemande **seulement** si
   le serveur ne les a pas données de lui-même. Un aller-retour chez les serveurs muets, zéro
   chez les autres.

**`QRESYNC` tourne donc contre un vrai serveur pour la première fois.** Le journal du 2026-09-08
le listait comme un risque ouvert — « couvert par sept tests d'intégration, et par rien d'autre ».
Il l'était parce qu'on ne l'avait jamais proposé au seul serveur du corpus qui sait le servir.

C'est aussi la correction d'une phrase écrite le même jour : « ni Gmail ni le Dovecot de
`mail.perso.invalid` n'annoncent `QRESYNC` ». Le relevé n'était pas mauvais, la question était
posée au mauvais moment. Pour Gmail, elle reste vraie.

#### Ce que ça donne

Cinq passes par compte, binaire de débogage, dispersion lue avant la médiane, première passe à
part parce qu'elle porte la connexion et le jeton :

| Compte | Dossiers | Passes (ms) | Médiane | La veille |
|---|---:|---|---:|---:|
| `contact@perso.invalid` | 15 | 341 294 298 355 305 | **305** | 929 |
| `moi@perso2.invalid` | 29 | 385 379 414 386 1848 | **414** | 1 361 |
| `compte-a@gmail.invalid` | 8 | 934 871 860 1588 834 | **871** | 1 856 |
| `compte-b@gmail.invalid` | 10 | 1208 1061 1216 1343 971 | **1 216** | 2 520 |
| `compte-c@gmail.invalid` | 18 | 1360 2723 1301 1301 1330 | **1 330** | 9 390 |

**Les cinq comptes passent.** Le pire tient 1,33 s contre un seuil de 5 s, et les cinq **ensemble**
tiennent en 5,1 s là où un seul en coûtait 9,4 s. Les 80 dossiers du corpus sont reconnus à jour
sans un seul `EXAMINE`.

La bimodalité de Gmail n'a pas disparu — une passe à 2 723 ms et une à 1 301 ms sur le même compte,
sans que rien change de notre côté — mais elle porte maintenant sur un aller-retour au lieu de
dix-huit, donc elle ne décide plus du verdict. C'est la raison de mesurer cinq fois.

La moitié « 0 octet écrit » tient : un dossier sauté n'écrit rien, et `refresh_ref_flags` n'est
même pas atteint. Seule la date de vérification est réécrite, pour tous les dossiers sautés d'un
compte dans **une** transaction — `synced_at` doit dire quand on a vérifié, pas quand on a écrit
pour la dernière fois.

#### `xtask measure-sync`

Le relevé ci-dessus n'était fait à la main nulle part. Il l'est maintenant par une commande, qui
ne compile pas — la règle du `CLAUDE.md` sur la machine qui vient de compiler — et qui affiche les
passes brutes avant la médiane, la première à part.

#### Ce qui reste ouvert

**Le pipelinage n'est plus nécessaire au critère 3**, et n'est donc plus une dette. Il le
redeviendrait sur un compte à cent dossiers dont beaucoup changent à chaque passage : le raccourci
évite l'`EXAMINE` des dossiers **à jour**, pas celui des autres.

**Un `STATUS` qui mentirait** ferait sauter un dossier qui a du courrier. C'est le même degré de
confiance que celui déjà accordé à l'`EXAMINE` du même serveur — les deux nombres viennent de la
même source, et le chemin normal les croit tout autant. Ce n'est pas une régression, c'est une
dépendance qu'il vaut mieux avoir écrite.

**Le critère 6 sur un vrai serveur** et **le critère 4 sur les dix comptes** restent à refaire ;
`IDLE` reste la dernière étape de l'ordre de travail.

**Trois comptes du profil ne sont toujours pas branchés** : Free et SFR, et `etudiant@ecole.invalid`
sur Office 365.

### Le critère 6 sur les vrais comptes, et le `MODSEQ` que Gmail partage — 2026-09-08

#### Ce que le test sur `mailfake` ne pouvait pas montrer

Le critère 6 était mesuré depuis le 2026-09-03, et son test est bon : un mot de passe sentinelle
passe par un vrai `LOGIN`, puis est cherché dans tout ce que le processus a écrit. Trois choses
lui échappaient, et deux d'entre elles sont apparues **après** lui :

- **OAuth2 n'existait pas.** Les secrets qui valent aujourd'hui un compte sont le jeton de
  rafraîchissement et le secret client, écrits par `mailauth` le 2026-09-04. Aucun ne passe par
  `mailfake` ;
- **le store faisait trois messages.** Le vrai en fait 48 529, avec 3,5 Gio de blobs, un index
  SQLite de 33 Mo et des segments tantivy. Un secret peut se retrouver dans une page libérée de
  SQLite ou un fragment d'index, c'est-à-dire ailleurs que là où on penserait à regarder ;
- **le chiffrement était absent.** `mailfake` parle en clair ; ici le secret traverse `rustls`.

#### Le harnais n'écrit aucun secret nulle part

C'est la contrainte qui a décidé de sa forme. Un vérificateur de fuite qui commence par poser les
secrets dans un fichier de contrôle a créé la fuite qu'il cherche — et un fichier supprimé laisse
ses octets dans l'espace libre.

Le contrôle positif est donc **en mémoire** : chaque secret réel est noyé dans un tampon fabriqué,
à cheval sur deux blocs de lecture, et le chercheur doit l'y retrouver. C'est plus fort que le
contrôle du test d'origine, qui prouvait que le chercheur trouve *une* sentinelle ; celui-ci
prouve qu'il trouve **celle-là**, avec ses octets à elle, sa longueur à elle.

Un second contrôle reste nécessaire, sur disque et avec une sentinelle synthétique qui ne vaut
rien : il prouve que le parcours d'arborescence et la lecture des fichiers marchent, ce qu'un
contrôle en mémoire ne dit pas.

Aucun secret n'est affiché non plus. Le rapport les désigne par leur rôle et leur longueur —
« compte #2, jeton de rafraîchissement (103 octets) ».

#### La lecture est par blocs, et le recouvrement n'est pas un détail

Règle 4 du `CLAUDE.md`. Le test d'origine lisait chaque fichier d'un coup : acceptable sur trois
messages, impossible sur 3,5 Gio.

Les blocs font 1 Mio et se recouvrent de **la plus longue aiguille moins un octet**. Un octet de
moins et le secret le plus long peut se glisser dans la couture entre deux blocs — ce qui rendrait
« aucune fuite » alors qu'il y en a une, exactement le genre de zéro que ce critère refuse. Le cas
a son test unitaire : une aiguille posée à cheval sur la frontière du premier bloc.

#### Le relevé

| | |
|---|---|
| Secrets cherchés | **17** — 4 jetons de rafraîchissement, 4 secrets client, 5 jetons d'accès, 1 mot de passe applicatif |
| Dont apparus pendant la passe | 4 jetons d'accès rafraîchis |
| Fouillé | 3,5 Gio de store, les journaux `TRACE` d'une vraie sync des 5 comptes, `%TEMP%` |
| Trouvés | **0** |
| Durée | 1 min 38 |

Les quatre jetons d'accès rafraîchis comptent double : ce sont les plus récents, donc ceux qu'un
bug d'écriture aurait posés en dernier. Les chercher demande de relire le trousseau **après** la
synchronisation, et pas seulement avant — un harnais qui ne le ferait pas passerait à côté du seul
secret que la passe a réellement fabriqué.

Ce qui n'est pas couvert et qui est dit dans la sortie : le jeton d'API du démon, secret d'une
autre nature, dans son propre trousseau.

#### Gmail partage un seul `MODSEQ` entre toutes les boîtes d'un compte

Trouvé en passant, et ça change ce qu'on peut attendre du raccourci du critère 3.

Pendant l'audit, trois messages sont arrivés sur `compte-a`. Résultat : **aucun** de ses huit
dossiers n'a été évité — pas seulement l'`INBOX` et « Tous les messages », mais aussi la
corbeille, les brouillons, le spam et les suivis, qui n'avaient rien reçu. La passe suivante, sans
rien de neuf, en a évité huit sur huit.

`mail account probe --detail` donne la réponse en une ligne :

```
INBOX                       messages 5010  uidnext 6317  uidvalidity  1  modseq 847930
[Gmail]/Brouillons          messages    0  uidnext  107  uidvalidity  6  modseq 847930
[Gmail]/Spam                messages    0  uidnext  432  uidvalidity  3  modseq 847930
[Gmail]/Tous les messages   messages 5050  uidnext 6873  uidvalidity 11  modseq 847930
```

**Le même `HIGHESTMODSEQ` partout**, boîtes vides comprises. Le compteur de Gmail est celui du
compte, pas celui de la boîte. Un message qui arrive dans l'`INBOX` fait donc avancer le `MODSEQ`
du spam, et `plan` conclut — correctement — que la boîte a pu changer.

Ce que ça veut dire, sans l'enjoliver :

- **le critère 3 tient**, parce qu'il porte sur une passe où *rien* n'a changé, et là le compteur
  ne bouge pas : 80 dossiers sur 80 évités ;
- **une passe où quelque chose est arrivé coûte, chez Gmail, ce qu'elle coûtait avant le
  raccourci** — un `EXAMINE` et un `UID FETCH … CHANGEDSINCE` par dossier du compte touché ;
- le `HIGHESTMODSEQ` par boîte de Gmail ne porte donc **aucune** information par boîte. Seul le
  `CHANGEDSINCE` en porte, et lui demande d'avoir sélectionné la boîte.

**Le pipelinage revient donc par cette porte-là**, et pas par celle du critère 3 : il n'accélérerait
rien sur une passe vide — il n'y a plus qu'un aller-retour — mais il diviserait le coût d'une passe
où du courrier est arrivé. C'est la prochaine optimisation utile, et elle a maintenant une raison
mesurée plutôt qu'une intuition.

#### `mail account probe --detail`

La sonde affiche désormais, sur demande, les quatre nombres que `plan` compare à l'état local pour
chaque boîte. C'est ce qui a transformé « pourquoi ce dossier n'a-t-il pas été évité ? » d'une
supposition en une lecture. Un champ absent s'affiche `—` et non `0` : zéro veut dire « la boîte
est vide », et confondre les deux ferait chercher un bug là où il n'y a qu'un silence.

### `IDLE` : le courrier arrive sans qu'on demande — 2026-09-09

L'étape 9 de l'ordre de travail, et la dernière avant le critère 4. Le `docs/PHASE-2.md` la
range dans le confort — « une sync périodique correcte le remplace » — et c'est cette phrase
qui a décidé de toute l'architecture du module.

#### `IDLE` est du confort par-dessus un mécanisme, et le code le dit

Un veilleur **ne synchronise pas**. Il met un `Kind::Sync` en file et s'en va. Il ne tient
aucun verrou du store, ne touche pas à l'index, et a le droit de tomber : un compte dont le
veilleur est mort se synchronise encore à l'échéance.

Cette hiérarchie n'est pas une précaution de style, elle répond à trois questions d'un coup.
Que faire d'un serveur sans `IDLE` ? Rien de spécial. Que faire d'un `IDLE` qui échoue ?
Reconnecter plus tard. Que faire de dix annonces d'affilée ? Une seule synchronisation.

**L'échéance déclenche aussi**, et pas seulement les événements. Un événement d'`IDLE` peut
être perdu — une connexion coupée sans rien dire, un serveur qui n'annonce pas tout — et un
veilleur qui ne se fierait qu'aux annonces laisserait le compte périmé indéfiniment. Vingt-quatre
minutes : la RFC 2177 §3 demande de ressortir de l'`IDLE` au moins toutes les 29, au-delà de quoi
un serveur a le droit de raccrocher.

#### Un dossier, pas quatre-vingts

`IDLE` ne rapporte que la **boîte sélectionnée**. Surveiller les 80 dossiers du corpus
demanderait 80 connexions simultanées ; Gmail en accorde une quinzaine par compte. Le veilleur
surveille donc l'`INBOX`, et une arrivée y déclenche une synchronisation du compte entier.

Ce n'est pas un compromis regrettable : le courrier qui arrive arrive dans l'`INBOX`, et le
reste — un message classé par une règle du serveur, un brouillon écrit sur un téléphone — n'a
aucune raison d'être vu à la seconde. Et la découverte de ce matin sur le `MODSEQ` partagé de
Gmail rend la synchronisation du compte entier de toute façon obligatoire : chez lui, une
arrivée marque tous les dossiers comme ayant pu changer.

#### Le silence est le cas normal, et c'est lui qui a demandé du travail

`IDLE` est la seule commande d'IMAP où le client **n'attend rien de précis**. Une lecture sans
borne s'y bloque pour toujours, et un démon qu'on ne peut pas arrêter n'est pas arrêtable.

L'attente est donc découpée en tranches de cinq secondes, et c'est un trait — `Timed` — qui rend
la borne exprimable sur un flux générique, plutôt qu'un `TcpStream` en dur dans le client. La
tranche est une **lecture bornée**, pas un sommeil : l'événement remonte dès qu'il arrive, le
découpage ne sert qu'à regarder le drapeau d'arrêt.

Deux détails qui auraient mordu :

**Une expiration au milieu d'une ligne n'est pas un silence.** Les octets déjà pris au socket
sont perdus, donc le dialogue n'est plus synchronisé et continuer lirait la fin d'une ligne comme
le début d'une autre. Les deux cas se distinguent par un seul fait : le tampon est-il vide ?
Vide, rien n'a été consommé et reprendre est sûr ; non vide, c'est une erreur franche et la
connexion doit être jetée. Une expiration au milieu d'un enregistrement **TLS**, elle, n'est pas
un problème : `rustls` garde le fragment et reprend, il est écrit pour les sockets non bloquants.

**`WouldBlock` et `TimedOut` comptent tous les deux.** Un `SO_RCVTIMEO` dépassé rend l'un sur
Unix et l'autre sur Windows. N'en tester qu'un ferait marcher `IDLE` sur une plateforme et le
ferait tomber en panne franche sur l'autre.

#### Un test qui ne pouvait pas échouer, trouvé en essayant de le faire échouer

La tranche d'attente emprunte le délai de lecture du socket et doit le remettre : sans ça, la
moisson qui suit un `IDLE` expirerait au bout de quelques dizaines de millisecondes au lieu des
120 secondes ordinaires. Bug invisible en test court, fatal sur un vrai serveur.

Le test écrit pour verrouiller ça relisait le délai sur un `try_clone` du socket. **Il passait
aussi bien avec le correctif que sans** : sur Windows, un descripteur dupliqué ne rend pas la
même valeur. Le contrôle négatif — retirer la remise en place et voir si le test tombe — est ce
qui l'a montré ; il faut relire l'option sur le descripteur d'origine, que `into_stream` rend.
Avec ça, le test échoue bien : `Some(60ms)` au lieu de `Some(7s)`.

C'est la même leçon que le critère 8 de la phase 1, sous une autre forme : un zéro sans contrôle
ne prouve rien, et un test vert sans contrôle négatif non plus.

#### `mailfake` sait maintenant idler, et mal

Trois comportements, trois tests :

- il honore l'`IDLE` et annonce ce qu'on lui a demandé d'annoncer — `announcing_on_idle` ;
- il l'annonce puis le refuse — [`Fault::AdvertisesIdleThenRefuses`], et le client doit retomber
  sur l'échéance, pas éteindre le compte ;
- il l'accepte et **ne dit plus rien** — [`Fault::IdleStaysSilent`], qui est le cas normal d'une
  boîte tranquille et celui qui exerce les tranches.

Plus deux refus qu'il oppose au client, parce qu'un serveur de test doit être plus sévère qu'un
vrai : un `IDLE` sans boîte sélectionnée, et une sortie autre que `DONE` nu.

Sept tests d'intégration en tout, dont celui qui compte : après trois tranches vides, un `DONE`,
puis un `EXAMINE` qui doit encore répondre juste — la preuve que les tranches n'ont rien consommé
de travers.

#### Le relevé sur les vrais serveurs

| | |
|---|---|
| Comptes veillés | **5 sur 5**, `idle=true` partout |
| Temps de mise en place | ~0,6 s pour quatre comptes, 5 s pour le cinquième |
| Incidents | aucun — ni avertissement, ni reconnexion différée |
| Arrêt | `veille arrêtée` en moins d'une tranche, cinq fils joints |

Les cinq serveurs du corpus annoncent `IDLE`, ce que la sonde du matin disait déjà. Le veilleur
est monté par `Service::open`, donc il vaut pour toutes les surfaces du démon, et il est arrêté
**explicitement** avant que le processus rende la main : un fil tué au milieu d'un `IDLE` laisse
la connexion se faire réinitialiser, et certains fournisseurs limitent un compte qui accumule les
déconnexions sales.

#### Ce qui n'est pas éprouvé, et il faut le dire

**Une arrivée réelle déclenchant une synchronisation réelle n'a pas été observée.** La moitié
« le veilleur se pose, négocie, tient et s'arrête » l'est, sur les cinq comptes. La moitié « un
message arrive, le job part » ne l'est que contre `mailfake` — il faudrait qu'un message arrive
pendant une fenêtre de mesure, et la phase 2 n'a pas d'envoi pour s'en fabriquer un.

**Le chemin sans `IDLE` n'a pas de serveur réel pour l'exercer.** Les cinq l'annoncent. Il est
couvert par `Config::without_idle`, et par rien d'autre.

**La reconnexion à attente croissante n'a jamais servi.** Aucun des cinq comptes n'a échoué
pendant les essais. Trente secondes qui doublent jusqu'à un quart d'heure : c'est un choix
raisonné, pas un chiffre mesuré.

### Critère 4 sur les vrais serveurs, et les deux défauts que le banc a trouvés — 2026-09-09

L'étape 10, la dernière de l'ordre de travail. Le critère 4 était mesuré depuis le 2026-09-03
contre `mailfake` — 0,99 ms de p95 — et il restait à le refaire sur le réel.

#### Le store réel ne peut pas servir, et le store jetable si

Le critère dit « pendant une sync **complète** ». Sur le store réel, une moisson complète est
devenue impossible à provoquer : le correctif de reprise du 2026-09-08 ne redemande aucun corps
déjà possédé. Bonne propriété du produit, mesure perdue — la même réserve que pour le critère 2.

Un store **jetable** la rend possible sans rien casser. Les comptes y sont déclarés avec le même
hôte et le même identifiant que dans le store réel, donc le trousseau retrouve leurs secrets, et
les serveurs sont lus comme d'habitude. Le store réel n'est ouvert que pour y lire la liste des
comptes.

Ce que ça mesure de plus que `mailfake` : `rustls` et la vérification de certificat, OAuth2 pour
quatre comptes sur cinq, et des corps réels — dont les pièces jointes, qui sont ce qui fait
vraiment travailler l'écriture de blobs.

La moisson est **annulée dès la coquille refermée**, par la `Progress` du démon. Le coût en bande
passante est donc borné par la durée du banc, pas par la taille des comptes : ~1 500 messages par
exécution, et non les 105 508 du corpus.

#### Un compte ne suffit pas, et c'est le contrôle du banc qui l'a dit

Premier relevé, sur `compte-c` seul : passé, mais **2 changements** du store reçus pendant les
600 images, contre 58 sur `mailfake`. Deuxième relevé, sur `contact@perso.invalid` — la liaison la
plus rapide du corpus : **0 changement**, et le banc a refusé de rendre un chiffre.

Il avait raison, et c'est exactement à ça qu'il sert : sans recouvrement, le relevé mesure le
critère 2 sous un autre nom. La raison est arithmétique — la fenêtre de défilement dure **~3,3 s**
(600 images à ~5,5 ms), et une moisson sur une liaison à 260 ms d'aller-retour n'y valide que deux
ou trois lots de cent.

D'où la forme finale : **tous les comptes actifs moissonnent en parallèle**, ce qui est d'ailleurs
la lettre du critère — « la coquille qui défile pendant que dix comptes se synchronisent ». Cinq
écrivains concurrents sur la même base, cinq connexions TLS, et 3 à 5 changements reçus par
fenêtre.

#### Le relevé

| | `mailfake`, 2026-09-03 | Vrais serveurs, 2026-09-09 |
|---|---:|---:|
| Travail par image, p95 | 0,99 ms | **2,50 ms** |
| Pire image | 2,27 ms | **5,10 ms** |
| Changements pendant le défilement | 58 | 3 à 5 |
| Écrivains concurrents | 1 | **5** |

Trois fois moins bon, et six fois sous le budget de 16,7 ms. La différence est attendue : cinq
écrivains au lieu d'un, et un store qui reçoit de vrais corps compressés au lieu de messages de
trois lignes.

**Le critère 4 ferme un trou reconnu de la phase 1** : son critère 2 n'avait été mesuré que sur une
liste au repos, jamais pendant qu'une tâche de fond écrivait. La règle 3 du `CLAUDE.md` — *l'UI ne
bloque jamais sur le réseau ou sur un import* — est maintenant éprouvée sur les deux à la fois.

#### Premier défaut : cent écritures de fichiers dans une transaction

Le banc a fait échouer une moisson sur trois : `database is locked`, code SQLite 5, après les 5 s
de `busy_timeout`.

`fetch_bodies` faisait déjà attention au bon endroit — le `UID FETCH` a lieu **avant** d'ouvrir la
transaction, avec un commentaire qui dit pourquoi. Mais il écrivait les blobs **dedans** : cent
créations de fichiers compressés, quelques mégaoctets, et sur Windows autant de passages
d'antivirus, le verrou d'écriture de l'index tenu pendant tout ça.

Le correctif est de les écrire avant. Et il ne change **rien** aux garanties, parce qu'une
écriture de fichier n'a jamais été transactionnelle : un `ROLLBACK` ne l'aurait pas défaite. Ce
que la transaction protège est l'ensemble des lignes, et il reste entier. Ce qu'on risque en plus
est un blob écrit dont les lignes ne le sont pas, sur une coupure entre les deux — c'est-à-dire
exactement le cas « blob non référencé » que l'adressage par contenu rend inoffensif, que le
passage suivant retrouve par son empreinte, et que `mail doctor` sait déjà compter.

Mesuré : les échecs sont passés de deux exécutions sur trois à une, puis à zéro sur trois. **C'est
intermittent, donc zéro ne prouve pas la disparition** — le verrou tombe au moment où les cinq
moissons commitent leur dernier lot ensemble, à l'annulation. Le débit, lui, a monté franchement :
~1 400 messages écrits par exécution avant, ~1 570 après.

**Et ce n'est pas un chemin de production.** Le démon synchronise les comptes en série — un seul
fil de travail, une file — et `mail sync` boucle sur les comptes. Rien dans le produit ne fait
écrire cinq moissons à la fois ; c'est le banc qui crée ce mode, exprès. Le correctif profite
quand même au cas séquentiel : le verrou est tenu moins longtemps, donc la coquille et le démon se
gênent moins.

#### Deuxième défaut : un verrou classé comme définitif

`Error::retryable()` rangeait **toutes** les erreurs de store parmi les refus définitifs. Or
`database is locked` veut dire « quelqu'un d'autre écrivait, et il a fini depuis » : c'est
l'erreur réessayable par excellence. Un compte était donc abandonné pour une contention qui
n'existait plus une seconde plus tard.

Le correctif regarde le **code** d'erreur de SQLite, pas son message — le message est du texte
anglais que la prochaine version peut reformuler. `DatabaseBusy` et `DatabaseLocked` sont
réessayables ; une contrainte violée, une base corrompue, un fichier en lecture seule ne le sont
pas, et les réessayer en boucle cacherait le défaut.

Au passage : `mailcore::Error::Sqlite` s'affichait « erreur SQLite », sans sa cause. Un verrou et
une corruption s'écrivaient donc pareil dans un journal. Le projet logue avec `%source`, qui ne
prend que le `Display` — la cause était perdue **partout**. Elle y est maintenant.

#### La veille ne coûte plus rien au démarrage

Trouvé en relisant le code de la veille écrite le matin : `Watchers::start` lisait les comptes de
façon synchrone, donc rouvrait le store — pragmas et contrôle de migration compris — **sur le
chemin de démarrage de la coquille**. Le critère 1 lui donne 400 ms pour devenir utilisable, et un
travail qui n'a aucune raison d'être fait avant la première image ne doit pas l'être.

La lecture des comptes et le démarrage des veilleurs vivent donc dans un fil de supervision, qui
possède les fils de compte et les joint à l'arrêt. `Watchers::start` rend la main tout de suite.

**Critère 1 revérifié** sur le store réel de 48 530 messages, cinq exécutions, binaire release,
machine au repos : **222,8 ms** à froid, 212,9 ms de médiane à chaud, fenêtre et contexte à
174,8 ms. Budget 400 ms — passé, et la veille n'y apparaît pas.

#### `MAILCORE_UI_POSITION`

Un banc ouvre la coquille une fois par exécution, au-dessus de ce que l'utilisateur est en train
de faire. La variable la déporte, en coordonnées du bureau virtuel.

Un détail qui n'en est pas un : les coordonnées peuvent être **négatives**. L'écran secondaire de
la machine de référence est à `X=-1920` — à gauche du principal — et un parseur qui refuserait le
signe rendrait la moitié des configurations à deux écrans inatteignable. Le test le dit
explicitement. Un `NaN` ou un infini, en revanche, sont refusés : ils poseraient la fenêtre nulle
part, ce qui sur certains gestionnaires veut dire invisible, et c'est pire que mal placée.

La variable n'est pas transmise à la main par le harnais : l'environnement d'un processus fils est
hérité tel quel, faute d'`env_clear`. La poser deux fois serait une deuxième vérité à maintenir.

#### Ce qui reste ouvert

**Le recouvrement du banc se compte en unités.** 3 à 5 changements pendant 600 images, contre 58
sur `mailfake`. La fenêtre de défilement dure ~3,3 s et une moisson réelle ne commite que quelques
lots dedans. Le chiffre est honnête, mais il est moins sévère que celui de `mailfake` sur ce
point-là précisément.

**La contention à cinq écrivains n'est pas éliminée.** Réduite, réessayable, et hors du chemin de
production — pas éliminée. Le jour où une synchronisation concurrente deviendrait un mode du
produit, il faudra la reprendre.

**Les dix comptes sont cinq.** Free, SFR et l'Office 365 de `etudiant@ecole.invalid` ne sont
toujours pas branchés, et le critère parle de dix.

### Où en est la phase 2 — 2026-09-09

**Les dix critères sont mesurés, et les dix étapes de l'ordre de travail sont faites.** Aucun
chiffre du tableau ne repose sur une estimation. Ce qui suit est ce qu'il reste, nommé plutôt que
laissé implicite — la phase 1 s'est close de cette façon-là, et c'est la seule qui permette de
relire un verdict six mois plus tard.

#### Les dix critères, en une ligne chacun

| # | Verdict | Sur quoi |
|---|---|---|
| 1 | passé | 54 % de doublons, cinq comptes réels |
| 2 | passé | 27 Mo en sync, 96 Mio en moisson complète, 144 Mo en réindexation, seuil 500 |
| 3 | passé | pire compte à 1,33 s, seuil 5 s, `LIST-STATUS` |
| 4 | passé | 2,50 ms de p95, cinq moissons réelles concurrentes, budget 16,7 ms |
| 5 | passé | 0 connexion par notre code, la vérification de révocation nommée |
| 6 | passé | 17 secrets réels, 0 trouvé dans 3,5 Gio, journaux `TRACE`, `%TEMP%` |
| 7 | passé | 502 fichiers du profil Thunderbird, 0 modifié |
| 8 | subi puis corrigé | coupure réelle de Gmail, reprise sans perte ni doublon |
| 9 | passé | 2 000 messages renumérotés, 0 blob dupliqué |
| 10 | passé | repli sans `CONDSTORE` exercé en test |

#### Ce qui n'est pas mesuré, et pourquoi

Trois réserves, toutes écrites dans le tableau, toutes de la même famille — une mesure que le
produit rend impossible à prendre :

- ~~**la crête mémoire d'une moisson complète sur un vrai compte**~~ (critère 2) :
  **levée le 2026-09-09**, dans un store jetable. Et le « lot de cent borne la mémoire par
  construction » qui la justifiait était faux — voir le journal. **Un compte entier a été moissonné**
  le même jour, deux fois, sur deux comptes : 169,6 Mio et 96,7 Mio. Reste hors de portée le
  corpus **entier**, soit 6,3 Gio pour confirmer une addition ;
- **la cohabitation avec Thunderbird ouvert** (critère 7) : on a montré que *nous* n'écrivons
  pas dans son profil, pas que la cohabitation se passe bien. Il ne tournait pas pendant la
  fenêtre de mesure ;
- **le recouvrement du critère 4 se compte en unités** : 3 à 5 changements du store pendant les
  600 images, contre 58 sur `mailfake`. La fenêtre de défilement dure ~3,3 s, et une moisson
  réelle n'y commite que quelques lots.

Et deux chemins écrits, testés, jamais exercés contre un vrai serveur :

- ~~**`IDLE` sans arrivée réelle**~~ : **la chaîne entière est observée** depuis le 2026-09-09 à
  14:54:40 — un `* 19181 EXISTS` de Gmail, reconnu comme un changement, une synchronisation en
  file, un store à jour. Avec, sur la même fenêtre : 18 politesses ignorées, cinq échéances, zéro
  verrou, et la coquille embarquée qui tournait en même temps que le démon ;
- **le chemin sans `LIST-STATUS` et le chemin sans `IDLE`** : les cinq serveurs du corpus les
  annoncent tous les deux. Ils sont couverts par `mailfake`, et par rien d'autre.

#### Ce qui appartient à l'utilisateur

**Le corpus fait cinq comptes sur dix**, et les trois qui manquent demandent une action que le
code ne peut pas faire :

- **Free** : un mot de passe applicatif dans l'espace abonné, puis
  `mail account add --host imap.free.fr --username <adresse> --auth password` ;
- **SFR** : le même, `--host imap.sfr.fr` ;
- **`etudiant@ecole.invalid` sur Office 365** : une inscription d'application sur
  `entra.microsoft.com` — client public, redirection `http://127.0.0.1`, autorisations
  `IMAP.AccessAsUser.All` et `offline_access` — puis
  `mail account add --host outlook.office365.com --auth oauth2 --client-id <id>`. La commande
  affiche ces étapes elle-même.

  Le chemin reste **non éprouvé contre un vrai serveur**, et c'est le seul des trois qui porte un
  risque de code. Ce qui a été fait le 2026-09-09 est tout ce qui pouvait l'être sans le compte :
  la rotation du jeton de rafraîchissement — la seule différence avec Google qui casse en
  silence — est testée, et `--redirect-port` retire le seul blocage structurel que le
  consentement pouvait avoir. Voir le journal.

Les critères 1, 3 et 4 parlent de dix comptes. Ils sont mesurés sur cinq, et les brancher est ce
qui rendrait le tableau littéral.

#### Ce que la phase a appris, en trois lignes

**Une question posée au mauvais moment vaut une réponse fausse.** Les capacités demandées avant le
`LOGIN` ont fait synchroniser un compte sans aucune extension pendant des semaines, et ont fait
écrire dans ce journal une phrase fausse sur `QRESYNC`.

**Un test vert sans contrôle négatif ne prouve rien** — la même leçon que « un zéro sans contrôle
positif ne prouve rien », de l'autre côté. Deux tests écrits cette semaine passaient sans rien
vérifier, et les deux ont été trouvés en essayant délibérément de les faire échouer.

**Un banc de mesure est un test comme un autre.** Celui du critère 4 a trouvé deux défauts
réels — cent écritures de fichiers dans une transaction, et un verrou classé comme définitif — qu'aucun
test unitaire ne cherchait.

### La boucle de veille, enfin couverte — 2026-09-09

Complément à l'entrée `IDLE` de ce matin. Elle disait : « la moitié *un message arrive, le job
part* ne l'est que contre `mailfake` ». C'était optimiste — cette moitié n'était couverte **nulle
part**. Les tests du module portaient sur les fonctions pures autour de la boucle : la garde
anti-doublon, le sommeil interruptible. La boucle elle-même, celle qui décide *quand*
synchroniser, n'avait aucun test.

#### Une couture, la même que celle de la moisson

`session` chiffre, s'authentifie, puis appelle `watch_over` sur une connexion déjà authentifiée.
C'est mot pour mot la convention de `mailsync::sync_account` / `sync_account_over`, et pour la
même raison : `mailsync::connect` refuse un serveur en clair — à juste titre — donc une fonction
qui établit elle-même sa connexion est intestable contre `mailfake`.

Quatre tests d'intégration, avec un vrai registre de jobs :

- **une arrivée annoncée met une synchronisation en file**, et elle vise le bon compte ;
- **un serveur qui se tait ne déclenche rien** — un veilleur qui synchroniserait par acquit de
  conscience à chaque tranche de cinq secondes serait pire que pas de veilleur ;
- **un arrêt demandé sort en moins de deux tranches**, et c'est une sortie normale, pas une
  erreur ;
- **le chemin sans `IDLE` ne déclenche rien avant l'échéance** et reste arrêtable pendant
  l'attente.

Le premier a été vérifié par son contrôle négatif : `trigger` neutralisé, le test tombe sur
« aucune synchronisation mise en file après une arrivée annoncée ». C'est la deuxième fois
aujourd'hui que ce réflexe sert.

#### Un plancher entre deux `IDLE`

Trouvé en écrivant le test : `mailfake` annonce son événement **dès** la mise en attente, donc la
boucle tournait aussi vite que le bouclage le permettait — armer, lire, sortir, recommencer.

En production l'annonce est rare, donc le cas ne se présente pas. Mais un serveur qui compterait
mal, ou qui serait bavard, ferait exactement ça sur un vrai réseau, et le fournisseur y verrait un
client qui le martèle. Une seconde de plancher entre deux mises en attente ferme la porte, et ne
coûte rien quand il ne se passe rien : elle ne s'applique que si le tour a duré moins d'une
seconde.

C'est le même raisonnement que l'attente croissante à la reconnexion — se protéger de soi-même
avant que le fournisseur ne s'en charge.

#### Ce qui reste vrai de la réserve du matin

**Une arrivée réelle sur un vrai serveur n'a toujours pas été observée.** Ce qui est couvert
maintenant, c'est le mécanisme : annonce → job en file, et le contraire. Ce qui manque est le
dernier maillon — qu'un vrai Gmail annonce bien un `* n EXISTS` quand un message arrive. Il le
fait, c'est ce que `IDLE` est ; mais on ne l'a pas vu de nos yeux, et la phase 2 n'a pas d'envoi
pour se fabriquer l'occasion.

### Ce que la veille change pour les trois surfaces, et qu'il faut dire — 2026-09-09

La veille est montée par `Service::open`. Trois appelants l'utilisent, et ils ne sont pas dans la
même situation. Écrit ici parce qu'un effet de bord de cette taille ne doit pas se découvrir sur
une facture de données ou dans les journaux d'un fournisseur.

| Qui ouvre `Service::open` | Ce que ça implique maintenant |
|---|---|
| `maild` | Cinq connexions `IDLE` tenues tant que le démon tourne. C'est le comportement voulu : c'est lui le service. |
| `mail-shell` en mode embarqué | **Idem.** Ouvrir la coquille ouvre cinq connexions IMAP et les garde. |
| `mail-ui` en mode embarqué | **Idem.** |
| Une coquille en mode **distant** | Rien : elle ne monte pas de service, c'est le démon qui veille. |

#### Ce que ça coûte, et ce que ça donne

Ça donne ce qu'on voulait : le courrier arrive sans qu'on demande, y compris quand l'utilisateur
n'a lancé qu'une application de bureau, sans démon.

Ça coûte une connexion par compte, tenue ouverte. Sur une liaison mesurée ou sur batterie, ce
n'est pas neutre, et **aucune interface ne permet aujourd'hui de l'éteindre**. Le journal le dit
au démarrage — `veille démarrée comptes=5` — et c'est tout ce qu'il y a. Un réglage viendra avec
la surface qui l'expose ; en inventer un que rien n'affiche serait un réglage que personne ne
trouve.

#### La cohabitation démon + coquille embarquée devient un cas d'écriture concurrente

Avant aujourd'hui, une coquille embarquée n'écrivait dans le store que si l'utilisateur
déclenchait quelque chose. Maintenant, sa veille peut mettre une synchronisation en file toute
seule — et si un démon tourne en même temps sur le même store, **deux processus synchronisent en
parallèle**.

C'est exactement la contention que le banc du critère 4 a trouvée le même jour, à cinq écrivains.
À deux, les 5 s de `busy_timeout` couvrent largement, et le correctif de retryabilité fait que
l'échec éventuel se rattrape au tour suivant plutôt que d'abandonner le compte. Ce n'est donc pas
un défaut ouvert — mais c'est le moment de noter que ce correctif est **porteur**, et pas
théorique comme il en avait l'air ce matin.

`docs/ARCHITECTURE.md` recommande de toute façon un démon et des coquilles distantes. La
cohabitation d'un démon et d'une coquille embarquée sur le même store n'est pas le déploiement de
référence, et elle ne l'était pas avant non plus.

### Une affirmation vérifiée, et le trou qu'elle cachait — 2026-09-09

En écrivant le correctif des blobs hors transaction, j'avais justifié le risque résiduel ainsi :
« c'est le cas *blob non référencé*, et `mail doctor` sait déjà les compter ».

**C'était faux.** `mail doctor` comptait les *références* orphelines — une référence vers un
message absent — et les *blobs manquants* — un message dont le contenu a disparu. Le troisième
cas, un contenu que plus aucun message ne réclame, n'était rapporté nulle part.

Relire une affirmation pour la vérifier plutôt que pour la relire est ce qui l'a trouvée. La
justification tenait quand même — un `ROLLBACK` n'efface pas un fichier, donc le trou existait
avant le correctif — mais elle s'appuyait sur un filet qui n'existait pas.

#### `Store::orphan_blobs`, le symétrique de `missing_blobs`

Les deux sont possibles parce que les blobs vivent **hors** de SQLite, donc hors de ses
transactions. `missing_blobs` répond à « un message dont le contenu a disparu » ; celui-ci répond
à l'inverse.

Le recensement ne lit **aucun contenu** : un blob est nommé par son empreinte, c'est ce que veut
dire « adressé par contenu ». Il ne coûte donc que la lecture de noms de fichiers, et pas 3,5 Gio
de zstd. Par rappel plutôt qu'en rendant une liste — la règle 4 — même si les 48 532 empreintes du
corpus ne pèseraient qu'un mégaoctet et demi.

Un fichier dont le nom n'est pas une empreinte valide est **ignoré**, pas fatal : refuser tout
l'inventaire à cause d'un fichier qu'un outil extérieur aurait laissé traîner rendrait
l'inventaire inutile le jour où il y en a un. Ça a son test.

**Ça compte, ça n'efface rien.** Un blob non référencé n'est pas du courrier perdu, c'est de la
place, et le supprimer demande de savoir qu'aucun import en cours ne va le réclamer dans la
seconde. Le ramassage sera une action explicite, comme le nettoyage des dossiers disparus.

#### Le relevé

Sur le store réel : **0 blob non référencé** pour 48 532 contenus, et le parcours des noms tient
dans les ~4 s de l'ensemble du diagnostic. Le zéro vaut quelque chose parce que le compteur a son
contrôle positif en test — un blob écrit sans ses lignes est bien compté pour un.

C'est aussi une petite bonne nouvelle sur les coupures subies jusqu'ici : les lots interrompus du
2026-09-08 n'ont rien laissé derrière eux, ou ce qu'ils ont laissé a été réclamé depuis par le
même contenu.

### `* OK Still here` : le défaut qu'aucun test ne pouvait trouver — 2026-09-09

Le démon a été laissé tourner sur les cinq comptes réels pour voir si une arrivée finirait par
tomber pendant la fenêtre — la réserve nommée le matin. Ce qu'il a montré est autre chose.

#### Ce que le journal disait

```
09:31:05  DEBUG IDLE a parlé evenement=* OK Still here
09:31:06  INFO  arrivée annoncée : synchronisation compte=1
09:31:06  INFO  compte synchronisé account=1 folders=15 skipped=12  recus=46 nouveaux=2
09:33:06  DEBUG IDLE a parlé evenement=* OK Still here
09:33:06  INFO  arrivée annoncée : synchronisation compte=1
09:33:06  INFO  compte synchronisé account=1 folders=15 skipped=15  recus=0  nouveaux=0
09:35:06  … la même chose
09:37:06  … la même chose
09:39:06  … la même chose
```

**Toutes les deux minutes, à la seconde.** Dovecot envoie `* OK Still here` pendant un `IDLE`
pour dire qu'il est toujours là — c'est de la courtoisie, pas un événement. `idle_wait` rendait
**toute** réponse non étiquetée comme un événement, donc le veilleur y voyait une arrivée et
resynchronisait le compte. Quinze dossiers interrogés pour ne rien trouver, en boucle, pour
toujours.

La première passe, elle, a bien servi : 46 corps reçus, 2 nouveaux — le store avait pris du
retard depuis la dernière synchronisation manuelle. C'est ce qui rend le défaut sournois : la
sortie du premier tour est exactement ce qu'on espérait voir.

#### Pourquoi aucun test ne l'attrapait

`mailfake` n'envoie que ce qu'on lui demande d'envoyer, et on lui demandait `* 4 EXISTS`. Les sept
tests d'intégration de l'`IDLE` passaient tous, y compris celui qui vérifie qu'un serveur muet ne
réveille personne. Aucun ne décrivait un serveur **poli**, parce qu'il ne m'était pas venu à
l'esprit qu'un serveur parle sans rien dire.

C'est la limite exacte d'un serveur de test qu'on écrit soi-même : il ne connaît que les pannes
qu'on a imaginées. Le `docs/PHASE-2.md` disait « Dovecot reste utilisé, en dernier ressort, pour
vérifier qu'on parle bien à un vrai serveur » — la phrase était juste, et c'est le vrai serveur
qui a payé pour la vérifier.

#### Le correctif est une liste blanche, et c'est un choix

`announces_change` ne reconnaît que cinq formes : `* n EXISTS`, `* n RECENT`, `* n EXPUNGE`,
`* n FETCH (…)`, `* VANISHED …`. Tout le reste est traité comme « rien de neuf ».

À première vue c'est le mauvais sens — un changement qu'on ne reconnaîtrait pas serait manqué. Et
c'est pourtant le bon, pour une raison précise : **l'échéance de vingt-quatre minutes est le
filet**. Une synchronisation part de toute façon, donc une ligne mal classée coûte au pire un
retard borné. La liste noire a le défaut inverse, et il vient de se manifester : un faux positif
se paie **en continu**, un faux négatif se paie une fois et se rattrape tout seul.

`* BYE` est traité à part, en erreur : le serveur ferme, et continuer à attendre sur cette
connexion attendrait pour toujours. L'erreur est ce qui fait reconnecter le veilleur.

Un défaut de mon propre correctif, trouvé par le test que je venais d'écrire pour l'interdire :
`VANISHED` était comparé par préfixe, donc un `VANISHEDX` qu'une extension future introduirait se
serait fait lire comme celui-ci. Le mot entier, pas son préfixe — c'est la même exigence qu'un
test du client posait déjà sur les éléments d'un `FETCH`.

#### Trois gardes de régression

- au niveau du client : `* OK Still here` ne rend pas d'événement, **et la connexion tient** — la
  ligne a bien été lue, pas laissée dans le tuyau ;
- au niveau du client encore : `* BYE` met fin à l'attente au lieu de la faire boucler ;
- au niveau du veilleur : un serveur qui ne dit que des politesses ne met **aucune**
  synchronisation en file.

`mailfake` a gagné de quoi les servir : `announcing_on_idle` accepte n'importe quelle ligne, donc
la politesse comme l'arrivée.

#### Et la réserve du matin, alors ?

Elle tient toujours, et elle est même plus nette maintenant : **une arrivée réelle n'a pas été
observée**. Ce qui a été observé est un serveur réel qui parle pendant un `IDLE` — ce qui est déjà
une information qu'aucun test ne donnait — et un veilleur qui a maintenant appris à faire le tri.

#### Le correctif, vérifié contre le serveur qui a montré le défaut

Démon relancé sur les cinq comptes réels, cinq minutes d'observation :

| | Avant | Après |
|---|---:|---:|
| `* OK Still here` reçues | 5 en 10 min | 2 en 5 min |
| Prises pour une arrivée | **5** | **0** |
| Synchronisations inutiles | **5** | **0** |

La ligne apparaît maintenant dans le journal à sa vraie place — `IDLE : réponse sans changement,
on attend`, au niveau `debug`, du côté du client. Elle est visible pour qui la cherche, et elle
ne réveille plus personne.

#### Le `MODSEQ` partagé de Gmail a maintenant son contrôle

L'affirmation du 2026-09-08 — « le compteur de Gmail est celui du compte, pas celui de la
boîte » — reposait sur une seule observation : huit boîtes Gmail annonçant toutes `847930`.
Une valeur identique partout peut aussi vouloir dire « ce serveur ne suit pas les `MODSEQ` ».

`mail account probe --account 1 --detail` sur le Dovecot donne le contrôle qui manquait :

```
Zammad                        modseq     10
Assurance hackathon           modseq      3
Auto-entreprise.Instead       modseq      2
Auto-entreprise.AUDIOCAMP     modseq    761
Sent                          modseq    595
INBOX                         modseq   7776
```

**Quinze boîtes, quinze valeurs différentes**, chacune proportionnée à l'activité de sa boîte.
C'est ce qu'un `HIGHESTMODSEQ` par boîte doit donner, et c'est exactement ce que Gmail ne donne
pas. L'inférence devient une comparaison.

Conséquence pratique inchangée, mais mieux fondée : chez Gmail, `CONDSTORE` ne porte aucune
information **par dossier** — seul le `CHANGEDSINCE` en porte, et lui demande d'avoir sélectionné
la boîte. Chez Dovecot, le raccourci du critère 3 tire tout ce qu'il peut du `MODSEQ`.

### Correction : le pipelinage ne peut pas faire ce que je lui prêtais — 2026-09-09

Ce matin, en découvrant le `MODSEQ` partagé de Gmail, j'ai écrit : « **le pipelinage revient
donc par cette porte-là** […] il diviserait le coût d'une passe où du courrier est arrivé. C'est
la prochaine optimisation utile, et elle a maintenant une raison mesurée plutôt qu'une
intuition. »

La raison était mesurée. La conclusion est fausse, et pour une raison de protocole.

#### Ce que la RFC interdit

Le coût d'une passe « quelque chose est arrivé » chez Gmail, c'est dix-huit fois la séquence
`EXAMINE` → `UID FETCH … CHANGEDSINCE`. Or la RFC 3501 §5.5 range explicitement parmi les cas
**ambigus** une commande qui dépend de la boîte sélectionnée émise en même temps qu'une commande
qui la change. Un `FETCH` en vol pendant qu'un `EXAMINE` change de boîte n'a pas de sens : rien
ne dit à quelle boîte la réponse se rapporte.

L'alternance `EXAMINE` / `FETCH` est donc **séquentielle par construction**. Le pipelinage ne la
raccourcit pas d'un aller-retour. Ce que je lui prêtais, il ne peut pas le faire.

Ce qui raccourcirait cette séquence est autre chose : **plusieurs connexions**, une par dossier,
en parallèle. C'est ce que font les clients établis, et Gmail en accorde une quinzaine par compte.
Et ça tombe droit sur la contention SQLite mesurée le même jour par le banc du critère 4 — qui
cesserait alors d'être un artefact de banc pour devenir un chemin de production. Deux fois le même
jour, la même leçon : une optimisation de réseau se paie en contention locale.

#### Ce que le pipelinage ferait vraiment

Il y a un endroit où il est légal et où il paierait : **plusieurs `UID FETCH` de suite dans la
même boîte sélectionnée**. Rien n'y est ambigu — la boîte ne change pas.

C'est le cas de la **première** moisson d'un gros dossier : les corps partent par lots de cent,
donc l'`INBOX` de `compte-b` et ses 19 176 messages coûtent 192 allers-retours, soit ~50 s de pur
délai réseau à 260 ms, avant même de compter le téléchargement. Un pipeline de profondeur quatre
diviserait ça par quatre.

Ce n'est pas la passe incrémentale, c'est la première. Et la première n'a pas de critère.

#### Et est-ce que ça vaut le coup ?

Non, pas maintenant, et c'est la vraie réponse. Mesuré aujourd'hui :

| Passe | Coût |
|---|---:|
| Rien de neuf, cinq comptes | **5,1 s** au total |
| Du courrier en retard sur le Dovecot — 15 dossiers, 12 évités, 46 corps, 2 nouveaux | **492 ms** |
| Du courrier sur un compte Gmail — 18 dossiers, tous à réexaminer | **~9 s** |

**Personne n'attend ces neuf secondes.** C'est un job de fond ; la coquille lit le store, et le
critère 4 dit qu'elle le lit à 2,50 ms l'image pendant que ça écrit. Le seul chiffre que
l'utilisateur ressent est le délai entre l'arrivée d'un message et son apparition dans la liste,
et `IDLE` l'a fait tomber de « la prochaine fois que je demande » à « une dizaine de secondes ».

Le critère 3 porte sur la passe où rien ne change **parce que c'est celle qui tourne tout le
temps**. Optimiser la passe qui tourne quand il se passe quelque chose serait optimiser le cas
rare, au prix d'un client IMAP concurrent et d'une contention d'écriture qu'on vient de mesurer.

Ce qui reste noté, sans être une dette : le jour où la **première** moisson d'un compte de
cinquante mille messages devient pénible, le pipelinage des lots de corps dans une même boîte est
la bonne réponse, et elle est légale.

### Lever une réserve, et découvrir que sa justification était fausse — 2026-09-09

La réserve du critère 2, écrite la veille : « ce qui n'est pas mesuré, et il faut le dire : la
crête d'une **moisson complète** sur un vrai compte. Il faudrait retélécharger les 6,3 Gio, et le
correctif de reprise du même jour rend ce retéléchargement impossible à provoquer sans casser le
store. Le lot de cent messages borne la mémoire par construction ; ça reste un argument de
conception, pas un relevé. »

Le store jetable du critère 4 rendait la mesure possible — même hôte, même identifiant, donc même
secret au trousseau, et le store réel n'est ouvert que pour y lire le compte. Une commande de
plus, et la réserve tombait.

Elle est tombée, et elle a emporté son propre argument avec elle.

#### Le premier relevé

| | |
|---|---:|
| Compte | `contact@perso.invalid`, moisson complète |
| Écrit | 2 166 contenus, **1,2 Gio** de RFC 5322 |
| RSS au repos | 10,0 Mio |
| **RSS crête** | **266,5 Mio** |

Seuil 500 Mo : passé. Et pourtant ce chiffre accuse. Deux mille messages ne devraient pas coûter
un quart de gigaoctet à un code qui travaille par lots de cent.

La moyenne de ce compte est de **580 Ko par message** — beaucoup de pièces jointes. Cent messages
de 580 Ko font 58 Mo de corps tenus en mémoire d'un coup, plus la compression, plus ce que
l'allocateur garde.

#### « Le lot de cent borne la mémoire par construction » — non

Il borne le **nombre**. Pas les octets. Et le nombre ne dit rien de la mémoire :

| Cent messages de… | En mémoire |
|---|---:|
| 3 Ko | 300 Ko |
| 62 Ko — la moyenne du corpus | 6 Mio |
| 580 Ko — la moyenne de ce compte | 58 Mio |
| 25 Mo — une pièce jointe au plafond d'un fournisseur | **2,5 Gio** |
| 128 Mio — le plafond par message de `mailsync` | **12,8 Gio** |

La dernière ligne est le pire cas que le code d'hier permettait. Pas plausible, mais pas
impossible non plus : il suffit d'un dossier d'archives vidéo.

C'est exactement le genre d'affirmation qui survit parce que personne ne la mesure. La règle 4 du
`CLAUDE.md` — *rien ne charge un mbox entier en mémoire, tout est streamé* — était respectée à la
lettre côté lecture réseau, et contournée d'un cran plus loin : la lecture est bien streamante, et
on rangeait cent résultats avant d'écrire.

#### Le correctif : deux bornes, et la taille est gratuite

`RFC822.SIZE` arrive **dans le même aller-retour** que la liste des UID. Le plan demandait
`UID FETCH n:* (UID)` ; il demande maintenant `(UID RFC822.SIZE)`, et le découpage en lots
respecte les deux bornes — cent messages **ou** 32 Mio, celle des deux qui vient d'abord.

Trois décisions dans la fonction de découpage, chacune avec son test :

- **un message plus gros que la borne part seul**, sinon il ne partirait jamais. La borne dit
  « pas plusieurs gros ensemble », pas « jamais de gros » ;
- **une taille non annoncée compte pour 64 Ko** — la moyenne mesurée du corpus. La compter pour
  zéro ramènerait la borne à celle du nombre, c'est-à-dire au défaut qu'on corrige ; la compter
  pour le plafond ferait un aller-retour par message ;
- **chaque UID est dans exactement un lot, dans l'ordre.** Un découpage qui en perdrait ferait
  manquer du courrier ; qui en dupliquerait le retéléchargerait ; et l'ordre compte pour la
  reprise, puisque les lots validés sont les premiers.

`RFC822.SIZE` est une **annonce**, pas une mesure. Un serveur qui mentirait dessus ramènerait la
borne à celle du nombre — c'est écrit à côté du champ. La défense contre un littéral
surdimensionné reste le plafond par message, qui lui compte les octets reçus.

`mailfake` le sert désormais aussi. Sans ça, tous ses tests passeraient par le chemin « taille
inconnue », et le chemin réel — celui des cinq serveurs du corpus — ne serait couvert nulle part.

#### Ce que ça donne

| | Avant | Après |
|---|---:|---:|
| RSS crête | 266,5 Mio | **96,1 Mio** |
| Pire cas théorique | 12,8 Gio | **32 Mio** de corps |
| Durée, 1,2 Gio téléchargés | 108,8 s | 55,1 s |
| Contenus écrits | 2 166 | 2 194 |

**La crête tombe de 2,8 fois.** C'est structurel, et c'était le but.

La durée, elle, a été divisée par deux, et **je ne l'attribue pas au correctif** : 1,2 Gio en
108,8 s puis en 55,1 s, c'est 11 Mo/s puis 22 Mo/s, et rien n'exclut que la liaison ait été
simplement meilleure au second passage. Un lot plus petit laisse plausiblement le socket travailler
pendant qu'on compresse, mais ça reste une hypothèse : une seule paire de relevés ne sépare pas
l'effet du bruit. Le seul chiffre que ce banc établit est celui de la mémoire.

#### Ce que la mesure ne dit toujours pas

**La crête d'un compte entier.** Deux mille messages sur les 2 847 du plus petit compte, et pas
les 105 508 du corpus. Ce que le relevé établit est la **pente** : la mémoire d'un lot ne dépend
plus du nombre de lots, et elle est maintenant bornée par une constante qu'on peut lire. Le reste
serait 6,3 Gio de bande passante pour confirmer une addition.

### Le balayage revenait dès qu'un message arrivait — 2026-09-09

Une synchronisation du corpus réel, juste après avoir posé la borne en octets, sur une passe où
du courrier était arrivé :

| Compte | Dossiers | Évités | Corps reçus |
|---|---:|---:|---:|
| `compte-a` | 8 | **0** | 3 |
| `contact@perso.invalid` | 15 | 15 | 0 |
| `compte-b` | 10 | **0** | 8 |
| `moi@perso2.invalid` | 29 | **0** | 6 |
| `compte-c` | 18 | **0** | 19 |

**2 min 21 s** pour trente-six messages. Le matin, la même commande sur une passe vide tenait en
5,1 s.

#### Ce que je croyais, et ce qui se passait

J'avais écrit quelques heures plus tôt qu'une arrivée coûtait « ~9 s par compte Gmail », en
comptant les allers-retours d'`EXAMINE`. C'était le mauvais coupable.

Le vrai coût est le **balayage** : `UID FETCH 1:* (UID)`, une ligne de réponse par message du
dossier. Il s'exécutait dès qu'un dossier n'était pas `UpToDate` — et chez Gmail, dont le `MODSEQ`
est celui du compte, **une seule arrivée sort tous les dossiers du compte de `UpToDate`**. Les
dossiers « Tous les messages » de `compte-b` et de `compte-c` font 25 024 et 11 539 messages :
trente-six messages arrivés déclenchaient donc plus de cent mille lignes de réponse pour vérifier
que rien n'avait disparu.

Les deux découvertes de la journée se composaient sans que je le voie : le `MODSEQ` partagé
transformait un balayage occasionnel en balayage systématique.

#### La condition d'évitement demandait trop, et ça se démontre

Elle était : `plan == UpToDate` **et** `EXISTS == nos copies`. L'argument pour la première moitié,
écrit la veille : « un message purgé et un message reçu laissent `EXISTS` inchangé ».

L'argument est juste — mais il porte sur `EXISTS` comparé à lui-même d'un passage à l'autre, pas
sur la comparaison qui est réellement faite. Celle-ci est plus forte.

Soit `K` l'ensemble des UID connus **après** ce passage, donc y compris ceux qu'on vient
d'apprendre : le passage des corps énumère tout ce qui est au-delà de notre `UIDNEXT`. Soit `S`
celui du serveur, dont `EXISTS` donne le cardinal.

- les UID appris sont dans `S` par construction ;
- les purgés sont dans `K` et pas dans `S`.

Donc `|K| − |S|` **est** le nombre de purges, et `|K| = EXISTS` implique qu'il n'y en a aucune.
Le plan n'entre pas dans le raisonnement.

Le contre-exemple invoqué se résout tout seul : purger l'UID 3 et recevoir l'UID 11 laisse
`EXISTS` à 10, mais `K` en compte onze — l'inégalité apparaît, et le balayage a lieu. **C'est un
test**, pas une conviction : `a_purge_and_an_arrival_that_cancel_each_other_out_are_still_seen`.
Avec sa moitié inverse, sans quoi on aurait pu « corriger » le premier en balayant toujours.

#### Ce que la condition ne couvre pas

`EXISTS` est lu à l'`EXAMINE`, donc avant la moisson. Une purge survenue **après** cette lecture
peut laisser les comptes égaux et passer inaperçue jusqu'au passage suivant, où `EXISTS` sera
frais. Exposition d'un passage, qui se corrige seule — et la version d'avant l'avait exactement la
même : ce n'est pas une régression, c'est une limite qu'il valait mieux écrire.

#### Ce que ça donne

`Plan::Full` en profite aussi, au passage : après une moisson complète, `K = S` par construction,
et le balayage qui suivait était du travail pur.

Le relevé de l'effet demande une arrivée réelle, qui ne se commande pas. Ce qui est établi : la
condition est strictement plus large qu'avant, elle est démontrée plutôt que supposée, et les deux
tests tiennent les deux sens.

### `mailfake` était plus généreux qu'un vrai serveur — 2026-09-09

Remarqué en ajoutant `RFC822.SIZE` : le serveur de test **ignorait les éléments demandés** et
renvoyait toujours le corps. `UID FETCH 1:* (UID)` — l'énumération qui sert à savoir quels UID
existent — transférait donc tout le dossier une seconde fois.

Deux conséquences, et la première est la grave :

- **un client qui demanderait `(UID)` et s'appuierait quand même sur le corps aurait passé tous
  ses tests ici, et échoué en production.** C'est exactement le genre de faux positif qu'un
  serveur de test doit refuser de produire : le `docs/PHASE-2.md` justifie l'existence de
  `mailfake` par « un serveur correct ne sait pas répondre de travers », et un serveur *trop*
  complaisant est une autre façon de répondre de travers ;
- les tests transféraient deux fois les corps, donc étaient plus lents et moins fidèles à la fois.

Le corps ne part maintenant que si la commande le demande. `RFC822.SIZE` et les drapeaux partent
toujours — un vrai serveur les rend volontiers, et le client ne s'appuie sur aucun élément qu'il
n'a pas demandé.

#### Deux tests sont tombés, et c'était le but

`a_folder_cut_mid_harvest_resumes_instead_of_starting_over` et
`a_flag_changed_during_the_interruption_is_not_lost` coupent la connexion sur un **budget
d'octets** — 100 000 — choisi pour tomber au milieu du deuxième lot de corps.

Ce budget était calibré, sans que personne le sache, sur le **double transfert**. Les corps ne
passant plus qu'une fois, les 250 messages du test tiennent en ~79 Kio et la coupure n'avait plus
lieu : `partial == 250`, et l'assertion « la coupure n'a rien coupé » sautait.

C'est le bon comportement d'un test qui vérifie une coupure : il refuse de passer quand il n'y a
plus de coupure. Le budget est descendu à 50 000 — après l'énumération (~14 Kio) et le premier lot
de cent corps (~26 Kio), donc dans le deuxième — et le pourquoi est écrit à côté du chiffre, pour
que le prochain qui le déplace sache ce qu'il déplace.

**Deux tests calibrés sur un artefact du harnais, découverts en corrigeant l'artefact.** C'est un
argument de plus pour la fidélité d'un serveur de test : ce n'est pas seulement qu'il doit savoir
mentir, c'est qu'il ne doit pas être plus gentil que la réalité.

### L'échéance du veilleur, éprouvée sur les vrais serveurs — 2026-09-09

Démon laissé tourner vingt-cinq minutes sur les cinq comptes, dans l'espoir d'une arrivée
réelle. Elle n'est pas venue — mais **l'échéance, si**, et c'est l'autre moitié du mécanisme.

| Instant | |
|---|---|
| 12:56:28 | veille en place sur les cinq comptes |
| 13:20:31 | échéance compte 1, puis 5, 4, 3, 2 — en 1,8 s |
| 13:20:37 | les cinq synchronisations terminées |

**Vingt-quatre minutes exactement**, la valeur de `REARM`. Les cinq comptes se synchronisent en
**6,3 s** au total, tous dossiers évités.

#### Ce que ça démontre, et qui n'était qu'un raisonnement

**Le filet fonctionne.** C'est lui qui rend la liste blanche des annonces de changement
défendable : une ligne mal classée coûte au pire un retard borné, et la borne vient d'être
observée plutôt que supposée.

**Les veilleurs se décalent tout seuls.** Les cinq échéances tombent à 1,8 s d'intervalle, parce
que les connexions se sont établies dans cet ordre-là. Personne n'a écrit de décalage : il vient
du temps de connexion, et il suffit.

**Le fil de travail les sérialise.** Les cinq jobs terminent l'un après l'autre — 31,39 s, 31,88 s,
32,65 s, 34,44 s, 37,43 s — parce qu'il y a une file et un seul fil. C'est la démonstration
directe de ce que l'entrée sur la contention SQLite affirmait : **le produit n'écrit jamais à
cinq**, seul le banc du critère 4 le fait, exprès. Le correctif de retryabilité reste utile pour
la cohabitation d'un démon et d'une coquille embarquée, qui elle est possible.

#### La réserve, encore

**Aucune arrivée réelle en vingt-cinq minutes.** Les cinq comptes avaient reçu trente-six messages
dans l'heure précédente, donc le rythme moyen aurait dû en amener ; le courrier arrive par
paquets, pas à intervalle régulier. La moitié « un message arrive, le job part » reste couverte
par `mailfake` et par rien d'autre.

Ce qui a été observé sur un vrai serveur, en revanche : la pose, la négociation, la tenue, le
tri des politesses, l'échéance, et l'arrêt propre. Il reste une ligne à voir passer.

### Une arrivée réelle, enfin vue — 2026-09-09

La réserve écrite trois fois dans ce journal — « une arrivée réelle n'a pas été observée » —
tombe. Le démon laissé tourner sur les cinq comptes, à 14:54:40 :

```
IDLE a parlé              evenement=* 19181 EXISTS
arrivée annoncée : synchronisation   compte=3
synchronisation mise en file         compte=3 job=6
job terminé               id=6 state="done"        (31 s plus tard)
```

Un message est arrivé dans l'`INBOX` de `compte-b`, Gmail l'a annoncé par un `* 19181 EXISTS`, le
veilleur l'a **reconnu comme un changement** — pas comme une politesse — et la synchronisation a
suivi.

La chaîne entière est maintenant observée sur un vrai serveur : arrivée → annonce → job en file →
store à jour. Sur la même fenêtre d'observation : **18 politesses correctement ignorées**, une
arrivée déclenchée, cinq échéances, **zéro verrou** — et pourtant la coquille embarquée tournait
en même temps que le démon, donc deux veilles par compte et deux registres de jobs sur le même
store. La cohabitation que l'entrée précédente signalait comme possible s'est produite, et elle
s'est bien passée.

C'est aussi le contrôle de la liste blanche du tri des annonces, dans le bon sens : elle laisse
passer ce qui compte. L'autre sens — elle bloque les politesses — était mesuré depuis midi.

### La crête d'un compte entier, et ce qui la porte vraiment — 2026-09-09

La dernière réserve du critère 2 : « ce qui reste hors de portée est la crête d'un compte
**entier** ». Faite.

| Compte | Contenus | Octets RFC 5322 | Crête |
|---|---:|---:|---:|
| `contact@perso.invalid`, **entier** | 2 842 | 1,5 Gio | **169,6 Mio** |
| `moi@perso2.invalid`, **entier** | 3 477 | 850,7 Mio | **96,7 Mio** |

Seuil 500 Mo, au repos 10 Mio. Les deux passent, et l'écart entre les deux est la vraie
information.

#### Ce n'est pas le nombre de messages, c'est le plus gros

Le compte à 3 477 contenus tient à 96,7 Mio ; celui à 2 842 monte à 169,6. Moins de messages,
plus de mémoire. La borne du lot est à 32 Mio, donc quelque chose la dépasse — et un message plus
gros que la borne **part seul**, par construction.

Le plus gros blob du store réel fait **47,5 Mo compressé**, soit un message d'environ 48 Mio. Il
appartient à `contact@perso.invalid`. C'est lui qui porte la différence.

La borne réelle n'est donc pas 32 Mio, c'est **`max(32 Mio, le plus gros message)`**, et le
plafond par message de `mailsync` — 128 Mio — en fait le pire cas. Sous le seuil, mais il faut le
dire comme ça et pas comme « 32 Mio ».

#### Une hypothèse fausse, et je la laisse écrite

J'ai d'abord attribué les 110 Mio au-delà des 48 du corps à `zstd::encode_all`, qui alloue tout le
résultat compressé avant d'en écrire un octet : 48 Mio de corps **plus** 47,5 de compressé vivants
en même temps.

Le correctif — compresser en flux droit dans le fichier temporaire — est écrit, et **la crête n'a
pas bougé** : 168,9 Mio avant, 169,6 après. L'hypothèse était fausse. Ce qui reste est
vraisemblablement de la **rétention de tas** : l'allocateur ne rend pas au système les pages
libérées après un pic de 48 Mio, et la RSS mesure ce que le processus tient, pas ce qu'il
utilise.

Le correctif reste, et pas par entêtement : il divise par deux le **pire cas théorique** — un
message au plafond de 128 Mio n'alloue plus sa copie compressée — et c'est la règle 4 du
`CLAUDE.md` un cran plus bas, *rien ne charge un message entier deux fois*. Mais il ne se voit pas
dans le chiffre, et le prétendre serait s'attribuer un gain qu'on n'a pas mesuré.

#### Une variation de durée que je n'explique pas

Quatre relevés sur le même compte : 108,8 s, 55,1 s, 138,0 s, 77,3 s. Un facteur deux entre des
passages du même travail, alternant lent et rapide. Le cache de pages du serveur d'en face est
l'explication la plus plausible — la première moisson le remplit, la suivante en profite — mais
c'est une hypothèse, pas une mesure. **Le seul chiffre que ce banc établit est celui de la
mémoire.**

### Le chemin Microsoft, poussé aussi loin qu'on peut sans compte — 2026-09-09

Les trois comptes qui manquent au corpus demandent des identifiants que le code ne peut pas
fabriquer. Ce qui pouvait être fait sans eux l'a été.

#### Le jeton de rafraîchissement qui tourne

**Microsoft en renvoie un nouveau à chaque rafraîchissement, et l'ancien cesse de valoir.** Google
n'en renvoie pas. Ne pas garder le nouveau donne la panne la moins déboguable qui soit : le compte
marche, puis s'arrête un jour, sans que rien ait changé de notre côté.

Le code le gardait déjà. **Rien ne le vérifiait** : `session::access_token` fait son `POST` en TLS
vers un hôte réel, donc tout ce qui suit était hors de portée d'un test — et c'est justement là
que vit la décision propre à chaque fournisseur.

La décision est maintenant une fonction à part, `apply_tokens`, et elle a cinq tests : le jeton
qui tourne remplace l'ancien, un jeton absent laisse l'ancien en place, **un refus ne touche à
rien** — une panne passagère du fournisseur ne doit pas se transformer en consentement à
redemander —, un jeton vide n'efface pas, et un corps qui n'est pas du JSON ne panique pas.

Le premier a son contrôle négatif : la ligne qui garde le nouveau jeton retirée, le test tombe sur
« le jeton de rafraîchissement de Microsoft n'a pas été gardé ».

#### Le port de bouclage, épinglable

Le consentement ouvre un port **éphémère** sur `127.0.0.1`. Google autorise explicitement le port
à varier pour une redirection de bouclage ; la documentation de Microsoft est ambiguë entre
`http://localhost`, dont le port est ignoré, et une adresse littérale, dont il ne l'est
peut-être pas.

Un port éphémère devant un fournisseur qui compare l'URI à l'octet ne marcherait **jamais**, et
son message d'erreur ne dirait pas pourquoi. `mail account add --redirect-port PORT` est la sortie
de secours : enregistrer `http://127.0.0.1:PORT` chez le fournisseur, passer le même ici. C'est
une ligne de code et ça retire le seul blocage structurel que ce chemin pouvait avoir.

#### Ce qui reste, et qui n'est pas du code

- **Free** et **SFR** : un mot de passe applicatif à générer chez chacun, puis
  `mail account add --host imap.free.fr --username … --auth password`. Rien d'autre à écrire ;
  ce sont des serveurs à mot de passe, le chemin le plus éprouvé des trois.
- **`etudiant@ecole.invalid`** : une inscription d'application sur `entra.microsoft.com` — client
  public, redirection `http://127.0.0.1`, autorisations `IMAP.AccessAsUser.All` et
  `offline_access` — puis `mail account add --host outlook.office365.com --auth oauth2
  --client-id …`. La commande affiche ces étapes elle-même.

Le chemin Microsoft reste **non éprouvé contre un vrai serveur**. Ce qui a changé est qu'il n'a
plus de blocage connu, et que la seule de ses différences avec Google qui pouvait casser en
silence est maintenant testée.

### Le raccourci du menu Démarrer, et la console qui n'a rien à y faire — 2026-09-09

`mail-shell` était compilé pour le sous-système **console** : lancé depuis le menu Démarrer, il
ouvrait une fenêtre de terminal noire à côté de la sienne. Pour une application de bureau, c'est
un défaut de livraison.

`#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]` — **seulement en release**,
parce qu'en debug la console est le moyen le plus court de lire les traces pendant qu'on
développe.

Le prix, en release : les modes en ligne de commande du même binaire — `--purge-cache`, `--help` —
n'affichent plus rien depuis un terminal. Ils restent utilisables depuis un script, parce que le
sous-système ne change que l'allocation d'une console, pas les descripteurs hérités. **C'est
vérifié plutôt que supposé** : `xtask measure-ui --bench startup` lit toujours les relevés de la
coquille, qu'il prend sur un tuyau, et le critère 1 passe — 225,0 ms à froid, dispersion 188 à 225,
budget 400.

Deux relevés isolés à 183 ms avaient laissé croire que la console coûtait trente millisecondes de
démarrage. Cinq passes montrent une dispersion de 188 à 225 : **le gain est dans le bruit**, et je
ne l'attribue pas.
