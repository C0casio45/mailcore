# Vision

## Le problème constaté

Diagnostic réel mené le 2026-08-27 sur le Thunderbird de l'utilisateur, 10 comptes IMAP :

| Constat | Mesure |
|---|---|
| Profil | 11 Go, dont 11 Go de mbox |
| Fichiers mbox > 1 Go | 4 |
| Plus gros fichier | 1,44 Go (`[Gmail]/Tous les messages`) |
| Index de recherche gloda | 509 Mo |
| Dossiers | 93 mbox, 129 index `.msf` |

Duplication observée sur un seul compte Gmail :

```
INBOX                    1111 Mo
[Gmail]/Tous les messages 1441 Mo   ← les mêmes messages
[Gmail]/Messages envoyés   469 Mo   ← encore les mêmes
[Gmail]/Important          304 Mo   ← encore les mêmes
```

≈ 3,3 Go stockés pour ≈ 1,4 Go de contenu réel. Multiplié par 4 comptes Gmail.

Rien de tout ça n'est un bug. C'est la conséquence directe du modèle de stockage :
**un fichier plat par dossier**. Compacter un dossier de 1,4 Go, c'est relire et
réécrire 1,4 Go en synchrone, pendant que l'antivirus scanne chaque écriture.
D'où les gels applicatifs, et parfois système.

La duplication n'est pas réparable dans ce modèle : elle en découle.

## Le pari

Un message est **une donnée immuable identifiée par son contenu**. Un dossier, un
label, un compte ne sont que des *références* vers ce contenu. Poser ça comme
fondation fait disparaître trois problèmes d'un coup :

- La duplication Gmail devient impossible par construction : même contenu, même blob,
  N références.
- Il n'y a plus jamais de compactage : on n'écrit pas dans un fichier existant,
  on ajoute un blob et on met à jour un index.
- L'espace mort n'existe plus : supprimer une référence est une écriture de quelques
  octets, pas la réécriture d'un gigaoctet.

## AI-native, concrètement

Pas un chatbot greffé dans une barre latérale. L'IA fait partie du chemin de données :

- **Recherche sémantique** — un embedding par message, à côté de l'index plein texte.
  « la facture du plombier de l'an dernier » doit marcher sans se souvenir d'un mot exact.
- **Tri automatique** — classification à l'arrivée, apprise sur le comportement réel
  plutôt que sur des règles écrites à la main.
- **Résumé de fil** — un fil de 40 messages se lit en 5 lignes.
- **Rédaction assistée** — brouillons avec le contexte du fil et l'historique de
  l'interlocuteur.

Contrainte : tout doit rester utilisable **sans IA**. Une clé API absente ou un modèle
local indisponible dégrade des fonctionnalités, ne casse jamais le client.

## Non-objectifs

- Ne pas viser la parité fonctionnelle avec Thunderbird. Les deux tournent en
  parallèle pendant toute la montée en puissance ; mailcore n'a pas à tout faire
  pour être utile.
- Pas de calendrier, pas de carnet d'adresses, pas de chat en phase 1 à 3.
- Pas de webmail, pas de serveur. C'est un client local.
