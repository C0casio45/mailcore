# Phase 4 — retrouver un message qu'on ne sait pas nommer

## Objectif

« La facture du plombier de l'an dernier » doit trouver le message, même s'il ne contient ni
« facture », ni « plombier », ni aucun mot de la requête. C'est la première promesse de
`docs/VISION.md` sur l'IA, et c'est la seule de la phase.

**La phase 3 est close depuis le 2026-09-15** : le client écrit, envoie, répond dans le fil,
complète une adresse, lit une invitation. Ce qui manque n'est plus de faire — c'est de
**retrouver**.

## Pourquoi la recherche sémantique, et rien d'autre

`docs/VISION.md` nomme quatre choses : recherche sémantique, tri à l'arrivée, résumé de fil,
rédaction assistée. Trois raisons de ne prendre que la première.

**C'est celle qui se mesure.** Une recherche a une bonne réponse : le message qu'on cherchait est
dans les résultats, ou il n'y est pas. Un résumé n'a pas de bonne réponse, un tri automatique non
plus — on peut mesurer leur coût, pas leur justesse, et une phase dont le critère principal est
invérifiable n'est pas une phase, c'est une intention.

**C'est celle qui doit tourner en local, et ça change tout le reste.** Vectoriser 73 658 messages
veut dire faire passer **tout le corpus** dans un modèle. Envoyer ça à une API contredirait la
règle 5 de `CLAUDE.md` aussi sûrement qu'une image distante : ce serait la fuite de données la
plus complète que le projet puisse produire, faite volontairement. Le modèle est donc local, et
c'est une contrainte d'architecture, pas une préférence — voir la section suivante.

**Les trois autres sont ponctuelles, donc elles peuvent attendre et se discuter autrement.** Un
résumé part d'un clic sur un fil, une rédaction d'un clic sur « répondre ». Le consentement y a un
sens qu'il n'a pas pour une passe de fond qui traite tout. Cette différence-là mérite sa propre
phase, et la trancher maintenant préempterait une décision qui n'a pas besoin de l'être.

## Périmètre

Dedans :

- **Un embedding par message**, calculé par un modèle qui tourne sur la machine, rangé à côté de
  l'index plein texte. C'est ce que `docs/ARCHITECTURE.md` prévoit depuis la phase 1.
- **Le modèle lui-même** : son installation explicite, son chargement, et ce qui se passe quand
  il n'est pas là.
- **La recherche vectorielle** : de la requête au message, avec une latence mesurée sur le corpus
  réel.
- **La fusion avec tantivy.** Les deux recherches ne répondent pas à la même question, et la
  liste que l'utilisateur voit est une seule liste. La règle de fusion est un choix explicite,
  écrit à un seul endroit, et mesurable.
- **La passe incrémentale** : les vecteurs suivent la moisson, comme le carnet, l'index et les
  fils depuis les 2026-09-11 et 2026-09-13.
- **Un banc de qualité** avec une vérité terrain, écrit **avant** le moteur.

Dehors, et c'est délibéré :

- **Le tri automatique à l'arrivée.** Il se trompe visiblement — un message important classé
  ailleurs est une panne, pas une dégradation — et il demande d'apprendre sur le comportement,
  donc d'observer l'utilisateur. Les deux méritent mieux qu'un coin de phase.
- **Le résumé de fil et la rédaction assistée.** Ponctuels, donc un autre problème : celui du
  consentement à faire sortir un texte, ou celui d'un modèle génératif local. Ni l'un ni l'autre
  ne se fait à moitié.
- **Un modèle distant, pour quoi que ce soit.** Voir ci-dessous.
- **Un index approché type HNSW.** Le corpus tient en mémoire et un balayage complet est mesurable
  — voir le critère 2. Ajouter une structure approchée avant d'avoir montré qu'un balayage ne
  suffit pas, c'est payer une complexité et une classe de bogues pour un problème qu'on n'a pas.

## Ce que la vie privée exige, cette fois

`docs/PRIVACY.md` et la règle 5 de `CLAUDE.md` ne changent pas ; ce qui change est ce qu'elles
interdisent de faire, parce que la tentation est nouvelle.

**Aucun octet de courrier ne quitte la machine, jamais.** Pas d'API d'embeddings, pas de service
de vectorisation, pas de « juste les sujets pour commencer ». Un sujet est du contenu.

**Le modèle s'installe sur une commande explicite, et c'est la seule requête réseau de la phase.**
Télécharger un fichier de modèle est une requête ; elle est déclenchée par un humain qui tape une
commande, jamais par l'arrivée d'un message ni par l'ouverture de l'application. Et elle dit ce
qu'elle va chercher, où, et combien ça pèse, avant de le faire.

**L'empreinte du fichier téléchargé est vérifiée.** Un modèle est du code exécuté sur tout le
corpus. Le récupérer sans vérifier ce qu'on a reçu serait le plus gros trou du projet.

**Sans modèle, le client marche.** C'est la contrainte que `docs/VISION.md` pose déjà — « une clé
API absente ou un modèle local indisponible dégrade des fonctionnalités, ne casse jamais le
client » — et elle a son critère chiffré ici, parce qu'une contrainte sans mesure est un vœu.

## Critères d'acceptation — à mesurer, pas à estimer

| # | Critère | Seuil | État |
|---|---|---|---|
| 1 | **Trouver ce qu'un mot-clé ne trouve pas** | sur un jeu de requêtes réelles écrites à la main avec leur réponse attendue : le message visé dans les **10 premiers** pour **≥ 80 %** des requêtes, et **strictement mieux que tantivy seul** sur le sous-ensemble des requêtes qui ne partagent **aucun mot** avec le message visé | **référence mesurée le 2026-09-15** — 22 requêtes écrites sur le corpus réel, `cargo xtask measure-recall`. Tantivy seul : **0,0 %** sur les 10 requêtes sans mot commun, **58,3 %** sur les 12 autres, **31,8 %** sur le jeu entier. Le moteur reste à écrire |
| 2 | Latence de la recherche, bout en bout depuis le client | **< 50 ms** en p95 sur le corpus réel — le même seuil que le critère 4 de la phase 1, parce que c'est la même attente : une liste qui arrive pendant qu'on lâche la touche | à faire |
| 3 | Coût de la passe complète | le corpus entier vectorisé, **mesuré en messages/s et en durée totale**, et la passe doit être **reprenable** : tuée au milieu, elle repart où elle en était sans recalculer ce qui est fait | à faire |
| 4 | Coût d'un message qui arrive | **< 200 ms** de travail de fond, à comparer à sa propre passe complète — le même tableau que `measure-followup` pour l'index, le carnet et les fils | à faire |
| 5 | Mémoire de la passe complète | **RSS < 500 Mo**, le seuil du critère 2 de la phase 2. Et la mesure dit **où** elle va : le modèle, le lot, le magasin de vecteurs | à faire |
| 6 | Zéro requête réseau déclenchée par le contenu | **exactement 0**, le harnais de `mailprivacy` étendu à une moisson **et** à une passe de vectorisation. La seule requête tolérée du projet est l'installation du modèle, sur commande | à faire |
| 7 | Sans modèle, rien ne casse | le fichier de modèle supprimé : la coquille démarre **sous le même seuil qu'avant**, la recherche plein texte répond, et l'écran **dit pourquoi** le sémantique est absent et comment l'obtenir — critère 8 de la phase 3 appliqué ici | à faire |
| 8 | Le français, pas l'anglais | le critère 1 mesuré sur des requêtes **françaises** contre un corpus français. Un modèle anglophone donne des chiffres flatteurs sur un banc anglais et s'effondre sur le corpus réel | à faire |
| 9 | Les deux magasins ne se contredisent pas | drapeaux en SQLite, vecteurs ailleurs : le contrôle du 2026-09-11 sur l'index, refait ici. Un magasin de vecteurs vidé pendant que les drapeaux disent « fait » doit être **détecté au point d'usage**, pas laissé silencieux | à faire |

Le critère 1 est celui qui décide si la phase valait la peine. Les autres disent si elle est
utilisable.

## Ordre de travail

**Le banc avant le moteur**, et cette fois c'est écrit comme une étape, pas comme une intention.

1. ~~**Le jeu de requêtes et sa vérité terrain.**~~ Fait le 2026-09-15 : 22 requêtes écrites sur le
   corpus réel, dans `measurements/phase4-queries.toml` — **hors du dépôt**, parce qu'il nomme des
   messages réels. `cargo xtask corpus-sample` sort les messages à regarder pour l'écrire.

   Le sous-ensemble qui compte est celui des requêtes **sans mot commun** avec leur cible : c'est
   là que tantivy ne peut rien, donc là que le sémantique se justifie ou ne se justifie pas. Il
   fait 10 requêtes sur 22.

2. ~~**Le banc lui-même**, qui mesure tantivy seul sur ce jeu.~~ Fait le 2026-09-15 :
   `cargo xtask measure-recall`. Un chiffre de référence obtenu **avant** d'avoir un moteur à
   défendre — **0,0 % sur le sous-ensemble dur**. Voir le journal.

3. **Le modèle local** : le choisir, le charger, mesurer son coût par message et sa mémoire. Un
   modèle multilingue, parce que le corpus est français — voir les pièges. Rien n'est indexé à
   cette étape : on mesure le moteur, pas le résultat.

4. **Le magasin de vecteurs et le balayage.** Le critère 2 se mesure ici, sur des vecteurs
   aléatoires s'il le faut : la latence d'un balayage ne dépend pas de ce que les nombres veulent
   dire.

5. **La passe complète et sa reprise** — critères 3 et 5.

6. **La fusion avec tantivy**, et le critère 1 mesuré pour de bon.

7. **La passe incrémentale** — critère 4 — sur le modèle des trois dérivés existants, et une
   quatrième ligne dans `measure-followup`.

8. **L'API, la coquille, et l'absence de modèle** — critères 7 et 6.

Les étapes 1 et 2 sont ce qui rend la phase mesurable. Elles passent avant tout le reste pour la
même raison que la file d'envoi passait avant l'interface à la phase 3 : c'est la partie qu'on ne
peut pas ajouter après coup sans se mentir.

## Tâches de fin de phase

À faire **une fois toutes les étapes ci-dessus remplies**, pas avant.

- **Reprendre le dessin de la coquille**, avec Claude Design. L'interface a été construite par
  ajouts successifs — une fenêtre de rédaction, un panneau de file, un panneau de brouillons, un
  éditeur de signature, une page de paramètres, une vue source — chacun ajouté le jour où il
  devenait nécessaire et dessiné pour marcher, pas pour tenir ensemble. Le résultat fonctionne et
  n'a jamais été regardé comme un tout.

  **Pourquoi à la fin et pas maintenant** : la recherche sémantique change ce que l'écran
  principal doit montrer — une liste de résultats classés par pertinence n'est pas une liste de
  messages classés par date. Redessiner avant de savoir ce qu'il y a à dessiner ferait le travail
  deux fois.

  Ce qui ne bouge pas à cette occasion : le critère 1 de la phase 2 — la coquille démarre sous
  225 ms — et le critère 5 de la phase 3 — moins de 16,7 ms par image. Un dessin plus soigné qui
  coûterait une image par frappe serait une régression, quoi qu'il donne en capture d'écran.

## Les pièges connus, avant d'écrire une ligne

**Un modèle anglophone sur un corpus français.** Les modèles les plus cités sont entraînés sur de
l'anglais, et ils donnent des vecteurs pour du français — des vecteurs médiocres, mais des
vecteurs, donc rien ne signale le problème. Le critère 8 existe pour ça, et c'est aussi pourquoi
le jeu de requêtes de l'étape 1 est écrit avant de choisir le modèle : un banc écrit après le
choix ressemble toujours au choix.

**Un fil de 40 messages contient 39 copies du texte cité.** Vectoriser le corps brut ferait 40
vecteurs presque identiques, et une recherche rendrait le fil entier là où elle devrait rendre le
message qui répond. La citation doit être retirée avant la vectorisation — et l'endroit où on la
retire doit être **le même** que celui qui la reconnaît pour l'affichage, sinon les deux
divergeront.

**Un message de 1,4 Mo ne se vectorise pas entier.** Tout modèle a une fenêtre, et elle est
courte — quelques centaines de mots. Ce qu'on garde d'un long message est donc un **choix**, il
change le rappel, et il doit être écrit et mesuré plutôt que subi. Le début n'est pas évidemment
le bon morceau : un mail commence souvent par « Bonjour, j'espère que vous allez bien ».

**Une fusion de scores inventée.** Le score de tantivy est un BM25, celui du vectoriel un cosinus :
les additionner n'a aucun sens, les normaliser en a peu. La règle de fusion est un choix explicite,
écrit une fois, et le critère 1 la mesure — une moyenne pondérée par des coefficients devinés est
exactement le genre de chose qui donne un banc à 78 % qu'on n'arrive plus à améliorer sans savoir
pourquoi.

**La reprise d'une passe qui dure.** Vectoriser 73 658 messages n'est pas une passe de 2 s. Elle
sera tuée — machine éteinte, mise à jour, `Ctrl-C`. Le drapeau par ligne des trois dérivés
existants est la bonne forme, avec la leçon du 2026-09-11 : **rendre l'écriture idempotente est ce
qui rend l'ordre sûr**, et le drapeau se pose **après** que le vecteur est durable.

**Le premier relevé accusera le banc.** C'est arrivé au critère 1 de la phase 3, et ça arrivera
ici. Un rappel de 0 % ou de 100 % au premier essai est un symptôme, pas un résultat.

## Journal

### 2026-09-15 — le banc avant le moteur, et les deux défauts qu'il a trouvés sans moteur

Les étapes 1 et 2, dans l'ordre que la phase s'était fixé. Elles n'écrivent pas une ligne de
moteur sémantique, et elles ont pourtant trouvé deux défauts réels — un dans l'instrument, un
dans le produit livré.

#### Un tirage uniforme sur un corpus réel donne neuf notifications sur dix

L'étape 1 demande de regarder de vrais messages pour écrire des requêtes. `cargo xtask
corpus-sample` les sort, étalés sur toute la période du corpus plutôt que pris au début — les N
premiers messages d'un store sont ceux d'un dossier et d'une période, et un jeu de requêtes écrit
dessus mesurerait la recherche sur trois semaines de courrier.

Le premier tirage a donné **Facebook six fois, Dribbble quatre, YouTube, Twitch, Pinterest**.
Personne ne cherche la notification Facebook de mars 2016. Un corpus personnel n'est pas une
distribution uniforme de choses qu'on voudra retrouver : c'est une majorité écrasante de bruit
récurrent, et quelques centaines de messages qui comptent.

Le premier filtre essayé était le carnet d'adresses : ne garder que les expéditeurs à qui
l'utilisateur a **écrit** — `seen_to > 0`. Un expéditeur automatique ne reçoit jamais de réponse,
et aucune liste de domaines à bannir n'est à tenir à jour. Ça marche, et ça garde **32 messages
sur 5 063** : trop peu, et surtout ça exclut le cas canonique de la phase. « La facture du
plombier de l'an dernier » vient d'un expéditeur automatique.

**Ce qui distingue une facture d'une notification n'est pas l'humain derrière, c'est la rareté.**
Une facture arrive une fois, Facebook écrit quatre cents fois. Le filtre est donc l'union des
deux : un correspondant à qui on a écrit, **ou** un expéditeur vu au plus cinq fois. 633 messages
retenus sur 5 063, et l'échantillon devient exploitable — école, commandes, confirmations,
services ponctuels.

#### Le jeu de requêtes ne peut pas entrer dans le dépôt

Il nomme des messages réels par leur identifiant, et ses requêtes disent ce que quelqu'un cherche
dans son courrier. Il vit dans `measurements/`, gitignoré — même règle que les relevés de `xtask`
et que la pseudonymisation du même jour.

Deux décisions de forme, et la seconde a compté :

- **le partage d'un mot n'est pas déclaré dans le fichier, il est calculé par le banc.** Deux
  sources pour le même fait finiraient par se contredire, et c'est ce fait-là qui porte la
  conclusion de la phase ;
- **une cible absente fait échouer la commande**, au lieu d'être comptée comme un échec de
  rappel. Un jeu de requêtes est lié au store sur lequel il a été écrit : un réimport renumérote
  les messages, et le banc mesurerait alors contre des cibles qui ont glissé — en rendant un
  chiffre parfaitement présentable.

Et une liste de mots-outils, qui est **une décision de mesure et pas un détail** : sans elle, « le »,
« de » et « que » sont dans toutes les requêtes et dans tous les messages, donc tout partage un mot
avec tout, et le sous-ensemble qui décide de la phase disparaît. Elle ne contient que des
mots-outils : « compte » y serait tentant — il est dans des dizaines de messages — mais c'est
précisément ce qui rend facile, pour un moteur de mots, une requête qui le contient.

#### Le défaut du produit : l'apostrophe

La deuxième requête du jeu a fait tomber le banc :

```
Error: recherche « l'achat qui m'attend en boutique »
  1: Syntax Error: l'achat qui m'attend en boutique
```

**L'apostrophe ouvre une chaîne dans la grammaire de tantivy.** `l'achat` n'a pas de fin de
chaîne, donc la requête est refusée. En français, ça vise un utilisateur sur deux : `l'équipe`,
`aujourd'hui`, `qu'il`, `n'est`. `mail search` — et la barre de recherche de la coquille avec
elle — répondait « requête invalide » à une phrase française ordinaire, depuis la phase 1.

Le remède tient en une règle : essayer la **grammaire** d'abord, ce qui préserve `from:` et les
phrases exactes de qui les connaît ; si elle refuse **à cause d'une apostrophe**, réessayer sans
elles. Les deux apostrophes comptent, la droite et la typographique `U+2019` que les téléphones
et les correcteurs produisent sans prévenir.

#### Et le test qui a resserré le remède

Le premier jet neutralisait **toute** la syntaxe, pas seulement les apostrophes.
`a_malformed_query_is_an_error_not_a_silent_empty_result` est tombé, et sa justification écrite
— « une erreur, pas un silence » — **couvrait** le cas : `subject:(` est une grammaire qu'on a
voulue et ratée. Lui rendre les messages contenant le mot « subject » n'est pas un silence, c'est
pire. **Un refus se corrige ; une liste sans rapport se croit.**

C'est la règle du 2026-09-10 appliquée dans l'autre sens. Quand un test gêne, on relit sa
justification ; ce jour-là elle ne couvrait que la moitié du cas et le test a bougé. Ici elle
tenait, et c'est le code qui a bougé.

#### Le relevé de référence

`cargo xtask measure-recall`, 22 requêtes, corpus réel de 5 063 messages :

| sous-ensemble | requêtes | dans le top 10 | rappel |
|---|---:|---:|---:|
| **aucun mot commun avec la cible** | 10 | **0** | **0,0 %** |
| au moins un mot commun | 12 | 7 | 58,3 % |
| tout le jeu | 22 | 7 | 31,8 % |

**Zéro n'accuse pas le banc, cette fois.** La règle du projet veut qu'on suspecte l'instrument
devant un chiffre extrême, et c'est ce qui a été fait — mais ici zéro est ce que la définition
prédit : si la requête ne partage aucun mot avec sa cible, BM25 n'a littéralement rien à
accrocher. Le message ne peut sortir que par accident. Les contrôles le confirment dans l'autre
sens : « Colissimo », « AdSense désactivé », « VALORANT closed beta » et « assemblée générale CESI
Alumni » sont tous trouvés.

Ce que le 58,3 % dit, en revanche, est moins évident et plus utile : **partager un mot ne suffit
pas**. « Les émissions que je n'ai pas eu le temps de voir » partage un mot avec sa cible et ne la
trouve pas ; « ma participation au jury » la trouve au rang 50. Un mot incident noyé dans un corps
de mail ne pèse rien en BM25 — ce qui veut dire que le vrai périmètre du problème est plus large
que les 10 requêtes du sous-ensemble dur.

La barre est donc sans ambiguïté pour l'étape 3 : **tout ce qui dépasse 0 % sur le sous-ensemble
dur est un gain**, et le critère en demande 80.

Vérification : `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`,
`cargo test --workspace` — **1 337 tests verts**, soit 3 de plus : l'apostrophe remplacée par une
espace et non par rien, la typographique traitée comme la droite, et le contrôle qui protège le
refus de `subject:(`.

### 2026-09-15 (suite) — le modèle choisi par 400 octets, et le critère qu'il crève

L'étape 3 commence par le choix du modèle. Deux familles possibles en Rust sans Python :

- **un vrai transformeur** — `candle` plus `candle-transformers`, qui sait faire du BERT, avec un
  modèle multilingue type `multilingual-e5-small` ;
- **des embeddings statiques** — `model2vec-rs`, qui distille un transformeur en une **table de
  vecteurs par token** : à l'inférence, il n'y a plus de réseau, seulement une moyenne pondérée
  de lignes.

La règle du projet dit de commencer par le moins cher qui pourrait marcher, et `model2vec-rs`
porte en plus un argument qu'aucune alternative n'a : une variante `local-only` **sans client HTTP
compilé dedans**. « Zéro requête réseau » y devient vrai par construction, comme pour `mailcal`.

Le seul modèle multilingue publié est `potion-multilingual-128M`. Avant de télécharger ses 530 Mo,
la forme du tenseur se lit dans l'en-tête du fichier `safetensors`, que la première requête de
plage rend :

```
{"embeddings":{"dtype":"F32","shape":[500353,256],"data_offsets":[0,512361472]}}
```

**500 353 tokens × 256 dimensions en `f32`, soit 488 Mio de table.** Le critère 5 en accorde 500
pour *toute* la passe. Et le chemin de chargement du crate est pire que ça : `from_pretrained`
fait un `fs::read` du fichier entier — 488 Mio de `Vec<u8>` — puis construit un `Vec<f32>` de
488 Mio, donc une **crête de l'ordre de 950 Mio**. Le crate n'a pas de chemin qui garde la table
en `f32` sans la matérialiser : `from_borrowed` demande un `&'static [f32]`, et même une table
rangée en `i8` est convertie en `f32` au chargement.

Le critère 5 est donc crevé **avant qu'un seul vecteur ait été calculé**. Trois remarques.

**Le coût évité.** Quatre cents octets lus ont remplacé un téléchargement de 530 Mo et une
mesure. C'est la question la moins chère posée en premier, comme la passe d'index à vide du
2026-09-11 — et la réponse est la même : la mesure la plus rentable est celle qu'on n'a pas eu
besoin de faire.

**Le seuil était hérité, et il a été écrit sans savoir.** Les 500 Mo viennent du critère 2 de la
phase 2, où ils mesuraient le démon pendant une moisson. Ils ont été recopiés dans cette phase le
matin même, avant de savoir ce que pèse un modèle local. Ce n'est pas une raison pour le
déplacer — c'est une raison pour le dire.

**Le levier existe, et il est propre.** La table a 500 353 lignes parce que le vocabulaire est
celui de `bge-m3` ; un corpus de courrier français et anglais n'en emploie qu'une fraction.
`model2vec-rs` porte un `token_mapping` fait pour ça. Ne garder que les lignes que le corpus
utilise est une transformation **locale**, faite à l'installation, et son argument de justesse
tient en une phrase : un token qu'aucun message ne contient ne peut faire remonter aucun message.

Ce qui reste à trancher, et qui n'est pas une question technique : **tailler le vocabulaire** —
du travail en plus, un modèle plus petit et un chargement plus rapide — ou **assumer le seuil** et
le réécrire en distinguant ce que pèse le modèle de ce que coûte la passe.

### 2026-09-15 (suite) — le vocabulaire taillé, et le coupable qui n'était pas le bon

Le levier décidé plus haut : ne garder de la table que les lignes que le corpus emploie.
`cargo xtask model-trim` le fait, sur le store réel :

```
Messages parcourus 5063
Tokens employés    34853
Vocabulaire        34853 gardés sur 500353   (7.0 %)
Table avant        488.6 Mio
Table après        35.9 Mio   dont 1.9 Mio de carte
Rapport            13.6 ×
```

**Sept pour cent.** Un corpus de courrier français et anglais n'emploie qu'une fraction d'un
vocabulaire fait pour cent langues, et l'ordre de grandeur n'était pas garanti d'avance.

#### La carte doit couvrir ce qu'on jette

`model2vec-rs::pool_ids` lit `mapping[token]` et, **à défaut, se rabat sur le numéro du token
comme index de ligne**. Sur une table de 34 854 lignes, un token numéroté 480 000 — parfaitement
légal, il suffit d'une langue absente du corpus — sortirait de la table et ferait paniquer
`ndarray`. La carte porte donc les 500 353 entrées, y compris les 465 500 qu'on jette, qui
pointent vers une **ligne nulle** placée en tête. Deux mébioctets, qui ne se discutent pas.

Un token écarté dilue alors légèrement la moyenne du message où il apparaît, au lieu de la
fausser ou de planter — et c'est le comportement qu'on veut pour un mot qui n'existe dans aucun
message : il ne dit rien, il ne doit rien peser.

#### La vérification qui n'est pas optionnelle

Une carte fausse d'un cran donne un modèle qui **marche** — il charge, il encode, il rend des
vecteurs de la bonne taille — et dont chaque vecteur est faux. Rien ne le signalerait : ni le
chargement, ni la recherche, qui rendrait des résultats médiocres qu'on mettrait sur le compte du
modèle. `cargo xtask model-check` encode les mêmes messages avec les deux modèles et compare :

```
Messages comparés  200
Écart maximal      0e0
Verdict            identique — la carte est juste
```

Zéro exact, pas un epsilon. Le seuil était pourtant fixé à 1e-6, parce qu'une addition de
flottants n'est pas associative et que la ligne nulle décale les adresses — mais l'ordre de
parcours étant le même des deux côtés, l'égalité est bit à bit.

#### Et le relevé qui désigne un autre coupable

`cargo xtask model-bench`, trois exécutions, la première jetée, sur le modèle taillé :

| | |
|---|---|
| encodage | **0,161 ms par message**, soit **6 200 messages/s** |
| dispersion | 0,161 / 0,163 / 0,161 ms — le relevé le plus stable du projet |
| RSS crête | **655 Mio** |

Le débit règle le critère 3 sans discussion : les 73 658 messages du corpus tiennent en une
douzaine de secondes d'encodage. Mais **655 Mio pour une table de 36**, c'est un chiffre qui
n'a pas de sens, et un relevé unique aurait fait conclure que le modèle taillé coûte quinze fois
sa taille.

La décomposition par palier, que le critère 5 exigeait déjà — « la mesure dit **où** elle va » :

| palier | RSS | ajouté |
|---|---:|---:|
| au démarrage | 8,7 Mio | |
| store et index tantivy | 11,5 Mio | +2,8 |
| lignes du corpus en RAM | 15,3 Mio | +3,7 |
| **tokeniseur seul** | **502,0 Mio** | **+486,8** |
| table d'embeddings par-dessus | 546,5 Mio | +44,5 |

**Le coupable est le tokeniseur, pas la table.** Charger les 500 353 entrées de `bge-m3` avec le
crate `tokenizers` coûte 487 Mio — vingt-sept fois le poids du `tokenizer.json` qui les décrit.
La table, une fois taillée, ne pèse plus que 44 Mio à côté.

Autrement dit : **la taille a marché, et elle a réglé la plus petite des deux moitiés.** On a
retiré 452 Mio d'un problème qui en faisait 975, et les 487 restants sont ailleurs.

Ce que ça vaut quand même : la table taillée est un gain acquis — moins d'installation, moins de
chargement, et un modèle qui reste identique au bit près. Ce que ça ne règle pas : le critère 5,
qui reste crevé par un composant qu'on n'avait pas regardé.

La leçon est la même que celle du 2026-09-10, sur un autre objet : **un chiffre qui n'a pas de
sens accuse la mesure avant d'accuser ce qu'elle mesure** — et ici c'était vrai deux fois, parce
que la première décomposition accusait encore le mauvais composant tant que le tokeniseur n'était
pas mesuré seul.

### 2026-09-15 (suite) — le tokeniseur taillé, et le critère 5 qui passe de 655 à 100 Mio

La décomposition précédente avait désigné le tokeniseur : 487 Mio à lui seul, contre 44 pour la
table. Le même levier s'y applique, et il y est plus fort.

#### Renuméroter fait disparaître la carte

Le premier jet gardait la numérotation d'origine et rattrapait avec une carte de 500 353 entrées.
Tailler **aussi** le tokeniseur change la donne : un token qu'il ne connaît plus ne peut plus être
produit, donc il n'y a plus rien à rattraper. Les identifiants deviennent `0..n`, la table est
dans cet ordre, et la correspondance est l'identité. Plus de carte, plus de ligne nulle.

Ce qui rend la renumérotation sûre côté **segmentation**, et l'argument vaut d'être écrit :
Unigram choisit le découpage de score maximal parmi les morceaux disponibles. Les morceaux
retirés sont exactement ceux qu'aucun message n'a produits, donc le chemin optimal d'un texte du
corpus est toujours là — et retirer des options ne peut pas rendre meilleur un chemin qui ne
l'était pas. Pour un texte **neuf**, le découpage peut changer : c'est la contrepartie assumée,
et `model-check` est ce qui la met à l'épreuve.

#### Le token qu'on croyait garder et qu'on jetait

La première exécution s'est arrêtée sur `un token ajouté a été écarté`. La collecte cherchait les
tokens spéciaux dans la **différence** entre les deux vocabulaires du tokeniseur, celui avec les
tokens ajoutés et celui sans. Cette différence est **vide** : `[PAD]` et `[UNK]` sont déclarés en
tokens ajoutés *et* présents dans le vocabulaire du modèle. N'apparaissant dans aucun message,
ils étaient écartés — et un tokeniseur sans son token inconnu ne tokenise plus rien.

Ils se lisent maintenant là où ils sont écrits : `added_tokens[].id` et `model.unk_id`, dans le
fichier. Une astuce d'API remplacée par la lecture de la déclaration.

#### Le relevé

```
Vocabulaire        34 854 gardés sur 500 353   (7,0 %)
Table avant        488,6 Mio      Table après        34,0 Mio
Tokeniseur avant    17,8 Mio      Tokeniseur après    1,4 Mio
```

Et l'équivalence, sur 300 messages réels, avec une segmentation qui aurait pu bouger :

```
Écart maximal      0e0
Verdict            identique — la carte est juste
```

Ce que ça donne au chargement, trois exécutions, première jetée :

| | avant la taille | après |
|---|---:|---:|
| tokeniseur seul | 486,8 Mio | **22,0 Mio** |
| modèle chargé | 546,5 Mio | **71,8 Mio** |
| **RSS crête** | **655,6 Mio** | **99,9 Mio** |
| chargement | 1,17 s | **49 à 59 ms** |
| par message | 0,161 ms | 0,171 à 0,177 ms |

**Le critère 5 passe de crevé à tenu avec cinq fois de marge** : 100 Mio contre 500. Et le
chargement est vingt fois plus rapide, ce qui compte pour le critère 7 — une coquille qui met une
seconde de plus à démarrer parce qu'un modèle se charge ne serait pas acceptable.

Une honnêteté sur le débit : il est **légèrement moins bon** après la taille, 0,171–0,177 ms
contre 0,161–0,163 avant, soit 8 % de plus. L'écart est constant sur les trois exécutions, donc
ce n'est probablement pas du bruit — et il n'est pas expliqué : retirer la carte devrait retirer
un accès indirect, pas en ajouter. Le dire vaut mieux que de ne relever que ce qui arrange. À 5
700 messages/s, le corpus entier reste à une quinzaine de secondes d'encodage.

Ce que l'étape 3 laisse pour la suite : le modèle est choisi, mesuré, et tient dans le budget.
Reste à écrire le magasin de vecteurs — étape 4 — dont la taille est déjà connue, puisqu'elle ne
dépend que du nombre de messages et de la dimension : **73 658 × 256 × 4 octets, soit 72 Mio**.
