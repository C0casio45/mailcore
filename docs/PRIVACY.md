# Privacy by design

Le mail est le format de document le plus hostile qui soit : du HTML arbitraire écrit
par un inconnu, conçu pour pister. La position de mailcore est **rien ne sort sans un
clic explicite**.

Principe de mise en œuvre : la protection est **appliquée par le moteur de rendu**
(CSP, `sandbox`), jamais par du code applicatif qu'on pourrait oublier d'exécuter sur
un chemin détourné. Le code applicatif (assainissement) est la deuxième ceinture,
pas la première.

**Une exception mesurée, à lire avant de se fier à cette hiérarchie** : pour `<iframe>`,
WebView2 ouvre la connexion TCP *avant* de refuser la requête. Seul l'assainisseur ferme cette
fuite. Voir « Étage 2 » plus bas.

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

**Les hôtes sont conservés, les URL complètes non.** Une URL de traceur *est* l'identifiant
corrélé au destinataire : la recopier dans un rapport qui finira dans un journal ou dans une
interface reviendrait à conserver exactement ce qu'on dénonce. L'hôte suffit à dire qui piste.

**Ce relevé n'est pas une barrière**, et c'est ce qui l'autorise à être approximatif. Un faux
négatif y est un compteur trop bas, pas une fuite : la ressource reste bloquée par la CSP et
par l'assainisseur, comptée ou non. Le jour où quelque chose dépendrait de sa sortie pour
décider de charger, il faudrait le réécrire sur l'analyseur HTML.

Mesuré sur le corpus réel, 2026-08-30 : **5 193 images distantes retirées et 1 034 signaux de
traçage sur 500 messages**. Plus de dix images distantes par message, un signal de traçage
tous les deux messages. Ce n'est pas un cas limite qu'on prévient par principe.

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

### Ce que la mise en œuvre a précisé — 2026-08-30

**Les URL du CSS sont écartées par le nom de la propriété, pas par la valeur.** La liste
blanche des propriétés autorisées dans un attribut `style` ne contient aucune propriété
porteuse d'`url()` — ni `background`, ni `background-image`, ni `content`, ni `cursor`, ni
`filter`. Une URL ne peut donc pas entrer par le CSS sans qu'on inspecte la moindre valeur,
et il n'y a rien à contourner par un encodage. `position`, `top`, `left`, `z-index` et
`transform` sont exclues aussi : superposer du contenu permet de faire lire autre chose que
ce qui est affiché.

**Les blocs `<style>` partent entièrement**, contenu compris. `ammonia` n'assainit pas une
feuille de style : autoriser la balise laisserait passer `@import url(…)` intact, et écrire
un demi-analyseur CSS produirait exactement la barrière approximative contre laquelle ce
document met en garde. Le coût sur les mails très mis en page est assumé ; un vrai
assainisseur CSS est de phase 2.

**Le déblocage est un nouveau rendu, pas une retouche.** §2 demande « Afficher les images »
pour ce message seulement, §5 demande qu'aucune URL distante ne sorte : les deux ne tiennent
ensemble que si l'assainisseur prend une politique et qu'on le rappelle. Rien n'est
persisté, parce qu'il n'y a rien à persister — et le nombre d'images bloquées est un
sous-produit du rendu, donc il ne peut pas mentir sur ce qui a été retiré.

### La coquille native change la nature de la garantie — 2026-09-02

Tout ce qui précède décrit un moteur de rendu **configuré pour ne pas aller chercher une
ressource** : CSP `default-src 'none'`, `sandbox` sans `allow-same-origin`, origine opaque.
C'est solide, et c'est appliqué par le moteur plutôt que par notre code — c'était l'argument
décisif du choix de Tauri.

La coquille native (`crates/mail-shell`) n'a pas de moteur. Le corps d'un message y passe par
`mailhtml::sanitize`, comme avant, puis par `mailhtml::blocks` qui le découpe en paragraphes,
citations, puces et liens. Ce que l'interface dessine ensuite est du **texte et des
rectangles**.

**La garantie devient constructive au lieu d'être configurée.** Il n'y a pas de politique de
moteur à tenir : le chemin de rendu d'un message ne contient **aucun code capable d'émettre une
requête**. Pas de chargeur d'images, pas de résolution de police distante, pas de moteur qui
décide seul d'aller chercher une ressource. Une image d'un message n'est pas « bloquée », elle
est **annoncée** :

- `[image distante, non chargée]` quand la source a survécu à l'assainissement — donc quand
  l'utilisateur a demandé « Afficher les images » ;
- `[image bloquée]` quand le sanitizer l'a retirée ;
- `[image embarquée]` pour un `data:` ou un `cid:`, qui ne sortent pas de la machine.

Trois conséquences, dont deux sont des reculs à nommer :

1. **Les images embarquées ne sont pas affichées non plus.** Décoder un `data:` demanderait un
   décodeur d'images sur une entrée hostile — un `image` de plus dans l'arbre de dépendances, et
   une surface d'attaque nouvelle sur des octets écrits par un inconnu. Phase 2, avec des tests
   sur des fichiers malformés.
2. **Les liens ne sont pas cliquables** dans le volet de lecture. Ils sont soulignés, leur cible
   est connue, et rien ne s'ouvre. Un clic ouvrirait une URL écrite par l'expéditeur, ce qui est
   exactement la classe d'action qui doit rester explicite (§6).
3. **La fidélité baisse sur le mail très mis en page.** Une newsletter à tables imbriquées se
   lit en « mode lecture » : les tables deviennent des lignes, le CSS disparaît. Pour la
   correspondance et les listes de diffusion, c'est fidèle.

**Une précision que la relecture du 2026-09-03 a imposée.** « Aucun client HTTP dans la
coquille » était faux : elle en contient un, `mailapi::client`, pour parler à un démon distant.
La garantie exacte est plus étroite, et il faut l'énoncer telle quelle parce que c'est elle
qu'on tient : **aucun chemin de données ne va du corps d'un message vers ce client**. Ce que la
coquille envoie au démon, ce sont des identifiants de message, un curseur de pagination et la
requête de recherche que l'utilisateur a tapée — jamais un octet venu d'un message. C'est une
propriété tenue par relecture, pas une impossibilité de construction : elle est donc écrite ici
pour être relue, et non affirmée comme un fait de structure.

D'où l'échappatoire, et elle est bornée : **« Ouvrir dans le navigateur »** écrit le HTML
**déjà assaini** dans un fichier temporaire, avec `mailhtml::MESSAGE_CSP` en `<meta>` dans son
en-tête — un fichier local n'a pas d'en-tête HTTP pour la porter — puis le confie au navigateur
du système. Ce qui s'ouvre là est soumis à la même politique que ce que le service sert à la
coquille Tauri, et ne peut donc pas aller chercher une ressource de plus. C'est une action
explicite de l'utilisateur, sur un message qu'il a choisi.

Ce que ça déplace, dit franchement : le navigateur du système est un logiciel qu'on ne contrôle
pas, et la CSP y est notre seule barrière — il n'y a plus le `sandbox` ni l'origine opaque de
l'`<iframe>`. En échange, le chemin par défaut du volet de lecture n'a plus de moteur du tout.

## 6. Liens

- Aucune navigation automatique, aucun préchargement, aucune résolution DNS anticipée.
- La cible réelle est visible avant le clic.
- Avertissement quand le texte du lien ressemble à une URL différente de la cible,
  ou quand le domaine est en punycode.
- Ouverture dans le navigateur système, jamais dans le webview du client.

## 6 bis. Ce qui part porte ce qu'on peut lire — 2026-09-11

`docs/PHASE-3.md` promet que rien n'est ajouté en secret à un message qu'on envoie : pas de
`X-Mailer`, pas d'identifiant de suivi, pas de pixel. La promesse tenait sur la lecture du code
de `mailsmtp::compose`, ce qui n'est à la portée de personne d'autre que de qui l'écrit.

Elle est maintenant **vérifiable depuis l'application** :

- `outbox.source` rend les octets RFC 5322 **remis au `DATA`** — le blob lui-même, pas une
  recomposition, qui pourrait diverger de ce qui est parti ;
- `messages.source` fait la même chose d'un message reçu ;
- `mail source --outbox <ligne>` et un bouton « Source » dans la file de la coquille les
  montrent ;
- un test compare les noms d'en-têtes de ce que nous composons à une **liste close** de onze.
  Un en-tête de plus fait tomber le test.

Deux propriétés de la vue elle-même, qui sont des règles et non des détails :

- **elle n'interprète rien.** Du texte, une police à chasse fixe, aucun `mailhtml`, aucune URL
  suivie. La règle 5 du `CLAUDE.md` y est vraie par construction : il n'y a rien qui puisse
  aller chercher une ressource ;
- **elle neutralise les caractères de contrôle**, dans `mailcore::source`, avant qu'ils ne
  sortent. Un en-tête qui porte `ESC[2J` effacerait le terminal qui l'affiche, donc les en-têtes
  qu'on venait vérifier : une vue dont le rôle est de tout montrer est une cible, et l'octet qui
  la vise vient du message. Le `\r` d'une fin de ligne est la seule exception, et un `\r` seul
  est échappé comme les autres.

## 7. Réseau, au niveau du démon

- Aucune télémétrie. Aucun rapport de plantage sortant. Aucune vérification de
  mise à jour automatique.
- Le démon ne se connecte qu'aux serveurs mail configurés par l'utilisateur.
- Les identifiants et jetons OAuth vont dans le trousseau du système
  (Credential Manager sur Windows), jamais dans un fichier du profil.

### Appliqué côté client — 2026-08-30

`mail daemon login` range le jeton d'API dans le trousseau, une entrée par démon. Il le lit
sur **l'entrée standard**, jamais en argument : un argument de ligne de commande est visible
dans la table des processus par les autres utilisateurs de la machine, et reste dans
l'historique du shell. Aucune variable d'environnement côté client.

**Le démon, lui, lit bien `MAILCORE_TOKEN`, et ce n'est pas une contradiction.** `maild` est
démarré par un gestionnaire de services ou un `compose.yaml`, qui ont leurs propres
mécanismes de secrets et ne passent pas par le shell d'un utilisateur. Les deux côtés n'ont
pas la même surface d'exposition, donc pas la même règle.

Le client refuse par ailleurs d'**envoyer** un jeton à une adresse non locale, tant qu'il ne
parle pas TLS : mettre le secret d'une boîte mail en clair sur un réseau serait la faute que
le critère 10 interdit au démon. La vérification porte sur l'adresse résolue et a lieu avant
l'ouverture de la socket. Le contournement prévu est celui que `docs/ARCHITECTURE.md`
recommande déjà : un tunnel chiffré, et le client vise `127.0.0.1`.

### Un secret ne traverse pas l'API, même pour aller se ranger — 2026-09-11

La coquille sait maintenant déclarer un compte (`⚙ Paramètres`). Le champ de mot de passe écrit
dans le trousseau du système **depuis le processus de l'interface**, sans méthode d'API.

Une méthode `accounts.add` aurait deux conséquences, et aucune n'est acceptable :

- le secret **traverserait le JSON-RPC** pour aller se ranger ;
- quiconque détient le jeton d'un démon pourrait écrire dans le trousseau de **sa** machine.

En mode `--daemon`, la page refuse donc d'écrire et le dit — la même règle que `mail account …`
avec le même drapeau, pour la même raison. Et elle **lit** le store directement plutôt que
d'élargir `accounts.list`, qui ne rend délibérément ni hôte ni port : un écran local ne justifie
pas de donner l'infrastructure de lecture de quelqu'un à tout client distant.

Deux conséquences de forme, qui valent d'être écrites :

- **un secret déjà rangé n'est pas redemandé.** `mailauth::session::has_secret` dit sa
  *présence* sans le lire — comme `is_stored` — et un compte se redéclare sans retaper quoi que
  ce soit. Ça évite surtout de refaire un consentement OAuth2 complet pour aboutir au jeton déjà
  là, ce qui est du travail imposé **et** une occasion de tout casser ;
- **le formulaire ne peut pas laisser fuir ce qu'il porte.** `AccountForm` a un `Debug` écrit à
  la main qui masque jusqu'à la longueur du secret, parce que `Request` dérive `Debug` et qu'une
  demande refusée peut finir dans un journal. `#[derive(Debug)]` avait déjà affiché le mot de
  passe de `Credential` : c'est le défaut dangereux de ce projet, et il a maintenant trois tests
  de plus contre lui.
- Si une fonction IA appelle une API distante, elle est **désactivée par défaut**,
  et l'écran de configuration dit explicitement quelles données sortent et vers où.
  Les modèles locaux restent la voie par défaut quand ils suffisent.

## 8. Quand le démon et le client sont sur deux machines

C'est un déploiement pris en charge dès la phase 1, et il déplace une frontière : le
mail traverse le réseau entre le démon et ses propres clients. Il faut être exact sur
ce que la garantie couvre alors.

Ce qui ne change pas : **rien ne sort vers un tiers.** Le démon ne parle qu'aux clients
que l'utilisateur a autorisés, et le contenu d'un message ne déclenche jamais de
requête, où qu'il soit rendu.

Ce que ça ajoute comme obligations :

- **Chiffrement obligatoire hors interface locale.** Un corps de message, ses en-têtes
  et ses pièces jointes ne circulent jamais en clair sur un réseau, même un LAN
  domestique. TLS, ou un tunnel déjà chiffré monté en dehors de mailcore.
- **Authentification obligatoire hors interface locale**, par jeton porteur généré par
  le démon. Le jeton va dans le trousseau du système, comme les identifiants mail.
- **Fail closed.** Un démon configuré pour écouter sur une interface non locale sans
  jeton **refuse de démarrer**. Pas d'avertissement dans un journal que personne ne
  lit : une mauvaise configuration doit empêcher le service de tourner.
- **Le cache de lecture du client est une donnée sensible.** Il contient des sujets, des
  expéditeurs et des dates, donc les métadonnées de toute la boîte. Il vit dans le
  répertoire de données de l'utilisateur, jamais dans un emplacement partagé ou
  synchronisé, et se supprime avec l'application.
- **Aucun journal de contenu.** Ni sujet, ni adresse, ni corps dans les logs du démon,
  quel que soit le niveau de `tracing`. Un identifiant interne et un compteur suffisent
  à déboguer ; un `tracing::debug!` qui recopie un en-tête crée un deuxième exemplaire
  du mail, en clair, dans un fichier que personne ne surveille.

## Test de non-régression obligatoire

Un test d'intégration qui rend un message HTML contenant un pixel espion, une image
distante, une police web et un `<script>`, avec un serveur HTTP local instrumenté :
**zéro requête reçue**. Ce test tourne en CI et bloque la fusion s'il échoue.

Formulation précise, maintenant que le client parle au démon par le réseau : zéro
requête vers **une destination autre que le démon**, et zéro requête **déclenchée par
le contenu d'un message**, démon inclus. Sans cette précision, le test cesserait d'être
falsifiable le jour où l'UI devient légitimement bavarde.

### Deux étages, et un seul est écrit — 2026-08-30

**Étage 1, fait.** `crates/mailprivacy/tests/sanitiser.rs` — il vivait dans
`crates/mailhtml/tests/no_network.rs` jusqu'au 2026-09-02, voir plus bas pourquoi il a
déménagé. Serveur instrumenté, message
piégé qui tente tout ce que ce document énumère, assainissement, zéro requête reçue. Il
tourne en CI. Un dernier test y fait une requête volontaire et vérifie que le compteur
monte — sans lui, tous les zéros passeraient aussi bien avec un serveur mort.

**Étage 2, fait le 2026-09-02** — et sans `tauri-driver` : voir « Étage 2, fait » plus bas.
Le même message rendu dans le vrai moteur du système, même serveur instrumenté, même
assertion. **Lui seul prouve la CSP**, qui est la barrière appliquée par le moteur ; l'étage 1
ne valide que la ceinture que nous écrivons.

Une assertion a dû être reformulée en l'écrivant. « L'URL n'apparaît nulle part dans la
sortie » est trop fort : la contrebande `<scr<script>ipt src="…">` ressort en **texte**
échappé, inerte, et une comparaison de chaînes ne sait pas l'en distinguer d'un attribut.
Ce que le critère demande est « l'URL n'est nulle part où le moteur irait la chercher », et
ça se vérifie en relisant la sortie comme un moteur la relirait — pas comme une chaîne.

### Étage 2, fait — et il a trouvé une fuite que la CSP ne ferme pas — 2026-09-03

`crates/mailprivacy` porte les deux étages, et ils piègent **le même message**. L'étage 1 a
déménagé de `crates/mailhtml/tests/no_network.rs` pour cette raison : un vecteur d'attaque
ajouté d'un côté et pas de l'autre laisserait un trou que personne ne verrait, et `mailhtml` n'a
pas à dépendre d'un moteur de rendu.

L'étage 2 est un **binaire** et non un `#[test]` : un webview veut un fil d'événements sur le fil
principal du processus, que le harnais de test de Rust ne donne pas. La CI le lance comme une
étape, sur Windows et sur Linux — **le moteur n'est pas le même**, et un zéro sur l'un ne dit
rien de l'autre.

#### Quatre phases, dont deux contrôles

| Phase | Document | CSP | `sandbox` | Attendu | Relevé |
|---|---|---|---|---|---|
| **A** — contrôle | brut | aucune | permissif | charge | **19 vecteurs** |
| **B** — la CSP seule | **brut** | `MESSAGE_CSP` | `MESSAGE_SANDBOX` | rien | **1 vecteur** |
| **C** — les deux ceintures | assaini | `MESSAGE_CSP` | `MESSAGE_SANDBOX` | rien | **0** |
| **D** — contrôle final | brut | aucune | permissif | charge | **19 vecteurs** |

Relevé dans **WebView2** le 2026-09-03, sur les vingt-cinq vecteurs de
`mailprivacy::VECTORS` — dont plusieurs, comme le formulaire et le lien, ne partent pas d'eux-mêmes.

**Les deux contrôles rendent les zéros lisibles.** A prouve que le moteur *irait* chercher ces
ressources au début ; D prouve qu'il le ferait encore à la fin. Sans D, un zéro pourrait vouloir
dire « le moteur est mort en cours de route » — et c'est précisément ce que D a découvert à sa
première exécution, en révélant un blocage du serveur instrumenté.

**Un port instrumenté par vecteur et par phase.** Cent serveurs. C'est la troisième tentative
d'attribution, et les deux premières étaient fausses : un compteur cumulé se trompe dès qu'une
requête arrive juste après une clôture, et un préfixe de chemin est ignoré par une URL
racine-relative. Surtout, une **connexion TCP sans requête HTTP lisible** ne s'attribue à rien —
et c'est exactement la forme que prend la fuite ci-dessous.

#### La fuite : `<iframe>`, et ce qu'elle corrige dans ce document

Sous `MESSAGE_CSP`, qui porte `frame-src 'none'`, WebView2 refuse bien la requête du cadre
imbriqué. **Mais il a déjà ouvert la connexion TCP** vers l'hôte visé avant de l'abandonner. Le
serveur de l'expéditeur voit donc une connexion venant de l'adresse du lecteur, à l'instant où
il ouvre le message : c'est le signal qu'un pixel espion cherche à obtenir, obtenu sans qu'une
seule requête HTTP soit partie.

**Ça corrige la hiérarchie posée en tête de ce document.** « La protection est appliquée par le
moteur de rendu ; le code applicatif est la deuxième ceinture, pas la première » est vrai pour
dix-huit vecteurs sur dix-neuf. Pour `<iframe>`, **c'est l'inverse** : seul l'assainisseur ferme
la fuite, en retirant la balise. La phase C — les deux ceintures — est à zéro.

Ce n'est pas une raison d'affaiblir la CSP, qui reste ce qui arrête tout le reste. C'est une
raison de ne plus décrire l'assainisseur comme une simple redondance.

La liste de ces fuites est **fermée** dans le code (`ENGINE_LEAKS`) : une fuite d'un vecteur qui
n'y figure pas fait échouer l'exécution. Une régression du moteur, ou un vecteur nouveau, ne
peut pas se glisser dans un « c'est comme ça ».

#### Ce que le harnais garantit sur lui-même

Trois vérifications, toutes nées d'un défaut réel trouvé en relecture :

- **la durée de décantation est relevée**, pas supposée. Un zéro obtenu en moins de 2,5 s est un
  échec. Sans ça, un deuxième événement `load` — provoqué par le `<meta refresh>` de la phase de
  contrôle — clôturait la phase suivante après quelques millisecondes, et elle rendait zéro quoi
  que fasse la CSP ;
- **une erreur JavaScript dans la page hôte fait échouer l'exécution.** La page hôte est notre
  code : une erreur dedans veut dire qu'on n'a pas mesuré ce qu'on croit ;
- **toute impossibilité est un échec, jamais un test ignoré.** Un webview qu'on n'arrive pas à
  monter ne prouve pas l'absence de requête, il prouve qu'on n'a rien mesuré.

`MAILPRIVACY_ONLY=<nom>` ne rend qu'un vecteur : c'est l'outil qui a permis de nommer le coupable
parmi vingt-cinq.

#### Ce qui reste découvert

**macOS et WKWebView.** L'étage 2 ne met à l'épreuve que le moteur du système où il tourne. La
fuite `<iframe>` est peut-être propre à Chromium, ou peut-être partagée — personne ne l'a
mesuré.

**L'autre moitié du critère 8** — « zéro requête vers une destination autre que le démon » — se
vérifie ailleurs : les trois tests de parité des politiques de `mail-ui`, et la propriété
énoncée en §5.
