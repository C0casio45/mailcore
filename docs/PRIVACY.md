# Privacy by design

Le mail est le format de document le plus hostile qui soit : du HTML arbitraire écrit
par un inconnu, conçu pour pister. La position de mailcore est **rien ne sort sans un
clic explicite**.

Principe de mise en œuvre : la protection est **appliquée par le moteur de rendu**
(CSP, `sandbox`), jamais par du code applicatif qu'on pourrait oublier d'exécuter sur
un chemin détourné. Le code applicatif (assainissement) est la deuxième ceinture,
pas la première.

## 1. Aucun chargement distant par défaut

Le message est rendu dans une `<iframe>` avec une CSP stricte :

```
default-src 'none';
img-src 'self' data: blob:;
style-src 'unsafe-inline';
script-src 'none';
frame-src 'none';
connect-src 'none';
font-src 'self';
form-action 'none';
```

Conséquence : images distantes, feuilles de style externes, polices web, pixels
espions, requêtes de fond — tout est bloqué au niveau du moteur. Pas « pas affiché » :
**pas requêté**. Aucun paquet ne quitte la machine.

`sandbox="allow-popups allow-popups-to-escape-sandbox"` et rien d'autre.
Pas de `allow-scripts`, pas de `allow-same-origin`, pas de `allow-forms`.

## 2. Le déblocage est explicite, par message

Un bandeau : « Contenu distant bloqué — 3 traceurs détectés. [Afficher les images] ».

- Le déblocage vaut **pour ce message uniquement**, pas pour l'expéditeur, pas pour
  la session, jamais globalement.
- Une préférence par expéditeur est possible plus tard, mais toujours en opt-in
  explicite, jamais proposée dans le bandeau du premier message.
- L'état débloqué n'est pas persisté par défaut.

## 3. Détection et comptage des traceurs

À l'indexation, on relève les ressources distantes du corps HTML :

- images de 1×1 ou de dimensions déclarées nulles ;
- URL portant un identifiant unique corrélé au destinataire ;
- domaines de traceurs connus (liste embarquée, mise à jour hors ligne).

Le compte est affiché. On ne se contente pas de bloquer silencieusement : montrer
qui tente de pister est une fonctionnalité.

## 4. Accusés de réception

`Disposition-Notification-To` et `Return-Receipt-To` ne déclenchent **jamais** d'envoi,
et ne déclenchent **jamais** de fenêtre de confirmation. Une fenêtre est déjà une
fuite d'attention, et un clic mal placé une fuite tout court.

Comportement : un badge discret « l'expéditeur a demandé un accusé de réception ».
Une action manuelle enfouie dans le menu du message permet d'en envoyer un si
l'utilisateur le veut vraiment.

## 5. Assainissement avant rendu

Le HTML brut n'atteint jamais le webview. Il passe par `ammonia`, en **liste blanche** :

- `<script>`, `<object>`, `<embed>`, `<iframe>`, `<form>`, `<meta>`, `<link>` supprimés ;
- attributs `on*` supprimés ;
- `javascript:`, `data:` (hors images), `vbscript:` dans les URL supprimés ;
- CSS `position: fixed`, `@import` et `url()` distant neutralisés.

La CSP rendrait déjà tout ça inerte. On le fait quand même : deux barrières
indépendantes, parce qu'une CSP mal formée ne doit pas être un point de défaillance unique.

## 6. Liens

- Aucune navigation automatique, aucun préchargement, aucune résolution DNS anticipée.
- La cible réelle est visible avant le clic.
- Avertissement quand le texte du lien ressemble à une URL différente de la cible,
  ou quand le domaine est en punycode.
- Ouverture dans le navigateur système, jamais dans le webview du client.

## 7. Réseau, au niveau du démon

- Aucune télémétrie. Aucun rapport de plantage sortant. Aucune vérification de
  mise à jour automatique.
- Le démon ne se connecte qu'aux serveurs mail configurés par l'utilisateur.
- Les identifiants et jetons OAuth vont dans le trousseau du système
  (Credential Manager sur Windows), jamais dans un fichier du profil.
- Si une fonction IA appelle une API distante, elle est **désactivée par défaut**,
  et l'écran de configuration dit explicitement quelles données sortent et vers où.
  Les modèles locaux restent la voie par défaut quand ils suffisent.

## Test de non-régression obligatoire

Un test d'intégration qui rend un message HTML contenant un pixel espion, une image
distante, une police web et un `<script>`, avec un serveur HTTP local instrumenté :
**zéro requête reçue**. Ce test tourne en CI et bloque la fusion s'il échoue.
