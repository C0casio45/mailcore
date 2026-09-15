//! Déduplication.
//!
//! Deux niveaux, à ne pas confondre :
//!
//! 1. **Par contenu**, au niveau du blob. Octets RFC 5322 identiques = même
//!    [`crate::BlobHash`] = un seul blob, N lignes `refs`. C'est ce qui fait disparaître
//!    les 3,3 Go de duplication Gmail décrits dans `docs/VISION.md`.
//! 2. **Logique**, au niveau de l'index. Même `Message-ID` mais en-têtes `Received`
//!    différents : deux blobs distincts, présentés comme un seul message. On ne perd
//!    jamais un octet reçu — la dédup logique est une vue, pas une suppression.
