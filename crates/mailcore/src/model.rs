//! Le vocabulaire partagé par tous les crates.
//!
//! Une seule idée porte tout le reste : **un message est une donnée immuable identifiée
//! par son contenu**. Dossiers, labels et comptes ne sont que des références vers ce
//! contenu, matérialisées par [`MessageRef`]. C'est ce qui rend la duplication Gmail
//! impossible par construction — voir `docs/VISION.md`.

use crate::error::{Error, Result};

/// L'identité d'un message : BLAKE3 des octets RFC 5322 bruts.
///
/// Bruts veut dire bruts : tels que reçus, avant toute normalisation d'en-tête. Deux
/// copies du même message dans `INBOX` et dans `[Gmail]/Tous les messages` donnent le
/// même hash, donc un seul blob.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BlobHash([u8; 32]);

impl BlobHash {
    /// Calcule l'identité d'un message à partir de ses octets RFC 5322 bruts.
    #[must_use]
    pub fn of(rfc822_bytes: &[u8]) -> Self {
        Self(*blake3::hash(rfc822_bytes).as_bytes())
    }

    /// Reconstruit une identité depuis ses 32 octets, tels que stockés dans l'index.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Les 32 octets du hash.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// La représentation hexadécimale minuscule, 64 caractères.
    #[must_use]
    pub fn to_hex(self) -> String {
        blake3::Hash::from_bytes(self.0).to_hex().to_string()
    }

    /// Décode une représentation hexadécimale.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidHash`] si la chaîne n'est pas 64 caractères hexadécimaux.
    pub fn from_hex(hex: &str) -> Result<Self> {
        blake3::Hash::from_hex(hex)
            .map(|h| Self(*h.as_bytes()))
            .map_err(|_| Error::InvalidHash(hex.to_owned()))
    }

    /// Le chemin relatif du blob dans le store : `<aa>/<bb>/<hash>`.
    ///
    /// Shardé sur deux octets pour qu'aucun répertoire ne dépasse quelques milliers
    /// d'entrées — les systèmes de fichiers Windows n'aiment pas les répertoires plats
    /// de plusieurs centaines de milliers de fichiers.
    #[must_use]
    pub fn shard_path(self) -> camino::Utf8PathBuf {
        let hex = self.to_hex();
        camino::Utf8PathBuf::from(&hex[0..2])
            .join(&hex[2..4])
            .join(&hex)
    }
}

impl std::fmt::Display for BlobHash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.to_hex())
    }
}

/// Identifiant interne d'un message dans l'index de métadonnées.
///
/// Distinct du [`Rfc822MessageId`] : celui-ci vient du réseau et n'est ni unique ni
/// digne de confiance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MessageId(pub i64);

/// L'en-tête `Message-ID`, tel qu'écrit par l'expéditeur.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Rfc822MessageId(pub String);

/// Identifiant interne d'un fil.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ThreadId(pub i64);

/// Identifiant interne d'un compte.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AccountId(pub i64);

/// Identifiant interne d'un dossier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FolderId(pub i64);

/// D'où un compte tient son courrier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountKind {
    /// Importé depuis des fichiers mbox. Pas de serveur, pas de synchronisation.
    Mbox,
    /// Synchronisé depuis un serveur IMAP.
    Imap,
}

impl AccountKind {
    /// L'étiquette persistée dans `accounts.kind`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Mbox => "mbox",
            Self::Imap => "imap",
        }
    }

    /// Relit une étiquette. Une valeur inconnue rend [`AccountKind::Mbox`] : un compte dont on
    /// ne reconnaît pas la sorte ne doit surtout pas être synchronisé par défaut.
    #[must_use]
    pub fn from_str_lossy(label: &str) -> Self {
        match label {
            "imap" => Self::Imap,
            _ => Self::Mbox,
        }
    }
}

/// Comment on prouve son identité au serveur.
///
/// **Le secret n'est pas ici, et il n'y a pas de variante qui le porte.** Il vit dans le
/// trousseau du système, adressé par l'identifiant du compte. Voir la migration v2.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthKind {
    /// Mot de passe, ou mot de passe applicatif.
    Password,
    /// `XOAUTH2` — un jeton d'accès, rafraîchi hors bande.
    OAuth2,
}

impl AuthKind {
    /// L'étiquette persistée dans `accounts.auth`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Password => "password",
            Self::OAuth2 => "oauth2",
        }
    }

    /// Relit une étiquette.
    ///
    /// # Errors
    ///
    /// [`Error::UnknownAuth`] sur une valeur inconnue. **Pas de repli ici** : se tromper de
    /// mécanisme d'authentification, c'est envoyer un secret dans un champ qui ne l'attend
    /// pas. Mieux vaut refuser d'ouvrir le compte.
    pub fn parse(label: &str) -> crate::Result<Self> {
        match label {
            "password" => Ok(Self::Password),
            "oauth2" => Ok(Self::OAuth2),
            other => Err(crate::Error::UnknownAuth {
                found: other.to_owned(),
            }),
        }
    }
}

/// Comment le canal est chiffré.
///
/// **Il n'y a pas de variante en clair.** Un IMAP sans chiffrement transporte l'identifiant
/// et le mot de passe en clair sur le réseau ; il n'existe pas de configuration où on
/// l'accepterait, donc il n'y a pas de valeur pour le dire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Security {
    /// TLS dès la connexion — le port 993.
    Tls,
    /// Connexion en clair, puis `STARTTLS` obligatoire — le port 143.
    StartTls,
}

impl Security {
    /// L'étiquette persistée dans `accounts.security`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Tls => "tls",
            Self::StartTls => "starttls",
        }
    }

    /// Relit une étiquette.
    ///
    /// # Errors
    ///
    /// [`Error::UnknownSecurity`] sur une valeur inconnue, pour la même raison qu'`AuthKind` :
    /// un repli silencieux choisirait le mode de chiffrement à la place de l'utilisateur.
    pub fn parse(label: &str) -> crate::Result<Self> {
        match label {
            "tls" => Ok(Self::Tls),
            "starttls" => Ok(Self::StartTls),
            other => Err(crate::Error::UnknownSecurity {
                found: other.to_owned(),
            }),
        }
    }

    /// Le port d'usage pour ce mode.
    #[must_use]
    pub const fn default_port(self) -> u16 {
        match self {
            Self::Tls => 993,
            Self::StartTls => 143,
        }
    }

    /// Le port de **soumission** SMTP pour ce mode.
    ///
    /// Séparé de [`Security::default_port`], et pas par symétrie : lire 993 pour un envoi
    /// enverrait le message sur le port IMAP, où il n'y a rien qui parle SMTP — et l'erreur
    /// ressemblerait à une panne réseau. Deux protocoles, deux paires de ports.
    ///
    /// 465 est le `submissions` de la RFC 8314, TLS dès la connexion, et c'est celui que la RFC
    /// recommande. 587 est le `submission` de la RFC 6409, en clair puis `STARTTLS`.
    ///
    /// **Pas le 25.** C'est le port de relais entre serveurs ; les fournisseurs le bloquent en
    /// sortie, et il n'exige pas de chiffrement.
    #[must_use]
    pub const fn submission_port(self) -> u16 {
        match self {
            Self::Tls => 465,
            Self::StartTls => 587,
        }
    }
}

/// De quoi joindre un serveur IMAP. **Jamais le secret.**
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Server {
    /// Nom d'hôte.
    pub host: String,
    /// Port.
    pub port: u16,
    /// Identifiant présenté au serveur.
    pub username: String,
    /// Mécanisme d'authentification.
    pub auth: AuthKind,
    /// Mode de chiffrement.
    pub security: Security,
}

/// Un compte.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Account {
    /// Identifiant interne.
    pub id: AccountId,
    /// D'où vient son courrier.
    pub kind: AccountKind,
    /// Nom lisible, choisi par l'utilisateur.
    pub display_name: String,
    /// Le serveur, pour un compte IMAP. `None` pour un compte mbox — il n'en a pas.
    pub server: Option<Server>,
    /// Le serveur de **soumission**, quand il est configuré.
    ///
    /// ## `None` veut dire « ce compte ne peut pas envoyer », et c'est un état honnête
    ///
    /// Il n'est pas déduit de [`Account::server`]. La déduction marche pour Gmail, pour Free,
    /// pour Microsoft, puis vient un hébergeur dont le serveur d'envoi n'est pas sur le même
    /// nom — et l'envoi part alors chez un tiers, ou échoue sans dire pourquoi. Voir la
    /// migration `SCHEMA_V4`.
    ///
    /// Le port se replie sur [`Security::submission_port`], l'identifiant et le mécanisme sur
    /// ceux de la lecture. Le **mode de chiffrement**, jamais : le deviner rétrograderait le
    /// chiffrement à l'insu de l'utilisateur.
    pub submission: Option<Server>,
    /// Faux quand l'utilisateur a mis la synchronisation en pause.
    pub enabled: bool,
}

impl Account {
    /// Vrai si ce compte a de quoi envoyer.
    ///
    /// Ce que l'interface doit lire avant de proposer un bouton « envoyer » — critère 8 : mieux
    /// vaut dire « ce compte n'a pas de serveur d'envoi » que laisser écrire un message pour le
    /// refuser au dernier moment.
    #[must_use]
    pub const fn can_send(&self) -> bool {
        self.submission.is_some()
    }
}

/// Ce que le store sait de l'état d'un dossier côté serveur.
///
/// Chaque champ optionnel distingue « **jamais synchronisé** » de « le serveur a répondu
/// zéro ». Un `0` par défaut rendrait les deux indistinguables, et une resynchronisation
/// complète se déclencherait — ou pas — sur une valeur qui ne veut rien dire.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SyncState {
    /// Le nom de la boîte **tel que le serveur l'écrit**, à lui renvoyer verbatim.
    ///
    /// De l'UTF-7 modifié en principe, et rien ne garantit qu'il soit valide : d'où des
    /// octets et non une chaîne. Vide pour un dossier mbox.
    pub remote_name: Vec<u8>,
    /// `UIDVALIDITY`. Un changement invalide tous les UID du dossier.
    pub uidvalidity: Option<u32>,
    /// `UIDNEXT` au dernier passage : au-dessus, c'est du nouveau.
    pub uidnext: Option<u32>,
    /// `HIGHESTMODSEQ` de `CONDSTORE`, quand le serveur le donne.
    pub highest_modseq: Option<u64>,
    /// Secondes Unix du dernier passage réussi.
    pub synced_at: Option<i64>,
    /// Faux pour une boîte que l'utilisateur n'a pas abonnée.
    pub subscribed: bool,
}

/// Une copie d'un message côté serveur : un UID dans un dossier.
///
/// Distincte de [`MessageRef`], et c'est le cœur de la migration v2 : deux UID peuvent porter
/// le même contenu dans le même dossier, alors qu'il n'y a qu'une référence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RemoteCopy {
    /// Le dossier.
    pub folder: FolderId,
    /// L'UID côté serveur. Non nul, et unique dans son dossier pour un `UIDVALIDITY` donné.
    pub uid: u32,
    /// Le contenu qu'il désigne.
    pub message: MessageId,
    /// Les drapeaux de **cette copie**, tels que le serveur les donne.
    pub flags: MessageFlags,
    /// Le `MODSEQ` de cette copie, si le serveur parle `CONDSTORE`.
    pub modseq: Option<u64>,
}

/// Le rôle d'un dossier, quand on a pu le déduire de son nom ou de ses attributs IMAP.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FolderKind {
    /// Boîte de réception.
    Inbox,
    /// Messages envoyés.
    Sent,
    /// Brouillons.
    Drafts,
    /// Corbeille.
    Trash,
    /// Indésirables.
    Junk,
    /// Archive, ou `[Gmail]/Tous les messages`.
    Archive,
    /// Rôle inconnu : dossier créé par l'utilisateur, ou nom non reconnu.
    Other,
}

impl FolderKind {
    /// L'étiquette persistée dans `folders.kind`.
    ///
    /// Une chaîne et non un entier : un `SELECT` à la main sur le store reste lisible, et
    /// ajouter un rôle ne renumérote rien.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Inbox => "inbox",
            Self::Sent => "sent",
            Self::Drafts => "drafts",
            Self::Trash => "trash",
            Self::Junk => "junk",
            Self::Archive => "archive",
            Self::Other => "other",
        }
    }

    /// Devine le rôle d'un dossier à partir de son nom.
    ///
    /// ## Pourquoi c'est ici et pas dans `mailimport`
    ///
    /// Deux portes d'entrée mènent au store — un mbox Thunderbird et un serveur IMAP — et
    /// elles doivent ranger un dossier **pareil**. Deux heuristiques divergentes feraient de la
    /// `Corbeille` d'un import et de la `Corbeille` d'une synchronisation deux rôles
    /// différents pour le même dossier.
    ///
    /// Pour IMAP, ce n'est que le **repli** : les attributs `SPECIAL-USE` de la RFC 6154
    /// (`\Sent`, `\Trash`…) sont ce que le serveur affirme, et ils gagnent. Le nom ne sert que
    /// quand le serveur ne dit rien.
    ///
    /// Comparaison sur le **dernier segment** du chemin, insensible à la casse. Les noms
    /// français et Gmail sont dedans : un profil réel a `[Gmail]/Tous les messages`,
    /// `Éléments envoyés` et `Corbeille`, pas `All Mail`, `Sent` et `Trash`.
    ///
    /// En cas de doute, [`FolderKind::Other`]. Se tromper sur un rôle affiche une mauvaise
    /// icône ; l'inventer trie du courrier dans le mauvais dossier.
    #[must_use]
    pub fn guess(folder: &str) -> Self {
        let last = folder.rsplit('/').next().unwrap_or(folder);
        let name = last.trim().to_lowercase();

        match name.as_str() {
            "inbox" | "courrier entrant" | "boîte de réception" | "boite de reception" => {
                Self::Inbox
            }
            "sent"
            | "sent messages"
            | "sent items"
            | "envoyés"
            | "envoyes"
            | "messages envoyés"
            | "messages envoyes"
            | "éléments envoyés"
            | "elements envoyes" => Self::Sent,
            "drafts" | "brouillons" => Self::Drafts,
            "trash" | "deleted items" | "corbeille" => Self::Trash,
            "junk"
            | "spam"
            | "bulk mail"
            | "indésirables"
            | "indesirables"
            | "courrier indésirable"
            | "courrier indesirable" => Self::Junk,
            "archive" | "archives" | "all mail" | "tous les messages" => Self::Archive,
            _ => Self::Other,
        }
    }

    /// Le rôle qu'un attribut `LIST` de la RFC 6154 désigne, s'il en désigne un.
    ///
    /// ## Pourquoi l'attribut gagne sur le nom
    ///
    /// Le nom est une devinette ; l'attribut est une **déclaration du serveur**. Un utilisateur
    /// qui a renommé sa corbeille `Poubelle` a un dossier que l'heuristique ne reconnaît pas et
    /// que `\Trash` désigne sans ambiguïté.
    ///
    /// `\All` et `\Archive` rendent tous les deux [`FolderKind::Archive`] : le premier est le
    /// `Tous les messages` de Gmail, le second l'archive classique, et l'interface les traite
    /// de la même façon. `\Flagged` et `\Important` n'ont pas de rôle ici — ce sont des vues,
    /// pas des dossiers de rangement.
    #[must_use]
    pub fn from_special_use(attribute: &str) -> Option<Self> {
        match attribute.trim().to_ascii_lowercase().as_str() {
            "\\inbox" => Some(Self::Inbox),
            "\\sent" => Some(Self::Sent),
            "\\drafts" => Some(Self::Drafts),
            "\\trash" => Some(Self::Trash),
            "\\junk" => Some(Self::Junk),
            "\\all" | "\\archive" => Some(Self::Archive),
            _ => None,
        }
    }

    /// Relit une étiquette. Une valeur inconnue rend [`FolderKind::Other`] plutôt qu'une
    /// erreur : un store écrit par une version plus récente doit rester lisible.
    #[must_use]
    pub fn from_str_lossy(value: &str) -> Self {
        match value {
            "inbox" => Self::Inbox,
            "sent" => Self::Sent,
            "drafts" => Self::Drafts,
            "trash" => Self::Trash,
            "junk" => Self::Junk,
            "archive" => Self::Archive,
            _ => Self::Other,
        }
    }
}
/// Les drapeaux d'un message, au sens IMAP.
///
/// Portés par la référence ([`MessageRef`]) et non par le message : le même contenu peut
/// être lu dans `INBOX` et non lu dans `[Gmail]/Tous les messages`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MessageFlags(u32);

impl MessageFlags {
    /// Lu.
    pub const SEEN: Self = Self(1 << 0);
    /// Marqué.
    pub const FLAGGED: Self = Self(1 << 1);
    /// Répondu.
    pub const ANSWERED: Self = Self(1 << 2);
    /// Brouillon.
    pub const DRAFT: Self = Self(1 << 3);
    /// Supprimé côté serveur, pas encore purgé.
    pub const DELETED: Self = Self(1 << 4);

    /// Le masque des drapeaux connus de cette version.
    const KNOWN: u32 = 0b1_1111;

    /// Aucun drapeau.
    #[must_use]
    pub const fn empty() -> Self {
        Self(0)
    }

    /// La représentation entière, pour la persistance.
    #[must_use]
    pub const fn bits(self) -> u32 {
        self.0
    }

    /// Reconstruit depuis la représentation entière, en ignorant les bits inconnus.
    #[must_use]
    pub const fn from_bits_truncate(bits: u32) -> Self {
        Self(bits & Self::KNOWN)
    }

    /// Vrai si tous les drapeaux de `other` sont posés.
    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// Pose les drapeaux de `other`.
    #[must_use]
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// Retire les drapeaux de `other`.
    #[must_use]
    pub const fn difference(self, other: Self) -> Self {
        Self(self.0 & !other.0)
    }

    /// Ne garde que les drapeaux communs.
    #[must_use]
    pub const fn intersection(self, other: Self) -> Self {
        Self(self.0 & other.0)
    }

    /// Les drapeaux qu'**une seule copie suffit à poser**.
    ///
    /// Un dossier IMAP peut contenir deux copies du même contenu, sous deux UID — voir la
    /// migration v2. Quand elles ne s'accordent pas, il faut une règle, et elle n'est pas la
    /// même pour tous les drapeaux.
    ///
    /// Pour ceux-là, c'est un OU : le contenu **a été** lu, marqué, répondu. Afficher
    /// « non lu » parce qu'un doublon invisible porte un drapeau différent serait afficher
    /// une information fausse.
    pub const ANY_COPY: &'static [Self] = &[Self::SEEN, Self::FLAGGED, Self::ANSWERED];

    /// Les drapeaux qu'il faut sur **toutes les copies**.
    ///
    /// `\Deleted` veut dire « marqué pour la purge », pas « parti ». Tant qu'une copie n'est
    /// pas marquée, le contenu reste dans le dossier : un OU ferait disparaître de la liste
    /// un message que le serveur a toujours. `\Draft` suit la même logique — une copie qui
    /// n'est pas un brouillon fait que le contenu existe ailleurs qu'en brouillon.
    pub const ALL_COPIES: &'static [Self] = &[Self::DRAFT, Self::DELETED];

    /// Réduit les drapeaux de plusieurs copies d'un même contenu à ceux de sa référence.
    ///
    /// Une liste vide rend [`MessageFlags::empty`] : pas de copie, pas de drapeau. C'est
    /// aussi la seule réponse possible, et elle vaut mieux qu'un panic sur un cas que la
    /// synchronisation rencontrera le jour où un dossier se vide.
    #[must_use]
    pub fn reduce(copies: &[Self]) -> Self {
        let Some((first, rest)) = copies.split_first() else {
            return Self::empty();
        };
        let mut any = *first;
        let mut all = *first;
        for copy in rest {
            any = any.union(*copy);
            all = all.intersection(*copy);
        }

        let mut out = Self::empty();
        for flag in Self::ANY_COPY {
            out = out.union(any.intersection(*flag));
        }
        for flag in Self::ALL_COPIES {
            out = out.union(all.intersection(*flag));
        }
        out
    }
}

/// Un message, tel que l'index de métadonnées le connaît.
///
/// Le corps n'est pas là : il se relit depuis le blob désigné par [`Message::blob`].
/// Cette structure doit rester assez petite pour qu'en paginer 100 000 reste indolore.
#[derive(Debug, Clone)]
pub struct Message {
    /// Identifiant interne.
    pub id: MessageId,
    /// L'identité du contenu — la clé du blob.
    pub blob: BlobHash,
    /// L'en-tête `Message-ID`, s'il était présent et lisible.
    pub rfc822_id: Option<Rfc822MessageId>,
    /// Le fil auquel il appartient, si la passe de threading est déjà passée dessus.
    pub thread: Option<ThreadId>,
    /// Date de l'en-tête `Date`, en secondes Unix, ou date de réception à défaut.
    pub date: i64,
    /// Adresse de l'expéditeur, normalisée en minuscules.
    pub from_addr: String,
    /// Nom affiché de l'expéditeur, décodé.
    pub from_name: Option<String>,
    /// Sujet, décodé.
    pub subject: String,
    /// Taille du message RFC 5322 brut, en octets.
    pub size: u64,
    /// Vrai si le MIME déclare au moins une pièce jointe.
    pub has_attachments: bool,
}

/// La référence d'un message dans un dossier. **La table qui tue la duplication.**
///
/// Un message dans `INBOX` et dans `[Gmail]/Tous les messages` = un [`Message`],
/// deux `MessageRef`.
#[derive(Debug, Clone, Copy)]
pub struct MessageRef {
    /// Le message référencé.
    pub message: MessageId,
    /// Le dossier qui le référence.
    pub folder: FolderId,
    /// Les drapeaux, propres à cette référence.
    pub flags: MessageFlags,
}

/// Un dossier ou un label.
#[derive(Debug, Clone)]
pub struct Folder {
    /// Identifiant interne.
    pub id: FolderId,
    /// Le compte propriétaire.
    pub account: AccountId,
    /// Chemin complet, séparé par `/`, tel que présenté à l'utilisateur.
    pub path: String,
    /// Rôle déduit.
    pub kind: FolderKind,
}

/// Un fil de discussion, reconstruit par `jwz`.
#[derive(Debug, Clone)]
pub struct Thread {
    /// Identifiant interne.
    pub id: ThreadId,
    /// Le message le plus ancien du fil.
    pub root: MessageId,
    /// Sujet normalisé (`Re:`, `RE:`, `TR:`, `Fwd:` retirés), pour le repli de threading.
    pub subject_norm: String,
    /// Date du message le plus récent, pour trier les fils.
    pub last_date: i64,
    /// Nombre de messages.
    pub message_count: u32,
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn hash_roundtrips_through_hex() {
        let h = BlobHash::of(b"From: a@b\r\nSubject: x\r\n\r\ncorps\r\n");
        assert_eq!(h.to_hex().len(), 64);
        assert_eq!(BlobHash::from_hex(&h.to_hex()).unwrap(), h);
    }

    #[test]
    fn identical_bytes_give_identical_identity() {
        // Le pari du projet, en une assertion.
        assert_eq!(BlobHash::of(b"meme contenu"), BlobHash::of(b"meme contenu"));
        assert_ne!(BlobHash::of(b"contenu a"), BlobHash::of(b"contenu b"));
    }

    #[test]
    fn shard_path_splits_on_two_bytes() {
        let h = BlobHash::of(b"x");
        let hex = h.to_hex();
        let path = h.shard_path();
        let mut components = path.components();
        assert_eq!(components.next().map(|c| c.as_str()), Some(&hex[0..2]));
        assert_eq!(components.next().map(|c| c.as_str()), Some(&hex[2..4]));
        assert_eq!(components.next().map(|c| c.as_str()), Some(hex.as_str()));
        assert!(components.next().is_none());
    }

    #[test]
    fn invalid_hex_is_rejected_not_panicked() {
        assert!(BlobHash::from_hex("pas du hex").is_err());
        assert!(BlobHash::from_hex("").is_err());
        assert!(BlobHash::from_hex(&"a".repeat(63)).is_err());
    }

    #[test]
    fn flags_are_per_reference_and_composable() {
        let f = MessageFlags::empty().union(MessageFlags::SEEN);
        assert!(f.contains(MessageFlags::SEEN));
        assert!(!f.contains(MessageFlags::FLAGGED));
        assert_eq!(MessageFlags::from_bits_truncate(f.bits()), f);
    }

    #[test]
    fn unknown_flag_bits_are_dropped_not_kept() {
        assert_eq!(MessageFlags::from_bits_truncate(u32::MAX).bits(), 0b1_1111);
    }
}

/// Identifiant d'une ligne de la file d'envoi.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OutboxId(pub i64);

/// Où en est un message de la file d'envoi.
///
/// ## Deux valeurs font tout le travail, et c'est la frontière entre elles
///
/// [`SendState::Sending`] veut dire « l'enveloppe est ouverte, **aucun octet du corps n'est
/// parti** ». [`SendState::Committing`] veut dire « le point final est peut-être passé ». La
/// première se reprend sans risque ; la seconde ne se reprend pas du tout.
///
/// Ce qui rend la distinction vraie n'est pas la définition mais l'**ordre d'écriture** :
/// `Committing` est écrit dans le store et rendu durable **avant** que le point final ne
/// parte. Un processus tué en `Sending` n'avait donc rien commis, et le serveur abandonne sa
/// transaction vide à son propre délai.
///
/// Le sens de l'erreur est choisi. La frontière peut créer un **faux doute** — écrite, puis le
/// processus meurt avant le premier octet du corps — jamais un faux « rien n'est parti ». Un
/// faux doute coûte une décision à l'utilisateur ; l'inverse coûte un doublon chez le
/// destinataire, et personne ne peut le retirer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendState {
    /// Accepté localement, rien n'est parti.
    Queued,
    /// L'enveloppe est ouverte, le corps n'a pas commencé.
    Sending,
    /// Le point final est peut-être passé. **Aucune reprise automatique.**
    Committing,
    /// Le serveur a répondu qu'il prenait le message.
    Sent,
    /// Refus définitif, **avant** le corps. Rendu à l'utilisateur.
    Failed,
}

impl SendState {
    /// L'étiquette persistée dans `outbox.state`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Sending => "sending",
            Self::Committing => "committing",
            Self::Sent => "sent",
            Self::Failed => "failed",
        }
    }

    /// Relit une étiquette.
    ///
    /// # Errors
    ///
    /// [`Error::UnknownSendState`] sur une valeur inconnue. **Pas de repli**, et ici c'est plus
    /// grave qu'ailleurs : un état illisible traité comme `Queued` renverrait un message
    /// peut-être déjà parti.
    pub fn parse(label: &str) -> crate::Result<Self> {
        match label {
            "queued" => Ok(Self::Queued),
            "sending" => Ok(Self::Sending),
            "committing" => Ok(Self::Committing),
            "sent" => Ok(Self::Sent),
            "failed" => Ok(Self::Failed),
            other => Err(crate::Error::UnknownSendState {
                found: other.to_owned(),
            }),
        }
    }

    /// Vrai si personne ne sait si le message est parti.
    ///
    /// **La seule question qui décide d'une reprise automatique.** Elle est vraie pour
    /// `Committing` et pour rien d'autre : c'est le seul état où le serveur a peut-être le
    /// message sans qu'on ait sa réponse.
    #[must_use]
    pub const fn is_doubtful(self) -> bool {
        matches!(self, Self::Committing)
    }

    /// Vrai si la boucle de remise peut prendre ce message.
    ///
    /// `Sending` en fait partie — c'est le rattrapage d'un processus tué avant le corps, et
    /// c'est correct par l'ordre d'écriture décrit sur cette énumération. `Committing` n'en
    /// fait pas partie, et c'est toute la règle du critère 2.
    #[must_use]
    pub const fn is_deliverable(self) -> bool {
        matches!(self, Self::Queued | Self::Sending)
    }
}

/// Un message qui attend de partir, ou qui a fini de partir.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outgoing {
    /// Identifiant de la ligne.
    pub id: OutboxId,
    /// Le compte qui l'envoie — il décide du serveur et de l'identifiant.
    pub account: AccountId,
    /// Les octets RFC 5322, dans le magasin de blobs.
    pub blob: BlobHash,
    /// La taille du message, en octets.
    ///
    /// Sert à annoncer `SIZE` **avant** le transfert. Zéro pour une ligne écrite avant la
    /// migration v7 : rien n'est alors annoncé, et le refus arrive tard comme avant.
    pub size: u64,
    /// L'expéditeur de l'enveloppe.
    pub sender: String,
    /// Les destinataires de l'**enveloppe**, copies cachées comprises.
    ///
    /// Pas reconstructible depuis les en-têtes : un `Bcc` est ici et dans aucun en-tête.
    pub recipients: Vec<String>,
    /// Où on en est.
    pub state: SendState,
    /// Combien de fois on a essayé. Sert au recul entre deux tentatives.
    pub attempts: u32,
    /// Quand l'utilisateur a demandé l'envoi.
    pub queued_at: i64,
    /// Quand la dernière tentative a eu lieu.
    pub tried_at: Option<i64>,
    /// Ce qu'il faut montrer à l'utilisateur — critère 8. **Jamais un secret.**
    pub last_error: Option<String>,
    /// Avant cet instant, ne pas réessayer.
    pub retry_after: Option<i64>,
    /// Vrai si renvoyer ce message **tel quel** a une chance de marcher — critère 8.
    ///
    /// ## Un bit, écrit par la couche d'envoi, que le store n'interprète pas
    ///
    /// Il accompagne [`Self::last_error`] : la phrase dit quoi faire, ce bit dit si l'interface
    /// a le droit de le faire **pour** l'utilisateur. Un « Renvoyer » sur une adresse qui
    /// n'existe pas renverrait à la même adresse ; sur un serveur qui était occupé, il marche.
    ///
    /// Le store ne le calcule pas et ne le lit pas : seule la couche qui a parlé au serveur sait
    /// pourquoi il a refusé. Voir `mailsmtp::Refusal::worth_retrying`, et la migration v10 pour
    /// la raison de ranger un booléen plutôt que la famille du refus.
    ///
    /// Faux sur tout ce qui n'est pas fini : une ligne encore en file n'a rien à renvoyer.
    pub resendable: bool,
}
