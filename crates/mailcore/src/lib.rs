//! # mailcore
//!
//! Le coeur de mailcore : un store de messages adressés par leur contenu, un index de
//! métadonnées, un index plein texte, et une API de requête au-dessus des trois.
//!
//! Ce crate ne dépend d'aucun toolkit UI, d'aucun client réseau et d'aucun runtime
//! async. Il est synchrone : `rusqlite` et `tantivy` le sont, et les envelopper
//! n'ajouterait qu'une couche. Les appelants asynchrones (`maild`, `mailmcp`) passent
//! par `spawn_blocking`. Voir `docs/ARCHITECTURE.md`.

#![forbid(unsafe_code)]

pub mod contacts;
pub mod dedup;
pub mod error;
pub mod index;
pub mod model;
pub mod progress;
pub mod query;
pub mod source;
pub mod store;
pub mod thread;

pub use error::{Error, Result};
pub use model::{
    Account, AccountId, AccountKind, AuthKind, BlobHash, Folder, FolderId, FolderKind, Message,
    MessageFlags, MessageId, MessageRef, OutboxId, Outgoing, RemoteCopy, Rfc822MessageId, Security,
    SendState, Server, SyncState, Thread, ThreadId,
};
pub use progress::Progress;
pub use query::{
    Attachment, FolderSummary, Mailbox, MessageDetail, Rendered, SearchResult, attachment_bytes,
    calendar_parts, invitation_of,
};
pub use source::Source;
pub use store::Store;
pub use store::blobs::{BlobStore, PutOutcome};
pub use store::drafts::{Draft, DraftAttachment, DraftId};
pub use store::read::{Cursor, ListItem, StoreStats};
pub use store::write::{NewMessage, Writer};
