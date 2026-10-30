//! Le type d'erreur de la bibliothèque.
//!
//! `thiserror` ici, `anyhow` seulement dans les binaires — voir `CLAUDE.md`.

/// Alias de commodité pour les résultats de `mailcore`.
pub type Result<T> = std::result::Result<T, Error>;

/// Toute erreur remontée par `mailcore`.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Échec d'entrée/sortie sur le store.
    #[error("erreur d'entrée/sortie sur {path}")]
    Io {
        /// Le chemin concerné, pour que le message soit exploitable sans backtrace.
        path: camino::Utf8PathBuf,
        /// La cause système.
        #[source]
        source: std::io::Error,
    },

    /// Échec au niveau de l'index de métadonnées.
    #[error("erreur SQLite : {0}")]
    Sqlite(#[from] rusqlite::Error),

    /// Échec au niveau de l'index plein texte.
    #[error("erreur tantivy")]
    Tantivy(#[from] tantivy::TantivyError),

    /// La requête de recherche est mal formée.
    ///
    /// Distinct de [`Error::Tantivy`] à dessein : c'est une erreur de l'utilisateur, pas une
    /// panne de l'index. L'appelant doit pouvoir afficher « votre requête est invalide »
    /// sans laisser croire que le moteur est cassé.
    #[error("requête invalide")]
    Query(#[from] tantivy::query::QueryParserError),
    /// Le blob demandé n'existe pas dans le store.
    #[error("blob absent : {0}")]
    BlobNotFound(crate::model::BlobHash),

    /// Une chaîne hexadécimale ne décrit pas un hash BLAKE3 valide.
    #[error("hash invalide : {0}")]
    InvalidHash(String),

    /// Le schéma sur disque est plus récent que celui que ce binaire sait lire.
    #[error("version de schéma non gérée : {found} (attendu au plus {supported})")]
    UnsupportedSchema {
        /// La version lue dans `user_version`.
        found: u32,
        /// La version la plus récente que ce binaire gère.
        supported: u32,
    },

    /// Le schéma sur disque est **plus ancien** que celui de ce binaire, et le store a été ouvert
    /// en lecture ([`crate::Store::open_reader`]) : un lecteur ne migre jamais.
    ///
    /// Une migration est irréversible, et c'est le premier geste d'un écrivain. Le serveur MCP
    /// lancé par un client la faisait pourtant à l'ouverture, comme le reste du démon : le
    /// 2026-10-30, il a migré du schéma 15 au schéma 17 le corpus de mesure, que personne n'avait
    /// décidé de migrer. Le message dit donc qui le peut.
    #[error(
        "le store est au schéma {found}, ce binaire lit le schéma {expected}, et une ouverture \
         en lecture ne migre jamais : ouvrez-le d'abord une fois avec l'application ou avec la \
         commande `mail`, qui le migrent, puis relancez"
    )]
    SchemaBehind {
        /// La version lue dans `user_version`.
        found: u32,
        /// La seule version que ce binaire sait lire.
        expected: u32,
    },

    /// Aucun store à l'endroit désigné : `index.sqlite` n'y est pas.
    ///
    /// Seule l'ouverture en lecture rend cette erreur — l'autre crée le store. Un lecteur pointé
    /// au mauvais endroit doit le dire, pas laisser derrière lui un store vide qu'on prendrait
    /// ensuite pour le bon.
    #[error(
        "aucun store sous {root} : index.sqlite est absent, et une ouverture en lecture n'en \
         crée pas"
    )]
    NoStore {
        /// La racine désignée.
        root: camino::Utf8PathBuf,
    },

    /// Une écriture de fichier demandée à un store ouvert en lecture : un blob, un verrou.
    ///
    /// SQLite refuse lui-même toute écriture sur une connexion de lecteur ; ce qui est à côté de
    /// SQLite — les blobs, les fichiers de verrou — est refusé ici, avant de toucher au disque.
    #[error("store ouvert en lecture : rien n'est écrit sous {path}")]
    ReadOnly {
        /// Ce qui aurait été écrit.
        path: camino::Utf8PathBuf,
    },

    /// SQLite ne peut ni ouvrir ni créer le journal d'une base en mode WAL, et un lecteur en a
    /// besoin.
    ///
    /// La documentation de SQLite (« Read-Only Databases », `wal.html`) : une base WAL ouverte en
    /// lecture se lit si ses fichiers `-wal` et `-shm` existent déjà, **ou** si le répertoire
    /// permet de les créer. Ni l'un ni l'autre — un support en lecture seule, un répertoire sans
    /// droit d'écriture — et la première lecture échoue sur « unable to open database file », qui
    /// ferait chercher la base elle-même.
    #[error(
        "le journal WAL de {path} ne peut être ni ouvert ni créé : une lecture a besoin des \
         fichiers -wal et -shm à côté de la base, ou du droit de les y créer"
    )]
    WalUnavailable {
        /// La base.
        path: camino::Utf8PathBuf,
        /// L'échec de SQLite.
        #[source]
        source: rusqlite::Error,
    },

    /// `accounts.auth` porte une valeur que ce binaire ne connaît pas.
    ///
    /// Refusé plutôt que replié : se tromper de mécanisme d'authentification, c'est envoyer
    /// un secret dans un champ qui ne l'attend pas.
    #[error("mécanisme d'authentification inconnu : {found}")]
    UnknownAuth {
        /// La valeur lue.
        found: String,
    },

    /// `accounts.security` porte une valeur que ce binaire ne connaît pas.
    ///
    /// Refusé plutôt que replié : un repli choisirait le mode de chiffrement à la place de
    /// l'utilisateur.
    #[error("mode de chiffrement inconnu : {found}")]
    UnknownSecurity {
        /// La valeur lue.
        found: String,
    },

    /// Une ligne du store porte une valeur d'une forme impossible.
    ///
    /// Le seul usage aujourd'hui est un `blob_hash` qui ne fait pas 32 octets dans la file
    /// d'envoi. Ailleurs, un hachage tronqué est complété de zéros : une lecture qui échoue
    /// s'affiche mal et le passage suivant corrige. Dans la file, elle désignerait le **mauvais
    /// message** à envoyer, et rien ne corrigerait après.
    #[error("colonne {what} d'une forme impossible dans le store")]
    CorruptRow {
        /// La colonne fautive.
        what: &'static str,
    },

    /// `outbox.state` porte une valeur que ce binaire ne connaît pas.
    ///
    /// Refusé, et c'est le refus le plus important des trois : un état illisible traité comme
    /// « à envoyer » renverrait un message peut-être déjà parti. Voir
    /// [`crate::model::SendState`].
    #[error("état d'envoi inconnu : {found}")]
    UnknownSendState {
        /// La valeur lue.
        found: String,
    },

    /// Un `MODSEQ` hors des 63 bits que la RFC 7162 lui accorde.
    ///
    /// La valeur est décrite comme « positive unsigned 63-bit » : elle tient donc dans un
    /// entier signé de 64 bits, qui est ce que SQLite sait stocker. Une valeur au-dessus vient
    /// d'un serveur hors spécification, et la tronquer ferait rater des changements pour
    /// toujours — la moisson incrémentale croirait avoir déjà vu ce qui arrive après.
    #[error("MODSEQ hors des 63 bits de la RFC 7162 : {found}")]
    ModseqOutOfRange {
        /// La valeur reçue.
        found: u64,
    },

    /// Un compte IMAP sans serveur, ou un compte mbox avec.
    ///
    /// Le schéma laisse les colonnes de serveur nullables — il le faut, un compte mbox n'en a
    /// pas. La cohérence entre `kind` et ces colonnes est donc tenue par le code, et cette
    /// erreur est ce qui empêche de la tenir en silence.
    #[error("compte {account} incohérent : {reason}")]
    InconsistentAccount {
        /// L'identifiant du compte fautif.
        account: i64,
        /// Ce qui manque, ou ce qui est en trop.
        reason: String,
    },

    /// Le magasin de vecteurs ne porte pas l'en-tête attendu.
    ///
    /// Refusé plutôt que deviné, et c'est le même raisonnement que [`Error::UnknownSecurity`]
    /// transposé : un magasin lu avec la mauvaise dimension ou le mauvais format rendrait des
    /// vecteurs de la bonne **taille** et du mauvais **sens**. La recherche marcherait, et ses
    /// résultats seraient du bruit qu'on mettrait sur le compte du modèle.
    #[error("magasin de vecteurs illisible ({path}) : {reason}")]
    VectorFormat {
        /// Le fichier fautif.
        path: camino::Utf8PathBuf,
        /// Ce qui ne va pas, en clair.
        reason: String,
    },

    /// Une autre passe écrit déjà ce magasin de vecteurs — `mail vectors`, le démon, un banc.
    ///
    /// Pas une panne : la passe qui reçoit ce refus réessaiera plus tard. Deux écrivains à la fois
    /// mêleraient leurs records, et l'un pourrait effacer le fichier sous l'autre.
    #[error("une autre passe écrit déjà les vecteurs de ce store ({path})")]
    VectorsBusy {
        /// Le verrou tenu.
        path: camino::Utf8PathBuf,
    },

    /// Un vecteur n'a pas la dimension du magasin.
    ///
    /// Le cas réel est un modèle changé sans refaire la passe complète. Comparer les deux ne
    /// planterait pas : il suffirait de s'arrêter au plus court. C'est exactement pour ça que
    /// c'est une erreur — un classement calculé sur deux modèles différents n'a aucun sens et
    /// n'a aucun symptôme.
    #[error("vecteur de dimension {found}, attendu {expected}")]
    VectorDim {
        /// La dimension du magasin.
        expected: usize,
        /// Celle du vecteur présenté.
        found: usize,
    },

    /// La mise en forme rangée d'une valeur a échoué à l'**écriture**.
    ///
    /// Le seul usage est la signature d'un compte, sérialisée avant d'aller dans sa colonne.
    /// Cette erreur n'est pas atteignable pour ce type — un document n'a ni flottant ni clé de
    /// dictionnaire non textuelle, les deux seules choses que `serde_json` refuse d'écrire —
    /// et elle existe quand même, parce que l'alternative est un `expect` que le `CLAUDE.md`
    /// interdit. Paniquer en enregistrant la signature de quelqu'un serait le pire des deux :
    /// une erreur rendue laisse la fenêtre ouverte et le texte à l'écran.
    ///
    /// Le sens inverse — une colonne illisible — n'est **pas** une erreur : voir
    /// [`crate::Store::signature`], qui rend « pas de signature » plutôt que d'empêcher
    /// d'écrire un message.
    #[error("valeur {what} impossible à mettre en forme pour le store")]
    Unserialisable {
        /// La colonne visée.
        what: &'static str,
    },
}

impl Error {
    /// Vrai si l'échec est une **contention passagère** sur l'index, et non un refus définitif.
    ///
    /// ## Pourquoi ça mérite une méthode
    ///
    /// `database is locked` veut dire « quelqu'un d'autre écrivait ». C'est l'erreur
    /// réessayable par excellence, et la seule façon de la distinguer d'un schéma cassé est de
    /// regarder le code d'erreur de SQLite — pas son message, qui est du texte anglais que la
    /// prochaine version peut reformuler.
    ///
    /// `busy_timeout` couvre déjà la contention ordinaire (voir `store::db`). Ce qui remonte
    /// jusqu'ici est donc une attente qui a dépassé cinq secondes — mesurée le 2026-09-09 avec
    /// cinq moissons concurrentes — et à qui il faut laisser une deuxième chance plutôt
    /// qu'abandonner le compte.
    #[must_use]
    pub fn is_locked(&self) -> bool {
        let Self::Sqlite(rusqlite::Error::SqliteFailure(failure, _)) = self else {
            return false;
        };
        matches!(
            failure.code,
            rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
        )
    }
}

#[cfg(test)]
mod tests {
    use super::Error;

    /// Fabrique l'échec SQLite d'un code donné, comme `rusqlite` le rendrait.
    fn failure(code: rusqlite::ErrorCode) -> Error {
        Error::Sqlite(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error {
                code,
                extended_code: 0,
            },
            Some("database is locked".to_owned()),
        ))
    }

    #[test]
    fn a_busy_or_locked_database_is_a_passing_contention() {
        assert!(failure(rusqlite::ErrorCode::DatabaseBusy).is_locked());
        assert!(failure(rusqlite::ErrorCode::DatabaseLocked).is_locked());
    }

    #[test]
    fn a_real_refusal_is_not_a_contention() {
        // Le cas qui compte : une contrainte violée ou une base corrompue ne se règlent pas en
        // réessayant, et les réessayer en boucle cacherait le défaut.
        assert!(!failure(rusqlite::ErrorCode::ConstraintViolation).is_locked());
        assert!(!failure(rusqlite::ErrorCode::DatabaseCorrupt).is_locked());
        assert!(!failure(rusqlite::ErrorCode::ReadOnly).is_locked());
    }

    #[test]
    fn what_is_not_an_sqlite_failure_is_not_a_contention() {
        assert!(!Error::Sqlite(rusqlite::Error::QueryReturnedNoRows).is_locked());
        assert!(
            !Error::ModseqOutOfRange { found: u64::MAX }.is_locked(),
            "seule une contention de l'index en est une"
        );
    }
}
