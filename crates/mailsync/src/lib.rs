//! IMAP → store. **En lecture seule côté serveur.**
//!
//! ## Ce que ce crate ne fait pas, et c'est une décision
//!
//! Il ne supprime rien, ne déplace rien, ne crée aucune boîte. `docs/PHASE-2.md` :
//!
//! > Lire, oui ; écrire dans la boîte d'un fournisseur, non. Un bug de sync qui supprime du
//! > courrier chez Gmail n'est pas rattrapable, et cette phase est précisément celle où les
//! > bugs de sync se trouvent.
//!
//! La conséquence technique est [`Client::examine`] plutôt que `SELECT` : la boîte est ouverte
//! en lecture seule, ce qui rend l'écriture impossible au niveau du protocole et non au niveau
//! de notre discipline. Le seul droit d'écriture prévu — le drapeau `\Seen` — viendra plus
//! tard, avec sa préférence et son test.
//!
//! ## Ce que la moisson garantit
//!
//! **Idempotence.** Passer deux fois sur le même dossier n'écrit rien la seconde fois. Ce
//! n'est pas un mécanisme ajouté : l'adressage par contenu de `mailcore` fait qu'un blob déjà
//! présent n'est pas réécrit, et `remote_uids` a une clé primaire.
//!
//! **Aucune perte sur coupure.** L'écriture se fait par lots transactionnels. Une coupure au
//! milieu perd le lot en cours, jamais ceux d'avant, et la reprise repart de ce qui est validé
//! — pas du début.
//!
//! **Rien d'indexé par numéro de séquence.** Un numéro de séquence change dès qu'un message
//! est purgé. Seul l'UID est stable, et `mailfake` a une panne exprès pour qu'un client qui
//! l'oublierait échoue en test.
//!
//! ## Les deux chemins de moisson
//!
//! | Ce que le serveur offre | Comment on moissonne |
//! |---|---|
//! | `CONDSTORE` activé, `HIGHESTMODSEQ` connu | `UID FETCH 1:* (FLAGS) (CHANGEDSINCE n)` — seuls les changements |
//! | rien de tout ça | `UID FETCH <derniers>:*` pour le neuf, plus une comparaison des UID connus |
//!
//! Le deuxième chemin **doit être exercé en test**, et c'est pour ça que `mailfake` sait
//! annoncer `CONDSTORE` puis le refuser : un repli qu'on n'exécute jamais n'existe pas, et
//! celui-là ne s'exécuterait qu'un jour de panne.

#![forbid(unsafe_code)]

pub mod client;
pub mod error;
pub mod headers;
pub mod mutf7;
pub mod sasl;
pub mod sync;
pub mod tls;

pub use client::{
    Client, Fetched, LINE_LIMIT, MESSAGE_LIMIT, MailboxStatus, Resynced, Selected, Timed,
};
pub use error::{Error, Result};
pub use sync::{
    AccountReport, Credential, Discovered, Enabled, Plan, Report, discover, enable, harvest,
    harvest_watched, sync_account, sync_account_over,
};
pub use tls::connect;
