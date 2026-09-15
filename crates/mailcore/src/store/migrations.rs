//! Schéma versionné, appliqué via `PRAGMA user_version`.
//!
//! Une migration est un pas en avant, jamais en arrière : pas de `down`. Un binaire qui
//! ouvre un store dont la version est plus récente que la sienne **refuse**
//! ([`Error::UnsupportedSchema`]) au lieu de le corrompre.

use rusqlite::Connection;

use crate::error::{Error, Result};

/// La version de schéma que ce binaire sait lire et écrire.
pub const SCHEMA_VERSION: u32 = 13;

/// Le schéma initial.
///
/// ## `refs` porte la date, et c'est délibéré
///
/// La requête qui décide du critère 2 — 60 fps sur 100 000 messages — est « les messages du
/// dossier X, du plus récent au plus ancien, page N ». La clé de tri vit dans `messages`,
/// la clé de filtre dans `refs` : sans dénormalisation, chaque page impose une jointure
/// puis un tri sur tout le dossier.
///
/// `date` est donc recopiée dans `refs`, ce qui permet à `refs_folder_date` de servir la
/// pagination par clé en un seul parcours de plage. La dénormalisation est sans risque ici
/// pour une raison précise : la date vient de l'en-tête d'un message immuable, donc elle ne
/// change jamais après insertion. Il n'y a pas de mise à jour à propager, donc pas de dérive
/// possible.
///
/// Ce que l'index ne couvre pas : le sujet et l'expéditeur affichés dans la ligne de liste.
/// Ils se lisent dans `messages` par clé primaire, cinquante fois par page. Les recopier
/// aussi dans `refs` dupliquerait les deux plus gros champs textuels du schéma pour
/// économiser cinquante lectures d'index — mauvais échange.
const SCHEMA_V1: &str = r"
CREATE TABLE accounts (
    id           INTEGER PRIMARY KEY,
    kind         TEXT NOT NULL,
    display_name TEXT NOT NULL
);

CREATE TABLE folders (
    id         INTEGER PRIMARY KEY,
    account_id INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    path       TEXT NOT NULL,
    kind       TEXT NOT NULL,
    UNIQUE (account_id, path)
);

CREATE TABLE threads (
    id              INTEGER PRIMARY KEY,
    -- Nullable : un fil existe dès son premier message, sa racine peut être révisée
    -- quand un message antérieur arrive plus tard.
    root_message_id INTEGER,
    subject_norm    TEXT NOT NULL,
    last_date       INTEGER NOT NULL,
    message_count   INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE messages (
    id              INTEGER PRIMARY KEY,
    -- 32 octets bruts, pas 64 caractères hexadécimaux : moitié de la place, moitié de
    -- l'index. UNIQUE est la contrainte qui rend la duplication impossible au niveau du
    -- stockage, pas seulement improbable.
    blob_hash       BLOB NOT NULL UNIQUE,
    -- L'en-tête Message-ID. Nullable et non unique : il vient du réseau, des expéditeurs
    -- l'omettent, et d'autres réutilisent le même sur des messages différents.
    message_id      TEXT,
    -- Nullable, et c'est voulu : au moment de l'import on ne sait pas encore à quel fil un
    -- message appartient. Le threading est une passe séparée (étape 5), parce que jwz a
    -- besoin de voir les messages qui arriveront après celui-ci. Écrire un fil bidon à
    -- l'import pour satisfaire un NOT NULL reviendrait à stocker une réponse fausse.
    thread_id       INTEGER REFERENCES threads(id),
    date            INTEGER NOT NULL,
    from_addr       TEXT NOT NULL,
    from_name       TEXT,
    subject         TEXT NOT NULL,
    size            INTEGER NOT NULL,
    has_attachments INTEGER NOT NULL DEFAULT 0
);

-- La table qui tue la duplication. Un message dans INBOX et dans [Gmail]/Tous les
-- messages : une ligne `messages`, deux lignes `refs`.
CREATE TABLE refs (
    message_id INTEGER NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    folder_id  INTEGER NOT NULL REFERENCES folders(id) ON DELETE CASCADE,
    -- Dénormalisée depuis messages.date. Immuable, donc pas de dérive possible.
    date       INTEGER NOT NULL,
    -- Les drapeaux appartiennent à la référence, pas au message : le même contenu peut
    -- être lu dans un dossier et non lu dans un autre.
    flags      INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (message_id, folder_id)
) WITHOUT ROWID;

-- Critère 2. Sert la pagination par clé :
--   WHERE folder_id = ? AND (date, message_id) < (?, ?)
--   ORDER BY date DESC, message_id DESC LIMIT ?
-- Jamais OFFSET : un OFFSET 50000 lit 50 000 lignes pour en jeter 49 950.
CREATE INDEX refs_folder_date ON refs (folder_id, date DESC, message_id DESC);

-- Compter les références d'un blob avant de le supprimer, et lister les dossiers d'un
-- message.
CREATE INDEX refs_message ON refs (message_id);

-- Threading : retrouver un message par son Message-ID pour résoudre In-Reply-To.
CREATE INDEX messages_message_id ON messages (message_id) WHERE message_id IS NOT NULL;

-- Afficher un fil dans l'ordre.
CREATE INDEX messages_thread_date ON messages (thread_id, date);

-- Retrouver ce que la passe de threading n'a pas encore traité, sans parcourir la table.
CREATE INDEX messages_unthreaded ON messages (id) WHERE thread_id IS NULL;

-- get_contact_history : les échanges avec une adresse, du plus récent au plus ancien.
CREATE INDEX messages_from_date ON messages (from_addr, date DESC);

-- Trier la liste des fils.
CREATE INDEX threads_last_date ON threads (last_date DESC);
";

/// Ce qu'il faut pour synchroniser depuis un serveur — phase 2.
///
/// ## `remote_uids` est une table, et pas une colonne de `refs`
///
/// La tentation est d'ajouter `uid` à `refs`, dont la clé est déjà `(message_id, folder_id)`.
/// Ça ne marche pas, et le cas n'est pas exotique : **deux UID d'un même dossier peuvent
/// porter un contenu identique.** Une double remise, un `APPEND` répété, un utilisateur qui
/// copie un message sur lui-même — et le serveur a deux copies, avec deux UID, deux jeux de
/// drapeaux et deux existences indépendantes.
///
/// Avec un `uid` dans `refs`, la deuxième copie ne rentre pas : la clé primaire est déjà
/// prise. On perdrait donc l'une des deux, et **la suppression de l'autre retirerait la
/// référence** alors que le serveur en a encore une. La liste afficherait un message en moins
/// que la boîte.
///
/// Deux tables, donc, avec deux rôles nets :
///
/// - `refs` est la table de **l'affichage** : une ligne par message et par dossier, c'est ce
///   que la liste pagine, et c'est la table qui tue la duplication ;
/// - `remote_uids` est la table de la **synchronisation** : une ligne par copie côté serveur.
///
/// Une référence existe tant qu'au moins un UID lui répond. C'est l'adressage par contenu
/// appliqué au réseau, et non une exception à celui-ci.
///
/// ## Les drapeaux existent en deux endroits, et l'un dérive de l'autre
///
/// `remote_uids.flags` est ce que le serveur dit de **cette copie**. `refs.flags` est ce que
/// l'interface affiche pour **ce message dans ce dossier**, et il est recalculé depuis les
/// copies. La règle, quand deux copies ne s'accordent pas : **lu si au moins une copie est
/// lue**. Le contenu a été lu ; prétendre le contraire parce qu'un doublon invisible porte un
/// drapeau différent serait afficher une information fausse.
///
/// ## `remote_name` est un BLOB, et le chemin reste pour l'affichage
///
/// Un nom de boîte IMAP est de l'UTF-7 modifié, et rien ne garantit qu'il soit valide.
/// `path` est le décodage au mieux — ce que l'utilisateur lit, ce sur quoi porte
/// `UNIQUE (account_id, path)`. `remote_name` est la suite d'octets **telle que le serveur
/// l'écrit**, parce que c'est elle qu'il faut lui renvoyer dans un `SELECT`. Un dossier dont
/// le nom se décode mal doit rester ouvrable ; il ne le serait pas si on ne gardait que le
/// décodage.
///
/// ## Aucune colonne où mettre un secret
///
/// `accounts` gagne l'hôte, le port, l'identifiant et le mode d'authentification. **Pas le
/// mot de passe, pas le jeton** : ils vivent dans le trousseau du système, comme celui du
/// démon. Le critère 6 de `docs/PHASE-2.md` — zéro identifiant en clair dans le store —
/// n'est donc pas une discipline à tenir, il est une propriété du schéma : il n'y a pas de
/// colonne où le ranger.
///
/// ## Pourquoi les compteurs de synchronisation sont nullables
///
/// `uidvalidity`, `uidnext`, `highest_modseq`, `synced_at` : `NULL` veut dire « jamais
/// synchronisé », et c'est différent de « le serveur a répondu zéro ». Un `NOT NULL DEFAULT 0`
/// rendrait les deux indistinguables, et une resynchronisation complète se déclencherait — ou
/// ne se déclencherait pas — sur un zéro qui ne veut rien dire. Les dossiers mbox de la phase
/// 1 gardent ces colonnes vides, ce qui est exact : ils n'ont pas de serveur.
const SCHEMA_V2: &str = r"
ALTER TABLE accounts ADD COLUMN host     TEXT;
ALTER TABLE accounts ADD COLUMN port     INTEGER;
ALTER TABLE accounts ADD COLUMN username TEXT;
-- 'password' | 'oauth2'. Le secret lui-même est dans le trousseau du système.
ALTER TABLE accounts ADD COLUMN auth     TEXT;
-- 'tls' | 'starttls'. Pas de 'plain' : il n'y a pas de cas où on l'accepterait.
ALTER TABLE accounts ADD COLUMN security TEXT;
ALTER TABLE accounts ADD COLUMN enabled  INTEGER NOT NULL DEFAULT 1;

-- Le nom tel que le serveur l'écrit, à lui renvoyer verbatim.
ALTER TABLE folders ADD COLUMN remote_name    BLOB;
ALTER TABLE folders ADD COLUMN uidvalidity    INTEGER;
ALTER TABLE folders ADD COLUMN uidnext        INTEGER;
ALTER TABLE folders ADD COLUMN highest_modseq INTEGER;
ALTER TABLE folders ADD COLUMN synced_at      INTEGER;
ALTER TABLE folders ADD COLUMN subscribed     INTEGER NOT NULL DEFAULT 1;

-- Une ligne par copie côté serveur. Voir la documentation de cette migration.
CREATE TABLE remote_uids (
    folder_id  INTEGER NOT NULL REFERENCES folders(id) ON DELETE CASCADE,
    uid        INTEGER NOT NULL,
    message_id INTEGER NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    -- Ce que le serveur dit de cette copie. `refs.flags` en dérive.
    flags      INTEGER NOT NULL DEFAULT 0,
    -- Le MODSEQ de CONDSTORE, quand le serveur en donne un. NULL sinon.
    modseq     INTEGER,
    PRIMARY KEY (folder_id, uid)
) WITHOUT ROWID;

-- Recalculer `refs.flags` depuis les copies, et savoir si un message a encore une copie
-- quelque part avant de retirer sa référence.
CREATE INDEX remote_uids_message ON remote_uids (message_id, folder_id);

-- La moisson incrémentale sans CONDSTORE demande « les UID que j'ai déjà dans ce dossier »
-- en une plage ordonnée. La clé primaire la sert, mais seulement dans ce sens-là — d'où
-- l'index ci-dessus pour l'autre.
CREATE INDEX remote_uids_modseq ON remote_uids (folder_id, modseq)
    WHERE modseq IS NOT NULL;
";

/// La file d'envoi.
///
/// ## Pourquoi une table, et pas un fichier ni une mémoire
///
/// C'est le seul journal du projet dont la perte est **irréparable**. Tout le reste se
/// reconstruit depuis le serveur : un dossier mal synchronisé se resynchronise, un index
/// tantivy se réindexe. Un message qu'on a peut-être envoyé ne se redemande pas au serveur —
/// SMTP ne permet pas de poser la question.
///
/// D'où une table, dans la transaction du même fichier que le reste, et une machine à états
/// dont chaque transition est **écrite avant l'action qu'elle décrit**, jamais après.
///
/// ## `state` a cinq valeurs, et deux d'entre elles font tout le travail
///
/// | valeur | ce qui est sûr | ce que la file en fait |
/// |---|---|---|
/// | `queued` | rien n'est parti | à remettre |
/// | `sending` | l'enveloppe est ouverte, **aucun octet du corps n'est parti** | à remettre, sans risque de doublon |
/// | `committing` | le point final est peut-être passé | **rien d'automatique**, l'utilisateur tranche |
/// | `sent` | le serveur a répondu `250` | fini |
/// | `failed` | refus définitif, avant le corps | rendu à l'utilisateur |
///
/// La ligne qui compte est `sending` contre `committing`, et c'est ce que la frontière entre
/// les deux garantit : `committing` est écrit **avant** l'envoi du point final. Donc un
/// processus tué en `sending` n'avait rien commis — le serveur a une transaction vide, qu'il
/// abandonne à son propre délai — et le message peut repartir sans risque. Un processus tué en
/// `committing` a peut-être commis, et rien ne lèvera le doute.
///
/// Le sens de l'erreur est choisi : la frontière peut créer un **faux doute** — écrite, puis le
/// processus meurt avant d'écrire le premier octet du corps — jamais un faux « rien n'est
/// parti ». Un faux doute coûte une décision à l'utilisateur ; l'inverse coûte un doublon chez
/// le destinataire.
///
/// ## L'enveloppe est stockée à part des en-têtes
///
/// `sender` et `recipients` sont l'enveloppe SMTP, et elle ne dit pas la même chose que les
/// en-têtes du message : une copie cachée est dans `recipients` et dans aucun en-tête. Les
/// recalculer depuis le corps au moment de la remise ferait disparaître les `Bcc`.
///
/// ## Le corps est un blob adressé par contenu
///
/// Le même magasin que les messages reçus, et pour la même raison : rien de volumineux dans
/// SQLite. Un message de 25 Mo dans une colonne `BLOB` ferait grossir l'index de métadonnées
/// que l'UI relit à chaque ouverture.
const SCHEMA_V3: &str = r"
CREATE TABLE outbox (
    id          INTEGER PRIMARY KEY,
    account_id  INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    -- 32 octets bruts : les octets RFC 5322 sont dans le magasin de blobs.
    blob_hash   BLOB NOT NULL,
    -- L'enveloppe SMTP. Voir la documentation de cette migration : elle n'est pas
    -- reconstructible depuis les en-têtes.
    sender      TEXT NOT NULL,
    -- Une adresse par ligne. Un tableau JSON ne servirait qu'à ajouter un analyseur, et une
    -- adresse ne peut pas contenir de retour à la ligne : `compose::Address::parse` le refuse.
    recipients  TEXT NOT NULL,
    -- 'queued' | 'sending' | 'committing' | 'sent' | 'failed'. Pas de valeur par défaut :
    -- une ligne sans état serait une ligne dont personne ne sait quoi faire.
    state       TEXT NOT NULL,
    attempts    INTEGER NOT NULL DEFAULT 0,
    queued_at   INTEGER NOT NULL,
    tried_at    INTEGER,
    -- Le texte rendu à l'utilisateur — critère 8. Jamais un code seul, jamais un secret.
    last_error  TEXT,
    -- Avant cet instant, ne pas réessayer. NULL veut dire tout de suite.
    retry_after INTEGER
);

-- « Qu'y a-t-il à remettre, maintenant ? » est la seule question que la boucle d'envoi pose,
-- et elle la pose souvent. Sans cet index elle parcourt les envois déjà faits.
CREATE INDEX outbox_pending ON outbox (state, retry_after);
";

/// Le serveur de soumission, par compte.
///
/// ## Pourquoi ce n'est pas déduit du serveur IMAP
///
/// Parce que la déduction marche jusqu'au jour où elle échoue, et ce jour-là elle envoie le
/// message au mauvais endroit. `imap.gmail.com` → `smtp.gmail.com` est vrai ; `mail.free.fr` →
/// `smtp.free.fr` est vrai ; `outlook.office365.com` → `smtp.office365.com` est vrai. Puis vient
/// un hébergeur dont le serveur d'envoi n'est pas sur le même nom, ou un compte dont
/// l'identifiant de soumission diffère de celui de lecture — ce qui existe — et la déduction
/// devient un envoi qui échoue sans dire pourquoi, ou un `SUBMIT` chez un tiers.
///
/// Les colonnes sont donc **nullables** : un compte sans serveur de soumission ne peut pas
/// envoyer, et c'est un état honnête. `mail account submission` les remplit, et
/// `mail account list` dit lesquels manquent.
///
/// ## Ce qu'elles ne contiennent pas
///
/// Aucun secret, comme `accounts` n'en contient déjà aucun. Le mot de passe ou le jeton vit dans
/// le trousseau du système, sous la même clé que celle de la lecture : les fournisseurs du corpus
/// acceptent le même secret pour IMAP et pour la soumission, et en demander un second serait
/// exiger de l'utilisateur une saisie qui ne sert à rien.
///
/// `smtp_auth` est là malgré ça, parce que le **mécanisme** peut différer même quand le secret
/// ne diffère pas : un serveur qui annonce `AUTH PLAIN` en IMAP et impose `XOAUTH2` en
/// soumission existe. Absent, le mécanisme de la lecture est repris.
const SCHEMA_V4: &str = r"
ALTER TABLE accounts ADD COLUMN smtp_host     TEXT;
ALTER TABLE accounts ADD COLUMN smtp_port     INTEGER;
-- Nullable : absent, l'identifiant de la lecture est repris.
ALTER TABLE accounts ADD COLUMN smtp_username TEXT;
-- 'password' | 'oauth2'. Nullable : absent, le mécanisme de la lecture est repris.
ALTER TABLE accounts ADD COLUMN smtp_auth     TEXT;
-- 'tls' (465) | 'starttls' (587). Pas de 'plain' : un SUBMIT en clair transporte le mot de
-- passe **et** le message.
ALTER TABLE accounts ADD COLUMN smtp_security TEXT;
";

/// Le carnet d'adresses, dérivé du corpus.
///
/// ## Ce que la table contient, et pourquoi ce ne sont pas des « contacts »
///
/// Une ligne par adresse vue, avec ce que le corpus en dit. Pas de fiche, pas de photo, pas de
/// numéro de téléphone : `docs/PHASE-3.md` met CardDAV dehors, et une table de fiches sans
/// protocole pour les remplir serait une table vide avec un formulaire.
///
/// Ce qu'elle sert, en revanche, est la seule chose dont un client mail a besoin cent fois par
/// jour : **compléter un destinataire**. Le corpus réel en porte 105 000 références, donc autant
/// d'occasions d'avoir déjà vu la personne à qui on écrit.
///
/// ## Les deux compteurs ne valent pas la même chose
///
/// `seen_to` compte les fois où **l'utilisateur a écrit** à cette adresse. `seen_from` compte
/// les fois où elle lui a écrit. Le premier est un signal d'intention ; le second est une
/// statistique de réception, et une lettre d'information y monte à deux mille sans qu'on lui ait
/// jamais répondu.
///
/// D'où deux colonnes et non un total. Le classement les pondère différemment et **plafonne**
/// `seen_from` — voir `contacts::score`. Sans le plafond, taper « c » proposerait le service
/// client d'un marchand avant le collègue à qui on écrit chaque semaine.
///
/// ## L'adresse est la clé, en minuscules
///
/// La partie domaine d'une adresse est insensible à la casse (RFC 5321 §2.4), et la partie
/// locale l'est en théorie mais pas en pratique : aucun fournisseur du corpus ne distingue
/// `Marie@` de `marie@`. Garder les deux comme deux lignes donnerait deux propositions pour une
/// personne, ce qui est pire que l'inexactitude théorique.
///
/// Le **nom affiché**, lui, garde sa casse : c'est ce que l'utilisateur lit.
const SCHEMA_V5: &str = r"
CREATE TABLE contacts (
    -- L'adresse en minuscules. Voir la documentation de cette migration.
    address    TEXT PRIMARY KEY,
    -- Le nom affiché le plus récemment vu, tel quel. NULL quand aucun message n'en portait.
    name       TEXT,
    -- Le même nom, replié en minuscules **par Rust**, pour la recherche.
    --
    -- Et ce n'est pas une dénormalisation de confort : `lower()` de SQLite ne replie que
    -- l'ASCII. Sur un corpus français, `instr(lower(name), 'éloïse')` ne trouve jamais
    -- « Éloïse » — la majuscule accentuée reste telle quelle. Le repli Unicode se fait donc en
    -- Rust, à l'écriture, et la recherche compare deux chaînes déjà repliées.
    --
    -- Les accents sont **gardés** : replier « é » en « e » ferait trouver « Eloise » en tapant
    -- « éloïse », ce qui est souhaitable, mais aussi confondre des noms distincts. Le faire
    -- demande une table de translittération, et c'est un choix à mesurer, pas à improviser.
    name_fold  TEXT,
    -- Combien de fois l'utilisateur a écrit à cette adresse. **Le signal d'intention.**
    seen_to    INTEGER NOT NULL DEFAULT 0,
    -- Combien de fois elle lui a écrit. Plafonné au classement.
    seen_from  INTEGER NOT NULL DEFAULT 0,
    -- La date du message le plus récent qui la mentionne, en secondes Unix.
    last_seen  INTEGER NOT NULL DEFAULT 0
) WITHOUT ROWID;

-- La complétion par préfixe d'adresse. `WITHOUT ROWID` fait de la clé primaire l'index, donc
-- un `address >= 'ma' AND address < 'mb'` est un parcours de plage — ce que le critère 4
-- demande, et ce qu'un `LIKE '%ma%'` ne serait jamais.
--
-- Pas d'index sur `name`, **pour l'instant** : la complétion par nom est un parcours de la
-- table, et savoir si c'est tenable est le travail du critère 4. Un index sur un préfixe de nom
-- ne servirait de toute façon qu'au premier mot — « Dupont » ne se trouverait pas en tapant
-- « dup » si le nom est « Marie Dupont ». Le mesurer avant de l'ajouter.
CREATE INDEX contacts_rank ON contacts (seen_to DESC, last_seen DESC);
";

/// Le drapeau `\Seen`, marqué localement et poussé vers le serveur.
///
/// ## Pourquoi il faut une table, et pas seulement une écriture dans `refs`
///
/// `refs.flags` est **dérivé** de `remote_uids.flags` — voir `Writer::refresh_ref_flags`. Y
/// écrire « lu » directement serait effacé au prochain recalcul, c'est-à-dire à la prochaine
/// moisson du dossier : l'utilisateur verrait le message redevenir non lu tout seul.
///
/// La marque locale va donc dans `remote_uids.flags`, qui est la table dont `refs` dérive. Mais
/// celle-là est écrasée par ce que le serveur répond à chaque moisson. Sans rien de plus, le
/// message redeviendrait non lu au premier passage suivant.
///
/// D'où cette table : elle retient **ce qu'il reste à dire au serveur**. La moisson la vide
/// avant de relire les drapeaux, donc l'ordre est : pousser, puis relire. Poser la marque
/// localement sans la file serait une interface qui mentirait jusqu'à la synchronisation
/// suivante.
///
/// ## Une ligne par copie, pas par message
///
/// Le serveur ne connaît pas les messages, il connaît des UID dans des boîtes. Un même contenu
/// dans `INBOX` et dans `Tous les messages` a deux UID, et marquer l'un ne marque pas l'autre —
/// Gmail fait exception en les liant, la plupart des serveurs non.
///
/// ## Pourquoi seulement `\Seen`
///
/// C'est la seule écriture côté serveur que la phase 2 autorise (`docs/PHASE-2.md`). La table
/// ne porte donc pas de colonne « quel drapeau » : l'ajouter avant d'en avoir besoin serait
/// deviner la forme d'un besoin futur. Une migration l'ajoutera le jour où `\Flagged` ou
/// `\Answered` seront au programme.
const SCHEMA_V6: &str = r"
CREATE TABLE flag_pushes (
    folder_id INTEGER NOT NULL REFERENCES folders(id) ON DELETE CASCADE,
    uid       INTEGER NOT NULL,
    -- Quand la marque a été posée localement, en secondes Unix. Sert au diagnostic : une
    -- poussée qui traîne depuis des jours dit qu'un compte ne se synchronise plus.
    marked_at INTEGER NOT NULL,
    PRIMARY KEY (folder_id, uid)
) WITHOUT ROWID;
";

/// La taille du message en file.
///
/// ## Pourquoi elle doit être en base et pas relue du blob
///
/// `MAIL FROM ... SIZE=` s'annonce **avant** le transfert : c'est ce qui permet à un serveur qui
/// connaît sa limite de refuser tout de suite, plutôt que de laisser 25 Mo monter pour les
/// refuser à la fin. Or depuis que le corps passe du magasin au socket en flux — critère 3 —
/// personne ne connaît plus sa longueur : un flux ne la donne pas.
///
/// La relire du blob voudrait dire le décompresser en entier une première fois pour le compter,
/// puis une seconde pour l'envoyer. La taille est connue à la mise en file, où le message vient
/// d'être écrit ; la garder là est la seule lecture qui ne coûte rien.
///
/// Le défaut à zéro couvre les lignes écrites avant cette migration : `SIZE=0` n'est pas
/// annoncé, et l'envoi retombe sur le comportement d'avant — un refus tardif au pire.
const SCHEMA_V7: &str = r"
ALTER TABLE outbox ADD COLUMN size INTEGER NOT NULL DEFAULT 0;
";

/// La signature d'un compte, rangée comme **document** et non comme HTML.
///
/// ## Pourquoi pas la colonne de HTML qu'on attendrait
///
/// Parce qu'un aller-retour par le HTML perd les lignes blanches, et qu'une signature commence
/// par « Cordialement, » suivi d'une ligne blanche. `mailhtml::blocks` jette les blocs
/// entièrement blancs — c'est la bonne règle pour *afficher* du courrier reçu, où un `<div>`
/// vide ne doit pas fabriquer une ligne — donc ranger le HTML voudrait dire perdre cette ligne
/// à **chaque ouverture de l'éditeur**. Le HTML est ce qui **part** ; il se recalcule à l'envoi.
///
/// La forme rangée est donc la sérialisation de `mailhtml::rich::Document`, dont les noms de
/// champs sont ce format. Elle est relue avec les invariants remis d'aplomb, jamais supposés :
/// une colonne illisible rend « pas de signature » plutôt qu'une erreur, parce qu'une signature
/// abîmée ne doit pas empêcher d'écrire un message.
///
/// ## Une colonne sur `accounts`, et pas une table
///
/// Une signature par compte, et le compte est déjà la ligne qu'on lit pour envoyer. Une table à
/// part demanderait une jointure pour un champ dont il existe exactement zéro ou une occurrence.
/// Elle n'est pas lue par [`super::Store::full_accounts`] : c'est le plus gros champ textuel du
/// schéma après le corps d'un message, et l'affichage de la liste des comptes n'en a pas besoin.
const SCHEMA_V8: &str = r"
ALTER TABLE accounts ADD COLUMN signature TEXT;
";

/// Les brouillons : ce qu'on a commencé à écrire et pas envoyé.
///
/// ## Une table à part, et **surtout pas** un état de la file d'envoi
///
/// La tentation est d'ajouter un `SendState::Draft` à `outbox`, qui a déjà tout ce qu'il faut.
/// C'est refusé, et pour la raison la plus sérieuse du dépôt : la clause `WHERE` de
/// `Store::deliverable` et `SendState::is_deliverable` décident **ce qui part chez quelqu'un**,
/// un message en `committing` ne se remet jamais automatiquement, et un test vérifie que les
/// deux sont d'accord. Ajouter un état à cette énumération, c'est toucher au seul morceau
/// irréparable du projet pour une commodité de rangement.
///
/// Et sur le fond, un brouillon n'est pas un message sortant : il n'a pas de blob RFC 5322, ses
/// destinataires ne sont pas validés, son sujet peut être vide, et il n'a **aucune** chance de
/// partir tant que personne n'a cliqué. Ce sont deux cycles de vie différents, donc deux tables.
///
/// ## Les champs sont ceux du formulaire, pas ceux d'un message
///
/// `to`, `cc` et `bcc` sont les chaînes **telles que tapées**, virgules comprises : un
/// brouillon rouvert doit montrer exactement ce qui était à l'écran, y compris une adresse
/// incomplète en cours de frappe. Les découper à l'enregistrement et les recoller à la
/// relecture perdrait « jean@, marie » au milieu d'une saisie.
///
/// `refs` porte la chaîne `References` séparée par des espaces, comme l'en-tête lui-même : elle
/// n'est ni lue ni interrogée ici, seulement rendue au client qui la renverra à `outbox.send`.
///
/// ## Les pièces jointes sont dans une table fille, et désignées par leur contenu
///
/// Un `blob_hash`, jamais un chemin — la règle du critère 3. Une pièce déjà rangée dans le
/// magasin y reste ; le brouillon ne fait que la nommer. `ON DELETE CASCADE` pour que jeter un
/// brouillon ne laisse pas des lignes orphelines, et `rank` pour garder l'ordre d'ajout.
const SCHEMA_V9: &str = r"
CREATE TABLE drafts (
    id          INTEGER PRIMARY KEY,
    account_id  INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    to_field    TEXT NOT NULL DEFAULT '',
    cc_field    TEXT NOT NULL DEFAULT '',
    bcc_field   TEXT NOT NULL DEFAULT '',
    subject     TEXT NOT NULL DEFAULT '',
    body        TEXT NOT NULL DEFAULT '',
    in_reply_to TEXT,
    refs        TEXT NOT NULL DEFAULT '',
    sign        INTEGER NOT NULL DEFAULT 1,
    updated_at  INTEGER NOT NULL
);

-- La liste des brouillons est « le plus récemment touché d'abord », comme toute liste de
-- brouillons : c'est celui qu'on rouvre.
CREATE INDEX drafts_updated ON drafts (updated_at DESC);

CREATE TABLE draft_attachments (
    draft_id  INTEGER NOT NULL REFERENCES drafts(id) ON DELETE CASCADE,
    rank      INTEGER NOT NULL,
    blob_hash BLOB NOT NULL,
    filename  TEXT NOT NULL,
    mime      TEXT NOT NULL,
    size      INTEGER NOT NULL,
    PRIMARY KEY (draft_id, rank)
) WITHOUT ROWID;
";

/// Un bit sur la ligne de file : renvoyer ce message **tel quel** a-t-il une chance ?
///
/// ## Pourquoi un bit et pas la famille du refus
///
/// La famille — quota, destinataire, taille, authentification — est une notion SMTP : elle se
/// lit sur un code et une étape du protocole, et son énumération vit dans `mailsmtp`. La ranger
/// ici obligerait le store à connaître SMTP, ou à garder une étiquette textuelle qu'il ne sait
/// pas interpréter et que chaque client réinterpréterait à sa façon.
///
/// Ce dont un client a besoin est plus étroit : **un bouton « Renvoyer » a-t-il un sens ?** La
/// réponse est un booléen, elle est calculée par le seul endroit qui sait pourquoi le serveur a
/// refusé, et elle est rangée à côté de la phrase qu'elle accompagne. C'est le même choix que
/// `Outgoing::doubtful` sur le fil : dériver une fois, du bon côté, plutôt que faire deviner.
///
/// ## `DEFAULT 0`, et c'est le défaut prudent
///
/// Une ligne `failed` écrite avant cette migration n'a pas de bit, donc elle ne proposera pas
/// de renvoi. C'est le bon sens du doute : proposer un geste dont on ignore l'effet est pire
/// que de n'en proposer aucun, et l'utilisateur garde `mail outbox --forget` puis une nouvelle
/// rédaction.
const SCHEMA_V10: &str = r"
ALTER TABLE outbox ADD COLUMN resendable INTEGER NOT NULL DEFAULT 0;
";

/// v11 — `messages.contacts_counted` : le carnet peut avancer sans tout refaire.
///
/// ## Le problème que ça résout
///
/// `contacts::rebuild` vide le carnet puis repart de zéro, parce que les compteurs
/// **s'ajoutent** : repasser sur un message déjà compté le compterait deux fois. Conséquence,
/// le carnet ne suivait aucune moisson — une personne à qui on venait d'écrire n'apparaissait
/// qu'à la prochaine reconstruction complète, lancée à la main.
///
/// Enchaîner une reconstruction complète après chaque moisson n'était pas envisageable : `IDLE`
/// met un `Kind::Sync` en file **à chaque arrivée de courrier**, et une passe complète sur
/// 73 000 messages se compte en minutes. Un mail reçu aurait coûté des minutes de processeur.
///
/// ## Pourquoi une colonne, et pas un filigrane sur l'identifiant
///
/// La solution courte serait de retenir « compté jusqu'à l'identifiant N » et de ne regarder
/// que les messages au-dessus. Elle marcherait **aujourd'hui**, et elle repose sur une
/// invariante que le schéma ne promet pas : `messages.id` est un `INTEGER PRIMARY KEY sans
/// `AUTOINCREMENT`, donc un alias de `rowid`, et SQLite **réutilise** un `rowid` libéré. Rien
/// ne supprime de ligne `messages` à ce jour ; le jour où quelque chose le fera, un message
/// inséré sous le filigrane serait sauté pour toujours, en silence, et le carnet cesserait
/// d'apprendre sans que rien ne le dise.
///
/// Un drapeau par ligne ne dépend d'aucun ordre : « ce message a été compté » est un fait
/// rangé dans la ligne, pas une déduction sur son identifiant.
///
/// ## La migration efface le carnet, et c'est volontaire
///
/// Toutes les lignes arrivent à `0` — « pas encore compté ». Si le carnet gardait ses
/// compteurs, la première passe incrémentale les doublerait. Le vider est sans perte : le
/// carnet est **dérivé** du corpus, et la première passe le refait en entier.
const SCHEMA_V11: &str = r"
ALTER TABLE messages ADD COLUMN contacts_counted INTEGER NOT NULL DEFAULT 0;
DELETE FROM contacts;
CREATE INDEX IF NOT EXISTS idx_messages_contacts_counted
    ON messages(contacts_counted) WHERE contacts_counted = 0;
";

/// v12 — `messages.indexed` : l'index plein texte peut avancer sans tout refaire.
///
/// Le même besoin que [`SCHEMA_V11`] pour le carnet, et le même remède : un drapeau par ligne
/// plutôt qu'un filigrane sur un identifiant que le schéma ne promet pas monotone.
///
/// ## Ce qui diffère du carnet, et qui demande une vérification de plus
///
/// Le drapeau du carnet et les compteurs qu'il protège vivent **dans le même fichier SQLite** :
/// ils ne peuvent pas se contredire. Ici, le drapeau est dans SQLite et les documents sont dans
/// tantivy — deux magasins, donc deux vérités possibles.
///
/// Elles divergent quand l'index est recréé sans que le store le sache : répertoire de recherche
/// supprimé, schéma illisible, disque remplacé. `open_or_create` recrée alors un index **vide**,
/// et des drapeaux qui disent « indexé » feraient sauter tous les messages — la recherche
/// resterait vide pour toujours, sans que rien ne le signale.
///
/// La divergence est donc cherchée au point d'usage, dans `index::update` : un index vide alors
/// que des messages se disent indexés est le symptôme, et remettre les drapeaux est le remède.
/// Écrire ça dans `open_or_create` aurait été plus direct et faux : cette fonction est appelée
/// sur des chemins de **lecture** — `Mailbox::open`, `mail search` — qui ne doivent pas écrire.
const SCHEMA_V12: &str = r"
ALTER TABLE messages ADD COLUMN indexed INTEGER NOT NULL DEFAULT 0;
CREATE INDEX IF NOT EXISTS idx_messages_unindexed
    ON messages(indexed) WHERE indexed = 0;
";

/// v13 — les faits dérivés du threading, rangés au lieu d'être relus dans les blobs.
///
/// ## Pourquoi le remède des deux passes précédentes ne suffisait pas
///
/// Le carnet et l'index plein texte sont devenus incrémentaux avec un drapeau par ligne : « ce
/// message a été compté ». Un fil ne se laisse pas traiter ainsi, et c'est écrit depuis la
/// phase 1 : **un fil se calcule à partir de messages qui arrivent après celui qu'on traite.**
/// Un message rattaché aujourd'hui peut devoir l'être autrement demain, quand son parent arrive.
///
/// « Incrémental » n'y veut donc pas dire « ne regarder que le nouveau », mais « recalculer le
/// voisinage touché ». Ce qu'il faut pour ça n'est pas un drapeau, c'est de savoir **qui est
/// voisin de qui** sans relire 73 000 blobs — la relecture est ce qui coûte 334 s à froid.
///
/// ## Les trois faits, et ce que chacun permet de demander
///
/// - `messages.subject_norm` — le sujet normalisé. Permet « quel groupe de sujet ce message
///   rejoindrait-il ? » en SQL, sur une colonne indexée ;
/// - `message_references` — les `Message-ID` que chaque message référence, un par ligne. Permet
///   la question que les blobs ne savent pas poser : « qui répond à ce message ? » se lit dans
///   les en-têtes des **autres**, et les parcourir tous est précisément ce qu'on veut éviter ;
/// - `messages.thread_link` — comment un message a été rattaché : par une référence résolue, ou
///   sans. Le repli par sujet n'est ouvert qu'aux seconds, et sans ce fait il faudrait
///   recalculer la résolution de toutes les références pour le savoir.
///
/// Les trois sont écrits **au moment où un message est rattaché**, dans la même transaction. Un
/// blob n'est donc lu qu'une fois dans la vie d'un message, là où il était relu à chaque passe.
///
/// ## La migration n'efface pas les fils, et ne les croit pas non plus
///
/// Contrairement à [`SCHEMA_V11`], rien n'est vidé : les fils existants sont **justes**, ils
/// sont seulement dépourvus des faits qui permettent de les prolonger. `thread_link` reste
/// `NULL` sur eux, et `Store::threaded_without_facts` compte exactement ces lignes-là :
/// `thread::update` voit qu'elle ne peut pas travailler localement et refait une passe
/// complète, une fois. Effacer ici aurait laissé un store sans fils entre la migration et la
/// première passe.
const SCHEMA_V13: &str = r"
ALTER TABLE messages ADD COLUMN subject_norm TEXT;
ALTER TABLE messages ADD COLUMN thread_link  INTEGER;

CREATE TABLE message_references (
    message_id INTEGER NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    -- Le `Message-ID` référencé, sans chevrons, tel que `mail-parser` le rend. Il peut
    -- désigner un message qu'on n'a pas : c'est le cas le plus fréquent — 11 004 références
    -- non résolues sur le corpus de la phase 1 — et le garder est ce qui permet de rattacher
    -- le parent le jour où il arrive.
    rfc822_id  TEXT NOT NULL
);

-- « Qui référence ce message ? » — la question du voisinage, celle qui coûtait une relecture
-- complète des blobs.
CREATE INDEX message_references_target ON message_references (rfc822_id);

-- Retirer les références d'un message avant de les réécrire, sans parcourir la table.
CREATE INDEX message_references_source ON message_references (message_id);

-- Le groupe de sujet d'un message qui arrive. Partiel : seuls les messages sans référence
-- résolue sont éligibles au repli, donc seuls eux sont cherchés par sujet.
CREATE INDEX messages_subject_norm ON messages (subject_norm) WHERE thread_link = 2;

-- Les fils rattachés avant cette migration : ils n'ont pas leurs faits, et `thread::update`
-- les compte pour savoir qu'une passe complète est due.
CREATE INDEX messages_without_thread_facts ON messages (id)
    WHERE thread_link IS NULL AND thread_id IS NOT NULL;
";

/// Applique les migrations manquantes.
///
/// Sans effet si le store est déjà à jour — l'appeler à chaque ouverture est le mode
/// d'emploi normal.
///
/// # Errors
///
/// [`Error::UnsupportedSchema`] si le store a été écrit par un binaire plus récent,
/// [`Error::Sqlite`] si une instruction échoue.
pub fn apply(conn: &Connection) -> Result<()> {
    let current = user_version(conn)?;

    if current > SCHEMA_VERSION {
        return Err(Error::UnsupportedSchema {
            found: current,
            supported: SCHEMA_VERSION,
        });
    }
    if current == SCHEMA_VERSION {
        return Ok(());
    }

    // Une seule transaction : un schéma à moitié appliqué serait pire qu'aucun schéma.
    let tx = conn.unchecked_transaction()?;
    if current < 1 {
        tx.execute_batch(SCHEMA_V1)?;
    }
    if current < 2 {
        tx.execute_batch(SCHEMA_V2)?;
    }
    if current < 3 {
        tx.execute_batch(SCHEMA_V3)?;
    }
    if current < 4 {
        tx.execute_batch(SCHEMA_V4)?;
    }
    if current < 5 {
        tx.execute_batch(SCHEMA_V5)?;
    }
    if current < 6 {
        tx.execute_batch(SCHEMA_V6)?;
    }
    if current < 7 {
        tx.execute_batch(SCHEMA_V7)?;
    }
    if current < 8 {
        tx.execute_batch(SCHEMA_V8)?;
    }
    if current < 9 {
        tx.execute_batch(SCHEMA_V9)?;
    }
    if current < 10 {
        tx.execute_batch(SCHEMA_V10)?;
    }
    if current < 11 {
        tx.execute_batch(SCHEMA_V11)?;
    }
    if current < 12 {
        tx.execute_batch(SCHEMA_V12)?;
    }
    if current < 13 {
        tx.execute_batch(SCHEMA_V13)?;
    }
    // `PRAGMA` ne se paramètre pas ; la valeur interpolée est une constante de compilation.
    tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    tx.commit()?;

    tracing::info!(from = current, to = SCHEMA_VERSION, "schéma migré");
    Ok(())
}

/// La version de schéma inscrite dans le fichier.
///
/// # Errors
///
/// [`Error::Sqlite`] si le pragma est illisible.
pub fn user_version(conn: &Connection) -> Result<u32> {
    Ok(conn.query_row("PRAGMA user_version", [], |row| row.get(0))?)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::store::db;

    /// Une base en mémoire, migrée, clés étrangères actives.
    fn migrated() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        db::apply_pragmas(&conn).unwrap();
        apply(&conn).unwrap();
        conn
    }

    /// Insère un compte, deux dossiers, un fil. Rend les identifiants des deux dossiers.
    fn fixture(conn: &Connection) -> (i64, i64) {
        conn.execute(
            "INSERT INTO accounts (id, kind, display_name) VALUES (1, 'mbox', 'Compte')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO folders (id, account_id, path, kind) VALUES
                 (10, 1, 'INBOX', 'inbox'),
                 (20, 1, '[Gmail]/Tous les messages', 'archive')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO threads (id, subject_norm, last_date) VALUES (100, 'essai', 42)",
            [],
        )
        .unwrap();
        (10, 20)
    }

    fn insert_message(conn: &Connection, id: i64, blob: &[u8], date: i64) {
        conn.execute(
            "INSERT INTO messages
                 (id, blob_hash, message_id, thread_id, date, from_addr, subject, size)
             VALUES (?1, ?2, NULL, 100, ?3, 'a@b.c', 'essai', 100)",
            rusqlite::params![id, blob, date],
        )
        .unwrap();
    }

    #[test]
    fn applies_to_an_empty_database() {
        let conn = migrated();
        assert_eq!(user_version(&conn).unwrap(), SCHEMA_VERSION);
    }

    #[test]
    fn is_idempotent() {
        let conn = migrated();
        apply(&conn).unwrap();
        apply(&conn).unwrap();
        assert_eq!(user_version(&conn).unwrap(), SCHEMA_VERSION);
    }

    #[test]
    fn refuses_a_schema_from_the_future() {
        let conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "user_version", SCHEMA_VERSION + 7)
            .unwrap();

        assert!(matches!(
            apply(&conn),
            Err(Error::UnsupportedSchema { found, supported })
                if found == SCHEMA_VERSION + 7 && supported == SCHEMA_VERSION
        ));
    }

    #[test]
    fn one_message_two_folders_one_row() {
        // La thèse du projet, au niveau du schéma. Ce test est la raison d'être de `refs`.
        let conn = migrated();
        let (inbox, archive) = fixture(&conn);
        insert_message(&conn, 1, &[0xAA; 32], 500);

        for folder in [inbox, archive] {
            conn.execute(
                "INSERT INTO refs (message_id, folder_id, date) VALUES (1, ?1, 500)",
                [folder],
            )
            .unwrap();
        }

        let messages: i64 = conn
            .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))
            .unwrap();
        let refs: i64 = conn
            .query_row("SELECT COUNT(*) FROM refs", [], |r| r.get(0))
            .unwrap();

        assert_eq!(messages, 1, "le message a été stocké deux fois");
        assert_eq!(refs, 2);
    }

    #[test]
    fn identical_content_cannot_be_inserted_twice() {
        let conn = migrated();
        fixture(&conn);
        insert_message(&conn, 1, &[0xAA; 32], 500);

        let again = conn.execute(
            "INSERT INTO messages
                 (id, blob_hash, thread_id, date, from_addr, subject, size)
             VALUES (2, ?1, 100, 500, 'a@b.c', 'essai', 100)",
            rusqlite::params![&[0xAA_u8; 32]],
        );
        assert!(again.is_err(), "blob_hash UNIQUE ne tient pas");
    }

    #[test]
    fn the_same_reference_cannot_exist_twice() {
        let conn = migrated();
        let (inbox, _) = fixture(&conn);
        insert_message(&conn, 1, &[0xAA; 32], 500);

        conn.execute(
            "INSERT INTO refs (message_id, folder_id, date) VALUES (1, ?1, 500)",
            [inbox],
        )
        .unwrap();
        let again = conn.execute(
            "INSERT INTO refs (message_id, folder_id, date) VALUES (1, ?1, 500)",
            [inbox],
        );
        assert!(again.is_err());
    }

    #[test]
    fn a_reference_to_a_missing_folder_is_rejected() {
        // Vérifie du même coup que `PRAGMA foreign_keys` est bien actif.
        let conn = migrated();
        fixture(&conn);
        insert_message(&conn, 1, &[0xAA; 32], 500);

        let orphan = conn.execute(
            "INSERT INTO refs (message_id, folder_id, date) VALUES (1, 999, 500)",
            [],
        );
        assert!(orphan.is_err(), "clé étrangère non appliquée");
    }

    #[test]
    fn deleting_a_folder_removes_its_references_not_the_messages() {
        let conn = migrated();
        let (inbox, archive) = fixture(&conn);
        insert_message(&conn, 1, &[0xAA; 32], 500);
        for folder in [inbox, archive] {
            conn.execute(
                "INSERT INTO refs (message_id, folder_id, date) VALUES (1, ?1, 500)",
                [folder],
            )
            .unwrap();
        }

        conn.execute("DELETE FROM folders WHERE id = ?1", [inbox])
            .unwrap();

        let refs: i64 = conn
            .query_row("SELECT COUNT(*) FROM refs", [], |r| r.get(0))
            .unwrap();
        let messages: i64 = conn
            .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))
            .unwrap();

        assert_eq!(refs, 1);
        assert_eq!(messages, 1, "supprimer un dossier a supprimé du contenu");
    }

    /// Une base au schéma v1 seulement, avec un compte, deux dossiers et un message.
    ///
    /// Écrit le v1 à la main plutôt que d'appeler `apply` : une migration se teste depuis
    /// l'ancien schéma, pas depuis le nouveau.
    fn at_v1() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        db::apply_pragmas(&conn).unwrap();
        conn.execute_batch(SCHEMA_V1).unwrap();
        conn.pragma_update(None, "user_version", 1).unwrap();
        fixture(&conn);
        insert_message(&conn, 1, &[0xAA; 32], 500);
        conn.execute(
            "INSERT INTO refs (message_id, folder_id, date, flags) VALUES (1, 10, 500, 3)",
            [],
        )
        .unwrap();
        conn
    }

    #[test]
    fn migrating_from_v1_keeps_the_data() {
        let conn = at_v1();
        apply(&conn).unwrap();

        assert_eq!(user_version(&conn).unwrap(), SCHEMA_VERSION);
        let (flags, date): (i64, i64) = conn
            .query_row(
                "SELECT flags, date FROM refs WHERE message_id = 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(flags, 3, "les drapeaux d'une référence existante ont bougé");
        assert_eq!(date, 500);
    }

    #[test]
    fn an_mbox_folder_has_no_server_counters() {
        // `NULL` veut dire « jamais synchronisé ». Un dossier venu d'un mbox n'a pas de
        // serveur, donc ses compteurs doivent rester vides et non valoir zéro.
        let conn = at_v1();
        apply(&conn).unwrap();

        let (uidvalidity, synced): (Option<i64>, Option<i64>) = conn
            .query_row(
                "SELECT uidvalidity, synced_at FROM folders WHERE id = 10",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert!(uidvalidity.is_none());
        assert!(synced.is_none());
    }

    #[test]
    fn two_uids_of_the_same_content_in_one_folder_both_survive() {
        // **La raison d'être de `remote_uids`.** Avec un `uid` dans `refs`, la deuxième copie
        // ne rentrerait pas : la clé primaire `(message_id, folder_id)` est déjà prise.
        let conn = migrated();
        let (inbox, _) = fixture(&conn);
        insert_message(&conn, 1, &[0xAA; 32], 500);
        conn.execute(
            "INSERT INTO refs (message_id, folder_id, date) VALUES (1, ?1, 500)",
            [inbox],
        )
        .unwrap();

        for uid in [5, 9] {
            conn.execute(
                "INSERT INTO remote_uids (folder_id, uid, message_id) VALUES (?1, ?2, 1)",
                rusqlite::params![inbox, uid],
            )
            .unwrap();
        }

        let copies: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM remote_uids WHERE folder_id = ?1 AND message_id = 1",
                [inbox],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(copies, 2, "une copie côté serveur a été perdue");
    }

    #[test]
    fn removing_one_copy_leaves_the_reference_while_another_remains() {
        // Le bug que la table évite : supprimer l'UID 5 ne doit pas faire disparaître le
        // message de la liste tant que l'UID 9 existe.
        let conn = migrated();
        let (inbox, _) = fixture(&conn);
        insert_message(&conn, 1, &[0xAA; 32], 500);
        conn.execute(
            "INSERT INTO refs (message_id, folder_id, date) VALUES (1, ?1, 500)",
            [inbox],
        )
        .unwrap();
        for uid in [5, 9] {
            conn.execute(
                "INSERT INTO remote_uids (folder_id, uid, message_id) VALUES (?1, ?2, 1)",
                rusqlite::params![inbox, uid],
            )
            .unwrap();
        }

        conn.execute(
            "DELETE FROM remote_uids WHERE folder_id = ?1 AND uid = 5",
            [inbox],
        )
        .unwrap();

        // La question que la sync posera avant de retirer une référence.
        let orphaned: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM remote_uids WHERE folder_id = ?1 AND message_id = 1",
                [inbox],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(orphaned, 1, "la référence serait retirée à tort");
    }

    #[test]
    fn the_same_uid_cannot_exist_twice_in_a_folder() {
        let conn = migrated();
        let (inbox, _) = fixture(&conn);
        insert_message(&conn, 1, &[0xAA; 32], 500);
        conn.execute(
            "INSERT INTO remote_uids (folder_id, uid, message_id) VALUES (?1, 5, 1)",
            [inbox],
        )
        .unwrap();

        let again = conn.execute(
            "INSERT INTO remote_uids (folder_id, uid, message_id) VALUES (?1, 5, 1)",
            [inbox],
        );
        assert!(again.is_err(), "un UID est unique dans son dossier");
    }

    #[test]
    fn deleting_a_folder_removes_its_copies() {
        let conn = migrated();
        let (inbox, _) = fixture(&conn);
        insert_message(&conn, 1, &[0xAA; 32], 500);
        conn.execute(
            "INSERT INTO remote_uids (folder_id, uid, message_id) VALUES (?1, 5, 1)",
            [inbox],
        )
        .unwrap();

        conn.execute("DELETE FROM folders WHERE id = ?1", [inbox])
            .unwrap();

        let left: i64 = conn
            .query_row("SELECT COUNT(*) FROM remote_uids", [], |r| r.get(0))
            .unwrap();
        let messages: i64 = conn
            .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))
            .unwrap();
        assert_eq!(left, 0);
        assert_eq!(messages, 1, "supprimer un dossier a supprimé du contenu");
    }

    #[test]
    fn a_folder_name_that_is_not_valid_utf8_survives_a_round_trip() {
        // Un nom de boîte IMAP est de l'UTF-7 modifié, et rien ne garantit qu'il soit
        // valide. `remote_name` est ce qu'on renverra au serveur : il doit ressortir octet
        // pour octet, quel que soit son contenu.
        let conn = migrated();
        fixture(&conn);
        let raw: &[u8] = &[0x49, 0x4E, 0x26, 0xFF, 0xFE, 0x00, 0x42, 0x4F, 0x58];

        conn.execute("UPDATE folders SET remote_name = ?1 WHERE id = 10", [raw])
            .unwrap();

        let back: Vec<u8> = conn
            .query_row("SELECT remote_name FROM folders WHERE id = 10", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(back, raw);
    }

    #[test]
    fn accounts_has_no_column_that_could_hold_a_secret() {
        // **Le critère 6 de `docs/PHASE-2.md`, verrouillé au niveau du schéma.** « Zéro
        // identifiant en clair dans le store » n'est pas une discipline à tenir si aucune
        // colonne ne peut en accueillir un. Ce test fige la liste : ajouter `password` ou
        // `token` à `accounts` le fait tomber, ce qui est exactement le but.
        let conn = migrated();
        let mut statement = conn
            .prepare("SELECT name FROM pragma_table_info('accounts')")
            .unwrap();
        let mut columns: Vec<String> = statement
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<String>>>()
            .unwrap();
        columns.sort();

        assert_eq!(
            columns,
            vec![
                "auth".to_owned(),
                "display_name".to_owned(),
                "enabled".to_owned(),
                "host".to_owned(),
                "id".to_owned(),
                "kind".to_owned(),
                "port".to_owned(),
                "security".to_owned(),
                // La signature — `SCHEMA_V8`. Elle ne peut pas porter de secret **par nature de son
                // contenu** : c'est du texte que l'utilisateur écrit pour l'envoyer à ses
                // correspondants, donc destiné à sortir de la machine. Aucun chemin de code n'y
                // écrit autre chose que ce document, et c'est ce qui doit rester vrai — la
                // tentation à refuser serait d'y ranger un jeton « pour le confort », dans une
                // colonne dont le contenu part chez le destinataire.
                "signature".to_owned(),
                // Le serveur de soumission — `SCHEMA_V4`. Aucune de ces colonnes ne peut
                // porter un secret, et `smtp_auth` ne dit que le **mécanisme**.
                "smtp_auth".to_owned(),
                "smtp_host".to_owned(),
                "smtp_port".to_owned(),
                "smtp_security".to_owned(),
                "smtp_username".to_owned(),
                "username".to_owned(),
            ],
            "la forme de `accounts` a changé : si c'est pour y ranger un secret, non — \
             le trousseau du système est le seul endroit"
        );
    }

    #[test]
    fn deleting_an_account_takes_its_outbox_with_it() {
        // Sans la cascade, un compte supprimé laisserait des messages à envoyer que plus
        // personne ne peut envoyer — et qui resteraient éligibles à la boucle de remise.
        let conn = migrated();
        conn.execute(
            "INSERT INTO accounts (id, kind, display_name) VALUES (7, 'imap', 'compte')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO outbox (account_id, blob_hash, sender, recipients, state, queued_at)
             VALUES (7, X'00', 'a@b.fr', 'c@d.fr', 'queued', 100)",
            [],
        )
        .unwrap();
        conn.execute("DELETE FROM accounts WHERE id = 7", [])
            .unwrap();

        let left: i64 = conn
            .query_row("SELECT count(*) FROM outbox", [], |r| r.get(0))
            .unwrap();
        assert_eq!(left, 0, "la file du compte supprimé survit");
    }

    #[test]
    fn a_queued_message_cannot_exist_without_a_state() {
        // `state` est `NOT NULL` sans valeur par défaut, exprès : une ligne dont personne ne
        // sait si elle est partie est le seul cas que cette table doit rendre impossible.
        let conn = migrated();
        conn.execute(
            "INSERT INTO accounts (id, kind, display_name) VALUES (7, 'imap', 'compte')",
            [],
        )
        .unwrap();
        let outcome = conn.execute(
            "INSERT INTO outbox (account_id, blob_hash, sender, recipients, queued_at)
             VALUES (7, X'00', 'a@b.fr', 'c@d.fr', 100)",
            [],
        );
        assert!(outcome.is_err(), "une ligne sans état a été acceptée");
    }

    #[test]
    fn the_pending_query_uses_the_index_without_scanning() {
        // La boucle de remise pose cette question souvent, et un parcours de table la ferait
        // ralentir avec l'historique des envois déjà faits — qui, lui, ne se vide jamais.
        let conn = migrated();
        conn.execute(
            "INSERT INTO accounts (id, kind, display_name) VALUES (7, 'imap', 'compte')",
            [],
        )
        .unwrap();
        for i in 1..=200 {
            conn.execute(
                "INSERT INTO outbox (account_id, blob_hash, sender, recipients, state, queued_at)
                 VALUES (7, X'00', 'a@b.fr', 'c@d.fr', 'sent', ?1)",
                [i],
            )
            .unwrap();
        }
        let plan: String = conn
            .query_row(
                "EXPLAIN QUERY PLAN
                 SELECT id FROM outbox
                 WHERE state = 'queued' AND (retry_after IS NULL OR retry_after <= 999)
                 ORDER BY id",
                [],
                |row| row.get(3),
            )
            .unwrap();
        assert!(
            plan.contains("outbox_pending"),
            "la requête de remise ne passe pas par son index : {plan}"
        );
        assert!(
            !plan.contains("SCAN outbox"),
            "la requête de remise parcourt la table : {plan}"
        );
    }
    #[test]
    fn keyset_pagination_uses_the_index_without_scanning() {
        // Le critère 2 dépend de ce plan de requête. S'il régresse en parcours de table,
        // ce test tombe avant la mesure.
        let conn = migrated();
        let (inbox, _) = fixture(&conn);
        for i in 1..=50 {
            let mut blob = [0u8; 32];
            blob[0] = u8::try_from(i).unwrap();
            insert_message(&conn, i, &blob, i * 10);
            conn.execute(
                "INSERT INTO refs (message_id, folder_id, date) VALUES (?1, ?2, ?3)",
                rusqlite::params![i, inbox, i * 10],
            )
            .unwrap();
        }

        let plan: String = conn
            .query_row(
                "EXPLAIN QUERY PLAN
                 SELECT message_id FROM refs
                 WHERE folder_id = ?1 AND (date, message_id) < (?2, ?3)
                 ORDER BY date DESC, message_id DESC
                 LIMIT 20",
                rusqlite::params![inbox, 400, 40],
                |r| r.get(3),
            )
            .unwrap();

        assert!(
            plan.contains("refs_folder_date"),
            "l'index de pagination n'est pas utilisé : {plan}"
        );
        assert!(
            !plan.contains("SCAN"),
            "la pagination fait un parcours complet : {plan}"
        );
    }

    #[test]
    fn a_populated_outbox_survives_the_tenth_migration() {
        // **Le cas qui compte pour un `ADD COLUMN ... NOT NULL`** : une table qui a déjà des
        // lignes. Un défaut manquant ferait échouer l'`ALTER`, un défaut mal choisi mentirait
        // sur des lignes que personne n'a classées.
        let conn = Connection::open_in_memory().unwrap();
        db::apply_pragmas(&conn).unwrap();
        // Le schéma tel qu'il était avant cette migration. Écrit à la main plutôt que par
        // `apply` : `apply` va jusqu'à la version courante, donc il n'y aurait rien à migrer.
        for step in [
            SCHEMA_V1, SCHEMA_V2, SCHEMA_V3, SCHEMA_V4, SCHEMA_V5, SCHEMA_V6, SCHEMA_V7, SCHEMA_V8,
            SCHEMA_V9,
        ] {
            conn.execute_batch(step).unwrap();
        }
        conn.pragma_update(None, "user_version", 9_u32).unwrap();
        fixture(&conn);
        conn.execute(
            "INSERT INTO outbox
                 (id, account_id, blob_hash, sender, recipients, state, queued_at, size,
                  attempts, last_error)
             VALUES (7, 1, ?1, 'marie@exemple.fr', 'jean@ailleurs.fr', 'failed', 900, 42,
                     6, 'refus définitif à l''étape RCPT TO (550)')",
            [&[0xAB_u8; 32][..]],
        )
        .unwrap();

        // Le contrôle qui prouve que ce test exerce bien la migration : sans elle, la colonne
        // n'existe pas. Un jour où l'échelle ci-dessus serait fausse, ce test passerait sans
        // rien migrer.
        assert!(
            conn.query_row("SELECT resendable FROM outbox WHERE id = 7", [], |r| r
                .get::<_, bool>(0))
                .is_err(),
            "la colonne existe déjà : ce test ne mesure pas la migration"
        );

        apply(&conn).unwrap();

        assert_eq!(user_version(&conn).unwrap(), SCHEMA_VERSION);
        let (state, attempts, error, resendable): (String, i64, String, bool) = conn
            .query_row(
                "SELECT state, attempts, last_error, resendable FROM outbox WHERE id = 7",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(
            state, "failed",
            "l'état d'un envoi a bougé pendant la migration"
        );
        assert_eq!(attempts, 6);
        assert!(error.contains("550"), "le texte du refus a été perdu");
        // **Faux, et c'est le défaut prudent.** Cette ligne a échoué avant que le bit
        // n'existe : personne n'a classé son refus, donc personne ne peut promettre qu'un
        // renvoi marcherait. Voir `SCHEMA_V10`.
        assert!(
            !resendable,
            "une ligne d'avant la migration se dit renvoyable sans que rien ne l'ait classée"
        );
    }
}
