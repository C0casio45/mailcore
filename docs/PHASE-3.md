# Phase 3 — écrire, et connaître ses correspondants

## Objectif

Le client cesse d'être en lecture seule. Un message part, une réponse s'écrit dans le fil, une
adresse se complète en tapant trois lettres, une invitation reçue se lit comme un rendez-vous
plutôt que comme une pièce jointe illisible.

**La phase 2 est close** : dix critères mesurés sur cinq comptes réels, la synchronisation
IMAP tient, `IDLE` amène le courrier tout seul. Ce qui manque n'est plus de recevoir.

## Pourquoi l'envoi avant tout le reste

Trois raisons, dans cet ordre.

**C'est ce qui rend le client utilisable.** Un client qui reçoit sans répondre n'est pas à
moitié fini : il oblige à garder Thunderbird ouvert à côté pour la seule chose qui compte quand
un mail arrive. Tout le reste de la phase 3 est du confort par-dessus.

**C'est le seul morceau irréversible de tout le projet.** Un bug de lecture affiche faux ; un
bug d'envoi expédie un message à quelqu'un, et rien ne le rattrape. C'est le premier endroit du
dépôt où « on corrigera au prochain passage » ne s'applique pas.

**Les autres en dépendent.** Le carnet d'adresses sert d'abord à compléter un destinataire, et
la signature n'a de sens que dans un message qu'on écrit.

## Périmètre

Dedans :

- `mailsmtp` — le client SMTP. `EHLO`, `STARTTLS` ou TLS direct, `AUTH PLAIN` et `XOAUTH2`,
  `MAIL FROM` / `RCPT TO` / `DATA`. **Aucun secret nouveau** : les mêmes entrées de trousseau
  que l'IMAP, par `mailauth`.
- **La composition d'un message RFC 5322** : en-têtes, `Message-ID`, `Date`, `References` et
  `In-Reply-To` pour rester dans le fil, encodage des en-têtes non ASCII (RFC 2047), corps
  `multipart/alternative` texte + HTML, pièces jointes en `base64`.
- **La file d'envoi** : un message part une fois et une seule, y compris si le processus meurt
  entre l'acceptation du serveur et l'écriture locale. Voir le critère 2.
- **La rédaction dans la coquille** : une nouvelle fenêtre, répondre, répondre à tous,
  transférer. Brouillons dans le store.
- **Le carnet d'adresses**, dérivé du corpus d'abord : 105 000 références portent chacune une
  adresse et un nom affiché. Aucun protocole à ajouter pour la première version.
- **L'éditeur de signature**, riche et mesuré. Voir le critère 5, qui est le seul de la phase à
  porter sur du dessin. Fait le 2026-09-10.
- **Les invitations reçues** : une pièce `text/calendar` se lit comme un rendez-vous, avec ses
  dates dans le fuseau de l'utilisateur.
- **La source d'un message** : les octets tels qu'ils sont, pour un message reçu **et pour un
  message qu'on envoie**. C'est ce qui rend vérifiable l'engagement de la section suivante, et
  c'est la seconde moitié qui le tient. Faite le 2026-09-11.

Dehors, et c'est délibéré :

- **CardDAV et CalDAV.** Deux protocoles, deux flux de synchronisation bidirectionnelle, et la
  phase 2 a montré ce que coûte *un* protocole fait sérieusement. Le carnet dérivé du corpus et
  les invitations reçues couvrent l'essentiel de l'usage sans eux ; les brancher est une phase à
  part.
- **Créer ou modifier un rendez-vous.** Lire une invitation est un problème de parsing ;
  répondre à une invitation est un problème de protocole, et écrire dans un agenda distant en
  est un troisième.
- **Le chiffrement de bout en bout** — PGP, S/MIME. Ni l'un ni l'autre ne se fait à moitié.
- **Les listes de diffusion, les règles de tri, les filtres.** L'IA de `docs/VISION.md` est
  censée s'en occuper autrement, et écrire un moteur de règles maintenant préempterait ce
  choix.

## Ce que la vie privée exige en plus, cette fois

`docs/PRIVACY.md` prend un sens nouveau dès qu'on écrit vers l'extérieur.

**Un message ne part qu'après une action explicite.** Pas d'envoi différé automatique, pas de
« envoyer dans 5 s avec annulation » qui part quand l'application se ferme. Le clic est le
consentement.

**Rien de ce qu'on ajoute au message n'est invisible à l'utilisateur.** Pas d'en-tête
`X-Mailer` bavard, pas d'identifiant de suivi, pas de pixel. Ce que le message porte doit être
lisible dans une fenêtre « source du message » qu'on écrira. — **écrite le 2026-09-11**
(`mailcore::source`, `messages.source` et `outbox.source`, `mail source`, une fenêtre dans la
coquille). La seconde méthode est celle qui compte : elle lit le blob **remis au `DATA`**, donc
ce que nous composons, et un test vérifie que le bloc d'en-têtes ne porte rien hors d'une liste
close.

**Le carnet d'adresses ne sort pas.** Il est dérivé du corpus local ; aucune requête ne part
pour enrichir une fiche, ni photo, ni profil social. C'est la règle 5 appliquée aux contacts.

**Une invitation ne déclenche aucune requête.** Une pièce `text/calendar` peut porter une URL
d'organisateur ou de conférence. Elle s'affiche, elle ne se suit pas.

## Critères d'acceptation — à mesurer, pas à estimer

| # | Critère | Seuil | État |
|---|---|---|---|
| 1 | Un message envoyé arrive, et il est **conforme** | reçu sur un compte du corpus, `Message-ID` présent, `In-Reply-To`/`References` corrects sur une réponse, en-têtes non ASCII lisibles chez le destinataire. Vérifié en s'écrivant d'un compte réel à un autre | **partiel, mais très avancé (2026-09-10)** — deux aller-retours réels le 2026-09-09 (sujet accentué, tiret cadratin, ligne commençant par un point, aucun en-tête `Bcc`). La conformité d'une **réponse** est maintenant mesurée mécaniquement sur **57 840 vrais fils** du corpus : `Message-ID` présent, `In-Reply-To` et `References` corrects après un aller-retour assemblage → relecture, sujet identique — **100 %, aucun manquement**, dont 36 481 sujets non ASCII et une chaîne de 128 identifiants. Le banc a trouvé **deux défauts réels** qu'il a fallu corriger pour y arriver (sujet mis entre guillemets, identifiants sans chevrons). Ce qui reste : l'envoi réel d'une réponse, que seul un serveur peut confirmer |
| 2 | Envoi **exactement une fois**, y compris sur coupure | **0 doublon** et **0 perte** sur une coupure provoquée entre l'acceptation du serveur et l'écriture locale. Mesuré en tuant le processus à cet instant précis, pas en le simulant | **tenu (2026-09-09)** — processus enfant tué aux deux instants qui comptent, 1 message reçu dans les trois cas, contrôles négatifs sur les deux barrières |
| 3 | Un message de 25 Mo de pièces jointes | part sans charger le message entier en mémoire : **RSS < 200 Mo**, et l'encodage `base64` est streamé | **tenu (2026-09-09)** — crête **10,1 Mio**, dont **428 Kio** de croissance depuis le repos. Et la mémoire ne dépend pas de la taille : 200 Mo de pièce jointe donnent 584 Kio. Vérifié aussi sur un envoi réel de 3 Mo, revenu de Gmail à l'octet près |
| 4 | Autocomplétion d'un destinataire sur le corpus réel | **< 16,7 ms** par frappe en p95, sur les 105 000 références et leurs adresses distinctes | **tenu (2026-09-09)** — p95 **643,8 µs** sur 6 320 frappes, 4 635 adresses dérivées de 48 551 messages. Le pire préfixe touche 3 021 adresses, donc le relevé porte bien sur le cas difficile |
| 5 | L'éditeur de signature, en frappe et en collage | **< 16,7 ms** en p95 par image, sur un document de 200 lignes avec gras, italique, liens et listes. Et un collage de 50 Ko de HTML depuis un navigateur ne doit pas figer l'interface | **tenu (2026-09-10)** — mesuré sur l'**éditeur livré** (banc `signature`, trois exécutions) : frappe p95 **7,06 à 8,14 ms** par image, dont **5,55 à 6,09 ms** de travail, sur **31 511 glyphes et 808 intervalles stylés**. Collage de 50 096 octets converti en **2,54 à 2,92 ms**. Limite dite par le relevé : une image sur quelques centaines arrive à **42 ms**, pour **6,27 ms** de travail — c'est l'ordonnancement du système, pas la mise en page. La sonde `mail-spike-richtext` avait donné 0,31 à 0,47 ms sur le modèle nu ; l'écart est le champ de texte, la fenêtre et le compositeur |
| 6 | Une invitation `text/calendar` du corpus réel | dates, fuseau, organisateur et participants lus juste sur **tous** les `text/calendar` du corpus, ou refusés en nommant ce qui manque. **Zéro requête réseau** | **tenu (2026-09-10)** — **930 pièces** dans 927 messages, sur les 73 825 du corpus : **901 (96,88 %)** avec un instant absolu, **27 (2,90 %)** lues avec leur réserve nommée — heure flottante, RFC 5545 §3.3.5 — **2 (0,22 %)** refusées en nommant la raison, et **0 illisible sans raison**. Titre lu sur 99,68 %, organisateur sur 89,78 %, participants sur 92,15 % (8 150 en tout), fin sur 99,78 %. Contrôle : **0 pièce contenant `VEVENT` sans qu'un événement soit lu**. Zéro requête réseau **par construction** — `mailcal` n'a aucune dépendance, donc aucun client HTTP et aucune base de fuseaux |
| 7 | Aucun secret nouveau, aucun identifiant en clair | **exactement 0** — le même harnais que le critère 6 de la phase 2, étendu à SMTP | **tenu (2026-09-09)** — 13 secrets réels, **0 trouvé** dans le store, les journaux `TRACE` et `%TEMP%`, après une moisson IMAP des 5 comptes **et** une authentification SMTP réelle. La sonde s'arrête avant le `DATA` : rien n'est envoyé. Limite dite par le relevé : 4 comptes sur 5 n'ont pas de serveur d'envoi, donc leur chemin SMTP n'est pas mesuré |
| 8 | Un envoi refusé par le serveur | l'utilisateur voit **quoi** faire : quota, destinataire refusé, message trop gros, authentification. Pas un code numérique | **presque tenu (2026-09-10)** — les cinq familles sont **provoquées de bout en bout** contre un serveur scripté : refus SMTP → état en file → phrase lue, une par famille, plus le refus de taille annoncé à l'`EHLO` (`crates/mailsmtp/tests/refus.rs`, 12 tests). Le banc a trouvé un défaut réel : un refus passager promettait « rien à faire » **après** l'épuisement des tentatives, sur une ligne morte. La phrase se compose donc après la décision, et le geste qu'elle nomme existe : `resendable` sur la ligne de file, bouton « Renvoyer tel quel » dans la coquille, `mail outbox --retry`. Reste ouvert : **aucun vrai serveur n'a refusé** — non autorisé, rien n'a été envoyé |

Le critère 2 est celui qui porte le vrai risque de la phase, et c'est la raison de l'ordre de
travail ci-dessous.

Le critère 5 est le seul qui porte sur du dessin, et il est là parce qu'un éditeur riche écrit
naïvement est **inutilisable** : un document remis en page à chaque frappe passe la seconde par
image dès quelques centaines de lignes. Le seuil est celui du critère 2 de la phase 1, et pour
la même raison.

## Ordre de travail

**Étapes 1 à 6 faites le 2026-09-09.** Voir le journal en fin de fichier.

1. ~~**`mailsmtp` contre un serveur qu'on maîtrise.**~~ Fait. 102 tests — 83 unitaires, 19 d'intégration — dont un serveur scripté
   qui accepte le `DATA` et se tait ensuite — la panne qui définit le critère 2.
2. ~~**La composition RFC 5322.**~~ Fait, pièces jointes comprises : `multipart/alternative`
   enveloppé dans un `multipart/mixed`, base64 streamé. Voir le critère 3.
3. ~~**La file d'envoi et le critère 2.**~~ Fait, et mesuré sur un processus enfant tué aux deux
   instants qui comptent.
4. ~~**Le premier envoi réel.**~~ Fait : `STARTTLS` sur 587, `XOAUTH2`, et le message ressort de
   l'index après une resynchronisation. Le critère 1 reste ouvert sur la conformité d'une
   *réponse* ; le critère 8 reste ouvert sur les refus, qu'aucun vrai serveur n'a encore rendus.
5. ~~**La rédaction dans la coquille** : fenêtre, réponse.~~ Fait : une fenêtre de rédaction, un
   panneau de file d'envoi, et un fil de fond (`maild::outbox::Postman`) qui vide la file. Les
   pièces jointes se déposent sur la fenêtre. **Répondre, répondre à tous et transférer** sont
   là depuis le 2026-09-10, avec la citation de l'original — « à tous » retire les adresses des
   comptes connus, parce que se mettre en copie de sa propre réponse est le défaut le plus
   visible de ce bouton. **Les brouillons sont persistés** depuis le même jour (`SCHEMA_V9`) :
   fermer la fenêtre enregistre, et un panneau les liste. Reste un manque : un transfert ne
   reprend ses pièces jointes depuis le 2026-09-10, par `messages.stage_part`.
6. ~~**Le carnet dérivé du corpus**, et le critère 4.~~ Fait : 4 635 adresses dérivées du
   corpus, classement mesuré à 643,8 µs en p95, autocomplétion dans les trois champs de
   destinataires.
7. ~~**L'éditeur de signature**, et le critère 5.~~ Fait le 2026-09-10 : le modèle et ses deux
   traductions dans `mailhtml::rich`, l'éditeur dans `mail-shell::signature` — sélection,
   boutons de style, puces, liens — le rangement par compte (`SCHEMA_V8`, deux méthodes d'API),
   et la signature **montrée** dans la fenêtre de rédaction avant de partir. Le critère 5 est
   mesuré sur l'éditeur livré et non plus sur la sonde.
8. ~~**Les invitations reçues**, et le critère 6.~~ Fait le 2026-09-10 : `mailcal` lit une pièce
   `text/calendar` — dates, fuseau depuis la `VTIMEZONE` embarquée, organisateur, participants,
   répétition — ou la refuse en nommant ce qui manque ; `mailcore` la sort avec le message,
   l'API la sert comme rendez-vous, et la coquille l'affiche comme tel. Mesuré sur les 930
   pièces du corpus.
9. ~~**Les refus, et le critère 8.**~~ Fait le 2026-09-10 : les cinq familles provoquées de bout
   en bout contre un serveur scripté, la phrase composée **après** la décision de la file — un
   refus passager promettait une reprise qui n'aurait pas lieu — et le geste qu'elle nomme rendu
   possible : `outbox.resendable` (`SCHEMA_V10`), `mail outbox --retry`, un bouton dans la
   coquille. ← *tous les critères de la phase sont mesurés. Reste, sur 1 et 8, ce qui demande un
   envoi réel : une réponse remise à un vrai serveur, et un refus rendu par un vrai serveur.*
9. ~~**Le critère 3 et le critère 7.**~~ Faits le 2026-09-09 : les pièces jointes sont streamées
   de bout en bout, et le harnais à secrets traverse maintenant une authentification SMTP
   réelle sans envoyer de message.
10. ~~**La source du message.**~~ Faite le 2026-09-11 : `mailcore::source` rend les octets
    lisibles — en-têtes verbatim, caractères de contrôle neutralisés, réserves comptées —
    `messages.source` et `outbox.source` les servent, `mail source` et une fenêtre de la
    coquille les montrent. Elle n'a pas de critère chiffré : ce qu'elle ferme est l'engagement
    de vie privée qui n'avait aucune vérification, et le test qui le tient compare les en-têtes
    de ce que **nous** composons à une liste close.
11. ~~**La page de paramètres.**~~ Faite le 2026-09-11 : déclarer un compte, écrire son serveur
    de soumission, oublier son secret et lancer sa moisson, sans terminal. Le secret va du
    clavier au trousseau sans traverser ni l'API ni un historique de shell, et un secret déjà
    rangé est **réadopté** au lieu d'être redemandé — `mailauth::session::has_secret`, qui sert
    aussi à `mail account add` et lui évite un consentement OAuth2 pour rien. Le consentement
    lui-même est dans la page depuis le même jour, **sur son propre fil** : il attend un humain
    jusqu'à cinq minutes, et le servir en ligne aurait mis toute la boîte mail en file derrière.
12. ~~**Les trois dérivés suivent la moisson.**~~ Faits les 2026-09-11 (carnet, index) et
    2026-09-13 (fils). Les deux premiers avancent sur un drapeau par ligne ; le troisième ne le
    pouvait pas — un fil se calcule à partir de messages qui arrivent **après** celui qu'on
    traite — et recalcule donc le **voisinage** touché, sur une règle partagée avec la
    reconstruction (`mailcore::thread::partition`). Les faits dérivés sont rangés au lieu d'être
    relus dans les blobs (`SCHEMA_V13`), ce qui est la seule raison pour laquelle une passe
    locale est possible : la relecture est ce qui coûtait 334 s à froid.

Les étapes 1 à 4 étaient l'envoi lui-même. Elles sont passées avant tout le reste parce que c'est
la seule partie de cette phase qu'on ne peut pas corriger après coup.

## Les pièges connus, avant d'écrire une ligne

**Le double envoi est le mode de panne par défaut.** Un `250 OK` reçu et un processus qui meurt
avant d'avoir écrit « c'est parti » laissent une file qui réessaiera. Aucun protocole ne permet
de demander à un serveur SMTP « as-tu déjà reçu ce message ? ». La seule défense est locale, et
elle doit être écrite avant l'interface.

**Un `Message-ID` doit être unique et il vient de nous.** Le laisser au serveur — certains en
ajoutent un — casse le fil dès que le message revient par `IMAP` dans les envoyés.

**Le message envoyé doit être déposé dans le dossier des envoyés par nous.** Gmail le fait tout
seul, Dovecot non. Faire les deux donne un doublon chez Gmail ; ne faire ni l'un ni l'autre
perd le message chez Dovecot. Il faut détecter, pas supposer.

**Les en-têtes non ASCII sont un piège à deux étages** : encoder l'en-tête entier au lieu de
ses mots casse les adresses, et ne pas encoder du tout fait arriver du charabia. Le corpus a
des sujets en arabe, en hébreu et en devanagari — mesuré à la phase 1.

**`egui` n'a pas d'éditeur riche.** Il a un champ de texte. Un éditeur riche demande un modèle
de document, une mise en page incrémentale et un modèle de sélection — pas un widget à
configurer. C'est ce que le critère 5 mesure, et c'est pourquoi une sonde vient avant.

*Réponse, le 2026-09-10 : il a un champ de texte **et un `layouter`**, et c'est par là que ça
passe. Le champ édite une `String` — donc curseur, sélection, collage et annulation viennent
gratuitement — le `layouter` fait le gras et l'italique, et un document stylé se rattrape sur le
tampon à chaque image. La mise en page incrémentale n'a pas été nécessaire : `egui` met en cache
la mise en page par empreinte, et une remise en page complète de 200 lignes coûte quatorze
microsecondes. Le modèle de sélection non plus : le champ le tient, et il rend son curseur en
index de caractères.*

## Journal

### `mailsmtp` : le dialogue, et l'étape qui décide de tout — 2026-09-09

L'étape 1 de l'ordre de travail. Le client SMTP écrit contre un serveur scripté avant de toucher
à un vrai, comme `mailsync` l'a été contre `mailfake` — et pour une raison de plus : ce crate
écrit **vers l'extérieur**, et « le test envoie du courrier » n'est pas une propriété qu'on veut
d'une suite qui tourne à chaque `cargo test`.

64 tests. Ce qui est couvert, et pourquoi chaque morceau existe.

#### `Stage`, et le troisième état

Entre « rien n'est parti » et « c'est parti », il y a `Stage::Committing` : **après le point
final du `DATA`**. Le serveur a le message, et s'il se tait à cet instant, rien dans le protocole
ne dit s'il l'a gardé.

C'est toute la raison d'être de cette énumération. La file d'envoi tranchera ; pour trancher, elle
a besoin de savoir où ça s'est arrêté, et c'est tout ce que le protocole permet de lui dire
d'honnête. `Error::may_have_been_sent()` est cette question, et elle est **distincte** de
`retryable()` : une coupure après le point final est réessayable du point de vue du réseau et
dangereuse du point de vue du destinataire. Les mélanger dans un seul booléen est exactement
comment on envoie deux fois.

#### Le test qui a trouvé le défaut qu'il cherchait

`a_server_that_cuts_after_the_final_dot_leaves_a_doubt` : le serveur accepte le `DATA`, reçoit
tout, et ferme sans répondre. Le test vérifie que le doute est signalé.

Il a échoué au premier jet. Une coupure à cet instant rend `Error::Malformed` — « le serveur a
fermé sans répondre » — et `Malformed` **ne portait pas d'étape**. Donc `may_have_been_sent()`
répondait faux, et une file d'envoi aurait renvoyé le message. Le doublon chez le destinataire
était à une ligne de code.

L'étape est maintenant recollée sur l'erreur de l'analyseur, dans `read_reply` : `reply::read`
est pur et ne la connaît pas, ce qui est le bon découpage — mais l'information ne doit pas se
perdre au passage. Avec son contrôle inverse, sans lequel il ne prouverait rien :
`a_cut_before_the_data_leaves_no_doubt`, parce qu'un client qui signalerait un doute partout
ferait abandonner des messages qui n'ont jamais été envoyés.

#### Le point en début de ligne

RFC 5321 §4.5.2 : la fin des données est une ligne contenant un seul point. Un message dont une
ligne **commence** par un point terminerait donc le transfert au milieu — le destinataire reçoit
un message tronqué, et le reste part au serveur comme des commandes.

Le doublage est donc obligatoire, et il va avec la normalisation des fins de ligne : un début de
ligne ne se reconnaît qu'après un `CRLF`, et un message écrit sur Unix n'en a pas. Les deux
transformations sont dans la même fonction pour cette raison, et
`a_dot_after_a_bare_newline_is_doubled_too` est ce qui l'atteste.

Vérifié aussi de bout en bout : le serveur de test voit arriver `apres` après une ligne de point,
donc le transfert n'a pas été terminé par elle.

#### Ce que le refus doit dire

Le critère 8 demande que l'utilisateur voie **quoi faire**. `Error` distingue donc les familles
au lieu de recopier un code : un `452` est passager et se réessaie, un `550` est définitif et
revient à l'utilisateur, un dépassement de `SIZE` est le seul refus qu'on peut corriger **avant**
d'envoyer — et qu'on voit venir, puisque `SIZE` est annoncé à l'`EHLO`. Le message de 5 000
octets devant une limite de 1 000 est refusé **sans ouvrir l'enveloppe**.

#### Le secret ne part pas au hasard

Un compte OAuth2 s'authentifie en `XOAUTH2` ou pas du tout. Retomber sur `PLAIN` enverrait le
jeton d'accès dans le champ mot de passe, où un serveur le journalise avec l'identifiant en cas
d'échec — le jeton finirait écrit sur le disque de quelqu'un d'autre. C'est la même raison qui a
fait deux variantes de `Credential` dans `mailsync`.

Un serveur qui n'annonce pas le mécanisme est donc refusé **avant** que le secret ne quitte le
processus, et un test vérifie que rien n'est parti.

#### Deux défauts de mon serveur de test, et ce qu'ils apprennent

**Il envoyait une ligne par commande.** Une réponse multi-ligne part d'un bloc en réaction à
*une* commande ; en envoyer une ligne à la fois bloquait les deux côtés — le client attendait la
fin de la réponse, le serveur attendait la commande suivante. Une entrée de script est maintenant
une réponse entière.

**Il pouvait bloquer sans fin.** `join` attendait un serveur qui attendait une ligne d'un client
que le test tenait encore ouvert. La suite a tourné dix minutes avant d'être tuée. Deux
correctifs : un délai de lecture côté serveur, et `received` qui **consomme** le client — l'oubli
devient impossible plutôt que lent.

Un serveur de test qui peut bloquer est une suite qui bloque, et le diagnostic coûte plus cher que
les deux secondes du délai.

#### Ce qui n'est pas encore là

- **La connexion chiffrée.** Le dialogue est écrit contre un serveur en clair ; le connecteur TLS
  et le `STARTTLS` viennent avec l'étape 4, celle du premier envoi réel. Ajouter la pile
  maintenant serait du code non couvert qui prétend l'être.
- **L'assemblage du message.** `compose` a les primitives — adresses validées contre l'injection,
  mots encodés RFC 2047, pliage, `Message-ID` — pas encore le `multipart/alternative` ni les
  pièces jointes.
- **La file d'envoi**, donc le critère 2 pour de vrai. Le client sait dire qu'il y a un doute ;
  personne ne décide encore quoi en faire.
- **Tout le reste de la phase** : la rédaction dans la coquille, le carnet, la signature, les
  invitations.

---

### 2026-09-09 — étapes 3 et 4 : la file d'envoi, TLS, et le premier message réellement parti

Le critère 2 est tenu et **mesuré sur un vrai processus tué**. Le critère 1 a son premier
aller-retour complet. Ce qui suit dit comment, et surtout ce que ça ne garantit pas.

#### Le critère 2 : ce qui est promis, et ce qui ne peut pas l'être

SMTP ne permet pas de demander à un serveur « as-tu déjà reçu ce message ? ». Il n'existe donc
**pas** de code qui envoie exactement une fois face à une coupure arbitraire, et prétendre le
contraire serait le premier mensonge de la phase. Ce qui est tenu :

- **zéro perte** — le message est dans le store, validé, avant qu'un octet ne sorte ;
- **zéro doublon** — rien n'est remis automatiquement quand le serveur a *peut-être* le sien ;
- **le doute est borné** — il n'existe que sur la fenêtre entre le point final et la réponse.

Le mécanisme est un **ordre d'écriture**, et il est écrit à un seul endroit
(`mailsmtp::queue::deliver_one`) :

```text
  store: 'sending'    (durable, fsync)
  réseau: EHLO, AUTH, MAIL FROM, RCPT TO, DATA  → 354
  store: 'committing' (durable, fsync)   <- la frontière
  réseau: le corps, puis le point final
  réseau: la réponse du serveur
  store: 'sent' ou 'failed'
```

Tué **avant** la frontière : le serveur a une transaction vide, qu'il abandonne à son propre
délai. La ligne est en `sending`, donc remise — et ne pas la remettre serait une perte. Tué
**après** : la ligne est en `committing`, et personne ne la remet.

**Le sens de l'erreur est choisi.** La frontière peut créer un *faux doute* — écrite, puis le
processus meurt avant le premier octet du corps. Elle ne peut pas créer un faux « rien n'est
parti ». Un faux doute coûte une décision à l'utilisateur ; l'inverse coûte un doublon chez le
destinataire, que personne ne peut retirer.

#### La mesure : un processus enfant, tué à un instant choisi

`crates/mailsmtp/tests/exactly_once.rs`. Le binaire de test se relance lui-même — `current_exe`,
filtre sur un seul test, trois variables d'environnement — parle à un serveur SMTP scripté tenu
par le parent, et **sort** à l'instant nommé. Le parent relit le store et compte ce que le serveur
a accepté.

| coupure | serveur | store après | reprise | total reçu |
|---|---|---|---|---|
| après le `354`, avant la frontière | rien | `sending` | remise | **1** |
| après le `250`, avant l'écriture locale | le message | `committing` | **rien** | **1** |
| aucune (contrôle positif) | le message | `sent` | — | **1** |

La deuxième ligne est le libellé exact du critère. Les trois ensemble sont ce qui rend la mesure
utile : sans le contrôle positif, un transport qui n'enverrait **jamais rien** passerait les deux
premières — zéro doublon est trivial quand zéro message part.

**Pourquoi un vrai processus et pas une `Err`.** Un test qui rend une erreur a laissé tourner les
destructeurs, vidé les tampons, fermé la connexion SQLite : il valide un monde où rien de ce qui
menace la garantie ne se produit. Ce que le critère met en cause n'est pas la gestion d'une
erreur, c'est l'ordre des écritures sur le disque.

**Ce que la mesure ne couvre pas.** `exit()` rend la main au système, donc le cache de pages garde
les écritures non synchronisées : c'est une coupure de *processus*, celle du critère. Pour une
coupure d'*alimentation*, la défense est `synchronous = FULL` sur les transitions de la file — un
`fsync` par transition — et un disque qui ne mente pas sur ses `fsync`. Le premier est vérifiable
et vérifié ; le second est hors de portée du programme, et le dire vaut mieux que l'ignorer.

#### Les contrôles négatifs, dans les deux sens

La règle vit à deux endroits : la clause `WHERE` de `Store::deliverable` et
`SendState::is_deliverable`. Les deux ont été cassés à tour de rôle :

| ce qu'on casse | ce qui tombe |
|---|---|
| le filtre SQL accepte `committing` | le test du critère, **et** le test d'accord |
| le prédicat Rust accepte `committing` | le test d'accord seul |

Le premier essai n'a d'ailleurs rien attrapé : élargir le prédicat Rust seul laissait le filtre SQL
faire son travail, et le test du critère passait. C'est ce qui a fait écrire
`the_sql_filter_and_the_rust_predicate_agree` — deux barrières pour une règle, et un test qui
vérifie qu'elles disent la même chose. Élargir le SQL, lui, fait tomber le test du critère sur la
**seconde** barrière (`deliver_one` refuse une ligne douteuse qu'on lui passe directement), ce qui
est la preuve que les deux servent.

#### TLS, et le trou par lequel une injection passait

`mailsmtp::tls::connect` reprend le patron de `mailsync::tls` : magasin de confiance du système,
aucune dérogation, aucun repli en clair. Trois différences, toutes venues du protocole :

- **l'`EHLO` encadre le `STARTTLS` des deux côtés.** Il en faut un *avant* pour savoir si
  `STARTTLS` est annoncé, et un *après* parce que la RFC 3207 §4.2 demande d'oublier le premier ;
- **les ports sont 465 et 587**, pas 993 et 143. D'où `Security::submission_port`, pour que
  personne ne lise le port IMAP pour un envoi ;
- **`Client::into_stream` refuse un tampon non vide.** C'est le trou : ce qu'un serveur envoie
  après son `220 Ready` et avant la poignée de main est du texte en clair qu'un attaquant en
  position d'écrire sur le fil peut choisir, et il serait relu *après* le passage en TLS comme s'il
  en venait. La RFC demande de l'oublier ; le jeter en silence suffirait pour la lettre de la RFC
  et laisserait passer la tentative sans bruit. C'est donc une erreur.

Le test de ce refus était d'abord écrit **sur socket**, et il passait en déclarant le tampon vide
alors que le serveur avait bien envoyé les deux réponses : l'instant du tampon dépend de
l'ordonnanceur. Réécrit en mémoire, il n'a plus de course — et son contrôle négatif est resté sur
socket, parce que lui vérifie autre chose : que le dialogue *en clair* se borne à `EHLO` et
`STARTTLS`. C'est la seule assertion du crate sur ce qu'un observateur du réseau peut lire.

#### Deux fuites trouvées par des tests écrits pour elles

**`client_name` recopiait le domaine tel quel.** `client_name("a@b.fr\nQUIT")` rendait
`"b.fr\nQUIT"` — une injection de commande SMTP dans l'`EHLO`. Le domaine est maintenant validé
caractère par caractère, et le repli est `localhost`, qui ne désigne personne. Le nom annoncé finit
dans les en-têtes `Received` du destinataire : `DESKTOP-4F2K9A` y dirait le nom du poste de travail
à tous les correspondants.

**`Credential` dérivait `Debug`, donc affichait le secret.** Un `tracing::debug!(?credential)`
ailleurs dans le programme l'aurait mis dans un journal — `docs/PRIVACY.md` §8. Le `Debug` est
maintenant écrit à la main dans `mailsmtp` **et** dans `mailsync` : il ne montre que le mécanisme.
La dérivation est le défaut dangereux ici, et elle est correcte pour tout le reste du projet — le
test est ce qui empêche la prochaine dérivation distraite.

#### Le serveur de soumission n'est pas déduit

Schéma v4 : `smtp_host`, `smtp_port`, `smtp_username`, `smtp_auth`, `smtp_security`, nullables.
`imap.gmail.com` → `smtp.gmail.com` est vrai, et la déduction marche jusqu'au jour où elle échoue —
ce jour-là, elle envoie le message au mauvais endroit. `mail account list` **suggère** un hôte et
n'en écrit aucun ; `mail account submission` l'écrit.

Deux colonnes sont obligatoires ensemble : l'hôte et le mode de chiffrement. Les trois autres se
replient sur celles de la lecture, où le repli est sans danger. Le mode de chiffrement, jamais : le
deviner rétrograderait le chiffrement à l'insu de l'utilisateur. Une configuration à moitié écrite
est refusée à la lecture, parce que la taire ferait croire à l'utilisateur qu'il a configuré
l'envoi.

Le secret n'est pas redemandé : c'est celui de la lecture, déjà dans le trousseau sous la clé
`(host, username)` du serveur IMAP. **Conséquence à connaître** : un fournisseur qui exigerait un
secret d'envoi distinct n'est pas géré.

#### Le premier envoi réel, et son retour

Store réel (5 comptes, 48 548 contenus, 6,3 Gio), migré v2 → v4 sans incident.

    $ mail account submission --account 5 --host smtp.gmail.com --security starttls
    Compte #5 : envoi par smtp smtp.gmail.com:587 starttls oauth2.

    $ mail send --account 5 --to moi@perso2.invalid --subject "Essai mailcore — …" --body "…"
    Message #1 en file (454 octets).
    #1 envoyé.

`STARTTLS` sur 587, `AUTH XOAUTH2` avec le jeton du compte, sujet non ASCII en mot encodé RFC 2047,
et une ligne du corps commençant par un point — le piège qui tronque un message quand le doublage
manque.

Puis l'aller-retour, qui est ce que le critère 1 demande vraiment : `mail sync --account 5`, `mail
index`, et le message ressort de tantivy. Les trois vérifications qui comptent :

| ce qu'on cherche | résultat |
|---|---|
| le sujet accentué | `Essai mailcore — premier envoi`, décodé juste |
| la ligne **avant** le point | trouvée |
| la ligne **commençant** par un point, et celle d'après | trouvées, entières |

La troisième ligne est celle qui prouve le doublage : Gmail a retiré le point ajouté, comme la RFC
5321 §4.5.2 le demande, et la moitié d'après n'a pas été interprétée comme des commandes SMTP.

Une note de méthode : la première recherche de cette ligne, écrite sans guillemets, n'a rien
ramené — d'autres messages du corpus scoraient plus haut sur les mots isolés. Ça ressemblait à une
troncature. La requête en phrase exacte a répondu. **Une absence de résultat n'est pas une preuve
d'absence** quand la requête n'est pas celle qu'on croit poser.

#### Ce que ça change pour l'utilisateur

`mail doctor` compte maintenant la file, et les envois douteux sont le **seul** point de son
diagnostic qui demande une décision humaine plutôt qu'une commande à relancer. `mail outbox`
affiche les états en français, `committing` compris : « DOUTEUX — peut-être parti, aucune reprise
automatique ».

Et `mail send --dry-run` est le premier essai à faire : il assemble, affiche l'enveloppe et les
en-têtes, et n'ouvre aucune connexion. C'est ce qui montre qu'une copie cachée est dans l'enveloppe
et **dans aucun en-tête** — vérifié sur le chemin réel, pas seulement en test unitaire.

#### Un test rendu moins fragile

`without_a_front_directory_nothing_is_served_at_the_root` a échoué une fois pendant un
`cargo test --workspace` : seize démons se lancent en parallèle, et l'un n'avait pas ouvert sa
socket dans les cinq secondes du budget. La suite repassait seule et en bloc, ce qui dit que c'est
le budget et non le démon. Monté à vingt secondes — quinze secondes qu'il n'utilisera jamais
coûtent moins cher qu'un échec une fois sur vingt selon la charge.

#### Ce qui n'est pas encore là

- **Les pièces jointes et le `base64` streamé**, donc le critère 3. `compose` fait le
  `multipart/alternative` ; il ne fait pas encore le `multipart/mixed`.
- **La boucle d'envoi du démon.** `mail send --flush` remet à la main ; `maild` ne surveille pas
  encore la file, alors qu'il surveille déjà l'arrivée en IMAP.
- **Le critère 7** : le harnais à secrets de la phase 2 n'a pas encore été passé sur la file
  d'envoi ni sur les journaux SMTP.
- **Le critère 1 n'est pas clos** : l'aller-retour est vérifié, la conformité d'une *réponse* —
  `In-Reply-To`, `References` sur un vrai fil — ne l'est pas.
- **Le critère 8 est à moitié** : les familles d'erreur existent et sont affichées en français ;
  aucune n'a encore été **provoquée** par un vrai serveur.
- **Tout le reste de la phase** : la rédaction dans la coquille, le carnet, la signature, les
  invitations.

---

### 2026-09-09 (soir) — étape 5 : la rédaction, et le facteur qui vide la file

L'envoi passe maintenant par l'interface, et la file part toute seule. Trois morceaux, dans
l'ordre où ils dépendent l'un de l'autre : le facteur, les méthodes d'API, la fenêtre.

#### Le facteur, et pourquoi l'interface n'envoie rien elle-même

`maild::outbox::Postman` est un fil qui vide la file. Un clic sur « Envoyer » écrit une ligne et
rend la main ; c'est ce fil-là qui parle au serveur. C'est la règle 3 du `CLAUDE.md`, et le
calcul est simple : une poignée de main TLS, une authentification et un transfert se comptent en
dizaines de secondes, parfois en minutes sur une pièce jointe. Une interface qui attend ça est
figée.

Le corollaire compte autant : **le message est en file avant d'être parti**. Il survit donc à la
fermeture de la fenêtre, à l'arrêt du démon et à une coupure. La fenêtre le dit avec ces mots —
« il partira même fenêtre fermée » — parce que ce n'est pas ce qu'un client mail fait
d'habitude.

**Ce que le facteur ne décide pas** : ni le doute, ni le recul, ni l'abandon. Tout ça est dans
`mailsmtp::queue::deliver_one`, qui est aussi ce qu'appelle `mail send`. Deux moteurs, une seule
règle — et c'est délibéré : une deuxième copie de la décision finirait par diverger, et la
divergence s'appelle ici un doublon chez le destinataire.

Il dort sur une variable de condition, pas sur un délai. `Postman::nudge` le réveille après un
`outbox.send` ; sans ça, un envoi attendrait le tic suivant, et une interface qui met trente
secondes à partir donne l'impression de n'avoir rien fait. Le tic de trente secondes reste comme
filet : il rattrape les messages reportés par un recul, et ceux qu'un autre processus —
`mail send` — a mis en file sans pouvoir prévenir.

Le drapeau de réveil est **consommé** au réveil, et c'est le seul endroit du fil qui mérite un
test : sans la consommation, un `nudge` posé pendant un passage ferait tourner la boucle en
continu — un démon au repos à cent pour cent d'un cœur.

#### Trois méthodes d'API, et ce qu'elles refusent d'exposer

`accounts.list`, `outbox.list`, `outbox.send`. Toutes trois « servies par le transport », comme
`jobs.start` : elles ont besoin du trousseau du système et du facteur, dont une boîte mail ne
sait rien.

`accounts.list` ne rend **ni hôte, ni port, ni mécanisme, ni secret**. Un client qui rédige a
besoin de trois choses : quel compte, sous quelle adresse, et *est-ce que celui-là peut
envoyer*. Un hôte et un port dessinent l'infrastructure de messagerie de l'utilisateur, et
quiconque détient le jeton les lirait. Un test fige la forme plutôt que la confiance : il
cherche `smtp.exemple.invalid` et `587` dans la réponse et échoue s'il les trouve.

`outbox.send` n'a **ni champ `from`, ni champ `headers`**, et les deux absences sont des
décisions :

- pas de `from`, parce que l'expéditeur est l'identifiant du compte. Un client qui détient le
  jeton ne peut donc pas usurper une adresse. Un champ en trop est *ignoré* par serde plutôt que
  refusé, donc un test vérifie que l'ignorer n'a pas d'effet : la ligne part sous l'identifiant
  du compte, quoi que le client ait écrit ;
- pas de `headers`, parce que la liste que le démon écrit est courte et fixe. C'est ce qui rend
  vraie la promesse de ce document : tout ce que le message porte est lisible par l'utilisateur.
  Un `headers` libre serait un `X-Mailer`, puis un identifiant de suivi, puis un pixel.

Et le refus a lieu **avant** l'écriture. Une adresse qui porte un retour à la ligne, aucun
destinataire, un compte sans serveur d'envoi : rien n'est écrit, et le test le vérifie sur le
store, pas sur la valeur rendue. Mettre en file un message qui ne peut pas partir remplirait la
file de lignes que personne ne peut remettre.

#### Un fixture faux depuis le début, révélé par la première lecture des comptes

Le fixture partagé des tests d'API créait un compte de type `imap` **sans serveur**, ce que
`Store::full_accounts` refuse à juste titre — un hôte vide finirait dans un résolveur. Personne
ne le voyait, parce qu'aucune méthode d'API ne lisait les comptes. `accounts.list` les lit, et
le premier test qui s'en est servi est tombé sur « les comptes ne sont pas lisibles ».

Le fixture déclare maintenant un compte cohérent, sans serveur de soumission — ce qui en fait
d'ailleurs le contrôle négatif de l'envoi, gratuitement.

#### La réponse reprend le fil, elle ne le reconstruit pas

`References` n'était pas exposé : `mailcore` le relit du blob au moment du threading et ne le
garde pas. `MessageDetail` porte maintenant la chaîne, lue **du seul en-tête `References`**, dans
l'ordre.

La distinction avec `thread::referenced`, qui mêle `In-Reply-To` et `References`, est réelle :
pour **rattacher** un message, l'ordre est sans importance et tout lien est bon à prendre ; pour
**répondre**, la RFC 5322 §3.6.4 demande la chaîne du parent prolongée de son `Message-ID`, dans
l'ordre. Reconstruire depuis le fil local donnerait une chaîne différente de celle des autres
clients, et un lecteur qui regroupe sur `References` verrait deux fils au lieu d'un.

Un message sans `Message-ID` — le corpus en a — donne une réponse sans `In-Reply-To` plutôt
qu'un en-tête inventé.

#### La fenêtre

Une fenêtre `egui`, pas un panneau : elle se déplace, se redimensionne, et surtout **ne remplace
pas la liste** — on écrit en regardant le message auquel on répond. C'est la première chose qui
manque dans les clients qui ouvrent la rédaction en plein écran.

Ce qu'elle ne fait pas : **valider les adresses**. La validation est chez le démon, qui écrit la
commande SMTP ; une copie ici serait une deuxième règle à tenir d'accord avec la première, et du
mauvais côté puisque le démon sert aussi d'autres clients. Un test fige cette limite : une
adresse hostile passe le découpage **intacte**, et c'est le démon qui la refuse.

Ce qu'elle fait, elle : montrer le refus, et **ne pas jeter le brouillon** quand il arrive. Un
refus veut dire que le message n'est pas en file ; effacer ce que l'utilisateur vient d'écrire
serait le punir d'une faute de frappe.

Trois détails qui viennent d'un usage et pas d'une spécification :

- le découpage des destinataires accepte la virgule **et** le point-virgule. Outlook sépare au
  point-virgule, et un utilisateur qui colle une liste venue d'ailleurs la collerait telle
  quelle. Une virgule finale n'est pas une adresse vide : c'est une frappe ordinaire ;
- un brouillon qui n'a **que** des copies cachées est envoyable. C'est un cas réel — une annonce
  à une liste dont les destinataires ne doivent pas se voir — et le refuser serait une politesse
  imposée. Le sujet et le corps, eux, peuvent être vides ;
- le bouton est désactivé pendant qu'un envoi est en vol. Sans ça, deux clics rapides mettent
  **deux** messages en file, et le destinataire en reçoit deux. C'est le seul doublon de tout le
  chemin que la file d'envoi ne peut pas empêcher : elle garantit qu'un message part une fois,
  pas que l'utilisateur n'en a demandé qu'un.

Le brouillon vit dans un `Option`, et le fermer le jette. C'est assumé : les brouillons persistés
sont une fonction en soi — un dossier `Drafts` côté serveur, une synchronisation, une reprise. Un
`Option` dit la vérité sur ce que le programme fait ; un brouillon gardé en mémoire ferait croire
à une persistance qui n'existe pas.

#### Ce que la barre du haut dit de la file

Un bouton « File », et son libellé porte l'information qui compte :

| état | libellé |
|---|---|
| rien | `File` |
| trois en attente | `File (3)` |
| un douteux | `File ⚠ 1` |

Le doute passe **devant** le compte en attente, parce que c'est le seul état qui demande une
décision et qu'il ne doit pas se noyer dans un nombre. Dans le panneau, un envoi douteux est
écrit avec les mots de la situation : « La coupure est tombée entre la fin de l'envoi et la
réponse du serveur : il a peut-être le message. Rien ne sera renvoyé automatiquement. » Pas un
code, pas une icône d'erreur qui laisserait croire qu'il n'est pas parti.

Et « ✉ Écrire » est désactivé quand aucun compte n'a de serveur d'envoi, avec l'explication au
survol. Ouvrir un formulaire dont le bouton refuse sans dire pourquoi est exactement ce que le
critère 8 interdit.


#### La vérification de bout en bout, sur le corpus réel

`maild http` sur le bouclage, store réel, et trois appels :

| appel | ce qu'il a montré |
|---|---|
| `accounts.list` | cinq comptes, `can_send` vrai pour le seul configuré, **aucun hôte ni port** dans la réponse |
| `outbox.send` | ligne 2 en file, 609 octets, deux destinataires d'enveloppe |
| `outbox.list` | `state: sent`, `doubtful: false`, `attempts: 1` |

Le journal du démon, dans l'ordre : `facteur démarré` au lancement, puis — après l'appel —
`connexion de soumission établie host=smtp.gmail.com port=587 size_limit=Some(35882577)`, puis
`message remis au serveur de soumission job=2`. Onze secondes entre l'appel et la remise, dont
l'essentiel est la poignée de main TLS et l'authentification : le réveil a bien court-circuité le
tic de trente secondes.

Puis `jobs.start kind=sync account=5`, et le message revient : `fetched: 1, duplicates: 1` — le
contenu était déjà dans le store, puisque c'est nous qui l'avons écrit, et la dédup par contenu
le dit. `messages.get` sur la ligne rendue donne :

- le sujet avec son tiret cadratin et ses accents, intact ;
- le corps entier, y compris `.elle doit arriver entière` **et** `et la suite après elle.` — le
  doublage du point a tenu sur le chemin réel ;
- `references: []`, ce qui est correct pour un message qui ouvre un fil ;
- `to` avec une seule entrée : **aucun en-tête `Bcc`**, alors que l'enveloppe en portait un.

Une note de méthode, parce qu'elle a failli me faire conclure à une corruption : la même réponse,
lue à travers `curl | python -m json.tool` dans un shell Git-Bash, affichait
`Envoi par le facteur â€" Ã©tape 5`. Un message du corpus **antérieur à tout ce travail** montrait
le même motif, ce qui a désigné le tube et non le store. Relue dans un outil qui respecte
l'UTF-8, la réponse est juste. **Un affichage abîmé n'est pas une donnée abîmée**, et le contrôle
qui tranche est de regarder une donnée dont on sait qu'elle était bonne avant.

#### Ce qui n'est pas encore là

- **Le HTML dans la rédaction.** Le champ est un `TextEdit` multiligne : du texte brut. Le
  `multipart/alternative` existe côté `compose`, l'éditeur riche est l'étape 7.
- **Les pièces jointes**, donc le critère 3.
- **Les brouillons persistés**, et le transfert.
- **Le carnet d'adresses**, donc le critère 4 : les champs de destinataires n'ont pas
  d'autocomplétion. C'est l'étape 6.

---

### 2026-09-09 (nuit) — étape 6 : le carnet, et le critère 4

Le critère 4 est tenu avec 26 fois de marge. Ce qui suit dit surtout **où était le travail**, et
ce n'était pas la latence.

#### Le problème n'était pas de trouver, c'était de classer

Sur le corpus réel, taper « a » correspond à **3 021 adresses sur 4 635**. Un carnet qui les rend
par ordre alphabétique est inutilisable : la personne cherchée est page huit. La latence, elle,
n'a jamais été en cause — 4 635 lignes tiennent en mémoire.

Deux signaux, et ils ne valent pas la même chose :

- **avoir écrit à quelqu'un est une intention** ;
- **avoir reçu de quelqu'un est une statistique**. Une lettre d'information monte à des
  centaines de réceptions sans qu'on lui ait jamais répondu.

Sur ce corpus, la mesure le confirme : la première adresse a 502 réceptions **et** 135 envois ;
une autre en a 638 pour 71 envois. Les compter ensemble ferait remonter les diffusions.

D'où deux colonnes, un poids sur les envois, et un **plafond** sur les réceptions. Le plafond est
le paramètre le plus important du module, et il doit rester strictement sous le poids des envois.

**Au premier jet, il ne l'était pas** : plafond à cinq, poids à quatre. Le test
`writing_once_beats_receiving_a_thousand_times` a échoué en le disant, et il avait raison — la
documentation affirmait une propriété que les constantes contredisaient. C'est exactement ce
qu'un test de propriété doit attraper : pas un bogue de calcul, une incohérence entre l'intention
écrite et le code.

Le résultat, sur le corpus réel, en tapant « a » : les huit premiers sont les huit correspondants
principaux de l'utilisateur. Aucune diffusion, aucun `noreply@`.

#### `lower()` de SQLite ne replie que l'ASCII

Le deuxième défaut, et il aurait été **invisible** : `instr(lower(name), 'éloïse')` ne trouve
jamais « Éloïse ». La majuscule accentuée reste telle quelle. Sur un corpus français, c'est la
plupart des noms.

Trouvé par `a_non_ascii_prefix_still_finds_a_name`, écrit pour ça. La correction est une colonne
`name_fold`, repliée **par Rust** — dont `to_lowercase` est Unicode — et la recherche compare
deux chaînes déjà repliées.

Les accents sont **gardés** : replier « é » en « e » ferait trouver « Eloise » en tapant
« éloïse », ce qui est souhaitable, mais aussi confondre des noms distincts. Ça demande une table
de translittération, et c'est un choix à mesurer, pas à improviser.

#### La recherche est une sous-chaîne, pas un préfixe

`instr` et non `LIKE 'x%'` sur le nom, donc taper « dup » trouve « Marie **Dup**ont ». C'est la
recherche qu'un utilisateur fait vraiment, et un index sur un préfixe de nom ne la permettrait
pas — d'où l'absence d'index, documentée comme un choix et non comme un oubli.

Le coût : « ann » remonte aussi « Y**ann** » et « Yo**ann** ». Sur ce corpus ça ne gêne pas,
parce que le classement les met après les vrais résultats — vérifié. Sur un corpus beaucoup plus
grand, le bruit pourrait remplir les huit places, et il faudra alors préférer les
correspondances en début de mot. **Pas encore**, parce que ce n'est pas encore un problème.

#### La mesure

`cargo xtask measure-complete`. Ce qui est simulé est une **frappe** et non une requête : `a`,
puis `an`, puis `ann`… Les préfixes viennent des vingt adresses les mieux classées du carnet
réel, parce que leur distribution de premières lettres est celle du corpus — une liste inventée
mesurerait un carnet imaginaire.

| relevé | p50 | p95 | p99 | max | pire préfixe |
|---|---|---|---|---|---|
| 3 160 frappes | 180,8 µs | 585,8 µs | 1,17 ms | 1,42 ms | « a » → 3 021 adresses |
| 6 320 frappes | 191,5 µs | **643,8 µs** | 1,23 ms | 2,02 ms | « a » → 3 021 adresses |

Le premier relevé a été pris juste après une compilation, ce que le `CLAUDE.md` interdit. Le
second, machine retombée au repos, donne 10 % de plus — de la dispersion, pas un symptôme, et le
verdict ne dépendait pas d'un facteur 26.

Le contrôle qui rend le relevé interprétable est la dernière colonne : sans elle, un p95
flatteur obtenu sur des préfixes qui ne correspondent à rien serait indiscernable d'un vrai. La
mesure échoue d'ailleurs bruyamment si le pire préfixe touche moins de cinquante adresses.

#### La reconstruction

135 secondes pour 48 551 messages, 4 635 adresses retenues, **1 737 messages reconnus comme
envoyés**. Ce dernier nombre est le contrôle du carnet : un zéro voudrait dire que le classement
n'a plus qu'un signal sur deux, et l'autocomplétion redeviendrait « par ordre de réception ».
`mail contacts rebuild` le dit explicitement quand c'est le cas.

Le tri « envoyé » / « reçu » se fait sur les adresses des comptes : un message dont le `From` est
l'une d'elles est un message envoyé, et ses `To`/`Cc` sont des adresses à qui on a écrit. Sans
cette règle, le seul en-tête lu avec certitude serait `From`, et le carnet ne saurait que « qui
m'écrit ».

Trois décisions de plus, chacune avec son test :

- **la reconstruction est complète, pas incrémentale.** Les compteurs s'ajoutent, donc repasser
  sur un message déjà compté le compterait deux fois. Une mise à jour incrémentale demanderait de
  savoir quels messages ont déjà été vus — une colonne de plus et une dérive silencieuse. La
  reconstruction complète est idempotente par construction, et
  `two_rebuilds_give_the_same_book` le vérifie ;
- **un nom ne recule jamais.** Il n'est écrasé que par un message plus récent, et un nom absent
  n'écrase rien. Sans ça, l'ordre de parcours déciderait du nom affiché ;
- **l'utilisateur ne se propose pas lui-même.** Son adresse est dans le `To` de tout ce qu'il
  reçoit, donc elle arrive forcément au carnet.

#### L'autocomplétion dans la fenêtre

`contacts.complete` est servie par le **répartiteur** et non par le transport, contrairement à
`outbox.send` : c'est une lecture du store. Conséquence utile — `mailmcp` l'a aussi, et un modèle
qui rédige un message a le même besoin qu'un humain.

Deux choses ont demandé du soin côté coquille, et les deux sont des fonctions pures avec leurs
tests :

**Le fragment complété est le dernier, pas tout le champ.** Un champ contient
« jean@x.fr, mar » : ce qu'on complète est « mar ». Sans ce découpage, taper un second
destinataire ne proposerait rien — le champ entier ne correspond à aucune adresse.

**L'insertion garde les destinataires déjà saisis.** Remplacer tout le champ les effacerait, ce
qui est le pire défaut qu'une autocomplétion puisse avoir : elle détruirait le travail qu'elle
prétend accélérer. Le cas dégénéré — un champ qui n'est que des séparateurs — a d'ailleurs
révélé un vrai défaut : deux `trim` enchaînés s'arrêtent au premier caractère de l'autre sorte,
et `" ; , "` gardait un « ; » orphelin. Un seul passage sur un prédicat le corrige.

Et une réponse en retard est **jetée**. Le fil du service est sériel, donc une complétion peut
arriver après que l'utilisateur a tapé une lettre de plus ; l'afficher ferait clignoter la liste
entre deux états. Un jeton par demande, comme le curseur de `Reply::Page` — la même leçon que la
phase 1, appliquée sans avoir eu à la réapprendre.

Le score est affiché au survol de chaque proposition. C'est ce qui rend le classement
**critiquable** au lieu d'être subi : « pourquoi celui-là d'abord ? » a une réponse lisible.


#### Correction : le redimensionnement de la fenêtre de rédaction ne servait à rien

Question posée après coup — « à quoi sert de rétrécir la fenêtre nouveau message ? » — et la
réponse honnête était **à rien**. Deux défauts, dans les deux sens :

**Vers le bas, aucune borne.** `resizable(true)` avait été mis pour l'agrandissement, et egui
laisse par défaut réduire jusqu'à seize pixels. En dessous d'une certaine taille, les étiquettes
ne tiennent plus à côté de leurs champs : le formulaire est là et ne sert à rien, ce qui est pire
qu'une fenêtre fixe. Bornée à 420 × 300 px — la largeur qui garde « Copie cachée » et une adresse
sur la même ligne, la hauteur qui garde les en-têtes, du corps et le bouton visibles ensemble.

**Vers le haut, rien ne suivait.** Le corps avait `max_height(320.0)` et `desired_rows(12)`, deux
valeurs figées — donc agrandir verticalement n'agrandissait pas le corps, qui est la seule chose
qu'on veuille agrandir. Et le commentaire d'à côté disait « le corps prend la place qui reste » :
il affirmait exactement le contraire du code.

C'est le même genre d'écart que le plafond du carnet, et il vaut d'être noté comme tel :
**un commentaire qui décrit une intention non implémentée est pire qu'aucun commentaire**, parce
qu'il fait passer la relecture suivante à côté.

Le corps suit maintenant la hauteur disponible, moins une réserve pour le séparateur et le
bouton — sans elle, le corps pousserait « Envoyer » hors de la fenêtre. Le nombre de lignes
demandé au `TextEdit` en est dérivé plutôt qu'écrit en dur : `desired_rows` est un nombre de
lignes, pas une hauteur, donc une valeur fixe parie sur une taille de police.

Deux garde-fous, et le second est le plus intéressant :

- `rows_for` ne rend jamais moins de trois, y compris sur une hauteur négative ou `NaN` — un
  `desired_rows(0)` donne un champ d'une ligne dans une fenêtre pourtant grande ;
- la cohérence des quatre constantes est un `const { assert!(…) }`, donc **une erreur de
  compilation** et non un test. Vérifié en abaissant la hauteur minimale à 120 px : le build
  échoue en nommant le problème. Un test l'aurait dit aussi ; un `const` le dit avant qu'il
  existe un binaire pour le tester.

#### Ce qui n'est pas encore là

- **Aucune reconstruction automatique.** `mail contacts rebuild` à la main, ou le job
  `contacts` du démon. Le carnet ne suit donc pas une synchronisation : une personne à qui on
  vient d'écrire n'apparaît qu'au prochain passage. C'est le prochain morceau à écrire, et il
  demande de rendre la reconstruction incrémentale — donc de résoudre le double comptage
  autrement.
- **Aucune fiche.** Pas de nom modifiable, pas de regroupement de plusieurs adresses d'une même
  personne. CardDAV est hors phase, et une fiche sans protocole pour la remplir serait un
  formulaire vide.
- **Le démarrage de la coquille est à 178,5 ms** avec tout ça — sous le seuil de 225 ms du
  critère 1 de la phase 2. Relevé juste après compilation, donc pessimiste.

---

### 2026-09-09 (nuit, suite) — trois questions d'usage, quatre défauts

Trois remarques posées en regardant l'interface, et chacune a mis au jour un défaut réel. C'est
la valeur d'un utilisateur qui se sert du logiciel : aucun de ces quatre points n'était visible
depuis les tests.

#### « La rédaction n'est pas une vraie fenêtre »

Exact, et c'était un `egui::Window` — un panneau flottant **dans** la fenêtre principale.
Conséquences, toutes constatables : impossible de le déplacer sur un autre écran, absent de la
barre des tâches et d'Alt-Tab, coupé par les bords de la fenêtre parente. L'intention documentée
était « écrire en regardant le message auquel on répond » ; elle n'était pas tenable.

C'est maintenant un **viewport immédiat** — une vraie fenêtre du système. Immédiat et non
différé : le rappel d'un viewport différé doit être `Send + Sync + 'static`, donc il ne peut pas
toucher le brouillon local, et il faudrait mettre tout l'état de la rédaction derrière un
`Arc<Mutex<…>>`. Le prix de l'immédiat est que la fenêtre parente redessine quand l'enfant
redessine, ce qui pour une fenêtre ouverte quelques minutes est sans conséquence.

Le changement a demandé de séparer **où** dessiner de **quoi** dessiner : `compose_window` tient
la fenêtre, `compose_form` prend `&self` et dessine, `settle_completion` prend `&mut self` et
décide. La séparation n'est pas un rangement — c'est ce qui a rendu le changement de contenant
possible sans toucher au formulaire, et c'est ce qui rend impossible de demander une complétion
depuis une fermeture qui tient déjà `ui`.

Le « chevron » de la même remarque était la flèche de replier d'`egui::Window`. Elle disparaît
avec le contenant, sans une ligne pour ça.

#### « Le statut d'un mail ne se met pas à jour, il reste non lu »

Exact aussi, et pour une raison simple : **rien ne marquait un message lu**. `MessageFlags::SEEN`
n'était jamais qu'**lu** du serveur. `docs/PHASE-2.md` annonçait « pas d'écriture côté serveur
IMAP, sauf le drapeau `\Seen` » — l'exception était déclarée, elle n'avait jamais été écrite.

La conception a dû répondre à un piège du schéma. `refs.flags` est **dérivé** de
`remote_uids.flags` : y écrire « lu » directement serait effacé au prochain recalcul, donc à la
prochaine moisson, et le message redeviendrait non lu tout seul. La marque va donc dans les
copies, et `refs` en est recalculé.

Mais `remote_uids.flags` est écrasé par ce que le serveur répond à chaque moisson. Sans rien de
plus, le message redeviendrait non lu au premier passage suivant. D'où une table `flag_pushes`
qui retient **ce qu'il reste à dire au serveur**, et un ordre : pousser, **puis** relire.

Trois écritures dans une transaction, et le module ne fait aucun réseau : la poussée est faite
par la moisson, qui a la connexion. Ouvrir un message est un clic, et un clic n'attend pas un
serveur.

##### Le défaut que seul le corpus réel a montré

La poussée était branchée dans `harvest_watched`, ce qui semblait le bon endroit. Le journal du
démon a dit autre chose :

```
compte synchronisé account=5 folders=29 skipped=29
```

**Les vingt-neuf dossiers ont été évités**, donc `harvest_watched` n'a jamais été appelé, donc la
poussée n'a jamais eu lieu. Le raccourci `LIST-STATUS` ne regarde que ce que le serveur annonce —
et une marque posée localement ne change rien de ce qu'il annonce.

Le défaut était complet et silencieux : `mail doctor` comptait la poussée en attente, la moisson
évitait tout, et le `\Seen` ne partait jamais. **Aucun test ne l'avait vu**, parce qu'aucun
n'avait à la fois un dossier *à jour* et une marque *en attente* : les tests de marquage
partaient d'un store neuf, ceux du raccourci n'ouvraient pas de message.

Corrigé par un seul refus en tête d'`up_to_date` — la seule raison **locale** de ne pas sauter un
dossier — et verrouillé par `a_folder_with_a_pending_seen_push_is_never_skipped`. Deux contrôles
négatifs : retirer le refus fait échouer sur « le dossier a été évité », remplacer `SELECT` par
`EXAMINE` fait échouer sur « la poussée est restée en attente ».

##### `EXAMINE` ne permet pas d'écrire, et c'est voulu

La moisson ouvre les dossiers en `EXAMINE`, en lecture seule, précisément pour qu'un bogue ne
puisse pas abîmer une boîte. Un `UID STORE` y est refusé — à juste titre. Le dossier est donc
rouvert en `SELECT` **seulement** s'il a une poussée en attente ; partout ailleurs, `EXAMINE`
reste le chemin. Le droit d'écrire est demandé au dernier moment et pour un dossier à la fois.

Et `+FLAGS.SILENT`, pas `FLAGS` : `FLAGS` **remplace** la liste entière, donc effacerait
`\Flagged` et `\Answered`. La différence est d'un caractère et elle détruit des données de
l'utilisateur.

`mailfake` a dû apprendre `UID STORE`, dont sa documentation disait explicitement « ni `STORE` —
la phase 2 n'écrit pas ». Il refuse après un `EXAMINE`, ce qui rend le refus observable en test
plutôt qu'en production.

##### La vérification, sur Gmail

```
drapeau lu poussé folder=80 pushed=2
compte synchronisé account=5 folders=29 skipped=28
```

`skipped=28` et non 29 : exactement le dossier concerné a été ouvert. Puis un second passage,
qui relit les drapeaux du serveur — le message reste lu. C'est ce qui prouve que la marque a
atteint Gmail et pas seulement le store local.

#### « Et le bouton file ? »

Trois défauts, et le troisième était le pire.

**Le badge mentait.** `Reply::Changed` ne rafraîchissait pas la file, donc « File (1) » restait
affiché après qu'un message était parti. Le facteur travaille en fond, son écriture bouge la
révision du store, et l'abonnement le disait déjà — il suffisait de l'écouter. Un compteur qui ne
se met à jour qu'au clic est un compteur qui ment.

**Le panneau était un historique sans fin.** Il montrait toutes les lignes, `sent` comprises, et
elles ne se purgent jamais — c'est le journal d'envoi, le supprimer perdrait la trace de ce qui
est parti. Après un mois d'usage, la seule ligne qui demande une décision aurait été noyée dans
un mur de « envoyé ». Le panneau montre maintenant ce qui **n'est pas fini** d'abord, puis cinq
envoyés et le compte de ceux qui ne sont pas montrés.

**Et il n'offrait aucun moyen de décider.** Le pire des trois : il affichait « rien ne sera
renvoyé automatiquement » et s'arrêtait là. Un avertissement qu'on ne peut pas lever finit par ne
plus être lu — y compris le jour où il compte. C'est le critère 8 pris à l'envers : l'utilisateur
voyait *quoi* mais pas *quoi faire*.

Deux issues, parce que l'utilisateur revient de chez son fournisseur avec l'une de deux réponses,
et elles demandent l'inverse l'une de l'autre :

| ce qu'il a vérifié | ce qu'il demande | ce qui arrive |
|---|---|---|
| il n'est pas arrivé | « renvoyer » | la ligne repart en file |
| il est arrivé | « clore » | la ligne passe à `sent`, **aucun octet ne part** |
| il ne sait pas | ne rien faire | l'état actuel |

`Store::resolve_doubt` **refuse toute ligne qui n'est pas douteuse**, et ce refus n'est pas de la
prudence : sans lui, la fonction serait un contournement général de la file — un `resend` sur un
identifiant quelconque renverrait un message déjà envoyé, exactement le doublon que tout le reste
du chemin existe pour empêcher. Vérifié sur le store réel :

```
$ mail outbox --resend 1
Error: le message #1 est à l'état « sent » : seul un envoi incertain se tranche.
```

Rien ne l'appelle automatiquement : ni le facteur, ni `--flush`, ni un redémarrage. C'est le
critère 2 — un message peut-être parti ne repart pas sans qu'un humain l'ait demandé.

#### Ce que ces quatre défauts ont en commun

Aucun n'était un bogue de calcul. Trois étaient des **écarts entre une intention écrite et le
code** — un commentaire qui disait « prend la place qui reste » sur une hauteur figée, une phase
qui annonçait une exception jamais implémentée, un panneau qui demandait une décision sans
l'offrir. Le quatrième était une interaction entre deux optimisations correctes prises
séparément.

C'est le même motif que le plafond du carnet plus tôt dans la journée, et il mérite d'être noté :
**la documentation d'une intention n'est pas une garantie de son implémentation.** Ce qui l'a
attrapé ici n'est pas un test — c'est quelqu'un qui s'est servi du logiciel.

---

### 2026-09-09 (21h30) — les pièces jointes, et le critère 3

Le critère 3 est tenu, et la marge est telle qu'elle dit quelque chose de plus que le seuil : la
mémoire ne dépend **pas** de la taille du message.

| pièce jointe | message assemblé | croissance du RSS |
|---|---|---|
| 25 Mo | 34,2 Mio | **428 Kio** |
| 200 Mo | 273,7 Mio | **584 Kio** |

Le seuil était 200 Mo de crête absolue. Le relevé est à 10 Mio de crête, dont 428 Kio de
croissance depuis le repos. Multiplier le message par huit ajoute 150 Kio : c'est du bruit
d'allocateur, pas une fonction de la taille.

#### Ce qu'il a fallu rendre streamé, et dans quel ordre

La chaîne complète, du disque au socket. Chaque maillon en tampon suffisait à ruiner le
critère :

1. **`Draft::write_to`** remplace `assemble` : il écrit dans un puits au lieu de rendre un
   `Vec`. Les pièces jointes passent du magasin au puits par un tampon de quelques kilooctets,
   base64 encodé au passage ;
2. **`BlobStore::put_writer`** rend un puits dont la clé sera celle de ce qu'on y écrit. Sans
   lui, il aurait fallu un fichier temporaire de plus : assembler dedans, puis le relire pour le
   ranger. Or le magasin en écrit déjà un — c'est comme ça que la clé, qui est le hachage, peut
   être connue à la fin ;
3. **`BlobStore::put_reader`** pour l'inverse : ranger un fichier joint sans le charger ;
4. **`Client::finish_data_from`** lit le corps d'un flux, et **`Stuffing`** double les points au
   passage ;
5. **`Transport::deliver`** reçoit un `&mut dyn Read` et non plus une tranche.

#### Deux états portés, et un seul avait son contrôle négatif

`Stuffing` transforme par blocs, donc il porte de l'état entre deux écritures. Deux choses, et
les deux sont faciles à oublier :

**`at_line_start`** — sans lui, un bloc qui commence par un point ne sait pas qu'il est en début
de ligne, le point n'est pas doublé, le message arrive tronqué et la suite est lue comme des
commandes SMTP.

**`pending_cr`** — un bloc peut finir sur un `\r` dont le `\n` arrive dans le suivant. Traiter le
`\r` tout de suite émet un `CRLF`, puis le `\n` du bloc suivant un second : le message gagne une
ligne vide **toutes les 8 Kio**, exactement aux frontières de tampon. Invisible sur un petit
message.

J'ai écrit un test pour chacun. En les cassant exprès, **un seul des deux a échoué** :
remettre `at_line_start` à vrai à chaque écriture passait tous les tests. La raison est que mon
test vérifiait qu'un point en début de bloc **est** doublé — ce que la version cassée fait
aussi, et en plus elle double les points de milieu de ligne. Le destinataire lirait
`www..exemple.fr`.

Le test qui manquait est donc le contrôle négatif : **un point en milieu de ligne n'est jamais
doublé**. C'est la deuxième fois de ce projet qu'un contrôle négatif révèle qu'un test ne
testait pas ce que je croyais.

#### Les pièces jointes sont désignées par leur contenu, jamais par un chemin

`Attachment` porte un `BlobHash`, pas un `Utf8PathBuf`. Un chemin dans une demande de client
donnerait, à quiconque détient le jeton du démon, la lecture de n'importe quel fichier de la
machine — rangé dans un message, puis envoyé où il veut. C'est la même raison qui fait que
`jobs.start` prend un rang et jamais un chemin.

Avec un hachage, un client ne peut référencer que du contenu **déjà** dans le magasin. Y mettre
un fichier est une opération locale, faite par celui qui a le fichier : `mail send --attach` le
fait, un client distant ne peut pas encore.

Conséquence de conception : la pièce jointe existe deux fois — son fichier brut, et son base64
dans le message assemblé. Le premier devient orphelin dès la mise en file, et
`Store::orphan_blobs` le compte. Le supprimer automatiquement demanderait de savoir qu'aucun
autre brouillon ne l'utilise, ce qui suppose des brouillons persistés — ils n'existent pas.

#### La structure MIME s'emboîte, elle ne s'aplatit pas

| texte | HTML | pièces | structure |
|---|---|---|---|
| oui | non | non | `text/plain` |
| oui | oui | non | `multipart/alternative` |
| oui | non | oui | `multipart/mixed` [ `text/plain`, pièces… ] |
| oui | oui | oui | `multipart/mixed` [ `multipart/alternative`, pièces… ] |

Le `mixed` **enveloppe** l'`alternative`. Mettre les deux corps et les pièces au même niveau fait
afficher le texte *et* le HTML l'un après l'autre chez tous les lecteurs : c'est le défaut MIME
le plus courant, et un test l'interdit en vérifiant que le `mixed` précède l'`alternative`.

Deux autres, plus discrets :

- **les deux frontières sont différentes.** Une frontière partagée fermerait les deux niveaux
  d'un coup, et le lecteur perdrait les pièces jointes ;
- **le nom de fichier est encodé**, jamais recopié. Un nom accentué en octets bruts est hors RFC 5322, et un nom qui porterait un
  guillemet ou un retour à la ligne casserait la structure ou y injecterait un en-tête. Un test
  lui donne `a.pdf\r\nX-Injecte: oui` et vérifie que l'en-tête n'apparaît pas.

#### La taille annoncée compte le base64

`MAIL FROM ... SIZE=` s'annonce **avant** le transfert : c'est ce qui permet à un serveur qui
connaît sa limite de refuser tout de suite plutôt qu'après 25 Mo. Annoncer la taille brute
ferait passer un message sous une limite qu'il dépasse d'un tiers, et le refus arriverait au pire
moment — celui où le doute commence.

Mais depuis que le corps est un flux, **personne ne connaît plus sa longueur** : un flux ne la
donne pas. D'où la migration v7, qui met `size` dans la ligne de file — elle est connue à la mise
en file, où le message vient d'être écrit. La relire du blob demanderait de le décompresser une
première fois pour le compter, puis une seconde pour l'envoyer.

Un test vérifie que `Attachment::encoded_len` correspond aux octets réellement écrits, sauts de
ligne compris. Sans lui, c'était un calcul plausible.

#### La mesure a écrit dans le store de production, et c'était ma faute

`measure-attachment` prenait un `--store`. Je l'ai visée sur le store réel — 5 comptes, 6,3 Gio
de courrier — et elle y a laissé **deux lignes de file et 533 Mo de blobs orphelins**.

Rien n'a été perdu et rien n'a été envoyé : les deux lignes étaient à `sent`, donc le facteur ne
les aurait pas reprises. Mais c'est un coup de chance de conception, pas de prudence : une mesure
qui écrit n'a rien à faire dans un store qu'on garde.

La correction est de ne pas lui laisser le choix. `measure-attachment` **crée son propre store
jetable** et n'accepte plus de `--store`. Un paramètre qui peut désigner les données de
production est un paramètre qui les désignera.

#### Le nettoyage a demandé deux fonctions qui manquaient, et en a révélé une troisième

Pour nettoyer honnêtement il a fallu :

- **`mail outbox --forget <id>`**, qui retire une ligne **finie**. Elle refuse `queued`,
  `sending` et surtout `committing` : retirer une ligne en cours perdrait un message que le
  facteur allait remettre, retirer une ligne douteuse effacerait la trace d'un message peut-être
  parti — exactement ce que le critère 2 existe pour conserver ;
- **`mail doctor --purge-orphans`**, qui supprime les blobs que plus rien ne désigne. Le drapeau
  est explicite parce qu'un import écrit ses blobs **avant** les lignes qui les désignent : un
  blob fraîchement écrit ressemble à un orphelin, et purger pendant un import supprimerait du
  courrier en cours d'arrivée. Rien ne l'appelle automatiquement.

Résultat sur le store réel : **458 Mio rendus**, 4,0 Go → 3,6 Go.

Et le décalage qui a tout révélé : le diagnostic comptait **6** orphelins, la purge en a supprimé
**4**. `orphan_blobs` ne lisait que `messages` ; `purge_orphan_blobs`, elle, lisait aussi
`outbox`. Deux définitions d'« orphelin » dans le même module.

Le sens de l'erreur était le bon — compter en trop annonce de la place qu'on n'a pas, l'inverse
aurait purgé des messages en attente d'envoi. Mais c'était un mensonge en attente, et le
prochain écart n'aurait pas forcément été du bon côté.
`the_count_and_the_purge_agree_on_what_an_orphan_is` le verrouille.

#### La vérification de bout en bout

Un envoi réel de 3 Mo à Gmail, puis retour par IMAP :

```
$ mail send --account 5 --attach piece.bin …
Message #3 en file (4.1 Mio).
#3 envoyé.
```

Et ce que `messages.get` rend après resynchronisation :

```
pieces : [{'mime': 'application/octet-stream', 'name': 'piece.bin', 'size': 3145728}]
```

**3 145 728 octets, exactement ce qui est parti.** C'est le contrôle qui compte : si le repliage à
76 caractères ou le doublage des points avaient abîmé quoi que ce soit, la taille décodée
différerait. Le nom du fichier est intact, et le sujet accentué aussi.


#### Le critère 7, et la sonde qui n'envoie rien

Le harnais à secrets de la phase 2 balayait après une moisson IMAP. C'était la moitié du
risque : l'envoi est le second endroit où un secret part sur le fil, et `AUTH PLAIN` porte le mot
de passe en base64 quand `AUTH XOAUTH2` porte le jeton.

Envoyer un vrai message pour le mesurer était exclu : une vérification de confidentialité qui
expédie du courrier à chaque exécution a un effet de bord que personne n'a demandé.

D'où `AuthOnly`, un transport qui fait la connexion chiffrée, l'`EHLO` et l'`AUTH` avec le vrai
secret, puis rend une erreur. **Il n'appelle ni `open_data` ni la frontière du doute** : il n'y a
donc aucun chemin de code par lequel un octet de message pourrait partir, et ce n'est pas une
promesse mais une absence d'appel. Un contrôle le vérifie quand même — un verdict `Sent` fait
échouer la sonde, parce qu'il voudrait dire que la construction a changé.

La conséquence voulue : la file écrit `last_error` avec le texte du serveur, ce qui couvre le
second risque — un secret qui atterrirait dans un message d'erreur stocké.

Le relevé, sur les cinq comptes du corpus :

```
Contrôle positif en mémoire : les 13 aiguilles sont retrouvées, y compris à cheval sur
deux blocs de lecture.
Synchronisation des 5 compte(s)…
Sonde SMTP — authentification réelle, aucun envoi…
  #5 — authentifié sur smtp.gmail.com, verdict Failed
  4 compte(s) sans serveur d'envoi : leur chemin SMTP n'est pas mesuré.

  OK    le store : aucun secret trouvé.
  OK    les journaux capturés au niveau TRACE : aucun secret trouvé.
  OK    le répertoire temporaire du système : aucun secret trouvé.
```

**13 secrets réels, zéro trouvé**, dans le store, les journaux au niveau `TRACE` et `%TEMP%`.

Ce que le relevé dit de ses propres limites, et qu'il faut lire : **un seul compte sur cinq a un
serveur d'envoi configuré**, donc quatre chemins SMTP ne sont pas mesurés. Le harnais le dit
plutôt que de compter cinq comptes exercés — et il échoue bruyamment si *aucun* compte n'a de
serveur d'envoi, parce qu'un balayage qui ne traverse pas le chemin ne dit rien du critère.

#### Le harnais abandonnait sur un fichier verrouillé

Trouvé à la première exécution, et c'est un défaut du harnais et non du programme mesuré :

```
Error: Le processus ne peut pas accéder au fichier car un autre processus en a
verrouillé une partie. (os error 33)
```

Sous Windows, un fichier peut s'ouvrir et refuser de se **lire** —
`ERROR_LOCK_VIOLATION`, quand un autre processus verrouille une partie du contenu. `%TEMP%` d'une
machine qui sert en a toujours quelques-uns : un navigateur, un antivirus, un installeur.

La première version propageait l'erreur, donc le balayage entier abandonnait sur un fichier qui
n'avait rien à voir avec mailcore — et le critère 7 n'était pas mesurable sur une machine
ordinaire.

Les compter et les dire est la seule réponse honnête : **un fichier non inspecté n'est pas une
absence de fuite**, c'est un trou dans la couverture. Cacher le trou serait pire que le nombre.

#### Ce qui n'est pas encore là

- **Les pièces jointes dans la coquille.** Le chemin existe jusqu'à la CLI ; la fenêtre de
  rédaction ne sait pas encore joindre. Le glisser-déposer d'`egui` n'a besoin d'aucune
  dépendance, contrairement à un sélecteur de fichiers — c'est par là qu'il faut passer.
- **Les pièces jointes par l'API.** Un client distant ne peut pas joindre : il faudrait un
  moyen de mettre un fichier dans le magasin, et le seul honnête est un téléversement qui n'est
  pas écrit.

  *Répondu à moitié le 2026-09-10 : `messages.stage_part` laisse joindre **ce qui est déjà dans
  la boîte** — la pièce d'un message reçu — depuis n'importe quel client, distant compris, sans
  ouvrir de champ de chemin. Le téléversement d'un fichier local depuis une machine distante
  reste à écrire.*
- **Rien ne purge la pièce brute** après la mise en file. Elle devient orphelin et le
  diagnostic la compte, ce qui est honnête mais demande un geste.

---

### 2026-09-09 (22h) — la sonde de l'éditeur riche, et le critère 5

L'ordre de travail mettait cette étape en dernier des morceaux d'interface, avec un motif
explicite : *si un éditeur riche performant n'est pas atteignable dans `egui`, il vaut mieux le
découvrir avec le reste de la phase déjà livré.* La sonde répond, et la réponse est nette.

#### Le chiffre qui décide

| ce qui est mesuré | valeur | seuil |
|---|---|---|
| une frappe, image entière, p95 | **0,31 à 0,47 ms** | < 16,7 ms |
| la mise en page seule, 9 800 glyphes | **0,014 ms** | — |
| un collage de 50 Ko de HTML | **0,4 ms** | ne doit pas figer |

Le critère 5 est tenu avec un facteur trente-cinq au minimum. Et la mise en page — la seule chose
qui dépende de la taille du document — coûte **quatorze microsecondes**.

#### Le modèle qui suffit

Un `String` plus une liste d'intervalles stylés, et un `layouter` qui en construit un
`LayoutJob`. Pas de rope, pas de piece table : une insertion décale les intervalles qui suivent
le curseur, ce qui fait quelques dizaines d'entiers, et le coût réel est la mise en page — qu'
aucune structure de document n'évite.

C'est la conclusion utile de la sonde : **le modèle compliqué n'a pas de raison d'être.**

#### Le collage réutilise ce qui existe, et c'était la bonne surprise

`mailhtml::blocks` — déjà ce que la coquille emploie pour **afficher** un corps de message —
rend des `Run`, donc des fragments avec leur style. Les styles en ligne survivent : un mot en
gras au milieu d'un paragraphe collé arrive en gras.

Le convertisseur de collage n'a donc pas à être écrit, il existe. Et c'est le bon sens de la
dépendance : un convertisseur écrit exprès serait une deuxième implémentation du même découpage,
et c'est celle qui recevrait moins de tests que celle qui affiche le courrier reçu.

Ce qui n'est pas préservé — taille, police, couleurs, marges — l'est **volontairement** :
`docs/PRIVACY.md` veut un corps rendu sans moteur, et une signature qui recopierait le CSS d'un
site recopierait aussi ses polices distantes.

#### Le contrôle a dû être refait deux fois, et c'est l'histoire intéressante

**Premier contrôle : comparer le repos à la frappe.** L'idée était qu'au repos le cache de mise
en page d'`egui` sert, et qu'en frappe il ne sert pas — donc la frappe doit coûter plus. Le
relevé a donné 0,29 ms au repos et 0,26 ms en frappe : le contrôle a dit « ? » sur un relevé
parfaitement bon. Il supposait que la mise en page domine l'image ; elle ne domine pas.

**Deuxième contrôle : comparer deux tailles de document, par image.** Meilleure idée — un cache
jamais invalidé donnerait le même coût aux deux tailles. Mais il a **passé une fois et échoué la
suivante sur le même code** : 0,61 contre 0,22 ms, puis 0,41 contre 0,50 ms.

Deux exécutions identiques à 2,3× d'écart. Le `CLAUDE.md` dit qu'un tel écart n'est pas du bruit
mais un symptôme, et le symptôme était que **l'image entière est sous la milliseconde** : à cette
échelle, l'ordonnancement du système domine le travail, et la mesure ne distingue plus les
régimes.

**Troisième contrôle : mesurer la mise en page directement.** Deux cents itérations, hors du
cycle d'image, sur les deux tailles. Trois exécutions :

| glyphes | mise en page | rapport |
|---|---|---|
| 9 800 | 0,014 / 0,016 / 0,014 ms | — |
| 39 200 | 0,054 / 0,055 / 0,049 ms | **3,9× / 3,5× / 3,6×** pour 4,0× de glyphes |

Linéaire, et stable d'une exécution à l'autre. La mise en page a bien lieu, et le relevé par
image mesurait autre chose qu'elle.

C'est aussi ce qui explique le premier contrôle : à 14 µs, la mise en page est **un millième**
d'une image de 0,4 ms. Repos et frappe *doivent* être du même ordre.

#### Un compteur faux, trouvé en faisant varier la taille

Le relevé affichait `glyphes=22295` pour 200 comme pour 3 200 lignes. La cause : le bilan lisait
`self.document.glyphs()` **après** que la phase de collage avait remplacé le document — il
rapportait donc la taille du collage, toujours la même.

Le contrôle censé rendre le relevé interprétable était donc faux, et il l'était en silence. Un
chiffre constant quand l'entrée varie est le signe le plus lisible qu'on mesure la mauvaise
chose — et il ne se voit qu'en faisant varier l'entrée.

#### Ce que la sonde ne dit pas

Elle mesure la **mise en page** d'un document stylé et son édition au clavier. Elle ne dit rien
de :

- la **sélection** et l'application d'un style à une sélection — les boutons gras/italique. C'est
  du travail d'interface, pas de performance : le coût est celui de découper des intervalles ;
- le **curseur** dans un texte à styles multiples. `egui::TextEdit` le gère déjà, puisque la
  galère qu'il reçoit est celle du `layouter` ;
- la **conversion vers HTML** au moment d'envoyer. Le sens inverse du collage, et il n'existe pas
  encore.

Aucune de ces trois n'est un risque de faisabilité, ce qui était la question posée.

---

### 2026-09-10 — l'éditeur de signature, et le critère 5 mesuré sur ce qui est livré

L'étape 7 de l'ordre de travail. La sonde avait répondu à la question de faisabilité ; ce qui
restait était l'éditeur, son rangement, et le chemin qui met une signature dans un message.

#### Le modèle n'a rien coûté, parce qu'il était déjà écrit

`mailhtml::rich::Document` — un `String`, des intervalles stylés triés et disjoints, une nature
par ligne — était le modèle de la sonde, promu tel quel. Ce qui manquait était ses **deux
traductions**, que la documentation du module annonçait déjà :

- `from_html` — le collage. Il n'a pas de code d'analyse en propre : `sanitize::clean` puis
  `blocks::blocks`, les deux étages qui servent à **lire** le courrier. Un presse-papiers rempli
  par un navigateur est une entrée hostile au même titre qu'un corps de message, et un
  convertisseur écrit exprès serait une deuxième implémentation du même découpage — celle qui
  recevrait le moins de tests ;
- `to_html` — ce qui part. Sept balises, `<p> <b> <i> <a href> <ul> <li> <br>`, et **aucun
  attribut** en dehors du `href` : la garantie qu'aucune police ni ressource distante ne s'y
  glisse tient à l'absence d'un endroit où l'écrire, pas à une liste noire. Un test énumère les
  balises émises, parce qu'une affirmation de ce genre vieillit toute seule.

#### Le test de propriété qui a trouvé une limite au lieu d'un bogue

`document → HTML → document` doit être l'identité. Il ne l'était pas : les **lignes blanches**
disparaissaient.

La cause n'est pas un bogue. `mailhtml::blocks` jette les blocs entièrement blancs —
`blocks("<div></div><p>  </p>")` est vide — et c'est la bonne règle pour afficher du courrier
reçu, où un `<div>` vide ne doit pas fabriquer une ligne. `from_html` en hérite exprès.

Mais une signature commence par « Cordialement, » suivi d'une ligne blanche. Ranger la signature
en HTML aurait donc perdu cette ligne **à chaque ouverture de l'éditeur**. D'où la migration
`SCHEMA_V8` : la colonne porte le **document sérialisé**, et le HTML est ce qui part, recalculé
à l'envoi. La propriété a donc été réécrite pour dire ce qui est vrai — identité sur tout ce que
`blocks` sait porter — et la limite a son propre test, qui l'épingle avec sa raison.

C'est la leçon générale : **le format d'affichage n'est pas un format de rangement.** Le premier
a le droit de jeter ce qui ne s'affiche pas ; le second n'a pas le droit de perdre ce que
l'utilisateur a tapé.

#### Comment `egui` édite un document stylé, sans widget à écrire

`egui` n'a pas d'éditeur riche : il a un champ de texte et un `layouter`. Le montage tient en
trois lignes de responsabilité :

- le champ édite une `String` — donc curseur, sélection, glisser, annuler, coller et raccourcis
  du système marchent sans qu'on écrive quoi que ce soit ;
- `Document::reconcile` rattrape le document sur le tampon à chaque image : plus long préfixe
  commun, plus long suffixe commun, une substitution entre les deux. Une frappe ne décale donc
  que les intervalles qui la suivent ;
- le `layouter` construit une section par intervalle, ce qui donne le gras, l'italique et les
  liens à l'écran.

**Le prix, écrit là où il se paie** : pendant l'image où une touche est frappée, `egui` applique
l'évènement *puis* appelle le layouter — qui voit donc le tampon un caractère en avance sur le
document. Les sections sont pour cette raison découpées dans le **texte reçu** et jamais dans
celui du document, avec refus de toute borne qui trancherait un caractère. Un éditeur qui
paniquerait sur une lettre accentuée est inutilisable en français, et le décalage d'une image ne
se voit pas.

#### Les deux endroits où l'éditeur aurait pu mentir

**Une bascule de style ne doit pas écraser le reste.** Mettre en gras une phrase qui contient un
lien lui retirerait le lien si le bouton posait un style unique sur la sélection. Chaque fragment
garde donc **son** style avec un seul bit changé — et le sens de la bascule est décidé pour toute
la sélection, sinon une sélection mi-grasse s'inverse par morceaux et le bouton n'a pas d'effet
lisible.

**Un lien refusé à l'envoi doit être refusé au bouton.** La sortie HTML écarte en silence une
cible sans schéma ou en `javascript:` : le texte reste, le `href` part. Sans contrôle, le bouton
aurait accepté `exemple.fr`, l'aperçu l'aurait souligné, et le message serait parti sans le lien
— l'utilisateur ne l'apprenant jamais. `mailhtml::rich::link_may_leave` est devenu public pour
ça, et c'est **le même prédicat** que celui de la sortie : deux règles séparées finiraient par
ne plus dire la même chose.

#### Une signature rejoint un corps à un seul endroit

Le premier jet mettait l'assemblage dans la coquille. C'était une erreur : `mail send` en aurait
eu une seconde copie, et « comment une signature rejoint un message » est exactement le genre de
règle qui, dupliquée, envoie un jour à quelqu'un une signature en double.

L'assemblage vit donc dans `mailsmtp::compose::Draft::sign_with`, comme `queue::stage` avant
lui : **une implémentation, deux moteurs.** `outbox.send` prend un drapeau `signature`, faux par
défaut, et c'est ce drapeau qui dit **qui compose** — un client qui a déjà mis la signature dans
son texte ne doit pas la voir doublée. `mail send --signature` appelle la même fonction.

La coquille, elle, ne compose plus : elle **montre**. La signature s'affiche dans la fenêtre de
rédaction, rendue avec ses gras et ses puces, telle qu'elle partira — `docs/PHASE-3.md` demande
que rien de ce qu'on ajoute au message ne soit invisible — et une case à cocher permet de la
refuser pour ce message-ci. Visible et imposé ne vaudrait pas mieux qu'invisible.

Un défaut trouvé par le test de cet assemblage : un corps qui finit par `\r\n` — celui d'un
fichier lu par la CLI — donnait un `\r` **dans** une ligne du document et deux lignes blanches
avant la signature au lieu d'une. Les fins de ligne sont donc normalisées là où le corps entre
dans un document, et pas ailleurs.

#### Le critère 5, mesuré sur l'éditeur livré

La sonde mesurait un modèle nu. Le banc `signature` mesure ce qui est livré : le layouter, le
rattrapage du document à chaque image, et le `TextEdit` par-dessus. Trois exécutions, sur un
document de 200 lignes mêlant les quatre styles, après compilation et retour au repos.

| ce qui est mesuré | pire des trois exécutions | seuil |
|---|---|---|
| une frappe, **image entière**, p95 | **7,06 à 8,14 ms** | < 16,7 ms |
| une frappe, **notre travail** dans l'image, p95 | **5,55 à 6,09 ms** | — |
| l'image la plus longue | **42,45 ms**, dont **6,27 ms** de travail | — |
| au repos | **5,51 à 5,64 ms** | — |
| conversion d'un collage de 50 096 octets | **2,54 à 2,92 ms** | ne doit pas figer |
| images après le collage, p95 | **6,23 à 13,12 ms**, dont **5,15 ms** de travail | — |

Le contrôle qui rend le relevé interprétable : **31 511 glyphes et 808 intervalles
stylés** mis en page à chaque image — trois fois le document de la sonde, parce que les lignes
sont plus longues. Un p95 obtenu sur trois lignes serait indiscernable d'un vrai.

**Le critère est tenu avec un facteur deux**, et non trente-cinq comme la sonde le laissait
espérer : l'écart est tout ce que la sonde ne mesurait pas — le champ de texte, la fenêtre, le
compositeur. Le seuil porte sur l'image entière, et l'image entière coûte 8 ms.

**Deux chiffres demandent à être lus plutôt que classés.** L'image la plus longue fait 42 ms,
au-dessus du budget ; et le p95 des images qui suivent le collage monte à 13 ms sur une
exécution des trois. Le `CLAUDE.md` dit de lire la dispersion avant la médiane, et la
dispersion est ici entièrement dans le **délai entre images**, pas dans le travail : dans la
même image de 42 ms, `cpu_usage` en rapporte **6,27**. Après le collage, 13,12 ms de délai pour
5,15 ms de travail. Ce que ces écarts mesurent est l'ordonnancement du système — la même
observation qu'au troisième contrôle de la sonde, où deux exécutions identiques s'écartaient de
2,3× parce que l'image entière était sous la milliseconde.

La conclusion honnête est donc en deux parties : **notre travail par image ne dépasse jamais
8,2 ms**, mesuré sur trois exécutions ; et une image sur quelques centaines arrive en retard
pour une raison qui n'est pas la nôtre. Si un jour ce retard devient gênant, ce n'est pas la
mise en page qu'il faudra regarder.

Le collage, lui, coûte **six fois** le chiffre de la sonde — 2,7 ms contre 0,4 ms — et c'est
attendu : `from_html` traverse maintenant `sanitize::clean` en plus de `blocks`, donc la
barrière de `docs/PRIVACY.md`. Six fois plus cher, six fois sous le budget d'une image, et une
barrière de plus. Bon échange.

Et le repos est du **même ordre** que la frappe, ce qui est attendu et non suspect : la sonde
avait mesuré la mise en page à quatorze microsecondes pour 9 800 glyphes, soit un millième d'une
image. Ce qui coûte ici est le reste de l'image — le champ, la fenêtre, le compositeur — et il
coûte pareil qu'on tape ou non. Un repos beaucoup plus bas que la frappe voudrait dire que la
frappe fait autre chose que remettre en page.

#### Ce qui n'est pas là, et qu'il faut dire

- **Le presse-papiers d'`egui` ne donne que du texte brut.** `Event::Paste` porte une `String`,
  sans variante HTML : un Ctrl-V depuis un navigateur arrive donc en texte nu, que le champ colle
  très bien, sans styles. `Document::from_html` est écrit, testé et mesuré — c'est ce que le banc
  de collage traverse — mais rien ne l'alimente encore depuis un vrai presse-papiers. Le jour où
  une dépendance de presse-papiers riche entre dans le dépôt, `Editor::paste` est le point
  d'entrée ;
- **`mail account signature` n'existe pas.** La CLI sait *envoyer* avec la signature
  (`--signature`), pas la *composer* : la poser se fait dans la coquille, ou par
  `accounts.set_signature`. Un éditeur riche dans un terminal n'est pas un demi-travail, c'est un
  autre travail ;
- **le corps du message reste du texte brut.** Seule la signature est riche. Le modèle et
  l'éditeur sont réutilisables tels quels pour le corps, mais un corps riche pose une question
  que la signature ne pose pas : ce que devient le HTML **reçu** quand on répond dessus.

---

### 2026-09-10 — les invitations reçues, et trois défauts que l'usage a trouvés

L'étape 8, et un détour : l'utilisateur a lancé la coquille pendant le travail et a signalé
trois choses. Les trois étaient des défauts réels, dont deux dans le store.

#### Le relevé du corpus est venu avant le lecteur, et il a décidé de sa conception

`cargo xtask corpus-calendar` avant d'écrire une ligne d'analyseur. Ce qu'il a trouvé sur les
73 825 messages :

| ce que le corpus contient | |
|---|---|
| pièces `text/calendar` | **930**, dans 927 messages |
| avec leur `VTIMEZONE` embarquée | **853** |
| **fuseau nommé sans définition** | **0** |
| UTC seulement | 64 |
| plus grosse pièce | 31 145 octets |
| méthodes | `REQUEST` 573, `REPLY` 194, `CANCEL` 81, `PUBLISH` 9, **`COUNTER` 2** |
| producteurs | Exchange 484, Google 314, Mozilla 47, et onze autres |

**Le zéro de la troisième ligne est ce qui a décidé de tout le reste.** Les `TZID` les plus
fréquents du corpus sont des noms Windows — « Romance Standard Time » (1 079 occurrences),
« W. Europe Standard Time », « Eastern Standard Time » — qu'aucune base de fuseaux IANA ne
connaît. Il aurait donc fallu une table de correspondance Windows→IANA *et* une base de
fuseaux… sauf que **tous** ces fichiers embarquent leur `VTIMEZONE`, avec les décalages et les
règles de bascule dedans.

Le lecteur n'a donc besoin d'aucune base : il applique les décalages que le fichier porte
lui-même. C'est aussi la source la plus fiable qui existe, parce qu'elle vient du même
producteur que l'heure qu'elle qualifie. `mailcal` n'a **aucune dépendance**, et le « zéro
requête réseau » du critère 6 est vrai par construction et pas par vigilance.

Le corpus a aussi dicté trois ajouts qu'une lecture de la RFC seule n'aurait pas priorisés :
`COUNTER` (2 pièces — une contre-proposition affichée comme une invitation ferait croire qu'on
propose le créneau d'origine), `STATUS` (857 — `METHOD:CANCEL` dit ce que le *message* demande,
`STATUS:CANCELLED` ce que l'*événement* est) et `RECURRENCE-ID` (215 — « la réunion du 12,
exceptionnellement à 15 h », qui sans ça s'afficherait comme un déplacement de toute la série).

#### Un contrôle du relevé était faux, et le corpus l'a dit

Le premier relevé annonçait **1 705 répétitions pour 930 pièces**. Un chiffre supérieur au
nombre de pièces pour une propriété qui en porte au plus une par événement : c'est le genre
d'absurdité qui saute aux yeux **une fois écrite**, et pas avant.

La cause : une `VTIMEZONE` porte une `RRULE` par observance — « le dernier dimanche de mars » —
et le compteur les ramassait avec celles des événements. Le vrai chiffre est **65**. Corrigé en
ne comptant que ce qui est dans un `VEVENT`, et c'est la même leçon que le compteur de glyphes
de la sonde d'éditeur : un contrôle faux est pire qu'un contrôle absent, parce qu'on s'y appuie.

#### Le critère 6 se lit en trois branches, et j'en avais mal classé une

| | pièces | |
|---|---|---|
| instant absolu lu | **901** | 96,88 % |
| heure murale + réserve nommée | **27** | 2,90 % |
| refusées, raison nommée | **2** | 0,22 % |
| **illisibles sans raison** | **0** | ← le seul échec possible |

Les 27 sont des heures **flottantes** : ni `TZID`, ni `Z`. Le premier jet les comptait comme
refusées, ce qui était faux : RFC 5545 §3.3.5, une heure flottante désigne l'heure locale de
qui la lit, donc « 14:00 » *est* la bonne réponse. Les refuser aurait affiché « illisible » sur
des invitations que tous les autres clients montrent à 14:00.

D'où deux notions séparées, `has_instant` et `is_readable`, et un `Gap::is_refusal` qui
distingue **ce qui empêche d'afficher** de **ce qu'il faut dire à côté**. Sans début, il n'y a
rien à montrer ; sans fuseau, il reste un rendez-vous parfaitement affichable assorti d'une
phrase. Les mélanger était le vrai risque du critère.

#### Le contrôle qui empêche un refus de cacher un bogue

« Cette pièce ne contient aucun rendez-vous » est une réponse acceptable du critère — et elle
devient un mensonge si le rendez-vous est là et que le lecteur ne l'a pas vu. Le banc compte
donc les pièces dont le texte contient `VEVENT` alors qu'aucun événement n'a été lu. **Zéro**,
et c'est ce qui rend les deux refus crédibles.

#### Ce que l'usage a trouvé, et qui n'avait rien à voir avec les invitations

**« Le statut vu des mails ne se met pas à jour. »** Deux défauts distincts, empilés :

- `mark_seen` n'écrivait **rien** pour un message sans copie côté serveur — donc pour tout
  message importé d'un mbox, c'est-à-dire l'essentiel de ce corpus. La marque locale était
  conditionnée à un `UPDATE remote_uids` qui touchait zéro ligne. Un test affirmait ce
  comportement, avec pour raison écrite « marquer promettrait au serveur une poussée sur une
  copie qui n'existe pas » — vrai, et qui ne couvre que la moitié de ce que le test
  verrouillait. La marque va maintenant dans `refs`, et rien n'est promis au serveur ;
- même corrigé, la liste n'aurait pas bougé. `Mailbox::revision` reposait sur
  `PRAGMA data_version`, qui **ne bouge pas pour les écritures de la connexion qui
  l'interroge**. La coquille sert son propre store : ses propres écritures lui étaient
  invisibles, donc aucun `Changed`, donc aucun rafraîchissement. `Connection::total_changes`
  compte exactement ce que l'autre ne voit pas — l'un voit les autres, l'autre voit soi, et il
  n'y a aucun registre à tenir à la main.

**« On ne peut pas choisir avec quelle adresse on envoie ? »** La liste déroulante ne montrait
que les comptes capables d'envoyer et disparaissait quand il n'y en avait qu'un — or 4 comptes
sur 5 de ce profil n'ont pas de serveur de soumission. Il n'y avait donc rien à cliquer, et
rien qui l'explique. Elle montre maintenant **tous** les comptes, les non-expéditeurs grisés,
avec au survol la commande qui les rend utilisables. Un choix absent qu'on explique vaut mieux
qu'un choix absent.

**« Les proposés devraient être un dropdown, c'est moche. »** C'était une rangée de pastilles
sous le formulaire, qui avait un second défaut moins visible : elle ne disait pas à quel champ
elle appartenait. C'est maintenant une liste ancrée sous le champ qui a le focus, à sa largeur.

#### Un second parse MIME, évité de justesse

Le premier branchement appelait `invitation_of(&raw)` depuis `Mailbox::message`, avec les
octets bruts — donc un `MessageParser::parse` complet **en plus** de celui que la fonction vient
de faire, pour tous les messages, dont les 99 % qui n'ont pas d'invitation. Le critère 5 de la
phase 1 borne cette ouverture ; 4,3 ms de parse HTML payées deux fois n'y avaient rien à faire.

Corrigé en passant l'arbre déjà analysé. Vérifié après coup, sur le corpus réel : ouverture
p95 de **1,69 ms** en texte et **6,30 ms** en HTML, pour un seuil de 50 ms. La lecture de
l'invitation elle-même coûte **0,3 ms en moyenne**, 3,7 ms au pire, mesurée sur les 930 pièces.

#### Ce qui n'est pas là

- **Aucune conversion vers le fuseau de l'utilisateur.** L'heure murale est affichée avec le
  nom de son fuseau — « 14:00, Romance Standard Time » — et l'instant absolu est calculé, mais
  rien ne rend « 14:00 chez vous ». Le faire juste demande de connaître le décalage du lecteur
  **à la date de l'événement** : le décalage du jour donnerait une heure fausse d'une heure la
  moitié de l'année, ce qui est précisément le mode de panne qu'on refuse. Il faudrait une base
  de fuseaux locale — `/etc/localtime`, le registre Windows — donc un lecteur TZif ou une
  dépendance. C'est un travail à part, et l'affichage actuel n'induit personne en erreur ;
- **répondre à une invitation** — accepter, refuser, proposer autre chose. C'est hors du
  périmètre de la phase, explicitement : lire est un problème d'analyse, répondre un problème
  de protocole ;
- **les occurrences d'une série ne sont pas développées.** La `RRULE` est affichée telle
  qu'écrite. Une occurrence déduite de travers déplacerait un rendez-vous.

#### Répondre à tous, transférer, et la citation qui manquait — 2026-09-10

L'étape 5 était marquée faite avec trois manques listés. Deux sont comblés.

**Une réponse ne citait pas l'original.** Le champ du corps s'ouvrait vide. C'est tenable pour
un premier envoi, pas pour une réponse : celui qui la reçoit trois jours plus tard n'a aucun
moyen de savoir à quoi elle répond. La citation est du texte préfixé de `> `, avec une ligne
d'attribution — ce que font tous les clients depuis trente ans, et ça se relit dans un lecteur
en texte brut.

Elle est **bornée à 32 Kio**, et la coupe est écrite dans le texte. Le corps servi par le
service peut faire 256 Kio ; le mettre en entier dans un champ qui se remet en page à chaque
frappe coûterait une image, et le critère 5 de la phase 1 borne l'image. Une citation coupée en
silence laisserait croire que l'original s'arrêtait là.

**« Répondre à tous » veut dire « tous sauf soi ».** C'est le défaut le plus visible de ce
bouton écrit vite : le message revient dans sa propre boîte, et sur un fil de dix échanges il y
revient dix fois. Les adresses des comptes connus sont retirées de la copie, et l'expéditeur
aussi — il est déjà dans « À », et sans ce second filtre il reçoit la réponse deux fois. Le
bouton est grisé quand le message n'a qu'un destinataire : il ferait alors exactement ce que
fait « Répondre ».

**Un transfert n'entre pas dans le fil.** Ni `In-Reply-To`, ni `References` : le message part
vers quelqu'un qui n'a pas suivi la conversation, et l'accrocher au fil d'origine le ferait
ranger dans une discussion que le destinataire n'a jamais vue.

**Les pièces jointes d'un transfert ne suivent pas, et c'est dit dans le statut.** Le brouillon
ne désigne que du contenu **déjà dans le magasin de blobs** — c'est ce qui empêche un client de
faire lire un fichier arbitraire au démon, et c'est la règle du critère 3. Les reprendre
demanderait une méthode d'API qui range une partie de message dans le magasin, et elle n'existe
pas. Le dire au moment du geste vaut mieux que de laisser découvrir l'absence à la réception.

#### Les brouillons persistés — 2026-09-10

Le dernier manque listé à l'étape 5. Fermer la fenêtre de rédaction jetait tout ce qui n'était
pas parti, et c'est un défaut qui se paie une fois : celle où on ferme par mégarde.

**Une table à part, et surtout pas un état de la file d'envoi.** La tentation était d'ajouter un
`SendState::Draft` à `outbox`, qui a déjà tous les champs. C'est refusé pour la raison la plus
sérieuse du dépôt : la clause `WHERE` de `Store::deliverable` et `SendState::is_deliverable`
décident **ce qui part chez quelqu'un**, un message en `committing` ne se remet jamais
automatiquement, et un test vérifie que les deux sont d'accord. Ajouter un état à cette
énumération pour ranger des brouillons, c'est toucher au seul morceau irréparable du projet pour
une commodité.

Et sur le fond, ce sont deux cycles de vie : un brouillon n'a pas de blob RFC 5322, ses adresses
ne sont pas validées, son sujet peut être vide, et il n'a aucune chance de partir tant que
personne n'a cliqué. `SCHEMA_V9` ajoute donc `drafts` et `draft_attachments`.

**Rien n'est validé à l'enregistrement, et c'est le point.** `outbox.send` refuse une adresse à
moitié tapée, à juste titre. Refuser la même chose ici perdrait la frappe en cours, c'est-à-dire
exactement ce que les brouillons servent à ne pas perdre. Les champs d'adresses sont donc rangés
**tels que tapés** — « jean@, mar » compris — parce que rouvrir doit montrer ce qui était à
l'écran. Le compte, lui, est vérifié : un brouillon attaché à un compte inexistant ne pourrait
jamais partir.

**Un brouillon vide n'est pas enregistré, et efface celui qu'il remplaçait.** Ouvrir la fenêtre
par erreur puis la refermer est le geste le plus fréquent de tous ; il ne doit pas laisser de
ligne. Un brouillon qui n'a **qu'une pièce jointe** n'est pas vide, en revanche : quelqu'un a
déposé un fichier, et le jeter perdrait le geste.

**Le formulaire porte l'identifiant de son brouillon.** Sans lui, chaque enregistrement créerait
une ligne, et trois fermetures laisseraient trois brouillons du même message. Il est posé par la
réponse du service au premier enregistrement.

**Le brouillon est supprimé après la mise en file, jamais avant.** Si l'envoi avait été refusé,
c'est le brouillon qui aurait sauvé la frappe.

Un détail découvert par un test : **SQLite réattribue un `rowid` libéré**. Un brouillon
enregistré après que quelqu'un d'autre a supprimé le sien peut donc recevoir l'identifiant qui
vient d'être rendu. Le test a été retourné pour dire ce qui est vrai — il y a une ligne, et elle
porte le texte en cours — plutôt que d'affirmer une unicité que le moteur ne promet pas.

La migration a été vérifiée sur une **copie** du vrai index, comme le veut la règle : 41,2 Mo,
73 825 messages, migration en **2,4 ms**, aucune ligne perdue, aucune référence orpheline,
intégrité SQLite intacte.

#### `messages.stage_part` : reprendre une pièce jointe reçue — 2026-09-10

Deux manques que le journal listait se ferment avec la même méthode : **un transfert ne
reprenait pas les pièces jointes** de l'original, et **un client distant ne pouvait rien
joindre** — le seul chemin d'ajout écrivait directement dans le magasin depuis la coquille, ce
qui demande d'être sur la même machine.

Le client nomme un **message qu'il peut déjà lire** et un **rang dans la liste que
`messages.get` lui a rendue**. Le service décode la pièce, la range dans le magasin, et rend un
`Attached` — exactement ce que rend le dépôt d'un fichier sur la fenêtre. L'interface n'a donc
qu'une seule façon de joindre.

**Aucune capacité nouvelle n'est ouverte, et c'est le point.** Il n'y a pas de champ de chemin :
ce qui sort du magasin est ce qui y était déjà, sous une autre forme. C'est le même principe que
le rang de profil de `jobs.start`, et pour la même raison — un champ de chemin donnerait la
lecture de n'importe quel fichier de la machine du démon à quiconque détient le jeton. Un test
vérifie qu'un `path` glissé dans les paramètres ne change rien à ce qui est rangé.

**L'invariant qui compte est le rang.** Les deux côtés — la liste et la reprise — parcourent
`parsed.attachments()`, donc dans le même ordre. Deux parcours différents feraient joindre un
fichier à la place d'un autre, ce qui est la façon la plus discrète d'envoyer à quelqu'un un
document qui ne lui était pas destiné. Le test le vérifie sur un message à deux pièces, en
comparant nom **et** taille de chacune.

Ce que ça coûte, et c'est dit : la pièce sort **en mémoire** — 25 Mo pour une pièce de 25 Mo.
Le chemin d'envoi, lui, reste streamé de bout en bout (critère 3) ; décoder une partie d'un
message reçu demande d'avoir le message, que l'appelant lit déjà en entier.

### 2026-09-10 — le banc du critère 1, et les deux défauts qu'il a trouvés en une exécution

La conformité d'une **réponse** était la moitié ouverte du critère 1 depuis le 2026-09-09 :
deux aller-retours réels avaient validé un premier envoi, pas une réponse. Un envoi réel
demande un accord — il part chez quelqu'un — donc le banc fait ce qui peut se faire sans :
construire une réponse à **chaque fil du corpus**, l'assembler en octets RFC 5322, **relire ces
octets**, et comparer.

`cargo xtask measure-replies --store …` — rien n'est envoyé, rien n'est écrit.

#### Ce qu'il a fallu sortir de la coquille d'abord

La règle de fil — `In-Reply-To` prend le `Message-ID` du parent, `References` prolonge la
chaîne — vivait dans `Shell::start_reply`, donc **invérifiable ailleurs**. Elle est maintenant
`mailsmtp::compose::reply_threading` et `reply_subject`, et le banc appelle exactement ce que la
fenêtre de rédaction appelle. C'était le préalable : un banc qui réimplémente ce qu'il vérifie
ne vérifie rien.

#### Le premier relevé : 0 % de conformité

Et c'était **le banc** qui était faux, pas le produit — deux fois de suite. « 0 chaîne à
prolonger sur 2 000 messages réels » était le chiffre invraisemblable qui l'a dénoncé :
`References` est un `HeaderValue::TextList`, pas un `Text`, donc `as_text()` rendait toujours
`None`. Puis il a fallu comprendre que `mail-parser` rend les identifiants **sans leurs
chevrons**, des deux côtés de la comparaison.

Un banc qui accuse le produit avant de s'être vérifié lui-même est un banc qui coûte une
journée. Le réflexe utile est celui du `CLAUDE.md` sur la dispersion : un chiffre impossible est
un symptôme, et le premier suspect est l'instrument.

#### Puis deux vrais défauts, tous deux visibles chez le destinataire

**`Subject: "Re: Facture"`, guillemets compris.** `encoded_word` faisait deux métiers : encoder
du non-ASCII, et **citer** un nom d'affichage ASCII qui contient un caractère spécial de la RFC
5322. Or un nom d'affichage est un `phrase` dans un champ **structuré** — sans guillemets,
`Durand, Éloïse <e@x.fr>` serait lu comme deux adresses — tandis qu'un `Subject` est un champ
**non structuré** (RFC 5322 §3.6.5) où aucun caractère n'est spécial. Tout sujet contenant un
deux-points partait donc entre guillemets : **toute réponse**, puisqu'elle commence par « Re: ».
1 164 messages sur les 2 000 premiers.

Séparé en `encoded_word` (champ non structuré) et `display_name` (phrase structurée), avec le
contrôle inverse en test — un nom ASCII à virgule doit **rester** cité. Et un troisième emploi
est apparu au passage : le nom de fichier d'une pièce jointe, déjà entre guillemets dans son
format, qui a besoin d'un **échappement** et pas d'une citation.

**`In-Reply-To: abc@x.fr`, sans chevrons.** Un `msg-id` s'écrit entre chevrons — RFC 5322
§3.6.4 — et les identifiants arrivent des deux formes selon leur source : un en-tête brut les
porte, `mail-parser` les retire. La coquille lit l'API, qui lit `mail-parser` : ses réponses
partaient donc avec un `In-Reply-To` invalide, qu'un client qui regroupe sur cet en-tête ne
rattache pas. 415 fils sur les 2 000 premiers.

La remise en forme est faite **au point d'écriture**, dans `Draft::write_to`, et pas chez
l'appelant : c'est une propriété de l'en-tête, pas de qui le remplit, et une barrière par défaut
vaut mieux que la vigilance de trois appelants. Un identifiant qui contient un blanc est
**jeté** plutôt qu'écrit : il couperait l'en-tête en deux jetons, dont le second serait relu
comme un identifiant inventé.

#### Le relevé, sur tout le corpus

| | |
|---|---|
| messages du corpus | 73 825 |
| sans `Message-ID` | 102 — aucun fil à reprendre |
| sans adresse exploitable | 15 883 — voir plus bas |
| **réponses construites** | **57 840** |
| **conformes** | **57 840 — 100 %** |
| sujets non ASCII | 36 481 |
| avec une chaîne à prolonger | 8 383 |
| plus longue chaîne | 128 identifiants |
| identifiants refusés (hors RFC) | 1 |

#### Un troisième défaut, trouvé en refusant de laisser un chiffre sans explication

15 883 messages « sans adresse exploitable » — 21 % du corpus — est un nombre qui invite à une
conclusion fausse : *notre import perd les `From:`*. Le banc classe donc la forme, et le chiffre
qui tranche est le nombre de **valeurs distinctes** : **deux**. Une poignée répétée quinze mille
fois est un défaut systématique, pas du courrier biscornu.

La cause : un `From:` sans adresse — une phrase de 28 octets, écrite par un expéditeur
automatique — que `mail-parser` rend par `address()` faute de mieux, et que l'import rangeait
dans `from_addr`. Deux conséquences : la liste affichait une fausse adresse, et « Répondre »
produisait un destinataire que tout serveur refuse.

Une valeur sans arobase n'est pas une adresse. Elle devient donc le **nom**, et l'adresse reste
vide — ce qui est la vérité : ce message ne dit pas qui l'a envoyé. Corrigé aux **deux** portes
d'entrée, `mailimport::sender` et `mailsync::headers`, parce que deux chemins qui rangeraient un
message différemment feraient de la dédup un mensonge. Les messages déjà importés gardent
l'ancienne valeur jusqu'à un réimport.

#### Ce que ce banc ne remplace pas

La remise. Un serveur peut réécrire un en-tête, une passerelle peut recoder un sujet. Le dernier
mot du critère 1 reste un envoi réel d'un compte à un autre — mais il portera sur un message
dont la conformité est établie sur 57 840 cas au lieu d'un.

---

### 2026-09-10 — les cinq refus provoqués, et la phrase qui mentait après six tentatives

Le critère 8 était « à moitié » depuis le 2026-09-09, avec une formule que je reprends telle
quelle : *« les familles d'erreur existent et sont affichées en français ; aucune n'a encore été
provoquée »*. Le mot qui compte est **provoquée**. Ce qui existait était une classification et
six phrases, vérifiées par des tests qui fabriquaient l'`Err` à la main — c'est-à-dire qui
validaient la mise en forme d'une chaîne et rien du chemin qui y mène.

`crates/mailsmtp/tests/refus.rs` fait répondre un vrai serveur, à l'étape qui compte, avec le
code qui compte. Le chemin entier passe : le dialogue SMTP, `deliver_one`, l'état écrit dans le
store, et la phrase que l'utilisateur lira.

| famille | ce que le serveur rend | verdict | état laissé | ce que l'utilisateur lit |
|---|---|---|---|---|
| destinataire | `550` à `RCPT TO` | `Failed` | `failed` | vérifier l'adresse, puis renvoyer |
| quota | `452` à `RCPT TO` | `Deferred` | `queued` | rien à faire pour l'instant |
| taille | `552` à `DATA` | `Failed` | `failed` | retirer une pièce jointe |
| taille, avant transfert | `SIZE` annoncé à l'`EHLO` | `Failed` | `failed` | la même phrase |
| authentification | `535` à `AUTH` | `Failed` | `failed` | reconfigurer le compte |
| expéditeur | `550` à `MAIL FROM` | `Failed` | `failed` | le compte n'est pas autorisé |
| inconnue | `571` à `RCPT TO` | `Failed` | `failed` | la raison n'est pas reconnue |

#### Douze tests verts du premier coup, et pourquoi ça ne suffisait pas

Un banc qui passe immédiatement accuse d'abord le banc. Le contrôle : remettre
`message_for_user` à son ancien `failure.to_string()`. **Sept des dix tests d'alors sont
tombés**, et les trois restés verts sont exactement les trois qui n'interrogent pas la file —
deux tests purs de `classify` / `advice`, et le contrôle positif d'un envoi accepté. Le partage
était le bon, donc le banc mesure ce qu'il prétend.

#### Le défaut que le banc a trouvé, et il n'était pas dans les tests

En suivant la chaîne que ces tests exercent, un cas manquait : celui où la file **renonce**.

`Refusal::Quota` disait « L'envoi est réessayé automatiquement ; rien à faire », ce qui est vrai
tant que la file réessaie. Au bout de `MAX_ATTEMPTS`, `settle` écrit `failed` **avec la même
phrase** — parce que le texte était composé avant la décision. L'utilisateur lisait donc qu'il
n'avait rien à faire sur un message qui ne repartirait plus jamais, alors qu'il était désormais
le seul à pouvoir le faire partir.

C'est le critère 8 pris à l'envers, et c'est plus grave qu'un code numérique : un code laisse
chercher, une fausse promesse fait attendre. Le message reste en file, indéfiniment, sur la foi
d'une phrase.

Trois changements, dans cet ordre :

- **la phrase se compose après la décision.** `settle` calcule `retrying` — pas douteux,
  réessayable, et du crédit restant — puis appelle `message_for_user` ;
- **la phrase a deux morceaux.** `Refusal::cause()` dit ce que le serveur a refusé et ne dépend
  que de la famille ; `Refusal::action()` dit le geste qui reste à l'utilisateur ; la promesse de
  reprise **remplace** l'action au lieu de s'y ajouter. Deux morceaux plutôt que douze phrases —
  et « rien à faire » et « faites ceci » ne peuvent plus cohabiter dans la même phrase ;
- **le geste que la phrase nomme existe.** « Renvoyez le message plus tard » sans moyen de le
  renvoyer est le même défaut d'un cran plus loin.

#### Renvoyer un envoi échoué, et pourquoi ce n'est pas `--resend`

Une ligne `failed` n'avait qu'une sortie : `mail outbox --forget`, qui **jette le message**.
`--resend` ne s'appliquait pas, et à raison : il sort de `committing`, où le serveur a peut-être
le message. C'est un pari, et il demande d'avoir vérifié chez le fournisseur.

`failed` est l'inverse : le serveur a refusé, donc il n'a rien pris, donc **un renvoi ne peut pas
faire de doublon**. D'où `Store::retry_outgoing`, `outbox.retry`, `mail outbox --retry`, et un
bouton dans la coquille. Deux méthodes et non une troisième étiquette de `outbox.decide` : les
réunir demanderait à chaque client de savoir laquelle des deux situations il regarde, et un
client qui se tromperait enverrait deux fois.

Le compteur de tentatives **repart de zéro**, contrairement à `resolve_doubt` qui le conserve
comme historique. Ici il doit repartir : une ligne épuisée porte six tentatives, et les garder
ferait échouer le renvoi au premier refus passager. Un renvoi demandé par un humain est un envoi
neuf, pas la septième tentative d'un ancien.

#### Un bit sur la ligne de file, et pas la famille du refus

Offrir « Renvoyer » sur toute ligne échouée serait un bouton qui échoue à tous les coups sur une
adresse inexistante, une pièce jointe trop grosse ou un mot de passe périmé — ce que le critère 8
interdit autant qu'un code numérique. Le client a donc besoin de savoir si renvoyer **tel quel**
a une chance.

La tentation est de ranger la famille du refus dans le store. Refusé : la famille se lit sur un
code et une étape du protocole, son énumération vit dans `mailsmtp`, et la ranger dans `mailcore`
obligerait le store à connaître SMTP — ou à garder une étiquette textuelle qu'il ne sait pas
interpréter et que chaque client réinterpréterait à sa façon.

Ce dont un client a besoin est plus étroit : **un booléen**. `SCHEMA_V10` ajoute
`outbox.resendable`, écrit par la couche d'envoi dans la même transaction que l'état et la
phrase, et dérivé de `Refusal::worth_retrying` — qui n'avait jusqu'ici aucun appelant. C'est le
même choix que `Outgoing::doubtful` sur le fil : dériver une fois, du bon côté, plutôt que faire
deviner. Le défaut est `0`, donc une ligne échouée d'avant la migration ne propose rien :
personne n'a classé son refus, donc personne ne peut promettre qu'un renvoi marcherait.

`a_populated_outbox_survives_the_tenth_migration` vérifie le cas qui compte pour un
`ADD COLUMN NOT NULL` — une table déjà peuplée — et il commence par vérifier que la colonne
**n'existe pas** avant `apply`, sinon il passerait sans rien migrer.

#### La phrase n'est plus en gris

Dans la coquille, l'état (« échoué ») était en texte normal et la phrase du refus en `weak`. Le
mot qui ressortait était donc celui qui ne sert à rien, et l'action s'effaçait. Inversé : la
phrase est en texte normal sur sa propre ligne, et le bouton en dessous.

#### Ce qui reste, et ce que je n'ai pas fait

**Aucun vrai serveur n'a refusé.** Un serveur scripté dit ce qu'on lui dit de dire. Ce qui rend
la classification défendable malgré ça est qu'elle ne lit que le **code** et l'**étape**, tous
deux normalisés par la RFC 5321 ; le texte, écrit par chaque administrateur dans sa langue,
n'entre jamais dans la décision. Mais le dernier mot du critère reste un refus obtenu d'un vrai
serveur, et il demande un accord pour envoyer — accord que je n'ai pas.

Et provoquer les vrais cas coûterait à des tiers : un vrai quota demande de remplir la boîte de
quelqu'un, un vrai « trop gros » d'expédier 30 Mo, un vrai refus d'authentification de faire
bloquer un compte chez son fournisseur. Aucun des trois n'a sa place dans une suite qui tourne à
chaque `cargo test`.

#### Ce que la vérification a aussi appris

Le store de production de cette machine est **vide** — `mail doctor` : 0 contenu, 0 référence, 0
document indexé. Le corpus de 73 825 messages sur lequel les critères 1, 5 et 6 ont été mesurés
plus tôt dans la journée n'est plus sur aucun disque que je sache lire. Les relevés déjà
consignés restent ce qu'ils étaient ; ce qui n'est plus possible sans réimport, c'est de les
**refaire**. Le critère 8 n'en dépendait pas : il demande un serveur qui refuse, pas un corpus.

Vérification : `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`,
`cargo test --workspace` — **1 259 tests verts**, dont 12 pour ce critère, 5 nouveaux sur la file
et 3 sur l'API.

### 2026-09-11 — la source du message, et l'engagement qui n'avait aucune vérification

La section « vie privée » de ce fichier porte quatre engagements. Trois sont vérifiés par du
code : le carnet n'a pas de client HTTP, `mailcal` n'a aucune dépendance, un message ne part
qu'après un clic. Le quatrième ne l'était pas :

> **Rien de ce qu'on ajoute au message n'est invisible à l'utilisateur.** […] Ce que le message
> porte doit être lisible dans une fenêtre « source du message » qu'on écrira.

La fenêtre n'existait pas. L'engagement tenait donc sur un **argument** — `Draft` n'a pas de
champ `X-Mailer`, `SendParams` n'a pas de champ `headers`, et les deux le disent en commentaire
— ce qui est vrai, et ce qui demande de lire `mailsmtp::compose` pour s'en convaincre. Un
utilisateur ne lit pas `mailsmtp::compose`.

C'est la différence entre une promesse et une propriété. Une promesse se périme au premier
en-tête ajouté par commodité, et personne ne s'en aperçoit ; une propriété se vérifie.

#### Deux moitiés, et c'est la seconde qui compte

`messages.source` montre un message reçu. C'est utile — un `Received:` se lit, un
`Authentication-Results:` aussi — mais ça ne prouve rien de nous : ces octets sont écrits par
quelqu'un d'autre.

`outbox.source` montre une ligne de la file d'envoi, et c'est celle-là qui tient l'engagement :
elle lit **le blob remis au `DATA`**, pas une recomposition. Une vue qui réassemblerait le
message pour l'afficher pourrait diverger de ce qui est parti — et une vue de vérification qui
peut diverger de son objet ne vérifie rien.

Pourquoi ne pas attendre que le message revienne du dossier « Envoyés » : il revient après une
moisson, et jamais s'il a été refusé. Or c'est un message refusé qu'on veut le plus regarder.

| | |
|---|---|
| `messages.source` | ce que les autres nous écrivent |
| `outbox.source` | **ce que nous écrivons** — le blob du `DATA` |

Le test qui en fait une propriété est `what_we_compose_carries_nothing_the_user_cannot_read` :
il envoie par la vraie composition, relit la source par l'API, et compare les **noms d'en-têtes
de premier niveau** à une liste close de onze. Un en-tête de plus fait tomber le test, donc il
devra être ajouté à la liste, donc consciemment. Le contrôle négatif du `Bcc` y est repris tel
qu'il a été corrigé le 2026-09-10 : sur les **noms de ligne**, jamais par sous-chaîne — un
`Message-ID` est de l'hexadécimal, et `b`, `c`, `c` en sont des chiffres.

#### Le défaut que ce travail a trouvé : une vue qui montre tout peut être aveuglée par un octet

Une source s'affiche dans un terminal — `mail source` — et dans une fenêtre. Un message est
écrit par quelqu'un d'autre. Un en-tête qui contient `ESC[2J` efface l'écran sur lequel il
s'affiche : **les en-têtes qu'on venait vérifier disparaissent**, et il ne reste que ceux qui
suivent l'échappement. Un `\r` seul en milieu de ligne fait la même chose en plus discret, sur
une ligne au lieu d'un écran.

Une vue dont la raison d'être est « vous voyez tout » se fait donc cacher exactement ce qu'elle
promet de montrer, par le seul acteur qui aurait une raison de le vouloir. C'est du parsing
d'entrée hostile, à l'endroit où on croyait n'avoir qu'à imprimer.

Les caractères de contrôle sont donc rendus visibles — `\x1b`, quatre caractères imprimables —
**au point de rendu**, dans `mailcore::source`, et pas chez chacun des trois clients : trois
clients sont trois occasions d'oublier, et le JSON aurait transporté l'octet jusqu'au terminal.
C'est la même règle que les chevrons d'un `msg-id` le 2026-09-10 : une barrière par défaut vaut
mieux que trois appelants vigilants.

Trois caractères survivent, parce que chacun porte une structure :

| | |
|---|---|
| `\n` | sépare les lignes |
| tabulation | replie légitimement un en-tête (RFC 5322 §2.2.3) |
| `\r` **d'un `\r\n`** | est la fin de ligne elle-même |

Et le `\r` **seul** est échappé. C'est le contrôle négatif : vérifier qu'un échappement est
visible ne prouve rien tant qu'on n'a pas vérifié que la fin de ligne d'un message parfaitement
normal ne l'est **pas** — sinon la vue mettrait un `\x0d` au bout de chaque ligne de tout
message conforme, ce qui la rendrait illisible et donc inutile.

#### Le contrôle du banc

Seize tests verts du premier coup, ce qui accuse d'abord le banc. Contrôle : neutraliser
l'échappement dans `push`. **Trois tests tombent, et exactement les trois qui parlent
d'échappement** — les treize autres portent sur le découpage, la troncature et l'UTF-8, et
n'ont aucune raison d'y toucher. Le partage est le bon.

#### Ce que la vue dit d'elle-même

Une vue qui prétend montrer la vérité doit dire où elle a dû intervenir, sinon « vous voyez
tout » devient faux sans que personne ne puisse s'en apercevoir. Quatre réserves, affichées
**avant** ce qu'elles qualifient — une réserve annoncée sous mille lignes n'est jamais lue :

| champ | ce qu'il dit |
|---|---|
| `total` | la taille réelle, avant toute troncature |
| `headers_truncated` / `body_truncated` | où la vue s'arrête |
| `invalid_sequences` | des octets non UTF-8, rendus par `U+FFFD` |
| `escaped_controls` | des caractères de contrôle rendus visibles |

`invalid_sequences` a demandé d'écrire la conversion à la main plutôt que d'appeler
`String::from_utf8_lossy` : celle-ci remplace sans dire combien de fois, et compter les `U+FFFD`
du résultat après coup compterait aussi ceux qu'un message contient légitimement. Le test
`a_real_replacement_character_is_not_counted_as_invalid` le verrouille.

#### La lecture est bornée avant de lire, pas après

Le plus gros message du corpus fait 48 Mio. Le lire en entier pour en montrer le premier
mégaoctet serait la règle 4 du `CLAUDE.md` contournée au dernier maillon — celui qui affiche.
`Mailbox::source_of` ouvre un flux et lit au plus `MAX_HEADERS + MAX_BODY` : la borne est au
maillon qui lit, pas à celui qui jette.

Conséquence : la taille réelle ne peut plus être comptée sur ce qu'on a lu. Elle vient de la
colonne `size` — `Store::message_size` pour un message, la ligne de file pour un envoi — que
l'import et l'assemblage ont déjà relevée. C'est aussi pour ça que `render` prend un `total`
séparé de ses octets : c'est précisément quand la lecture est tronquée qu'on en a besoin.

Un cas hostile en sort, et il a son test : un message **sans ligne vide** et plus long que la
borne. Sans distinction, la vue dirait « ce message n'a pas de corps » d'un message qui en a un.
`unterminated` sépare les deux, et la seule façon de les distinguer est que la lecture a buté
sur son plafond.

#### Où la méthode est servie, et pourquoi pas où son préfixe le suggère

`outbox.source` est servie par le **répartiteur**, alors que `outbox.list`, `outbox.send`,
`outbox.decide` et `outbox.retry` sont servies par le transport. La règle écrite de
`SERVED_BY_TRANSPORT` est « a besoin de quelque chose qu'une boîte mail ne contient pas » — le
trousseau, le facteur, le temps qui passe. Lire un blob n'a besoin de rien de tout ça.

Suivre le préfixe plutôt que la règle aurait mis une lecture pure derrière un transport
asynchrone, et l'aurait rendue intestable sans monter un démon. C'est la leçon du 2026-09-10 sur
les tests, appliquée à un classement : relire la justification, pas le nom.

#### Passé par le vrai binaire, sur un vrai import

Les tests appellent des fonctions ; ce que l'utilisateur lance est `mail`. Un profil jouet —
dans le répertoire de travail, **jamais celui de Thunderbird** — avec un message qui porte les
trois pièges à la fois : un sujet en ISO-8859-1 non encodé, un `ESC[2J` au milieu de ce sujet,
et une ligne de corps commençant par un point.

`mail import` puis `mail source --id 1`, sur un terminal :

| ce que le message portait | ce qui s'affiche |
|---|---|
| `caf\xe9` (Latin-1) | `caf\u{fffd}`, et « 1 séquence d'octets non UTF-8 » |
| `\x1b[2J` | `\x1b[2J`, quatre caractères — **l'écran n'est pas effacé** |
| `=?UTF-8?B?w4lsb8Wvc2U=?=` | tel quel : c'est une source, pas une lecture |
| les `\r\n` | rien — pas de `\x0d` au bout des lignes |
| `.point en debut de ligne` | intacte |

C'est le relevé qui compte le plus de cette journée : le défaut d'échappement ne se voit que
là, sur un terminal qui obéit vraiment aux octets qu'on lui donne.

#### Ce qui n'est pas fait

**Le serveur MCP ne sert pas la source.** L'engagement dit « invisible à l'utilisateur », et un
modèle de langage n'est pas l'utilisateur. Lui donner les octets bruts d'un message ne sert
aucun usage qu'il n'ait déjà par `messages.get`, et lui donnerait les en-têtes `Received:` de
toute une boîte — des adresses IP, des noms de machines internes — pour rien.

**Rien n'est mesuré sur le corpus réel**, parce qu'il n'est plus sur le disque depuis le
2026-09-10. Ce qui est vérifié l'est sur des messages construits et sur des envois réels passés
par la vraie composition. Un relevé sur les 73 825 messages dirait combien portent des octets
non UTF-8 dans leurs en-têtes — la question a une réponse intéressante, et elle attendra un
réimport.

Vérification : `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`,
`cargo test --workspace` — **1 282 tests verts**, soit 23 de plus : 16 pour le rendu de la
source, 5 sur le répartiteur, et 2 de bout en bout sur la file d'envoi, dont celui qui compare
les en-têtes de ce que nous composons à sa liste close.

### 2026-09-11 (suite) — la page de paramètres, et le trousseau qui survit au store

Le store de production s'est retrouvé vide : 0 contenu, 0 référence, **et aucun compte
déclaré**. Il fallait donc redéclarer, et la seule façon de le faire était `mail account add`
dans un terminal. D'où la question posée : pourquoi pas une page de paramètres ?

La réponse est oui, et pas seulement pour le confort — **le secret**. En ligne de commande il
traverse un terminal : historique du shell, tampon de défilement, parfois un journal de session.
Dans un champ de mot de passe, il va du clavier au trousseau du système sans que rien d'autre le
voie. Et surtout : sans passer par quelqu'un qui écrit la commande à la place de l'utilisateur.

#### Le relevé qui a changé la conception

Avant de dessiner quoi que ce soit, une vérification : le trousseau a-t-il survécu ?

```
cmdkey /list | findstr mailcore
  oauth2:compte-b@gmail.invalid@imap.gmail.com
  oauth2:compte-a@gmail.invalid@imap.gmail.com
  … trois autres comptes, plus le jeton du démon
```

**Cinq entrées intactes.** Le store avait été vidé, le Credential Manager non — ce sont deux
magasins indépendants, et rien ne les avait jamais liés.

Or `mail account add --auth oauth2` déroulait **toujours** un consentement complet : navigateur,
identifiant client, écran du fournisseur, écouteur de bouclage. Pour aboutir au jeton déjà rangé,
à côté, depuis le début. C'est du travail imposé à l'utilisateur pour rien, et c'est aussi une
occasion de tout casser — un consentement raté laisse un compte à moitié déclaré.

`mailauth::session::has_secret(host, username, mécanisme)` répond à la question avant qu'on la
pose à l'utilisateur. Trois propriétés voulues :

- elle ne **lit** pas le secret, elle dit sa présence — la même règle que `is_stored` ;
- elle ne **devine** pas le mécanisme : un jeton OAuth2 n'est pas un mot de passe, et répondre
  « oui » à `password` parce qu'un jeton traîne ferait déclarer un compte qui échouerait à la
  connexion. Le test le vérifie dans les deux sens ;
- un mécanisme inconnu rend `false` plutôt qu'une erreur, parce que c'est un affichage — et
  l'appel qui compte, `secret_for`, refuse clairement.

`mail account add` réutilise donc ce qui est là, et `--renew` est le chemin de celui qui veut
justement remplacer : mot de passe changé chez le fournisseur, jeton révoqué.

#### Où la page écrit, et pourquoi pas par l'API

C'est la règle de `Link::stage`, appliquée à un secret au lieu d'un chemin — et la raison est
plus forte. Une méthode `accounts.add` voudrait dire deux choses :

- qu'un client qui détient le jeton du démon peut écrire dans le **trousseau de la machine du
  démon** ;
- que le secret **traverse le JSON-RPC** pour y arriver.

La CLI refuse déjà `mail account …` en mode `--daemon`, pour cette raison exacte. La page la
refuse pareillement, et l'écrit à l'écran plutôt que de griser un bouton sans rien dire.

Même raisonnement pour la **lecture**, et il a failli m'échapper : `accounts.list` ne rend ni
hôte ni port — c'est le critère 7, délibéré. Une page de paramètres a besoin des deux. La
tentation était d'élargir la méthode ; ç'aurait donné l'infrastructure de lecture de quelqu'un à
tout client qui détient le jeton, pour le confort d'un écran qui n'existe qu'en local. La page
lit donc le store directement, comme elle y écrit.

| ce qui est fait | par où |
|---|---|
| lire les comptes, hôte et port compris | le store, localement |
| déclarer un compte, écrire un secret | le trousseau puis le store, localement |
| écrire le serveur de soumission | le store, localement |
| oublier un secret | le trousseau puis le store, localement |
| **moissonner** | `jobs.start` — aucun secret en jeu, le démon sait déjà |

#### Le bouton qui manquait, et ce qu'il dit de la page

En relisant le parcours complet — déclarer, configurer l'envoi, puis… — il n'y avait pas de
« puis ». Rien dans la coquille ne lançait une moisson : un compte déclaré n'affichait aucun
dossier, et il fallait rouvrir un terminal pour `mail sync`.

**Une page de configuration qui laisse son travail à moitié renvoie à l'outil qu'elle était
censée remplacer.** Le bouton « Synchroniser » est donc sur chaque compte, et il est grisé avec
sa raison quand il ne marcherait pas — aucun secret enregistré, ou compte en pause. C'est la même
règle que le critère 8 : une phrase qui nomme une action oblige à rendre le geste possible.

#### Le secret dans un formulaire, et la leçon déjà payée

`AccountForm` porte un mot de passe, et `Request` dérive `Debug` — une demande qui échoue peut
finir dans un journal. `#[derive(Debug)]` y est le **défaut dangereux** : il a déjà affiché le
mot de passe de `Credential`, des deux côtés du projet.

`Debug` est donc écrit à la main, et masque même la **longueur** du secret, qui dit quelque
chose. Trois tests : le masque tient ; il est ciblé — le reste du formulaire s'affiche, sinon un
`Debug` muet passerait aussi ; et il tient **à travers l'enveloppe** `Request`, qui est le seul
endroit où la fuite se produirait vraiment.

#### Le consentement OAuth2, ajouté dans la foulée — et le fil qu'il a fallu

D'abord laissé de côté, puis repris tout de suite : parce qu'il est probablement **sur le chemin
critique**. Un écran de consentement Google resté en mode « test » expire ses jetons de
rafraîchissement au bout de **sept jours**, et ceux du trousseau datent du 2026-09-09. Une page
de paramètres qui renverrait au terminal précisément le jour où les jetons tombent n'aurait
servi à rien.

Ce qui a demandé une décision, c'est **où** il tourne. `mailauth::session::authorize` attend
qu'un humain revienne de son navigateur : `CONSENT_TIMEOUT` vaut cinq minutes. Le servir sur le
fil du service mettrait toute la boîte mail en file derrière lui — plus de page, plus de message
ouvert, pendant tout ce temps. L'interface ne serait pas figée, mais elle n'aurait plus rien à
afficher, ce qui revient au même pour qui regarde. C'est la règle 3 du `CLAUDE.md` prise un cran
plus bas que là où on la lit d'habitude : **le fil du service compte aussi**.

Le consentement part donc sur un fil à lui, détaché, qui parle par le même canal de réponses. Il
n'a **pas** de `Link`, donc pas de store : il écrit dans le trousseau et s'arrête là. Déclarer le
compte reste un second geste, et c'est voulu — un consentement réussi suivi d'une écriture de
store ratée laisserait un jeton orphelin, et la page annoncerait « compte enregistré » d'un
compte qui ne l'est pas.

Trois détails qui viennent de `mail account add`, repris parce qu'ils avaient déjà été pensés :

- **l'URL part vers l'interface avant l'attente**, et s'affiche dans un champ copiable. Le
  navigateur peut ne pas s'ouvrir — session distante, pas de navigateur par défaut — et cette
  adresse est alors le seul moyen de finir ;
- **l'identifiant client n'est pas masqué, le secret client l'est.** Le premier apparaît dans
  l'URL que le navigateur affiche : le masquer serait du théâtre, et ferait croire à un secret
  de plus. Le second n'en est pas un au sens cryptographique chez Google — PKCE existe pour ça —
  mais il a la forme d'un identifiant, et demander à quelqu'un de distinguer deux régimes de
  confidentialité pour deux valeurs collées dans la même console est le genre de nuance qui
  finit par coûter une fuite ;
- **le point de terminaison de jeton n'est pas configurable**, et le refus qu'on lit quand
  l'hôte est inconnu est la contrepartie visible de ce choix : le rendre configurable serait le
  moyen le plus simple de faire envoyer un jeton de rafraîchissement ailleurs.

Le bouton est offert **même quand un jeton est présent** — « Refaire le consentement… » — parce
qu'un jeton périmé est présent et inutilisable, et que c'est le cas fréquent après une semaine.
`has_secret` dit qu'un jeton est *rangé*, jamais qu'il est *valide* ; seule une connexion le dit.

Vérification : `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`,
`cargo test --workspace` — **1 297 tests verts**, soit 15 de plus : 1 sur `has_secret` et ses
deux contrôles négatifs, 10 sur la page — cinq sur le masquage des deux formulaires qui portent
un secret, trois sur le dessin sans fenêtre — 2 sur la fenêtre de source, et 3 sur les refus que
le mode distant oppose aux quatre écritures.

#### Le dessin se teste aussi, et c'était faux de dire le contraire

La première version de cette entrée disait : *« aucun banc de cette coquille ne tourne sans
ouvrir une fenêtre, et les tests portent donc sur ce qui décide, pas sur ce qui dessine »*. C'est
vrai des bancs — `startup`, `scroll`, `open`, `signature` ouvrent tous une fenêtre — et c'est
**faux** de la bibliothèque : `egui::__run_test_ui` monte un contexte sans police et exécute le
code de dessin. Quatre cents lignes n'avaient aucune couverture parce qu'on n'avait pas cherché.

Ce qui est vérifié maintenant, sans fenêtre :

- **chaque état de la page se dessine sans paniquer** — dix états, dont ceux qu'on ne voit
  qu'après un échec : le compte en pause, sans secret et sans serveur d'envoi à la fois, le
  serveur de soumission demandé pour un compte inconnu de la page, l'attente du navigateur. Une
  panique là serait un plantage sous les yeux de l'utilisateur, dans le premier écran qu'il
  ouvre ;
- **une page que personne n'a cliquée ne demande rien.** C'est la propriété qui compte : une
  page qui rendrait `Declare` sans clic écrirait un compte — et un secret — à **chaque image**.
  Le relevé porte sur tous les formulaires ouverts en même temps, parce que c'est l'état qui
  porte les boutons dangereux ;
- **deux images de suite laissent les formulaires où ils étaient.** Un champ qui se refermerait
  tout seul ferait disparaître la saisie sous les doigts.

Les mêmes trois tests existent pour la fenêtre de source, écrite le matin avec la même lacune.

Contrôle du banc : une panique posée dans la branche la moins accessible — celle de l'attente du
navigateur. **Un seul test tombe**, celui qui prétend parcourir tous les états, et il le dit par
le message de la panique. Le banc atteint donc bien ce qu'il annonce.

Ce qui reste hors de portée, et il faut le dire : `__run_test_ui` **dessine sans interagir**. Il
ne clique pas, donc « le bouton grisé le reste en mode distant » n'est pas vérifié — seulement
que le dessin qui le grise ne tombe pas. La première déclaration réelle d'un compte reste le
premier relevé de cet écran.

### 2026-09-11 (suite) — le carnet suit enfin la moisson, et la boucle qui ne finissait pas

Le journal du 2026-09-09 nommait ce morceau : *« Aucune reconstruction automatique. […] une
personne à qui on vient d'écrire n'apparaît qu'au prochain passage. C'est le prochain morceau à
écrire, et il demande de rendre la reconstruction incrémentale — donc de résoudre le double
comptage autrement. »*

C'est ce qui allait se voir dès la première moisson : du courrier, et une autocomplétion vide
jusqu'à un `mail contacts rebuild` lancé à la main — autant dire jamais.

#### Pourquoi on ne pouvait pas simplement enchaîner une reconstruction

La solution évidente — après une moisson, relancer le carnet — ne tient pas une seconde d'examen.
`IDLE` met un `Kind::Sync` en file **à chaque arrivée de courrier** (`watch.rs`), et une passe
complète sur 73 000 messages se compte en minutes. Un mail reçu aurait coûté des minutes de
processeur, en boucle.

C'est bien l'incrémental qu'il fallait, et tout tenait à une question : **comment savoir ce qui a
déjà été compté ?**

#### Le filigrane aurait marché aujourd'hui, et c'est pour ça qu'il est refusé

La réponse courte est un filigrane : retenir « compté jusqu'à l'identifiant N », ne regarder que
les messages au-dessus. Elle marche — rien ne supprime de ligne `messages` dans tout le dépôt,
vérifié.

Elle repose sur une invariante que **le schéma ne promet pas**. `messages.id` est un
`INTEGER PRIMARY KEY` sans `AUTOINCREMENT`, donc un alias de `rowid`, et SQLite **réutilise** un
`rowid` libéré. Le jour où quelque chose supprimera un message, un message inséré sous le
filigrane serait sauté pour toujours — en silence, le carnet cessant d'apprendre sans que rien
ne le signale.

`SCHEMA_V11` ajoute donc un **drapeau par ligne**, `messages.contacts_counted`. « Ce message a
été compté » devient un fait rangé dans la ligne, pas une déduction sur son identifiant. La
migration vide le carnet et remet tous les drapeaux à zéro : garder les compteurs ferait doubler
la première passe, et le carnet est dérivé — le vider ne perd rien.

#### Un seul code compte, deux entrées l'appellent

`rebuild` et `update` appellent la même fonction, `advance`. La seule différence est ce que
`rebuild` fait **avant** : vider. Deux boucles séparées auraient divergé, et un carnet reconstruit
qui ne donne pas le même classement qu'un carnet tenu à jour serait le pire des deux mondes —
la différence ne se verrait qu'à l'usage, sur un classement un peu faux que rien ne signale.

`an_incremental_pass_gives_exactly_what_a_rebuild_gives` fige ça : le même corpus monté des deux
façons — tout d'un coup, puis message par message comme une moisson les apporte — doit donner
deux carnets identiques.

#### L'ordre des deux écritures

Un lot est **d'abord** enregistré dans le carnet, **ensuite** marqué compté. Une coupure entre
les deux fait recompter le lot : le carnet connaît quelqu'un un peu trop, ce qu'un `rebuild`
corrige. L'ordre inverse perdrait le lot pour toujours, en silence.

Des deux dérives, on choisit celle qui se voit — c'est le raisonnement de
`mailsmtp::queue::deliver_one`, à enjeu bien moindre.

#### Le défaut que le contrôle négatif a trouvé, et il n'était pas dans le carnet

Vingt-cinq tests verts du premier coup, donc contrôle : retirer le marquage et regarder ce qui
tombe.

**Rien n'est tombé. Les tests ont pendu.** Sans marquage, la requête rend indéfiniment les mêmes
lignes, et la boucle ne se termine jamais. En test c'est une minute perdue ; dans le démon,
c'est un job de fond qui consomme un cœur pour toujours, que rien ne signale — le panneau des
tâches affiche « en cours », ce qui est vrai.

**Une boucle de fond qui ne finit jamais est pire qu'un résultat faux.** La passe est donc
**bornée par ce qui était en attente quand elle a commencé** : un relevé au départ, décrémenté à
chaque paquet. Ce que ça coûte : un message arrivé pendant la passe attend la suivante — sans
importance, puisqu'une passe suit chaque moisson.

Le contrôle refait après : **trois tests tombent, en 0,16 s**, et ce sont exactement les trois
qui parlent d'incrémental. Le banc mesure ce qu'il prétend, et il échoue au lieu de pendre.

#### Où la passe est appelée

Dans le corps du job `Kind::Sync`, après la moisson — pas dans un job enchaîné. Elle ne coûte
rien quand rien n'est arrivé (le relevé initial vaut zéro, la boucle ne s'exécute pas), et un
second job ferait clignoter le panneau des tâches à chaque message.

Un carnet qui échoue ne fait pas échouer la moisson : le courrier est arrivé, et c'est ce que
l'utilisateur attendait. Le job le dit dans son bilan et continue.

`Kind::Contacts` reste : c'est le moyen de repartir de zéro quand on soupçonne une dérive. Et la
dérive possible a maintenant un nom — un message dont toutes les références disparaissent garde
sa contribution au carnet. Elle ne va que dans un sens, et seule la reconstruction la retire.

#### Ce qui n'est pas fait

**L'index plein texte et les fils ne suivent toujours pas.** Après une moisson, la recherche
ignore les nouveaux messages et leurs fils ne sont pas rattachés. Les deux ont exactement le même
problème et appellent la même solution — `index::rebuild` efface tout, `thread::rebuild` repart
de zéro — mais ni l'un ni l'autre n'a de drapeau. C'est le prochain morceau, et il est plus gros :
un fil se recalcule à partir de messages qui arrivent **après** celui qu'on traite, donc
« incrémental » n'y veut pas dire la même chose.

**Rien n'est mesuré sur un corpus réel** : il n'y en a plus sur cette machine depuis le
2026-09-10. Ce qui est vérifié l'est sur des corpus construits de trois messages.

Vérification : `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`,
`cargo test --workspace` — **1 301 tests verts**, dont 4 nouveaux sur la passe incrémentale :
son équivalence avec une reconstruction, le double comptage empêché, la reconstruction qui ne
double rien après elle, et la passe vide qui ne parcourt rien.

### 2026-09-11 (suite) — l'index suit la moisson, et deux magasins qui peuvent se contredire

Le morceau que l'entrée précédente annonçait. Même problème que le carnet, même remède —
`SCHEMA_V12` ajoute `messages.indexed` — mais **une difficulté que le carnet n'avait pas**, et
c'est elle qui vaut d'être écrite.

#### Le drapeau et la donnée ne vivent pas au même endroit

Le drapeau du carnet et les compteurs qu'il protège sont dans le même fichier SQLite : ils ne
peuvent pas se contredire. Ici, le drapeau est dans SQLite et les documents sont dans tantivy.
**Deux magasins, donc deux vérités possibles.**

Elles divergent quand l'index est recréé sans que le store le sache : répertoire de recherche
supprimé, schéma illisible, disque remplacé. `open_or_create` rend alors un index **vide**, et
des drapeaux qui disent « indexé » feraient sauter tous les messages — la recherche resterait
vide **pour toujours**, sans que rien ne le signale. C'est exactement la panne silencieuse qu'on
ne veut pas : le client fonctionne, il ne trouve simplement plus rien.

Le symptôme est pourtant net — un index vide alors que des messages se disent indexés — et il se
lit en une requête. `index::update` le cherche à chaque passage et remet les drapeaux.

Où **ne pas** mettre ce contrôle : dans `open_or_create`. C'était la place la plus directe, et
elle est fausse — cette fonction est appelée sur des chemins de **lecture** (`Mailbox::open`,
`mail search`, `mail doctor`), qui ne doivent pas écrire dans le store. Un contrôle de cohérence
va au point d'**usage**, pas au point d'ouverture.

`an_index_wiped_behind_the_store_is_rebuilt_rather_than_left_empty` supprime le répertoire pour
de vrai et vérifie que la passe suivante réindexe tout.

#### Réindexer ne doit rien coûter, sinon l'ordre n'est pas sûr

Pour le carnet, recompter un message gonfle un compteur — visible, corrigible. Ici, réindexer
ajouterait un **second document** : la recherche rendrait deux fois la même ligne, et c'est le
genre de défaut qu'on met des semaines à rattacher à sa cause.

Chaque écriture retire donc d'abord le document de même identifiant — `delete_term` sur le champ
`id`, qui est `INDEXED` depuis toujours. Réindexer devient sans effet, et c'est **cette
idempotence qui rend l'ordre sûr** : valider l'index, puis marquer. Une coupure entre les deux
fait réindexer, sans conséquence. L'ordre inverse rendrait un message introuvable pour toujours.

`indexing_the_same_message_twice_does_not_duplicate_it` reproduit exactement cette coupure — les
drapeaux sont remis à la main entre deux passes — et vérifie qu'on trouve toujours deux
résultats, pas quatre.

#### Un mappeur partagé, parce que deux recopies divergent

`all_for_indexing` et `unindexed` rendent la même chose avec un `WHERE` de plus. Leurs deux
fermetures de lecture ont été fondues en une fonction : recopiées, elles divergeraient au premier
champ ajouté, et **un index reconstruit ne rendrait alors pas les mêmes résultats qu'un index
tenu à jour**. C'est la même raison qui fait que `rebuild` et `update` appellent tous deux
`advance`.

#### Ce que le démon fait maintenant après une moisson

| | |
|---|---|
| indexer ce qui est arrivé | `index::update`, puis `reload_search` — sans quoi le démon chercherait dans l'ancien index jusqu'à son redémarrage |
| avancer le carnet | `contacts::update` |
| **rattacher les fils** | toujours pas — voir plus bas |

Ni l'un ni l'autre ne fait échouer la moisson : le courrier est arrivé, et c'est ce que
l'utilisateur attendait. Un échec est dit dans le bilan du job et journalisé.

#### Ce qui n'est pas fait, et pourquoi c'est plus dur

**Les fils ne suivent toujours pas la moisson.** Le remède des deux passes précédentes ne s'y
applique pas : un drapeau « ce message a été rattaché » serait faux presque tout le temps. Un fil
se calcule à partir de messages qui arrivent **après** celui qu'on traite — c'est la raison
écrite depuis la phase 1 pour laquelle le threading est une passe séparée de l'import. Un message
rattaché aujourd'hui peut devoir être rattaché autrement demain, quand son parent arrive.

« Incrémental » n'y veut donc pas dire « ne regarder que le nouveau », mais « recalculer le
voisinage touché ». C'est un autre problème, et il mérite sa propre entrée plutôt qu'une
extension bâclée de celle-ci.

**Rien n'est mesuré sur un corpus réel** : il n'y en a plus sur cette machine depuis le
2026-09-10. Ce qui est vérifié l'est sur deux messages.

Vérification : `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`,
`cargo test --workspace` — **1 305 tests verts**, dont 4 nouveaux : l'équivalence des deux
chemins, le doublon empêché, la passe vide, et l'index effacé sous les pieds du store.

### 2026-09-11 (suite) — le banc des passes de suivi, et les 15 ms payées pour rien

Trois fois dans la journée, j'ai écrit qu'une passe incrémentale « ne coûte rien quand rien n'est
arrivé ». C'est un argument, et `IDLE` met un job en file à **chaque** arrivée de courrier : si
l'argument est faux, on vient d'installer un coût permanent dans le démon. Ce projet a une règle
contre ça — les critères se mesurent, ils ne s'estiment pas.

`cargo xtask measure-followup` répond à la question posée : combien coûte une passe **à vide**,
une passe pour **un message**, et la reconstruction complète qu'elles remplacent.

#### Ce que le banc refuse de prendre en paramètre

**Aucun `--store`.** Le banc écrit, donc il fabrique son propre répertoire jetable et l'efface en
partant. C'est la leçon du 2026-09-09 : `measure-attachment` prenait un `--store`, elle a été
visée sur les données de production, et elle y a laissé 533 Mo. Un paramètre qui peut désigner le
store réel le désignera.

Le corpus est synthétique — le vrai n'est plus sur cette machine depuis le 2026-09-10 — et c'est
écrit en tête du module. Ce qui est fidèle, c'est le **rapport** entre les trois régimes, qui est
la question. Ce qui ne l'est pas : le coût absolu sur de vrais messages, qui portent du HTML, des
pièces jointes et des en-têtes à rallonge.

#### Le premier relevé, et l'asymétrie qui saute aux yeux

Cinq mille messages, trois exécutions.

| passe | complète | à vide | un message | rapport |
|---|---|---|---|---|
| index | 856 – 876 ms | **11,2 – 15,0 ms** | 55,9 – 69,3 ms | 13 – 15× |
| carnet | 517 – 531 ms | 70,9 – 81,1 µs | 554 – 613 µs | 865 – 934× |

Le carnet est neuf cents fois moins cher qu'une reconstruction. L'index, quinze. Et surtout : une
passe d'index **qui n'a rien à faire** coûtait onze à quinze millisecondes.

La cause, en relisant : `update` ouvrait l'index tantivy **et son lecteur** avant de demander
s'il y avait du travail, parce que le contrôle de divergence en a besoin. Un `COUNT` sur une
colonne indexée coûte des microsecondes. **La question la moins chère passe en premier.**

| | à vide, avant | à vide, après |
|---|---|---|
| index | 11,2 – 15,0 ms | **19,0 – 22,7 µs** |

Six cents fois moins, pour une ligne. Le banc s'est payé en une heure.

#### Ce que le raccourci coûte, et le test qui a refusé de le laisser passer

Sortir avant d'ouvrir l'index saute aussi le contrôle de divergence — celui qui détecte un index
effacé sous les pieds du store. `an_index_wiped_behind_the_store_...` est tombé immédiatement.

Sa justification, relue avant de le toucher : « un index effacé doit se rattraper, sinon la
recherche reste vide sans que rien ne le signale ». Elle tient. Ce qui a changé est son
**échéance** : la réparation a lieu à la prochaine **arrivée de courrier** et non à la prochaine
passe — parce qu'un message qui arrive rouvre l'index, et le contrôle se fait alors.

C'est donc le test qui bouge, renommé `..._at_the_next_arrival`, et il vérifie maintenant **les
deux moitiés du marché** : qu'une passe sans rien de nouveau ne découvre rien, et qu'un message
qui arrive déclenche la réindexation complète. Ce qu'on achète avec les quinze millisecondes est
dans le test, pas seulement dans un commentaire.

Entre-temps la recherche ne rend rien — ce que `mail doctor` signale déjà comme une dérive
d'index, et qu'une réindexation corrige.

#### La dispersion a dit une deuxième chose

Le relevé d'après compilation :

| exécution | index complet | index, un message |
|---|---|---|
| 1 | **1,5 s** | **97,8 ms** |
| 2 | 979 ms | 55,7 ms |
| 3 | 991 ms | 73,3 ms |

La première exécution s'écarte de 50 % des deux autres, qui s'accordent. **Ce n'est pas du
bruit** : c'est la machine qui n'était pas retombée au repos après le `cargo build`, exactement le
biais qui avait faussé les relevés du 2026-09-02 d'un facteur trois et qui s'est reproduit sur le
critère 5 le lendemain. Quarante secondes de pause ne suffisent pas toujours ; lire la dispersion
avant la médiane, si.

#### Ce qui reste cher, et pourquoi ça ne l'est pas

Indexer **un** message coûte encore 56 à 73 ms, contre 0,6 ms pour le carnet. C'est le coût fixe
de tantivy — allouer le tas d'un `IndexWriter`, écrire un segment, le valider.

Ce coût est **par passe et non par message** : `advance` ouvre un writer par paquet de deux
mille lignes, donc une moisson qui apporte cent messages paie une fois, pas cent. C'est le régime
qui compte, et il est tenable : une arrivée de courrier coûte une soixantaine de millisecondes de
travail de fond, contre la seconde qu'aurait coûtée une reconstruction.

Vérification : `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`,
`cargo test --workspace` — **1 305 tests verts**. Le banc lui-même est dans `xtask/src/followup.rs`
et se lance par `cargo xtask measure-followup`.

### 2026-09-13 — les fils suivent la moisson, et le voisinage qu'il a fallu fermer

Le dernier des trois dérivés à ne pas suivre le courrier. Depuis le 2026-09-11, le carnet et
l'index avancent après chaque moisson ; les fils, non. Une réponse arrivait par `IDLE` et se
présentait comme un message isolé jusqu'à un `mail thread` lancé à la main — autant dire jamais,
puisque c'est le même « autant dire jamais » qui avait justifié les deux passes précédentes.

L'entrée du 2026-09-11 disait pourquoi on n'avait pas enchaîné : **le remède des deux autres ne
s'applique pas ici.** Un drapeau « ce message a été rattaché » serait faux presque tout le temps,
parce qu'un fil se calcule à partir de messages qui arrivent **après** celui qu'on traite. Ça
reste vrai. Ce qui a changé, c'est la lecture de la phrase : « incrémental » ne veut pas dire
« ne regarder que le nouveau », mais « recalculer le **voisinage** touché » — et un voisinage, ça
se définit, ça se ferme et ça se borne.

#### Ce qui coûtait, et ce n'était pas le calcul

Le partitionnement de 73 000 messages prend quelques millisecondes : c'est une union-find. Les
334 s à froid de la phase 1 sont **la relecture des 73 658 blobs**, un par un, pour en extraire
deux choses : le sujet normalisé et les identifiants cités par `In-Reply-To` et `References`.

Deux faits par message, recalculés depuis 4,3 Go de fichiers à chaque passe. `SCHEMA_V13` les
range : `messages.subject_norm`, et une table `message_references`. Un troisième les accompagne,
`messages.thread_link`, qui retient **comment** un message a été rattaché — par une référence
résolue, ou sans, donc éligible au repli par sujet.

Ce troisième n'est pas une commodité. Sans lui, retrouver le groupe de sujet d'un message qui
arrive demanderait de recalculer la résolution de **toutes** les références du store,
c'est-à-dire exactement ce qu'on venait d'éviter.

Un blob est donc lu **une fois dans la vie d'un message**, et la question « qui répond à ce
message ? » — celle que les octets ne savent pas poser, parce que la réponse est dans les
en-têtes des *autres* — devient une requête sur une colonne indexée.

#### Un seul endroit décide, et cette fois il a fallu l'extraire

`mailcore::thread::partition` est la règle de fil, et les deux passes l'appellent. C'est la leçon
du carnet, le 2026-09-11 : *deux chemins vers la même donnée partagent leur code, pas leur
intention.* Ici l'enjeu est plus grand que là-bas — un carnet qui diverge décale un classement,
un fil qui diverge coupe une conversation en deux — et la propriété est figée par
`an_incremental_pass_gives_exactly_what_a_rebuild_gives`, qui monte le même corpus deux fois :
d'un bloc, puis message par message. Un second test le monte **à l'envers**, parce que le cas que
le drapeau par ligne ne savait pas traiter est celui du parent qui arrive après sa réponse.

Le corpus de ces tests n'est pas décoratif. Il porte une conversation tenue par des références,
une conversation tenue par le **sujet** — le mécanisme principal sur un profil réel, 11,4 % des
messages seulement portant une référence, voir `docs/PHASE-1.md` — un message dont le parent n'a
jamais été reçu, deux sujets trop courts, et un groupe de sujet **juste au-dessus du plafond**.
C'est ce dernier qui a payé.

#### Fermer le voisinage, sinon la passe locale invente

La propriété qui rend les deux chemins équivalents tient en une phrase : **aucune arête du graphe
global ne sort du voisinage.** Trois façons d'en sortir, donc trois fermetures :

1. **une référence** — les porteurs des identifiants qu'un membre cite, et les messages qui
   citent l'identifiant d'un membre. Les deux sens : un parent qui arrive après sa réponse est le
   cas normal, pas l'exception ;
2. **un fil existant** — si un membre est déjà rattaché, tout son fil entre. Sans ça on couperait
   un fil en deux sans le savoir ;
3. **un groupe de sujet** — le groupe entier entre, ou aucun de ses membres n'est fusionné.

La troisième dépend de la première, et c'est ce qui oblige à boucler : un message rangé « sans
référence » cesse de l'être dès qu'un nouveau message le cite, et il **quitte** alors son groupe
de sujet. Les sujets sont donc réexaminés à chaque tour, sur la connaissance du tour, et jamais
mémorisés d'un tour à l'autre.

#### Le plafond se franchit dans les deux sens, et le premier essai n'en voyait qu'un

Le défaut que le corpus de vingt-six notifications a trouvé. À vingt-cinq messages de même sujet
le groupe fusionne ; à vingt-six il est refusé — c'est `MAX_SUBJECT_GROUP`, le garde-fou qui
empêche « Votre facture » de réunir des centaines de conversations.

La première version refusait bien de fusionner le vingt-sixième. Elle laissait les vingt-cinq
autres **réunis**, parce qu'un groupe déclaré trop gros n'entrait pas dans le voisinage — là où
une reconstruction les aurait tous séparés. Le relevé du test est sans ambiguïté : un fil de
vingt-cinq d'un côté, vingt-six fils d'un seul message de l'autre.

Un groupe refusé doit donc quand même entrer **s'il était réuni hier**, puisque seule une
composante recalculée réécrit un rattachement. Et pas au-delà : un sujet de newsletter réunit des
milliers de messages qui sont déjà chacun dans leur fil, et les faire entrer coûterait le prix
qu'on essaie d'éviter. Ce qui distingue les deux cas est le relevé du store **avant filtrage** :
au-dessus du plafond, le groupe était déjà refusé hier, donc il n'y a rien à dissoudre.

#### Deux autres défauts trouvés par le même test

**Un fil ne sert qu'une composante.** Quand un fil se sépare — son message central part rejoindre
une conversation, le reste tient par le sujet — les deux composantes ont des membres qui
désignent tous l'ancien fil. Réutiliser un identifiant de fil est la bonne chose à faire : un
identifiant stable est ce qui permet à une interface de rester sur le fil qu'elle affichait quand
une réponse arrive. Le faire **deux fois** transforme une séparation en fusion. Le registre des
fils déjà réclamés tient en trois lignes, et sans le test il n'aurait jamais existé.

**Un `msg-id` cité deux fois n'est pas deux références.** `In-Reply-To` et `References` se
recoupent presque toujours : le parent direct est cité par les deux. Le garder deux fois range
deux lignes identiques, donc fait rendre deux fois le même message à `threaded_referencing` — et
compte deux fois une référence non résolue, ce qui gonflerait d'autant la mesure du graphe troué.
Les références sont dédoublonnées à la lecture des en-têtes, une fois, au point d'écriture.

#### Ce que la passe refuse de faire, et le dit

Deux sorties, et les deux sont des reconstructions complètes annoncées dans `full_pass` :

- **un store rattaché avant `SCHEMA_V13`.** Ses fils sont justes, mais rien ne dit qui répond à
  quoi. La migration ne les efface donc pas — contrairement à celle du carnet, qui vidait pour
  éviter un double comptage — parce qu'effacer aurait laissé un store sans fils entre la
  migration et la première passe. `threaded_without_facts` compte exactement ces lignes, et la
  passe refait tout **une fois** ;
- **un voisinage au-delà de `MAX_NEIGHBOURHOOD`**, soit 5 000 messages. Le voisinage est fermé,
  donc il peut en principe atteindre la taille du plus gros fil du store, et recalculer un fil de
  dix mille messages à chaque arrivée de courrier coûterait plus que la reconstruction qu'on
  remplace. Sans borne, rien ne le dirait : le job s'afficherait « en cours ».

Et une barre de progression qui repart. Une passe qui renonce et recommence autrement ne peut pas
garder ce qu'elle avait compté : ça montrerait une barre pleine pendant toute la reconstruction,
c'est-à-dire une barre qui ment jusqu'au bout. `Progress::restart` la fait **reculer**, et c'est
voulu — une barre qui recule dit qu'il se passe autre chose, une barre bloquée à 100 % ressemble
à une panne.

#### Ce que ça coûte, mesuré et non estimé

`cargo xtask measure-followup` prend une troisième ligne. Le corpus synthétique a dû changer pour
que la mesure veuille dire quelque chose : il n'avait ni `Message-ID` ni `References`, donc la
passe de fils n'aurait mesuré que son chemin vide — rien à résoudre, aucun fil à réécrire. Il
porte maintenant des conversations de trois messages, et le message qui « arrive » est **une
réponse**, pas un message isolé.

Trois exécutions, machine laissée au repos quatre minutes après la compilation, 20 000 messages :

| passe | complète | à vide | un message qui arrive | rapport |
|---|---|---|---|---|
| index | 4,3 – 4,4 s | 104 – 148 µs | 84,5 – 96,9 ms | 44 – 52 × |
| carnet | 2,3 s | 134 – 223 µs | 740 – 846 µs | 2 747 – 3 089 × |
| **fils** | **2,4 s** | **20,8 – 45,4 µs** | **6,1 ms** | **395 – 400 ×** |

**La dispersion d'abord.** Les fils donnent 6,1 ms sur les trois exécutions, au dixième de
milliseconde près, et 2,4 s de reconstruction sur les trois : c'est le relevé le plus stable des
trois passes. La seule valeur qui s'écarte est la passe à vide de la **première** exécution —
45,4 µs, contre 22,7 et 20,8 ensuite. Le facteur deux sur le premier passage et l'accord des deux
suivants, c'est le biais du 2026-09-02 pour la **quatrième** fois : quatre minutes de pause n'ont
pas suffi. Le chiffre à retenir est donc 21 µs, pas la médiane des trois.

**Ce que le rapport dit.** Une réponse qui arrive coûte 6,1 ms de travail de fond, contre les
2,4 s qu'aurait coûtés une reconstruction — et 2,4 s, c'est sur 20 000 messages synthétiques aux
blobs minuscules et tous dans le cache. Le vrai point de comparaison est le relevé de la phase 1 :
**334 s à froid** sur 73 658 messages. C'est ce qui se serait produit à chaque arrivée de
courrier, et c'est la raison pour laquelle les fils ne suivaient pas.

**Une surprise, et elle est agréable.** La passe qu'on redoutait est la **moins chère des trois**
sur une arrivée : 6,1 ms, contre 740 µs pour le carnet et 85 à 97 ms pour l'index. L'index domine
parce qu'il paie le coût fixe de tantivy — allouer le tas d'un `IndexWriter`, écrire un segment,
le valider — déjà relevé le 2026-09-11. Les fils, eux, ne font que des requêtes sur colonnes
indexées et réécrivent trois lignes.

**Et une limite que le banc doit dire lui-même.** Les trois colonnes « à vide » ne sont pas
comparables **entre elles** : les passes tournent dans l'ordre index, carnet, fils, et la première
paie l'échauffement que les suivantes trouvent fait. Chaque ligne n'est comparable qu'à sa propre
colonne « complète », qui est la question posée. Accessoirement, la colonne « à vide » de l'index
est ici de 104 à 148 µs là où le 2026-09-11 relevait 19 à 23 µs : même ordre de grandeur, même
verdict — c'est une question, pas les 11 à 15 ms qu'elle a remplacées — mais l'écart n'est pas
expliqué, et le dire vaut mieux que le taire.

Le corpus reste **synthétique**, et le relevé ne vaut que comme rapport entre les trois régimes :
il n'y a plus de corpus réel sur cette machine depuis le 2026-09-10.

Vérification : `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`,
`cargo test --workspace` — **1 326 tests verts**, soit 21 de plus. Dont l'équivalence des deux
chemins, la même à l'envers, la réponse qui rejoint son parent, le parent qui réunit deux fils, le
message qui quitte son groupe de sujet, les deux invariants du schéma — aucun `thread_id` pendant,
aucun fil vide, aucun compteur qui dérive — la reconstruction forcée d'un store sans faits, et la
passe qui finit toujours.
