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
| 1 | **Trouver ce qu'un mot-clé ne trouve pas** | sur un jeu de requêtes réelles écrites à la main avec leur réponse attendue : le message visé dans les **10 premiers** pour **≥ 80 %** des requêtes, et **strictement mieux que tantivy seul** sur le sous-ensemble des requêtes qui ne partagent **aucun mot** avec le message visé | **NON TENU, remesuré le 2026-09-22** avec un transformeur (`multilingual-e5-small`) après l'avoir été le 2026-09-21 avec un embedding statique : **0,0 %** sur les 10 requêtes sans mot commun, pour les trois bras et **pour les deux familles de modèles**. **Et la mesure ne départage pas les moteurs**, ce qui est le vrai état au 2026-09-23. Le banc a été refait ce jour-là : le bras plein texte était **censuré à 500 résultats** en silence — donc ses « absente » voulaient dire « au-delà de 500 », et aucun de ses rangs n'était comparable —, la partition reposait sur « partage un mot » que quatre requêtes réelles ont réfuté, et les conclusions étaient tirées de cellules de 3 à 6 requêtes. La décision est maintenant portée par une **comparaison appariée sur tout le jeu** (test de signe exact, rapport géométrique des rangs), et elle rend : plein texte contre hybride **7 victoires contre 15, p = 0,134, ×0,43** — il en aurait fallu 17 sur 22. Les trois paires sont **indécidables à n = 26**. Rien n'est établi sur la comparaison des modèles entre eux ; ce qui est établi est la distance à parcourir, soit **deux victoires** |
| 2 | Latence de la recherche, bout en bout depuis le client | **< 50 ms** en p95 sur le corpus réel — le même seuil que le critère 4 de la phase 1, parce que c'est la même attente : une liste qui arrive pendant qu'on lâche la touche | **balayage mesuré le 2026-09-21** — `cargo xtask measure-scan`, 73 658 vecteurs de dimension 256 : **6,2 ms** de p95 au cas ordinaire, **10,2 ms** au pire cas du tas borné. Il reste 40 ms pour l'encodage de la requête et l'aller-retour JSON-RPC ; le bout en bout se mesure à l'étape 8 |
| 3 | Coût de la passe complète | le corpus entier vectorisé, **mesuré en messages/s et en durée totale**, et la passe doit être **reprenable** : tuée au milieu, elle repart où elle en était sans recalculer ce qui est fait | **tenu, remesuré le 2026-09-22** — la reprise a été vérifiée **sur le terrain** ce jour-là : une passe tuée à 37 472 messages a repris à 36 186, soit exactement le complément. Le coût, lui, dépend du modèle et de deux ordres de grandeur : statique **948 s** (78 msg/s) remesuré le jour même, transformeur **4,68 msg/s**, soit **4,37 h** — mesurées sur le corpus entier, la passe complète ayant été refaite le soir même pour que le magasin porte sa provenance. Et le débit varie d'un facteur 3,5 **à l'intérieur** d'une passe — `BatchLongest` fait payer à chaque lot son texte le plus long, donc le débit suit la distribution des longueurs. Drapeau par ligne (`SCHEMA_V14`), écriture idempotente |
| 4 | Coût d'un message qui arrive | **< 200 ms** de travail de fond, à comparer à sa propre passe complète — le même tableau que `measure-followup` pour l'index, le carnet et les fils | à faire |
| 5 | Mémoire de la passe complète | **RSS < 500 Mo**, le seuil du critère 2 de la phase 2. Et la mesure dit **où** elle va : le modèle, le lot, le magasin de vecteurs | **tenu avec l'embedding statique, NON TENU avec le transformeur (2026-09-22)**. Statique : **117 Mio** de crête — 8,7 au démarrage, 12,7 store ouvert, 69,5 modèle chargé, 117 passe finie. Transformeur : **1 304 Mio** de crête et 985 de résident, soit **2,6 fois le seuil**. Le remède est celui du 2026-09-15 — tailler le vocabulaire — et il porte ici sur deux fichiers, le `tokenizer.json` de 17 Mo et `embeddings.word_embeddings.weight`. Le relevé du 2026-09-22 établit au passage que **ce rognage ne coûte rien en rappel** : taillé contre brut, 3 680 contre 3 901 sur les cibles françaises, l'écart est du côté du bruit |
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

4. ~~**Le magasin de vecteurs et le balayage.**~~ Fait le 2026-09-21 : `mailcore::vectors`, un
   fichier plat en ajout seul, et `cargo xtask measure-scan`. La latence d'un balayage ne dépend
   pas de ce que les nombres veulent dire, donc le banc tourne sur des vecteurs fabriqués et
   n'attend pas le moteur. **6,2 ms de p95**, et le pire cas du tas borné à 10,2. Voir le
   journal.

5. ~~**La passe complète et sa reprise** — critères 3 et 5.~~ Fait le 2026-09-21 : le crate
   `mailvec` — modèle, extraction du texte, passe — plus `SCHEMA_V14` et
   `cargo xtask measure-vectors`. **339 s pour 73 658 messages, 117 Mio de crête.** Et le défaut
   que l'étape a trouvé au passage : les citations en texte brut n'étaient reconnues **nulle
   part**, ni pour la vectorisation ni pour l'affichage. Voir le journal.

6. ~~**La fusion avec tantivy**, et le critère 1 mesuré pour de bon.~~ Fait le 2026-09-21 :
   `mailcore::fusion` (rangs réciproques), `mailvec::search::Hybrid`, et le banc à trois bras.
   **Le critère 1 n'est pas tenu** — 0,0 % sur le sous-ensemble dur, pour les trois moteurs. Le
   modèle est la limite, et la mesure dit de combien. Voir le journal, et l'étape 6 bis.

6 bis. ~~**Changer de modèle, et remesurer.**~~ Fait le 2026-09-22 : `mailvec::bert`, `candle` et
   `multilingual-e5-small`. **Le critère 1 n'est toujours pas tenu**, et le relevé dit pourquoi —
   le modèle améliore la synonymie d'un facteur trois et n'améliore pas l'interlangue, là où sept
   des dix requêtes dures sont interlangues. Le critère 5 est crevé au passage (1 304 Mio). Voir
   le journal, et les étapes 6 ter et 6 quater.

   Le pari qui autorisait cette étape — « un modèle trente fois plus lent ferait passer la passe
   de 6 à 12 minutes » — était faux d'un facteur trente-cinq : elle passe à **4,37 h**. Les 4 %
   étaient mesurés, le « trente fois » ne l'était pas.

6 ter. ~~**Scinder le jeu de requêtes en « synonymie » et « interlangue », et rejouer les deux
   modèles sur les deux.**~~ Fait le 2026-09-22 : le partage est **calculé** — un comptage de
   mots-outils propres à une langue, sur le texte que l'indexation voit — et il rend une
   troisième réponse, **indéterminée**, plutôt que de trancher un message mixte au hasard. Le
   magasin de vecteurs porte sa provenance (format de version 2 : modèle, famille, empreinte des
   poids, budget, date), elle est **opposable à l'écriture**, et `measure-recall` l'imprime.

   Ce que l'étape a trouvé au passage et qui déplace des chiffres déjà écrits : la fonction qui
   calculait le rang médian rendait le **plus grand** des deux rangs du milieu sur un lot pair.
   Elle avait été écrite pour n = 22 ; la scission a fait des lots de trois et six, et le biais
   est devenu visible. Le relevé imprime maintenant les **rangs triés** à côté de la médiane, ce
   qui aurait suffi à le voir sans le chercher. Voir le journal.

   **Ce qui reste à faire avant qu'un modèle de plus soit téléchargé** : le jeu est trop petit là
   où la phase se joue. Trois requêtes de synonymie ne départagent pas un facteur deux. Le jeu
   doit grossir **du côté de la synonymie française** — du travail de vérité terrain, pas de
   moteur, donc il passe devant, comme à l'étape 1.

6 quater. **Le texte des pièces jointes, en amont des deux index.** Décidé le 2026-09-22, et placé
   **devant** les étapes 7 et 8. Le contenu d'une pièce jointe n'entre dans aucun index : une
   facture en PDF ne se trouve ni par son montant ni par sa référence, et aucun modèle ne le
   rattrapera puisque le texte n'est nulle part. C'est le premier cas d'usage de `docs/VISION.md`.

   Pourquoi devant : le critère 1 a échoué avec deux familles de modèles, donc la suite de la
   piste sémantique est un pari. Ce manque-ci est **certain, borné, et il sert les deux moteurs** —
   le plein texte en profite autant que le sémantique, et sans dépendre d'un modèle.

   - ~~**Compter avant d'écrire.**~~ Fait le 2026-09-22 : `cargo xtask attachment-census`. 7 094
     messages à pièce jointe, et un ordre de travail qui n'était pas celui qu'on croyait — les
     images dominent en nombre et ne portent pas de texte, les invitations en valent plus que
     les PDF et se lisent avec du code déjà écrit.
   - ~~**Les formats gratuits** : invitations et `text/*`.~~ Fait le 2026-09-23 :
     `mailcore::attachtext`, partagé par les deux index. **1 363 messages** gagnent un mot que le
     reste du message n'a pas, sans une ligne de dépendance nouvelle. Aucune régression sur le
     banc, et aucun gain non plus — le jeu de requêtes ne vise aucune pièce jointe, ce qui est une
     limite de l'instrument et non un résultat.
   - **Le PDF**, 1 222 messages, en attente d'une décision : il demande une dépendance qui
     analysera 1,1 Gio de fichiers écrits par des inconnus. Le gain attendu est du même ordre que
     ce que les formats gratuits viennent de rapporter.
   - **Hors périmètre, écrit pour ne pas le redécouvrir** : l'OCR des images (4 659 messages, donc
     un second modèle à installer) et `application/ms-tnef` (738 messages), qui est un conteneur
     et se traitera après les formats qu'il contient, jamais avant.

   Et ce que l'étape impose au jeu de requêtes : il doit grossir **dans deux directions** — la
   synonymie française (étape 6 ter) et le contenu des pièces jointes, faute de quoi le banc reste
   muet sur ce qu'on vient de livrer.

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

### 2026-09-21 — le balayage tient en 6 ms, et le banc a failli chronométrer le mauvais binaire

L'étape 4 : `mailcore::vectors`, un magasin et un balayage, plus `cargo xtask measure-scan`. Les
deux se mesurent **avant** qu'un seul vrai vecteur existe, parce qu'un produit scalaire coûte le
même prix sur un vecteur qui décrit un message et sur un vecteur tiré au sort.

#### Le format : ni SQLite, ni un index approché

Un fichier plat, `MAILVEC1`, seize octets d'en-tête et un record de `8 + 4×dim` par message.

**Pas SQLite**, parce que le balayage veut les octets contigus : une colonne `BLOB` ferait 73 658
lectures de lignes par requête, et la comparaison n'a besoin d'aucune des garanties
transactionnelles qu'on paierait. **Pas de HNSW** non plus — la phase l'a refusé tant qu'un
balayage complet n'aura pas été montré insuffisant, et le relevé plus bas dit qu'il ne l'est pas.

Il est **ajout seul**, et c'est ce qui rend l'ordre des écritures sûr. Un message vectorisé deux
fois écrit deux records ; la relecture garde le dernier, donc l'écriture est **idempotente**, donc
« vecteur durable **puis** drapeau posé » se coupe sans conséquence. C'est la leçon du 2026-09-11
sur l'index plein texte, réemployée telle quelle. Corollaire à l'autre bout : un processus tué au
milieu d'une écriture laisse une queue partielle, et la réouverture se replace **sur une frontière
de record** plutôt que de faire du record suivant un décalage de quatre octets.

#### Le piège du profil de débogage, que les bancs voisins n'avaient pas

C'est la vraie leçon de l'étape, et elle ne parle pas de vecteurs.

`measure-search` chronomètre tantivy ; `model-bench` chronomètre `model2vec-rs`. Ce sont des
**dépendances**, donc le `[profile.dev.package."*"]` du `Cargo.toml` racine les optimise même en
débogage, et les lancer par `cargo xtask` donne des chiffres justes. Le balayage, lui, vit dans
`mailcore`, qui est un **membre de l'espace de travail** — que cette ligne ne couvre pas. Le même
geste, sur le banc d'à côté, aurait rendu un p95 sans rapport avec le binaire livré, et **rien
dans la sortie ne l'aurait dit**.

`measure-scan` refuse donc de tourner en débogage, au lieu d'avertir : un relevé imprimé finit
recopié. De combien le chiffre faux se serait écarté, on ne le saura pas — le refus a été écrit
avant d'avoir relevé la valeur fausse, et aller la chercher maintenant demanderait de retirer le
garde-fou.

#### Le pire cas du tas borné valait d'être fabriqué

Le tas écarte un message dès que son score passe sous le pire des retenus. Cette porte est franchie
d'autant plus souvent que les scores arrivent en ordre croissant — ce qui n'arrive jamais par
hasard, donc jamais sur des vecteurs tirés au sort. Le banc fabrique ce cas : un arc du plan porté
par les deux premiers axes, dont le cosinus à la requête croît strictement avec le rang.

Il coûte **1,6 fois** le cas ordinaire. Un banc qui ne l'aurait pas fabriqué aurait publié le
chiffre flatteur, et la marge réelle serait restée inconnue jusqu'au jour où un corpus la
révélerait.

#### Le relevé

Quatre exécutions, la première jetée, `xtask.exe` en release, 73 658 vecteurs de dimension 256 :

| | p95 épars | p95 croissant |
|---|---:|---:|
| exécution 2 | 6,14 ms | 8,95 ms |
| exécution 3 | 5,93 ms | 9,88 ms |
| exécution 4 | 6,17 ms | 10,20 ms |

La dispersion se lit avant la médiane, et elle est de 4 % sur le cas ordinaire : le relevé est
sain. Reste une anomalie qu'on ne cache pas — la **première** exécution a rendu un RSS de 92,5 Mio
contre 81 à 82 pour les trois autres, pendant que ses latences, elles, ne bougeaient pas. Elle
n'est pas expliquée ; elle est jetée, ce qui est exactement ce à quoi la règle sert.

Sur la mémoire : **82 Mio de RSS pour un fichier de 72,5** — une dizaine de mébioctets au-dessus
des octets eux-mêmes. Ce qu'on peut en dire du critère 5 est une **addition et pas une mesure** :
le modèle taillé pèse 71,8 Mio, donc un processus qui ferait les deux tiendrait autour de 154, soit
trois fois sous le seuil. Ça se mesurera à l'étape 5, sur le vrai processus.

#### Ce que le critère 2 attend encore

Le banc mesure le **balayage**, pas le trajet. 6,2 ms des 50 accordées, il en reste 44 pour
l'encodage de la requête — 0,17 ms mesuré à l'étape 3 — et pour l'aller-retour JSON-RPC, que
`measure-api` sait déjà chiffrer. Le bout en bout se relèvera à l'étape 8, quand il y aura une
méthode à appeler ; d'ici là, le critère est **en bonne voie et pas tenu**, et le tableau le dit
dans ces termes.

Vérification : `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`,
`cargo test --workspace` — **1 353 tests verts**, dont 17 neufs sur le magasin : le record coupé en
deux, le magasin d'une autre dimension refusé des deux côtés, le vecteur écrit deux fois qui n'en
laisse qu'un, la limite qui garde les meilleurs et non les premiers vus, et le message dont aucun
token n'est connu qui rend des zéros et non des `NaN`.

### 2026-09-21 — la passe complète, et la citation que personne ne reconnaissait

L'étape 5 : le crate `mailvec` — le modèle, l'extraction du texte, la passe —, la colonne
`messages.vectorised` (`SCHEMA_V14`) et le banc `measure-vectors`. Les critères 3 et 5 tombent
tous les deux, et l'étape a trouvé un défaut qui ne parle pas de vecteurs.

#### Le défaut : une citation en texte brut n'en était une pour personne

`docs/PHASE-4.md` posait la règle avant la première ligne de code : la citation se retire au
**même endroit** que celui qui la reconnaît pour l'affichage, sinon les deux divergent. Le chemin
d'affichage est `mail-parser::body_html` → `sanitize` → `blocks`, et `blocks` marque les
`<blockquote>`. Il suffisait donc de jeter ces blocs-là.

Le test écrit pour le vérifier — sur de vrais octets RFC 5322, pas sur du HTML fabriqué — est
tombé :

```
la citation en texte brut est entrée dans le vecteur :
Re: chaudiere. Plomberie Martin. Bien recu, merci. > Voici le devis pour la chaudiere.
```

**La conversion de `mail-parser` remplace `\n` par `<br/>` et rien d'autre.** Les `>` de citation
survivent comme texte ordinaire, donc `blocks` n'y voit aucune citation. Et la conséquence
dépasse la phase : **la coquille ne les distinguait pas non plus**, depuis toujours. Il n'existait
aucun reconnaisseur de citation en texte brut dans le projet — ce qui rendait la règle de la
phase inapplicable, puisque l'endroit qu'elle désignait n'existait pas.

Le remède est `mailhtml::plain`, appelé par `Mailbox::render` **et** par `mailvec::text` : les
parties texte y sont converties par nous, les lignes `>` deviennent des `<blockquote>` imbriqués,
et il n'y a toujours qu'un seul détecteur. Le lecteur y gagne au passage, ce qui n'était pas le
but.

Deux choses à ne pas défaire :

- **une ligne blanche ferme le paragraphe, pas la citation.** Une citation réelle en contient ;
  les traiter comme une sortie découperait un fil en une dizaine de blocs alternés ;
- **`&` est échappé, et il ne l'était pas.** Un `&amp;` écrit en toutes lettres dans un mail en
  texte brut ressortait en `&`. C'est le symétrique exact du piège que le commentaire de `render`
  relevait déjà pour `<script>` : là l'échappement sauvait le texte, ici son absence le mangeait.

Ce que le remède ne fait **pas**, délibérément : reconnaître « Le 12 mars, Untel a écrit : ». Il
faudrait une liste de tournures par langue, elle serait fausse pour quelqu'un, et se tromper
efface un paragraphe légitime. Une citation non reconnue est un défaut mineur ; un texte effacé à
tort ne l'est pas.

Et la leçon de méthode : **aucun test existant n'est tombé sur ce changement de rendu.** Le
chemin texte brut n'était couvert par rien. La couverture vient des tests neufs, pas d'une
non-régression — et c'est l'aveu qu'il faut écrire plutôt que de compter 1 388 verts comme une
preuve que rien n'a bougé.

#### Un trait pour que la passe soit testable du tout

Le modèle n'est pas dans le dépôt : il s'installe. Une passe qui l'exigerait n'aurait **aucun
test** — et c'est la fonction du projet où une coupure mal gérée perd des messages en silence,
donc la dernière à laisser sans contrôle négatif. D'où `pass::Encoder`, deux méthodes, et un
encodeur déterministe côté test qui compte ce qu'on lui demande d'encoder.

Ce que ce compteur rend visible, et qu'aucune assertion sur le résultat ne verrait : le travail
**refait**. Une reprise qui recalculerait tout donne exactement le même magasin.

#### Le relevé

Trois exécutions, la première jetée, sur les 73 658 messages du corpus réel :

| | passe | débit | chargement | RSS crête |
|---|---:|---:|---:|---:|
| exécution 1 | 392,07 s | 188 /s | 296 ms | 117,1 Mio |
| exécution 2 | **339,60 s** | **217 /s** | 68,6 ms | 117,7 Mio |
| exécution 3 | **339,11 s** | **217 /s** | 68,4 ms | 117,0 Mio |

**Le biais du `CLAUDE.md`, pour la cinquième fois.** La première exécution s'écarte de 15 % et les
deux suivantes s'accordent à 0,14 % près ; le chargement du modèle y est quatre fois plus lent,
ce qui désigne le cache de fichiers. La parade — trois exécutions dont on jette la première —
marche encore, et c'est la dispersion, lue avant la médiane, qui l'a rendue lisible.

Décomposition de la mémoire, que le critère 5 exige : 8,7 Mio au démarrage, 12,7 store ouvert,
69,5 modèle chargé, **117 passe finie**. Quatre fois sous le seuil — le même critère était crevé à
655 Mio le 2026-09-15, avant la taille du vocabulaire et du tokeniseur.

Et le contrôle de reprise, à l'échelle du corpus — `measure-vectors --resume` :

```
Première passe    73658 vecteurs
Seconde passe     0 vecteurs, 119.23ms
Verdict           rien à refaire — la reprise ne recalcule pas
```

Les 119 ms sont le **contrôle de cohérence du critère 9** : `update_with` relit le magasin pour
voir s'il contredit les drapeaux, et relire 72 Mio coûte ça. Le chemin réel, `update`, ne les paie
pas — son `COUNT` sur colonne indexée répond avant, et c'est exactement ce que ce raccourci est là
pour éviter. Le banc appelle la variante sans raccourci parce que c'est celle qui exerce le
contrôle ; le dire évite de lire ces 119 ms comme le coût d'une moisson.

#### Le chiffre qui compte n'est pas celui du critère

L'étape 3 avait mesuré l'encodage seul à **5 700 messages/s**. La passe complète tourne à
**217**. Les 73 658 messages coûtent donc **13 s d'encodage sur 339** : le modèle, qui était tout
le sujet de la phase jusqu'ici, pèse **4 %** du coût de la passe. Le reste est la relecture des
blobs, le parsing MIME, l'assainissement HTML et le découpage en blocs.

C'est la même forme que la leçon des fils du 2026-09-13 — « ce qui coûte n'est pas l'algorithme
mais la relecture des blobs » — et ça vaut d'être noté avant l'étape 6 : optimiser le modèle
n'aurait presque aucun effet sur cette passe, et l'échanger contre un modèle plus lourd en
coûterait beaucoup moins qu'on ne le croirait. La marge est du côté qu'on ne regardait pas.

À dire aussi : `mailvec::text` passe par `sanitize` puis `blocks`, ce qui est **plus cher**
qu'aplatir le HTML comme le fait l'index plein texte. Le choix était écrit comme assumé avant la
mesure, au nom de l'unicité du détecteur de citation. Il est maintenant chiffré, et il reste
assumé : 339 s pour un corpus entier est une passe qu'on lance une fois.

#### Ce que la passe compte et ne cache pas

`Stats` porte `missing_blobs` et `empty` à côté de `vectorised`, et les trois sont distincts. Un
blob absent est **marqué quand même** — sinon la ligne revient à chaque passe pour toujours, et le
raccourci du `COUNT` qui évite de charger le modèle ne servirait plus jamais — mais il n'est pas
compté comme vectorisé : annoncer un bilan complet sur des messages qui n'ont pas de vecteur
serait le genre de faux qui rassure.

Sur ce corpus : **0 blob absent, 13 textes vides** sur 73 658. Le second chiffre est celui qui
aurait pu accuser l'extraction, et treize sur soixante-treize mille dit qu'elle ne perd rien.

Vérification : `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`,
`cargo test --workspace` — **1 388 tests verts**, soit 35 de plus. Les quatre qui portent l'étape :
une passe annulée qui repart où elle s'est arrêtée sans réencoder ce qui était fait, des vecteurs
écrits mais non marqués qui sont réécrits **sans doubler** le magasin, un magasin disparu détecté
au point d'usage — avec son contrôle inverse, qui vérifie qu'un magasin présent ne déclenche
**pas** de passe complète —, et un changement de dimension refusé plutôt qu'ajouté derrière.

### 2026-09-21 (suite) — le critère 1 mesuré, et il n'est pas tenu

L'étape 6 : la règle de fusion, la recherche hybride, le banc à trois bras. Elle répond à la
question qui décide de la phase, et la réponse est non. Elle dit aussi **pourquoi**, ce qui est
la seule chose qui rende un non utile.

#### Avant le résultat : le jeu de requêtes était périmé, et le garde-fou n'a rien vu

Le jeu du 2026-09-15 désignait ses cibles par `messages.id`. Le store a été réimporté depuis, et
`messages.id` est un `rowid` attribué à l'import :

```
--- l'ancien identifiant désigne-t-il encore le bon message ? ---
Vérifiables        18 (celles dont la note cite un sujet)
Encore justes      0
Ayant glissé       18
```

**Dix-huit sur dix-huit.** Le banc avait pourtant un contrôle, écrit exprès le 2026-09-15 : « une
cible absente fait échouer la commande ». Il n'a rien dit, parce que les 22 cibles **existaient**
toutes — elles désignaient simplement d'autres messages. Le bon geste sur la mauvaise chose.

Ce que ça vaut comme leçon, et elle dépasse ce banc : **un contrôle d'existence ne contrôle pas
une identité.** Ce qui doit désigner un message au-delà de la vie d'un store le désigne par son
**contenu** — `BlobHash`, l'identité que le projet emploie déjà pour dédupliquer. Elle survit à un
réimport, à un changement de machine, à un renumérotage. `Store::message_by_blob` est la porte, et
le banc refuse maintenant un jeu à l'ancien format en disant quoi lancer.

#### Et une décision sur ce qu'est une bonne réponse

Sur 73 658 messages, « quand mon paquet est arrivé » a **neuf** bonnes réponses : neuf avis de
livraison. Exiger celle que l'auteur du jeu avait sous les yeux en septembre ne mesure pas le
moteur, ça mesure sa chance. Le format porte donc **toutes** les réponses acceptables, et une
requête est satisfaite dès qu'une d'elles est dans les dix premiers.

C'est un assouplissement, et il faut le dire deux fois : **les chiffres d'aujourd'hui ne se
comparent pas à ceux du 2026-09-15**, qui étaient sur cible unique. C'est pour ça que le banc
mesure les **trois** bras sur le même jeu — la comparaison qui décide du critère est
hybride contre plein texte, et celle-là reste valable.

`cargo xtask queries-rebind` fait la migration, et il refuse ce qu'il ne peut pas déduire : 16
requêtes reliées par la phrase exacte citée dans la note, 2 par « tous les mots » quand la note
paraphrase, 4 par les mots de la requête pour les contrôles. Le relevé **dit par quelle règle**
chacune a été reliée : une règle plus faible n'invalide pas le lien, mais elle dit laquelle
relire en premier.

#### Le relevé

73 658 messages vectorisés, 22 requêtes, fusion par rangs réciproques à `K = 60` :

| sous-ensemble | plein texte | sémantique | hybride |
|---|---:|---:|---:|
| **aucun mot commun** (10) | **0,0 %** | **0,0 %** | **0,0 %** |
| au moins un mot commun (12) | 41,7 % | 41,7 % | 41,7 % |
| tout le jeu (22) | 22,7 % | 22,7 % | 22,7 % |

Trois colonnes identiques : le réflexe du projet est d'accuser l'instrument, et il a été suivi.
Le détail par requête montre que les trois bras diffèrent bel et bien — l'égalité des totaux est
une coïncidence à n = 22, pas un bug de branchement. Deux preuves indépendantes que la chaîne
marche :

- les quatre requêtes de **contrôle** sont trouvées par le sémantique seul, aux rangs 1, 1, 5 et
  7. Vecteurs, identifiants, encodage de la requête et balayage sont donc tous en place ;
- « l'application pour préparer des vacances » passe du **rang 29** en plein texte au **rang 7**
  en sémantique, et au **rang 2** après fusion. La règle de fusion fait exactement ce pour quoi
  elle a été écrite.

#### Ce que le modèle fait réellement — et la conclusion qu'il a d'abord fallu retirer

Le motif visible est net : **le sémantique ne trouve dans les dix premiers que ce qui partage un
mot avec sa cible.** 5 sur 12 là où un mot est commun, 0 sur 10 là où il n'y en a pas.

La première version de ce journal en concluait que `potion-multilingual-128M` est un « apparieur
lexical flou, pas un moteur de sens ». **C'était faux, et ça a été écrit avant d'avoir posé la
question la moins chère** — la faute de méthode que le projet documente depuis le 2026-09-10, et
qui consiste à conclure sur un composant sans être descendu d'un cran.

`cargo xtask model-probe` coûte une seconde et demande au modèle si des mots qu'un humain juge
proches le sont pour lui :

| mot | proche | cos | témoin | cos | écart |
|---|---|---:|---|---:|---:|
| acheter | acquérir | 0,719 | dormir | 0,011 | **+0,709** |
| clé de jeu | game key | 0,818 | parapluie | 0,087 | **+0,731** |
| commentaires | comments | 0,759 | betterave | 0,012 | **+0,747** |
| stockage en ligne | cloud storage | 0,376 | chaussure | −0,012 | +0,388 |
| pleine conscience | méditation | 0,360 | plomberie | 0,012 | +0,349 |
| facture | note à payer | 0,414 | bicyclette | 0,088 | +0,326 |
| signalement | dénoncé | −0,041 | casserole | 0,082 | −0,123 |

Sept paires sur dix nettement séparées, écart moyen **+0,327**, et le passage d'une langue à
l'autre marche **très bien** — ce qui démolit au passage l'explication « sept des dix ratés sont
interlangues ». Le modèle porte du sens.

#### Alors pourquoi 0 sur 10 : le signal est réel et deux cents fois trop faible

Deux hypothèses ont été écartées par la mesure plutôt que par l'argument.

**La dilution.** Un embedding statique est une moyenne ; une requête de six mots comparée à un
document de quatre cents pouvait perdre son signal. Le budget de texte est donc devenu un
paramètre, et il a été balayé — corps entier (2 000 caractères), 400, 150, et **0**, c'est-à-dire
sujet et expéditeur seuls, exactement le régime court où la sonde marche. **0,0 % sur le
sous-ensemble dur aux quatre valeurs.** La dilution n'explique rien.

**La fusion.** `K` balayé à 5, 20, 60, 200 : 0,0 % partout. On ne fusionne pas un résultat que ni
l'un ni l'autre n'a trouvé.

Restait la question qu'aucun de ces relevés ne posait : **de combien la cible est-elle manquée ?**
Un rappel binaire à dix confond « manquée de peu » et « pas du tout ». En demandant cinq mille
résultats au lieu de cinquante :

| sous-ensemble | plein texte | sémantique | hybride |
|---|---:|---:|---:|
| **aucun mot commun** — rang médian | absente | **1 992** | 2 412 |
| au moins un mot commun — rang médian | 57 | 182 | **30** |

**1 992 sur 73 658.** Le hasard donnerait 36 829 : le signal sémantique est **dix-huit fois
meilleur que le hasard**, donc parfaitement réel — et il lui manque environ **deux ordres de
grandeur** pour entrer dans dix résultats. C'est la formulation juste, et elle est très différente
de « le modèle n'a pas de sens ».

Et une chose que le rappel binaire cachait complètement : sur le sous-ensemble facile, **la fusion
fait mieux que chacun de ses deux bras** — rang médian 30, contre 57 pour le plein texte et 182
pour le sémantique. La règle de fusion n'est pas seulement correcte en théorie, elle gagne sur le
corpus réel. C'est le seul résultat positif de l'étape, et il aurait été perdu sans le rang
médian.

#### L'effet de bord de la fusion, à noter avant qu'il morde

Le réglage `K` repris de la publication n'a pas été éprouvé, faute d'avoir eu de la matière à
régler sur le sous-ensemble dur — c'est à dire, et c'est différent de « il est bon ».

Un effet de bord, en revanche, est réel : la fusion a
**dégradé** une requête, « l'achat qui m'attend en boutique », du rang 8 au rang 16. C'est le
défaut connu de RRF quand une des listes est du bruit — un document trouvé premier par le bruit
(`1/61`) bat un document trouvé huitième par le bon moteur (`1/68`). Le remède est un poids par
moteur, que `docs/PHASE-4.md` interdit de deviner et que rien ne justifie de mesurer tant que le
bras sémantique ne vaut pas mieux que ça.

#### Le sous-ensemble dur mesure autre chose que ce qu'il croyait

En regardant les dix requêtes une par une, **sept visent un message dont le sujet est en
anglais** pour une requête en français. C'est mécanique : une requête française ne partage aucun
mot avec un sujet anglais, donc elle tombe dans le sous-ensemble « aucun mot commun » par
construction, sans que la synonymie y soit pour quoi que ce soit.

Le sous-ensemble confond donc deux problèmes : la **synonymie** — paquet/colis, dénoncé/signalement
— et le **passage d'une langue à l'autre**. Le second est plus dur et n'était pas le sujet
annoncé de la phase. Les trois requêtes franco-françaises du lot échouent aussi, donc changer la
définition ne sauverait pas le résultat — mais la prochaine version du jeu doit séparer les deux,
sans quoi un futur modèle sera jugé sur un mélange que personne n'a choisi.

#### Ce que ce non autorise

L'étape 3 avait choisi le modèle le moins cher qui pouvait marcher — la règle du projet. Il ne
suffit pas, et **c'est maintenant mesuré plutôt que supposé** : il manque deux ordres de grandeur,
pas un réglage.

Ce qui rend la suite abordable est le relevé de l'étape 5 : le modèle pèse **4 % du coût de la
passe**, le reste étant la relecture des blobs et l'assainissement. Un modèle **trente fois plus
lent** ferait donc passer la passe complète de six à douze minutes, une fois. C'est le prix d'un
vrai transformeur — `candle` plus `multilingual-e5-small`, l'autre branche écartée le 2026-09-15.
La décision de la prendre n'est plus un pari : elle est payée par deux mesures, l'une qui dit de
combien l'actuel manque, l'autre qui dit que le remplacer ne coûte presque rien là où on croyait
que ça coûterait tout.

#### La leçon de méthode, qui vaut plus que le résultat

**Trois hypothèses ont été formulées, et les deux premières étaient fausses.** Le modèle n'est pas
dépourvu de sens — la sonde le montre. La dilution n'explique rien — le balayage du budget le
montre. Aucune des deux n'aurait été écartée par un argument, et la première avait déjà été écrite
dans ce journal et dans un message de commit avant d'être vérifiée.

Ce qui a fini par trancher n'est pas une hypothèse de plus mais un **changement d'échelle de la
mesure** : demander cinq mille résultats au lieu de cinquante. Un rappel binaire à dix confond
« manquée de peu » et « pas du tout », et tant qu'on ne regardait que lui, toutes les causes
avaient la même signature. La règle à en tirer : **un indicateur qui écrase un continuum en
oui/non ne diagnostique rien** — il faut lui adjoindre la grandeur qu'il a écrasée, ici le rang.

C'est la troisième fois de la phase que la question la moins chère aurait dû passer en premier :
quatre cents octets d'en-tête avant un téléchargement de 530 Mo le 2026-09-15, un `COUNT` avant
l'ouverture d'un index le 2026-09-11, une seconde de sonde avant une journée de conclusions
aujourd'hui.

Vérification : `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`,
`cargo test --workspace` — **1 403 tests verts**, dont 10 sur la règle de fusion et 5 sur la
recherche hybride. Ceux qui portent l'étape : « un message que seul un moteur a vu entre dans la
liste » et son inverse « l'accord des deux bat une première place isolée » ; « sans modèle, la
recherche répond quand même, et elle répond ce que le plein texte répondait ».

### 2026-09-22 — l'étape 6 bis : le transformeur mesuré, et le banc qui mélangeait deux problèmes

Le modèle décidé le 2026-09-21 est en place, le corpus entier est revectorisé, et **le critère 1
n'est toujours pas tenu** : 0,0 % sur le sous-ensemble dur, pour les trois bras, comme avec
l'embedding statique.

Ce n'est pas la partie intéressante. La partie intéressante est que le rang médian, lui, a bougé
dans les **deux sens à la fois** — et que la raison en était écrite dans le journal de la veille,
noir sur blanc, sans que personne en ait tiré le chiffre.

#### Deux hypothèses écartées avant d'écrire quoi que ce soit

Le 2026-09-21 s'est fait prendre à conclure sur un composant avant d'être descendu d'un cran. La
même discipline a donc été appliquée avant d'écrire « le transformeur est moins bon ».

**La normalisation.** Un `e5` sort d'une moyenne pondérée, donc des vecteurs **non unitaires**, là
où `model2vec` en rendait déjà de norme proche de 1. Si le balayage avait comparé par produit
scalaire brut, les vecteurs de grande norme auraient gagné et le classement aurait été corrompu
**par le changement de famille**, pas par le modèle. Coût de la vérification : deux lectures.
`VectorWriter::append` normalise à l'écriture, `Scan::search` normalise la requête, le cosinus est
un vrai cosinus. Écartée gratuitement.

**La dilution, de nouveau.** Le balayage du budget du 2026-09-21 l'avait réfutée — pour
**l'embedding statique**. Il ne dit rien du transformeur, parce que les deux ne voient pas le même
texte : le statique **moyenne** les 2 000 caractères du budget, le transformeur en **tronque** la
moitié à 256 jetons et met de l'attention sur ce qui reste. Et les cibles du sous-ensemble dur sont
des courriels de notification dont les premiers jetons sont surtout du gabarit.

L'hypothèse était sérieuse et bon marché à trancher : une passe à `--budget 0`, sujet et expéditeur
seuls, exactement le régime court où la sonde marche.

| sous-ensemble dur, rang médian sémantique | budget 2 000 | budget 0 |
|---|---:|---:|
| transformeur | **7 039** | 29 612 |

**Quatre fois pire sans le corps.** La dilution n'explique rien, et elle est réfutée dans le sens
**opposé** à celui qu'on cherchait : c'est le corps qui porte le signal. À noter pour la suite — le
réflexe « le sujet est l'essentiel d'un courriel » est faux sur ce corpus, et c'est mesuré.

#### Le relevé

> **Correction du même jour.** Les rangs médians de cette section ont été calculés avec une
> fonction `median` qui rendait le **plus grand** des deux rangs du milieu sur un lot pair. Le
> défaut a été trouvé quelques heures plus tard, en scindant le sous-ensemble dur — voir l'entrée
> de l'étape 6 ter. Les valeurs corrigées sont **6 531** (transformeur), **3 895** (statique
> taillé) et **5 628** (statique brut) sur le sous-ensemble dur, et sur le sous-ensemble facile
> 43 / 23 / 10 au lieu de 57 / 40 / 14. Les **rapports** entre colonnes bougent peu et les
> conclusions de cette entrée tiennent, à une exception près, signalée là où elle porte.

Tous les chiffres ci-dessous viennent d'**une seule session, un seul store, une seule profondeur**
— 73 658, c'est-à-dire le corpus entier, donc aucun rang n'est tronqué. C'est ce qui les rend
comparables entre eux, et c'est la seule comparaison qui vaille ici.

| | plein texte | statique | transformeur |
|---|---:|---:|---:|
| **dur (10) — rappel @10** | 0,0 % | 0,0 % | 0,0 % |
| **dur (10) — rang médian** | absente | **4 111** | 7 039 |
| facile (12) — rappel @10 | 41,7 % | 41,7 % | **50,0 %** |
| facile (12) — rang médian | 57 | 467 | **40** |
| facile (12) — hybride, rang médian | — | 53 | **14** |

Lu comme le 2026-09-21 le lisait — la médiane du sous-ensemble dur — le transformeur est **1,7 fois
pire**. Lu sur le sous-ensemble facile, il est **onze fois meilleur** en sémantique seul, il trouve
une requête de plus, et la fusion passe de 53 à 14.

Un modèle ne peut pas être meilleur partout et pire là où ça compte sans qu'une variable cachée s'en
mêle. Il fallait la trouver avant de conclure.

#### La variable cachée était écrite dans le journal depuis la veille

Le 2026-09-21 notait déjà que **sept des dix requêtes du sous-ensemble dur visent un message dont le
sujet est en anglais**, et que le sous-ensemble « confond donc deux problèmes : la synonymie et le
passage d'une langue à l'autre ». C'était juste, et c'est resté une remarque. En séparant les dix
rangs selon la langue du sujet visé — et en ajoutant le modèle statique **non taillé**, qui sert de
plancher de bruit :

| sous-ensemble dur, rang médian sémantique | transformeur | statique taillé | statique brut |
|---|---:|---:|---:|
| cible au sujet **français** (3 requêtes) | **1 304** | 3 680 | 3 901 |
| cible au sujet **anglais** (7 requêtes) | 11 236 | 8 872 | 13 731 |
| les dix ensemble | 7 039 | 4 111 | 6 581 |
| pour mémoire, le hasard | 36 829 | 36 829 | 36 829 |

**Le plancher de bruit se lit dans les deux dernières colonnes.** Ce sont deux versions du *même*
modèle, l'une étant l'autre au vocabulaire rogné : elles devraient donner la même chose. Elles
donnent 3 680 contre 3 901 en français — 6 % d'écart, soit rien — et 8 872 contre 13 731 en anglais,
un facteur 1,5. À n = 7, un facteur 1,5 est donc du bruit et ne se commente pas.

Ce qui dépasse ce plancher : le transformeur est **environ trois fois meilleur** que les deux
statiques sur les cibles françaises, et il ne se distingue d'eux sur les cibles anglaises ni dans un
sens ni dans l'autre. Autrement dit : **il améliore la synonymie, il n'améliore pas l'interlangue**,
et comme sept des dix requêtes sont interlangues, la médiane du sous-ensemble **rapporte l'échec
interlangue et masque le gain de synonymie**.

> **C'est l'exception annoncée plus haut.** Le partage en sept et trois a été fait à la main, et
> l'outil de l'étape 6 ter — qui compte des mots-outils sur le texte entier plutôt que sur le
> sujet — en trouve **six** interlangues, trois françaises et **une indécidable**, un message
> mixte. Avec ce partage-là et la médiane corrigée, le transformeur n'est plus « indistinct » sur
> l'interlangue : il y est **deux à trois fois pire**, pour un plancher de bruit de 1,65. Le sens
> de la conclusion — il gagne en synonymie, il ne gagne pas en interlangue — ne change pas, mais
> « ni dans un sens ni dans l'autre » était trop généreux. Voir l'entrée suivante.

C'est la même faute de forme que celle du 2026-09-21, d'un cran plus haut. Là, un indicateur binaire
écrasait un continuum et ne diagnostiquait rien. Ici, **une médiane agrège deux populations qui ne
répondent pas à la même question**, et le chiffre qu'elle rend ne décrit aucune des deux.

La règle : **avant de comparer deux moteurs sur un sous-ensemble, vérifier que le sous-ensemble est
homogène pour la propriété qu'on teste.** « Aucun mot commun » est un critère **syntaxique** ; il
sélectionne la synonymie *et* l'interlangue, parce qu'une requête française ne partage jamais un mot
avec un sujet anglais — par construction, sans que la synonymie y soit pour rien. Un sous-ensemble
est un instrument comme un autre : il mesure ce que sa définition sélectionne, pas ce que son nom
promet.

Et le corollaire de méthode, qui resservira : **quand deux variantes d'un même modèle sont
disponibles, les mesurer toutes les deux donne le plancher de bruit gratuitement.** Sans la colonne
« statique brut », l'écart de 8 872 à 11 236 sur les cibles anglaises se serait lu comme un résultat.

#### Une mesure du journal qui ne se reproduit pas

Le 2026-09-21 annonce un rang médian de **1 992** sur le sous-ensemble dur pour le statique. Il n'a
pas été retrouvé, et ça a été cherché :

| ce qui a été essayé | rang médian obtenu |
|---|---|
| statique taillé, profondeur 73 658 | 4 111 |
| statique taillé, profondeur 5 000 | 4 111 |
| statique brut, profondeur 73 658 | 6 581 |
| statique brut, profondeur 5 000 | absente |
| statique brut, profondeur 50 | absente |

Le jeu de requêtes n'a pas bougé depuis le 2026-09-21, `mailvec::text` non plus. Le débit désigne le
modèle employé ce jour-là — 225 msg/s aujourd'hui pour le brut contre 217 au journal, le taillé étant
à 78 — mais le brut donne 6 581, pas 1 992.

Ce qui manque pour trancher est ce qui aurait dû être enregistré : **`vectors.bin` est un artefact
mutable que chaque passe écrase, et rien n'y dit quel modèle ni quel budget l'a produit.** Le
2026-09-21 a enchaîné un balayage de budget à 2 000, 400, 150 et 0 ; le relevé de profondeur qui a
suivi a lu le magasin **laissé par le dernier essai**, et personne ne peut plus dire lequel.

C'est la même famille de défaut que celui du 2026-09-21 — le jeu de requêtes qui désignait ses cibles
par `messages.id` — et la même leçon d'un cran plus loin : **un instrument dont l'état n'est pas
capturé rend des chiffres qu'on ne peut pas défendre.** Le remède est symétrique de celui des cibles :
le magasin de vecteurs doit porter sa provenance — modèle, dimension, budget, date — et
`measure-recall` doit l'imprimer en tête de son relevé, à côté du nombre de vecteurs qu'il affiche
déjà. Tant que ce n'est pas fait, **aucun chiffre de rappel de cette phase n'est reproductible**, y
compris ceux d'aujourd'hui — ceux de ce relevé-ci ne valent que parce qu'ils ont été pris d'affilée,
dans une session dont le journal ci-dessus dit l'ordre exact.

#### Le coût : une prédiction fausse, et pourquoi elle l'était

Le 2026-09-21 autorisait ce changement de modèle par un calcul : le modèle pèse 4 % du coût de la
passe, donc « un modèle **trente fois plus lent** ferait passer la passe complète de six à douze
minutes ». L'arithmétique était juste. Le « trente » était un chiffre en l'air, et il est faux d'un
facteur trente-cinq.

| passe complète, budget 2 000 | statique | transformeur |
|---|---:|---:|
| débit | 78 msg/s | **5,0 msg/s** |
| durée sur 73 658 messages | 948 s | **≈ 4,1 h** (extrapolée) |

> **Mesurée depuis.** La passe transformeur a été refaite entièrement le soir même, pour que le
> magasin porte la provenance de l'étape 6 ter : **15 747 s, soit 4,37 h et 4,68 msg/s** sur les
> 73 658 messages. L'extrapolation ci-dessus était juste à 6 % près, ce qui est une chance et non
> une méthode — elle supposait que la seconde moitié du corpus ressemble à la première, alors que
> le débit varie d'un facteur 3,5 à l'intérieur d'une passe.
>
> Et un fait acquis gratuitement : cette seconde passe complète rend **exactement les mêmes rangs**
> que la première (1 304 en synonymie, 17 331 en interlangue, au message près). La passe est donc
> déterministe — ce qui n'allait pas de soi avec un lot dont le remplissage dépend des voisins, et
> ce qui rend les relevés rejouables une fois la provenance connue.

L'encodage seul du statique avait été mesuré à 5 700 msg/s ; le transformeur tient la passe entière à
5,0, donc il **est** la passe. Le modèle est passé de 4 % à environ 95 % du coût, et le rapport des
deux encodeurs est de l'ordre de **mille**, pas de trente.

La leçon n'est pas « le calcul était mauvais » — il était bon. Elle est que **le calcul reposait sur
une mesure et sur une devinette, et que seule la mesure était signalée comme telle.** Les 4 % étaient
chiffrés, le « trente fois » ne l'était pas, et c'est le second qui portait toute la conclusion. Un
produit n'est jamais mieux mesuré que son facteur le moins mesuré : écrire un nombre supposé à côté
d'un nombre mesuré lui prête une autorité qu'il n'a pas.

Deux précisions honnêtes sur ces chiffres :

- la durée du transformeur est **extrapolée** : la passe a été mesurée sur 36 186 messages en
  7 224,7 s, la première moitié ayant été faite la veille. Le débit, lui, est mesuré ;
- les 78 msg/s du statique taillé ont été **remesurés aujourd'hui**. La comparaison n'emploie que des
  chiffres du jour : un rapport entre deux mesures prises dans des états de machine différents ne
  mesure pas ce qu'il prétend.

#### Le débit suit la longueur des textes, pas leur nombre

Un échantillonnage du magasin de vecteurs toutes les minutes pendant la passe donne une courbe qui
n'est pas plate :

```
3,2 → 4,0 msg/s   (les deux premiers tiers)
9,3 → 11,3 msg/s  (le dernier tiers)
```

Un facteur 3,5 à l'intérieur d'une seule passe, sur une machine qui ne fait rien d'autre. La moyenne
de 5,0 msg/s ne décrit aucun des deux régimes.

La cause est dans `Bert::encode_as` : `PaddingStrategy::BatchLongest` complète tous les textes d'un
lot à la longueur du plus long, et l'attention est **quadratique** en cette longueur. Un lot de seize
messages courts contenant un seul message long coûte seize messages longs. Le débit suit donc la
**distribution des longueurs**, pas le compte des messages.

Ce qui en découle et qui n'est pas pris : trier par longueur avant de constituer les lots
regrouperait les textes courts entre eux. C'est un gain gratuit, et il attend que le choix du modèle
soit tranché — optimiser la vitesse d'un modèle qu'on s'apprête peut-être à remplacer serait le
travail deux fois.

C'est aussi, une fois de plus, la dispersion qui parle avant la médiane. Cinquième récidive de la
série ouverte le 2026-09-02, sauf que cette fois la dispersion n'était pas un biais de mesure :
c'était le phénomène.

#### Le critère 5 est crevé, et de loin

| | statique (2026-09-21) | transformeur |
|---|---:|---:|
| crête RSS de la passe | 117 Mio | **1 304 Mio** |
| accordé | 500 Mo | 500 Mo |

985 Mio de résident en régime, 1 304 de crête. Le remède connu — tailler le vocabulaire — est ce qui
avait ramené 655 Mio à 100 le 2026-09-15, et il porte ici sur **deux** fichiers : le `tokenizer.json`
de 17 Mo et la matrice `embeddings.word_embeddings.weight` du safetensors.

Une bonne nouvelle pour ce remède, tombée du tableau du plancher de bruit : **le rognage du
vocabulaire ne coûte rien en rappel.** Taillé contre brut, 3 680 contre 3 901 sur les cibles
françaises et 4 111 contre 6 581 sur les dix — l'écart est du côté du bruit, et il penche plutôt en
faveur du taillé. C'était une question ouverte que personne n'avait posée le 2026-09-15, où seule la
mémoire avait été mesurée.

Le rognage du transformeur n'est pas fait aujourd'hui, pour la même raison que le tri par longueur :
le modèle n'est pas choisi.

#### Ce qui est décidé pour la suite

Deux décisions prises avec l'utilisateur au vu de ces chiffres.

**Le banc se scinde avant qu'un modèle de plus soit téléchargé.** « Synonymie » et « interlangue »
deviennent deux sous-ensembles nommés, les deux modèles déjà mesurés sont rejoués sur les deux, et le
choix se fait ensuite. Rien n'interdit de conclure que l'interlangue sort du périmètre de la phase —
mais ce sera une décision écrite, pas un chiffre subi. Et la phase se l'était déjà interdit dans son
ordre de travail : « un jeu de requêtes écrit après le choix du modèle ressemble toujours au choix ».
Un jeu qui mélange deux problèmes est pire — il ressemble à n'importe quoi.

**L'extraction du texte des pièces jointes passe devant les étapes 7 et 8.** Le manque a été trouvé
hors plan, en cherchant une facture dans le corpus réel avec `mail attach` : le contenu des pièces
jointes n'entre dans **aucun** des deux index. Une facture en PDF ne se trouve donc ni par son montant
ni par sa référence, et **aucun modèle ne le rattrapera, puisque le texte n'est nulle part**. C'est le
cas d'usage que `docs/VISION.md` met en tête.

Le raisonnement qui le met devant le reste de la phase : le critère 1 a maintenant échoué avec **deux
familles de modèles**, et chaque tentative suivante est un pari sur un gain incertain. Le manque des
pièces jointes, lui, est **certain, borné, et il sert les deux moteurs** — le plein texte en profite
autant que le sémantique, et sans dépendre d'un modèle. Entre une amélioration incertaine du rappel
sur du texte qu'on indexe déjà et l'indexation d'un texte qu'on n'indexe pas du tout, l'ordre se
déduit tout seul.

Vérification : `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`,
`cargo test --workspace` — **1 406 tests verts**, soit 3 de plus, tous sur l'assainissement du nom
d'une pièce jointe : un nom qui remonte l'arborescence est réduit à son dernier segment, un nom
ordinaire — accents compris — traverse intact, et un nom qui ne laisse rien en reçoit un. Le
transformeur lui-même n'ajoute pas de test unitaire : ce qu'il fallait vérifier de lui est un
classement sur 73 658 messages, et c'est le banc qui le fait.

### 2026-09-22 (suite) — l'étape 6 ter : rendre le banc défendable, et ce que ça change

L'entrée précédente se termine sur deux décisions. Celle-ci fait la première : **le banc avant le
modèle**, une fois de plus, et pour la même raison qu'à l'étape 1. Trois défauts d'instrument en
sont sortis, dont deux qui déplacent des chiffres déjà écrits.

#### Le magasin de vecteurs dit maintenant d'où il vient

Le format passe en version 2 : l'en-tête porte un bloc `clé=valeur` avec le **modèle**, sa
**famille**, l'**empreinte de ses poids**, le **budget de texte** et la **date**. Trois choix
méritent d'être écrits.

**Dans le fichier, pas à côté.** Un fichier annexe ou une table SQLite se désynchronise de ce
qu'il décrit, et le projet a déjà payé ce prix exact le 2026-09-11 avec les drapeaux d'indexation
contre les documents tantivy. Une provenance qui peut mentir ne sert à rien : elle vit dans les
octets qu'elle décrit.

**Opposable à l'écriture, et pas seulement lisible.** `VectorWriter::open` refuse de continuer un
magasin dont la provenance n'est pas la sienne, exactement comme il refusait déjà une autre
dimension. C'est le refus qui **manquait** : le corpus porte deux modèles `potion` de dimension
256, donc le contrôle existant les laissait s'empiler dans le même fichier. Le budget en fait
partie pour la même raison — le balayage du 2026-09-21 a écrit quatre magasins à 2 000, 400, 150
et 0 caractères que rien ne distinguait.

La date, elle, est **exclue** de la comparaison. Une passe transformeur dure quatre heures ; la
refuser au motif qu'elle a été reprise le lendemain rendrait le critère 3 intenable un jour sur
mille. Un test le verrouille, parce que c'est le genre d'exclusion qu'on rétablit par zèle.

**L'empreinte plutôt que le nom seul.** Le nom sert à lire un relevé, l'empreinte sert à refuser :
un répertoire dont on remplace les poids garde son nom. Le projet désigne déjà ses messages et les
cibles de son banc par leur contenu ; un modèle, qui s'exécute sur tout le corpus, mérite au moins
autant.

Un magasin de version 1 se **lit** encore — refuser à la lecture obligerait à recalculer des
heures de vecteurs parfaitement valides — et ne s'**écrit** plus. `measure-recall` imprime alors,
en tête de relevé :

```
Provenance         INCONNUE — magasin au format de version 1.
                   Les chiffres ci-dessous ne disent pas quel modèle ni quel budget les a produits,
                   donc ils ne se comparent à rien.
```

C'est exactement ce qu'il fallait pouvoir dire du magasin du 2026-09-21, et que personne n'était en
mesure de dire.

#### Le sous-ensemble dur se scinde, et la scission se calcule

`Lang` sépare **synonymie** — cible française, comme la requête — et **interlangue** — cible
anglaise. Le partage est **calculé**, jamais déclaré dans le fichier de requêtes : c'est la règle
que `recall.rs` s'était déjà donnée pour le partage « aucun mot commun », et deux sources pour le
même fait finissent toujours par se contredire.

Trois précautions, dont deux paient tout de suite :

- **un comptage de mots-outils propres à une langue**, sur le texte que l'indexation voit et non
  sur le seul sujet. Le sujet aurait suffi pour la plupart des cibles et laissé
  « Entry confirmed: … Giveaway » sans le moindre mot-outil ;
- **un marqueur ambigu est exclu des deux listes.** « on », « a », « son », « car », « but »,
  « or », « en » existent dans les deux langues avec des sens différents : ils ne déplacent pas
  une requête d'un sous-ensemble à l'autre, ils la déplacent au hasard ;
- **une marge, et non une majorité.** Douze marqueurs contre onze n'est pas une langue, c'est un
  tirage. En dessous d'un facteur deux, la réponse est **indéterminée** — et une requête
  indéterminée n'est rangée d'office nulle part. Elle a sa propre ligne, qui n'apparaît que quand
  elle n'est pas vide, parce qu'une ligne vide affichée à chaque relevé devient du décor.

Le partage manuel de l'entrée précédente donnait sept interlangues et trois françaises. L'outil en
trouve **six**, trois et **une indécidable** — un message mixte, « 2 new comments in … refonte
templates ». Il a raison et le comptage à la main avait tort : ce message-là ne pose pas une
question à laquelle « français » ou « anglais » répond.

#### Et la scission a trouvé un défaut dans ce qui la résume

Le premier relevé scindé annonçait une médiane de **27 623** pour un lot de six rangs valant
1 277, 6 023, 7 039, 27 623, 34 252 et 35 428. La moitié des valeurs sont sous 7 039 : le chiffre
décrivait le contraire de ce qu'il prétendait.

La cause : `ranks[len / 2]`, c'est-à-dire le **plus grand** des deux rangs du milieu quand le lot
est pair. Sur les lots de vingt-deux et dix requêtes que le banc recevait jusqu'ici, le biais
existait déjà et ne sautait pas aux yeux. Sur des lots de trois et six, il saute.

**La leçon, qui est celle du 2026-09-21 d'un cran plus loin : un résumé statistique est un
instrument, et un instrument se vérifie sur les lots qu'on va vraiment lui donner.** Celui-ci
avait été écrit pour n = 22 et n'avait jamais été relu quand les lots ont changé de taille. Ce
n'est pas un bogue de calcul — la fonction faisait ce qui était écrit — c'est un écart entre
l'intention et le code, la même forme que le plafond du carnet d'adresses le 2026-09-11.

Le remède est en deux parties, et la seconde vaut plus que la première :

- la médiane est celle de tout le monde, moyenne des deux rangs du milieu sur un lot pair. Une
  cible absente ne se moyenne pas : si le rang supérieur du milieu manque, la médiane est absente,
  parce que fabriquer un nombre à partir d'une absence est pire que de dire qu'on ne sait pas ;
- **le relevé imprime les rangs triés à côté de la médiane.** Sur un lot de trois, une médiane
  résume trop peu de chose pour qu'on la croie sur parole. Les rangs bruts la rendent vérifiable
  d'un coup d'œil — la même exigence que le détail par requête, appliquée à la ligne qui prétend
  le condenser. Si ce remède-là avait existé, le défaut se serait vu sans qu'on le cherche.

#### Le relevé corrigé

Trois modèles, trois passes du jour, profondeur complète, et la médiane réparée.

| rang médian sémantique | transformeur | statique taillé | statique brut |
|---|---:|---:|---:|
| **SYNONYMIE** — cible française (3) | **1 304** | 3 680 | 3 901 |
| **INTERLANGUE** — cible anglaise (6) | 17 331 | **5 583** | 9 203 |
| indéterminée — message mixte (1) | **11 236** | 44 637 | 32 096 |
| les dix ensemble | 6 531 | **3 895** | 5 628 |
| pour mémoire, le hasard | 36 829 | 36 829 | 36 829 |

Le **plancher de bruit** reste les deux colonnes de droite, qui sont le même modèle à vocabulaire
près : 1,06 sur la synonymie, **1,65** sur l'interlangue, 1,44 sur les dix.

Ce qui se conclut, et ce qui ne se conclut pas :

- **la synonymie, oui.** Le transformeur est à 0,35 et 0,33 fois les deux statiques, contre un
  plancher de 1,06. C'est net, et les trois rangs vont dans le même sens — 78, 1 304 et 1 834
  contre 611, 3 680 et 4 111. À n = 3 il faut le dire comme ça : le gain est cohérent, il n'est
  pas établi ;
- **l'interlangue, non, et l'écart penche contre lui.** 1,88 et 3,10 fois pire, pour un plancher
  de 1,65. Le premier rapport est dans le bruit, le second en sort. Le sens est constant sur les
  deux références, la magnitude ne l'est pas. Ce qu'on peut écrire honnêtement : **le transformeur
  n'apporte rien en interlangue, et probablement y perd** ;
- **les dix ensemble, rien du tout.** 1,68 et 1,16 fois, pour un plancher de 1,44. L'agrégat est
  dans son propre bruit — ce qui est précisément la démonstration que la scission était le bon
  geste. Le chiffre qui a porté deux entrées de journal ne distingue rien.

Et le cas indéterminé, qu'on ne moyenne avec rien, mérite d'être noté pour la suite : sur le seul
message mixte du lot, le transformeur est **trois à quatre fois meilleur** que les deux statiques.
Un message qui parle les deux langues est peut-être ce qu'il sait faire de mieux. Une requête ne
fait pas un résultat ; elle fait une hypothèse à mesurer, et il faudra d'autres cas mixtes pour ça.

#### Ce que ça change pour le choix du modèle

Rien n'est tranché, et c'est le but. La phase voulait « ne pas choisir un modèle sur un banc qui
mélange deux problèmes » ; le banc ne les mélange plus, et il dit maintenant **lequel des deux**
un modèle résout. Deux conséquences immédiates :

- **un modèle ne se choisit plus sur « le sous-ensemble dur ».** Il se choisit sur la synonymie,
  et séparément on décide si l'interlangue entre dans le périmètre de la phase. Les deux réponses
  ne sont pas du même ordre : la première est un réglage de qualité, la seconde est une question
  de portée ;
- **les lots sont trop petits.** Trois requêtes de synonymie et six d'interlangue ne départagent
  rien de moins qu'un facteur deux. Avant le prochain modèle, le jeu doit grossir **du côté de la
  synonymie française**, qui est le sous-ensemble sur lequel la phase se joue réellement. C'est
  du travail de vérité terrain, pas de moteur — et c'est, encore une fois, ce qui doit passer en
  premier.

Vérification : `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`,
`cargo test --workspace` — **1 424 tests verts**, soit 18 de plus. Ceux qui portent l'étape : une
passe qui refuse de continuer un magasin qu'un autre modèle a commencé **à dimension égale**, son
contrôle inverse qui vérifie qu'elle continue le sien, le même refus sur un changement de budget,
une passe reprise le lendemain qui reste la même passe, un magasin de version 1 qui se lit et ne
s'écrit pas, une longueur de provenance absurde refusée **avant** d'allouer, un retour à la ligne
injecté dans une valeur qui ne casse pas le bloc, et la médiane d'un lot pair — avec le contrôle
qu'une cible absente ne s'y moyenne pas.

Le contrôle négatif du refus de provenance a été joué : neutralisé, il fait tomber
`a_pass_refuses_to_continue_a_store_that_another_model_began` et **elle seule**.

### 2026-09-22 (suite) — l'étape 6 quater commence par compter, et le compte réoriente l'étape

Le manque est réel et il a été vérifié à la main : le contenu d'une pièce jointe n'entre dans
aucun des deux index. Ce que ça ne disait pas, c'est **combien de messages sont concernés** et
**quels formats il faudrait savoir lire**. Écrire un extracteur PDF avant de le savoir, ce serait
choisir la réponse avant la question — la faute que cette phase a déjà payée trois fois.

`cargo xtask attachment-census` est en lecture seule et coûte une passe sur le corpus.

```
Messages           73 658
Avec pièce jointe   7 094  (9,6 %)
Pièces jointes     18 586
```

| type MIME | messages | pièces | octets | part des messages à pièce jointe |
|---|---:|---:|---:|---:|
| `image/png` | 2 932 | 8 899 | 870,4 Mio | 41,3 % |
| `image/jpeg` | 1 441 | 3 555 | 566,7 Mio | 20,3 % |
| **`application/pdf`** | **1 222** | 1 639 | **1,1 Gio** | **17,2 %** |
| `text/calendar` | 922 | 925 | 3,4 Mio | 13,0 % |
| `application/ms-tnef` | 738 | 738 | 410,0 Mio | 10,4 % |
| `text/x-amp-html` | 378 | 378 | 11,0 Mio | 5,3 % |
| `application/ics` | 364 | 364 | 791,8 Kio | 5,1 % |
| `image/gif` | 286 | 768 | 31,6 Mio | 4,0 % |
| OOXML (`docx`, `xlsx`, `pptx`) | 286 | 354 | 359,7 Mio | 4,0 % |
| `application/octet-stream` | 175 | 265 | 126,1 Mio | 2,5 % |

Soixante types suivent, pour 198 pièces au total.

#### Trois choses que ce tableau change

**Le format qui domine en nombre ne porte pas de texte.** Images : 4 659 messages en comptant
`png`, `jpeg` et `gif`, soit plus que tout le reste réuni. Les rendre trouvables demanderait de
l'OCR — une dépendance lourde, un coût par message sans commune mesure avec le reste, et une
question de vie privée différente puisqu'un modèle d'OCR est un second modèle à installer. C'est
**hors périmètre**, et il vaut mieux l'écrire ici que de le redécouvrir à mi-chemin.

**Le PDF est bien la cible, et le chiffre est modeste.** 1 222 messages, soit 1,7 % du corpus et
17 % des messages à pièce jointe. C'est peu en proportion et c'est exactement la bonne cible :
ce sont les factures, les relevés, les contrats — la catégorie que `docs/VISION.md` met en tête,
et celle où l'utilisateur sait qu'il a reçu quelque chose sans savoir le nommer. Un manque qui ne
touche que 1,7 % du corpus peut valoir plus qu'un gain de rappel sur les 98 autres, parce que ces
1,7 % sont ce qu'on cherche vraiment.

**Deux gisements se lisent sans aucune dépendance nouvelle**, et ils n'avaient pas été vus :

- `text/calendar` et `application/ics` font **1 286 messages** à eux deux — plus que les PDF.
  `mailcal` les analyse déjà, sans aucune dépendance, depuis la phase 3 ; leur `SUMMARY`,
  `LOCATION` et `DESCRIPTION` ne sont simplement jamais donnés à l'indexation ;
- les pièces `text/*` — `x-amp-html`, `html`, `plain`, `rfc822-headers`, `x-watch-html` —
  font 594 messages et passent par `mailhtml`, qui est déjà là. Avec une réserve honnête :
  `x-amp-html` et `x-watch-html` sont des **doublons** du corps par construction, donc les
  indexer n'ajoutera probablement rien. À vérifier plutôt qu'à supposer.

Autrement dit : **avant la première ligne d'extracteur PDF, il y a 1 286 messages à rendre
trouvables avec du code déjà écrit.** Le recensement n'a pas changé la cible, il a changé
l'ordre — et c'est précisément ce qu'on lui demandait.

#### Ce qui reste à trancher, et qui ne se tranche pas tout seul

Lire un PDF veut dire **analyser une entrée hostile écrite par l'expéditeur**, dans un format qui
est historiquement une des plus belles sources de vulnérabilités de l'informatique. Le projet a
une règle pour ça — tout parseur d'entrée hostile a ses tests de cas malformés — mais le choix
d'ajouter une dépendance qui fera ce travail sur 1,1 Gio de fichiers venus d'inconnus dépasse un
arbitrage d'implémentation. Il est posé à l'utilisateur plutôt que décidé ici.

Ce qui est acquis quel qu'en soit le verdict : `application/ms-tnef` (738 messages) est un
conteneur Outlook dont le contenu est lui-même des pièces jointes, donc il se traitera *après*
les formats qu'il contient, et jamais avant.

### 2026-09-23 — l'étape 6 quater : 1 363 messages de plus, et un raccourci qui vivait en trois exemplaires

Le recensement de la veille avait changé l'ordre de l'étape : avant la première ligne d'extracteur
PDF, il y avait 1 880 messages à rendre trouvables avec du code déjà écrit. C'est fait, et le
chiffre réel n'est pas celui-là.

#### Ce qui est extrait, et où ça vit

`mailcore::attachtext` lit deux familles, et rien d'autre :

- **les invitations** — `text/calendar`, `application/ics` — par `mailcal`, qui les analyse déjà
  depuis la phase 3 et n'a aucune dépendance. Ce qu'on en garde est ce qu'un humain cherche : le
  titre, le lieu, la description, et les **noms** des participants. Pas les dates, qui sont déjà
  des colonnes et ne se cherchent pas en toutes lettres ; pas les adresses, qui sont déjà indexées
  par les en-têtes ; pas les URL, que `docs/PRIVACY.md` interdit de suivre et que personne ne tape
  dans une recherche ;
- **les pièces `text/*`** — `html`, `plain`, `x-amp-html`… — par `mailhtml`, le même dégraissage
  que pour un corps.

Le module est dans `mailcore` et non dans chacun des deux index, parce qu'il y en a deux :
`index::writer` compose le document tantivy, `mailvec::text` compose le texte à vectoriser, et les
deux doivent voir **le même** texte. Deux extractions divergeraient, et la divergence se verrait
seulement à l'usage — un message trouvable en plein texte et pas en sémantique, sans explication.
C'est la règle du carnet d'adresses : deux chemins vers la même donnée partagent leur code.

Et un plafond, parce qu'une pièce jointe est écrite par l'expéditeur : 256 Kio de texte extrait par
message. Il ne mord jamais en régime ordinaire — la moyenne mesurée est de 35 Kio — et il empêche
un expéditeur de décider combien de mémoire une moisson consomme.

#### Le chiffre, et pourquoi ce n'est pas celui qu'on annonçait

| | messages |
|---|---:|
| estimation de la veille (1 286 ICS + 594 `text/*`) | 1 880 |
| **texte effectivement récupéré** | **1 520** |
| **dont la pièce jointe apporte un mot que le reste du message n'a pas** | **1 363** |

L'estimation était **24 % trop haute**, pour une raison bête et instructive : les colonnes du
recensement comptent des messages **par type**, et un message qui porte une invitation *et* une
pièce HTML était compté deux fois. Additionner des colonnes qui ne sont pas disjointes donne un
total qui n'existe pas. C'est la même famille d'erreur que la médiane de la veille — un résumé
qu'on lit sans vérifier ce qu'il agrège.

Le second chiffre est celui qui compte, et il a été ajouté pour ça. « Du texte en plus » ne veut
pas dire « un message de plus trouvable » : une invitation dont le titre reprend le sujet du
message n'apporte rien à l'index. **1 363 messages sur 1 520 apportent au moins un mot nouveau**,
soit 90 % — le gain est donc réel, et on le sait plutôt que de l'espérer.

#### Ce que le banc dit, et ce qu'il ne peut pas dire

L'index plein texte a été reconstruit — 389,8 s, 292,8 Mio de texte indexé — et le jeu de requêtes
rejoué :

| sous-ensemble facile, plein texte | avant | après |
|---|---:|---:|
| rappel @10 | 41,7 % | 41,7 % |
| rang médian | 43 | 42 |
| rangs | 1 1 1 1 8 29 57 97 245 | 1 1 1 1 7 29 56 95 242 |

**Aucun gain, et aucune régression.** Il faut dire les deux, et le second compte autant : ajouter
292,8 Mio de texte à un index aurait pu enfoncer des cibles sous du bruit, et ça n'arrive pas — les
quelques rangs qui bougent s'améliorent d'un ou deux crans.

L'absence de gain n'est pas un échec de l'extraction, c'est une **limite de l'instrument** :
aucune des 22 requêtes du jeu ne vise le contenu d'une pièce jointe. Le banc a été écrit à
l'étape 1 pour mesurer la synonymie, pas ça. Il ne pouvait donc rien montrer, et le savoir d'avance
valait mieux que de lire son silence comme un résultat.

Ce qui se conclut malgré tout : **1 363 messages sont maintenant atteignables par un mot qui ne
menait à rien hier.** C'est une mesure directe, elle ne dépend d'aucun jeu de requêtes, et elle
suffit à justifier l'étape. Ce qui manque pour aller plus loin — et qui rejoint ce que l'étape
6 ter demandait déjà — ce sont **des requêtes qui visent des pièces jointes**. Le jeu doit grossir
dans deux directions maintenant : la synonymie française, et le contenu des pièces.

#### Le défaut du jour, et il est meilleur que l'étape

Changer la composition du texte change les vecteurs. La provenance écrite la veille retenait le
modèle et le budget — **tout ce qui changeait les vecteurs ce jour-là**. Il a donc fallu lui
ajouter un cinquième champ, une révision de la composition, et ça mérite d'être noté tel quel :
**une garantie qui énumère ce qui compte doit être relue chaque fois qu'on ajoute quelque chose qui
compte.** L'énumération ne se met pas à jour toute seule, et celle-ci a tenu exactement une
journée.

Mais le vrai défaut est en dessous. Avec le champ ajouté et le refus de `VectorWriter::open` en
place, `mail vectors` annonçait toujours :

```
À vectoriser       0 messages
Rien à faire — tout le corpus a déjà son vecteur.
```

sur un magasin périmé. Parce que **le raccourci « y a-t-il du travail » existait en trois
exemplaires** :

1. `pass::update` court-circuitait sur un `COUNT` — la question la moins chère, ajoutée le
   2026-09-11 pour ne pas charger le modèle à chaque moisson ;
2. `mail vectors` refaisait le même `COUNT` pour son affichage, et **retournait avant d'appeler la
   passe** ;
3. le contrôle de provenance vivait dans l'ouverture du magasin en écriture.

Le troisième ne pouvait jamais être atteint, puisque les deux premiers rendaient la main. Corriger
le premier n'a rien corrigé : la commande n'y arrivait pas. Il a fallu le voir échouer **deux fois
sur le corpus réel** avant de comprendre que le problème n'était pas où je regardais.

La forme de la faute est l'inverse de celle du carnet d'adresses. Là-bas, deux chemins vers la même
donnée devaient partager leur **code** plutôt que leur intention. Ici, deux endroits posaient la
même **question** et l'un des deux ignorait la moitié de la réponse. D'où `pass::Pending`, qui est
maintenant la seule porte : elle rend le compte **et** la péremption, et son `is_empty` exige les
deux. La commande la pose au lieu d'y répondre elle-même.

Un magasin périmé est **refait**, pas refusé. Le refus existait déjà à l'ouverture en écriture et il
reste — c'est la barrière de dernier recours, au niveau où elle vit — mais l'appelant n'a rien à
comprendre : la passe remet les drapeaux à zéro, retire le magasin et recommence, exactement comme
elle le fait depuis le 2026-09-11 quand le magasin a **disparu**. Les deux divergences ont la même
conséquence — des drapeaux qui mentent — donc elles ont la même réponse.

Un test l'a suivi : celui qui vérifiait qu'un changement de budget **échoue** a changé de
conclusion. Son motif écrit était « deux budgets ne doivent pas partager un magasin » ; ce motif est
maintenant mieux tenu qu'avant, puisque la passe détecte et refait au lieu de rendre une erreur.
La garantie est la même, le remède est meilleur, donc c'est le test qui bouge — et il vérifie
désormais ce que l'ancien ne disait pas : qu'après un changement de budget le magasin porte
**exactement** les nouveaux vecteurs, sans garder les anciens à côté.

#### Ce qui reste pour le PDF

Rien n'a changé à la question posée la veille, et le chiffre du jour l'éclaire un peu : les formats
gratuits ont rapporté 1 363 messages. Les PDF en promettent 1 222 de plus, au prix d'une dépendance
qui analysera 1,1 Gio de fichiers écrits par des inconnus. L'ordre de grandeur du gain est donc
**le même** que ce qu'on vient d'obtenir sans rien risquer — ce qui ne tranche pas la question,
mais la pose enfin avec ses deux plateaux chargés.

Vérification : `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`,
`cargo test --workspace` — **1 443 tests verts**, soit 17 de plus. Ceux qui portent l'étape : une
invitation qui rend son titre et son lieu, `application/ics` lu comme `text/calendar`, un PDF et
une image qui ne rendent **rien** plutôt que leurs octets bruts, un HTML de pièce jointe dégraissé
comme un corps, une pièce énorme plafonnée, un plafond qui ne coupe jamais au milieu d'un
caractère, une invitation cassée qui ne fait pas paniquer, de l'UTF-8 invalide qui dégrade sans
perdre la pièce, le corps ordinaire qui n'est **pas** repris ici, le texte de la pièce jointe placé
**avant** le corps dans le vecteur, une pièce bavarde qui ne prend jamais plus de la moitié de la
place, un budget nul qui ne prend pas de pièce jointe non plus, une révision de composition refusée
comme un changement de modèle, un magasin périmé refait plutôt qu'annoncé à jour — et son contrôle
inverse, sans lequel la passe revectoriserait le corpus entier à chaque moisson.

#### La revectorisation, et ce qu'elle confirme

Le magasin s'est détecté périmé et refait tout seul — c'est la première fois que le contrôle
tourne pour de bon plutôt que dans un test :

```
Magasin périmé     écrit par une autre composition de texte ou un autre budget
                   — le corpus entier est revectorisé
Durée              12276.3s   73 658 messages   6 msg/s
Sans texte         10   (13 la veille)
```

Trois choses en sortent.

**Les trois messages « sans texte » en moins sont le gain, en miniature.** Ils n'avaient
strictement rien à vectoriser hier ; leur pièce jointe leur en donne. Un message sans texte a un
vecteur nul, donc il ne sort d'aucune recherche : ces trois-là étaient invisibles au sens le plus
fort du terme.

**Aucune régression sur aucun des deux bras**, et c'est ce qu'on venait vérifier :

| rang médian sémantique | avant | après |
|---|---:|---:|
| synonymie (3) | 1 304 | 1 296 |
| interlangue (6) | 17 331 | 17 482 |
| facile (12) | 23 | 24 |
| facile, hybride | 10 | 11 |

Tout bouge de moins de 2 %, dans les deux sens : c'est du bruit de reclassement, pas un effet. Le
texte ajouté ne déplace rien pour les messages qui n'en reçoivent pas, ce qui est exactement ce
qu'on veut d'un ajout ciblé.

**Et la dispersion, une fois de plus.** Cette passe complète a pris **3,41 h à 6,0 msg/s**, la
précédente **4,37 h à 4,68 msg/s** — même machine, même modèle, même corpus, **28 % d'écart**. Le
travail a légèrement augmenté entre les deux, pas diminué. Il n'y a donc rien à conclure du sens de
l'écart, et c'est le rappel qui compte : une durée de passe sur ce corpus se donne avec sa
dispersion ou ne se donne pas. Les 4,37 h écrites hier étaient un tirage, pas une constante.

Une remarque de forme sur la provenance, pour que personne ne la chasse plus tard : elle porte
`écrit le 2026-09-22` alors que l'étape est du 23. La date est en **UTC** et la passe a commencé
avant minuit. Ce n'est pas un défaut — un magasin doit se dater dans un référentiel unique, et
l'heure locale n'en est pas un — mais ça se lit de travers si on ne le sait pas.

### 2026-09-23 (suite) — le banc tronquait à 500, et il ne décidait rien

Quatre requêtes dictées par l'utilisateur sur son propre courrier ont réfuté le critère de
partition en une fois. Une consultation d'expertise externe a ensuite trouvé, dans le même
mouvement, un défaut plus grave que celui qu'on cherchait. Les deux sont réparés ici, et le
relevé qui en sort dit quelque chose de très différent de ce que disaient les précédents.

#### Le défaut : le bras plein texte était censuré à 500, en silence

`Mailbox::search` faisait `limit.min(MAX_RESULTS)` avec `MAX_RESULTS = 500`. Le banc demandait
73 658 résultats et en recevait 500, sans que rien ne le dise.

**Tout ce que les relevés des 21, 22 et 23 septembre impriment « absente » dans la colonne plein
texte veut donc dire « au-delà de 500 ».** Pas « introuvable ». Et une phrase de l'entrée
précédente — « le plein texte ne rend RIEN sur 73 658 messages » — était fausse : elle décrivait
une troncature d'instrument comme un résultat de moteur.

Le plafond n'était pas en cause. Il protège l'API d'un client qui demanderait cent mille lignes
pour vider le store, ce qui reste une bonne raison, et il est resté. Ce qui manquait est que **la
mesure n'est pas un client** : elle tourne dans le même processus, sur son propre store, et elle a
besoin du rang vrai. D'où `Mailbox::search_deep`, sans plafond, employée par le banc et par la
fusion — en régime ordinaire la fusion demande cinquante résultats, donc rien ne change pour qui
lit son courrier.

Ce que ça coûtait, et qui va plus loin qu'une imprécision : **un rang censuré n'a pas de sens
arithmétique.** On ne peut ni le moyenner, ni en prendre la médiane, ni le comparer à un rang non
censuré — et le banc faisait les trois. Une absence fabriquée par l'instrument s'était mise à
circuler comme une propriété du moteur.

C'est la troisième fois de la phase qu'un instrument accuse le moteur, après les cibles qui
avaient glissé et la médiane écrite pour n = 22. La règle se précise à chaque fois : **ce qui
tronque doit le dire, sans quoi il fabrique des absences.**

#### Ce que le vrai rang révèle

| rang de la cible, sous-ensemble sans mot commun | avant (censuré) | après |
|---|---|---|
| plein texte, synonymie | absente, absente, absente | 3 095, 13 316, 39 672 |
| plein texte, interlangue | six fois absente | 13 751, 18 130, 45 963, puis trois vraies absences |
| sémantique, synonymie | 94, 1 296, 1 792 | inchangé |

La comparaison était **impossible** avant, puisqu'on ne compare pas un nombre à une absence. Elle
est maintenant lisible, et elle est favorable au sémantique — d'un facteur qui reste à établir.

#### Le critère de partition est remplacé

« Partage un mot » testait l'**existence** d'un mot commun, pas son **pouvoir discriminant**.
« club » est partagé et ne discrimine rien ; un patronyme est partagé et discrimine à quelques
dizaines près. D'où le nouveau critère : **la fréquence documentaire du mot commun le plus rare**,
lue dans l'index par `Searcher::doc_freq`.

Sa lecture est littérale, et c'est ce qui le rend défendable : une requête réduite à ce seul mot a
**au plus `df` candidats**, donc le mot suffit à porter la cible dans les dix premiers si et
seulement si `df ≤ TOP`. Le seuil n'est pas inventé — c'est `TOP`, déjà motivé. C'est la première
partition de la phase qui n'a aucune constante à défendre.

Il ne regarde le classement d'**aucun** moteur, ce qui compte : définir la difficulté par le rang
BM25 aurait rendu circulaire toute comparaison incluant le plein texte — le sous-ensemble « dur
pour BM25 » contient la malchance de BM25, donc une régression vers la moyenne en faveur de
l'autre bras.

Trois états au lieu de deux, et c'est le troisième qui manquait :

| | requêtes |
|---|---:|
| aucun mot commun | 10 |
| **un mot commun, mais trop fréquent pour désigner** (`df > 10`) | **15** |
| un mot commun assez rare pour suffire (`df ≤ 10`) | 1 |

Le gros du jeu est dans la catégorie du milieu, que l'ancien critère rangeait en « facile ». Ce
sont les requêtes où un moteur de mots **situe sans désigner** : il met la cible dans un groupe de
plusieurs centaines et le classement à l'intérieur du groupe décide de tout.

#### Et la partition ne décide plus rien — c'est la comparaison appariée qui décide

Une cellule de trois requêtes ne départage rien, et aucune métrique n'y change quoi que ce soit :
le test de signe le plus permissif ne descend pas sous p = 0,25 à n = 3. Une médiane de trois
nombres est le nombre du milieu, et elle change quand un seul bouge.

Donc : **les sous-ensembles décrivent, ils ne concluent pas.** Sous neuf requêtes, le banc
n'imprime plus ni rappel ni médiane — seulement les rangs triés. Neuf est le plus petit effectif
où une décision à 5 % survit à une voix contraire (8 sur 9, p = 0,039) ; à huit il faut
l'unanimité, à cinq rien ne conclut.

La première version de cette constante disait dix, sur la foi d'un exemple juste mais non
minimal — et **c'est le test qui l'a attrapée**, pas une relecture. Un seuil dont la raison écrite
est vérifiable finit par être vérifié.

Ce qui décide est une **comparaison appariée sur tout le jeu** : les deux moteurs ont répondu aux
mêmes requêtes, et comparer deux agrégats jetait cette information. Test de signe exact bilatéral
— robuste, parce que les rangs de queue sont bruités et qu'un test sur les amplitudes prendrait ce
bruit pour du signal — plus le rapport géométrique des rangs comme taille d'effet.

#### Le relevé qui en sort, et il est sobre

```
texte contre sémantique    9 victoires, 14 défaites, 3 nuls   p = 0,405   ×0,63
texte contre hybride       7 victoires, 15 défaites, 4 nuls   p = 0,134   ×0,43
sémantique contre hybride 14 victoires,  9 défaites, 3 nuls   p = 0,405   ×0,77
```

**Indécidable sur les trois paires.** Les trois moteurs vont dans le sens attendu — l'hybride bat
le plein texte d'un facteur 2,3 en rapport géométrique, avec 15 victoires contre 7 — et **aucun
écart n'atteint le seuil**. Il aurait fallu 17 victoires sur 22 ; il y en a 15.

C'est le résultat le plus utile de la journée, et il est négatif dans les deux sens : il ne dit
pas que les moteurs se valent, il dit que **26 requêtes ne suffisent pas à les départager**. La
distance est chiffrée — deux victoires — donc l'effort à fournir l'est aussi : une dizaine de
requêtes de plus devrait trancher, à condition qu'elles aillent dans le même sens.

#### Ce que ça oblige à retirer du journal

Tout ce qui, dans les entrées des 22 et 23 septembre, conclut sur une cellule de trois ou six
requêtes est **non soutenu par la méthode d'aujourd'hui**. En particulier :

- « le transformeur est environ trois fois meilleur que les statiques sur les cibles
  françaises » — trois requêtes, indécidable ;
- « il est deux à trois fois pire en interlangue » — six requêtes, indécidable ;
- le « plancher de bruit » tiré de deux variantes du même modèle : deux variantes donnent **un**
  nombre, pas une distribution. Ce n'est pas un plancher, c'est un point. Il garde un usage — le
  **contrôle négatif de la méthode** : appliquée à ce couple sur les cibles françaises, la
  procédure de décision doit rendre « indécidable », et si elle conclut c'est qu'elle est trop
  lâche.

Les chiffres restent, les phrases qui les interprètent tombent. C'est la même correction que celle
du 2026-09-21, appliquée cette fois à ma propre façon de conclure plutôt qu'à un composant.

Vérification : `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`,
`cargo test --workspace` — **1 446 tests verts**, soit 3 de plus. Les trois portent la
statistique : le test de signe contre la table binomiale — dont sa symétrie et son cas à zéro
requête décidée —, les seuils de victoires que le banc annonce quand il ne conclut pas, et la
propriété qui justifie le seuil de neuf.
