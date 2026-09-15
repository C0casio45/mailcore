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
| 1 | **Trouver ce qu'un mot-clé ne trouve pas** | sur un jeu de requêtes réelles écrites à la main avec leur réponse attendue : le message visé dans les **10 premiers** pour **≥ 80 %** des requêtes, et **strictement mieux que tantivy seul** sur le sous-ensemble des requêtes qui ne partagent **aucun mot** avec le message visé | à faire |
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

1. **Le jeu de requêtes et sa vérité terrain.** Une trentaine de requêtes en français, écrites en
   regardant le corpus réel, chacune avec le message qu'elle doit trouver. C'est le seul travail
   de la phase qui ne peut pas être fait par un programme, et il vient en premier parce que sans
   lui, tout ce qui suit s'auto-évalue.

   Le sous-ensemble qui compte est celui des requêtes **sans mot commun** avec leur cible : c'est
   là que tantivy ne peut rien, donc là que le sémantique se justifie ou ne se justifie pas.

2. **Le banc lui-même**, qui mesure tantivy seul sur ce jeu. Un chiffre de référence obtenu
   **avant** d'avoir un moteur à défendre. Sans lui, le premier relevé du moteur sémantique n'aura
   rien à quoi se comparer, et « ça a l'air bien » remplacera la mesure.

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
