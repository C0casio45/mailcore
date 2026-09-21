//! Magasin de vecteurs et balayage.
//!
//! Un embedding par message, rangé dans un fichier plat à côté du store — étape 4 de
//! `docs/PHASE-4.md`. Ce module ne sait pas calculer un vecteur : il les range, les relit et
//! les compare. Le modèle est ailleurs, et c'est ce qui permet de mesurer la latence d'un
//! balayage **avant** d'avoir un modèle à défendre.
//!
//! ## Pourquoi un fichier plat et pas SQLite, ni un index approché
//!
//! La phase a tranché l'index approché : `docs/PHASE-4.md` refuse un HNSW tant qu'un balayage
//! complet n'a pas été montré insuffisant. Le corpus fait 73 658 messages et la dimension 256,
//! soit **72 Mio** — ça tient en mémoire, et un balayage est un produit scalaire par message.
//! Ajouter une structure approchée avant d'avoir le chiffre, c'est payer une complexité et une
//! classe de bogues pour un problème qu'on n'a pas.
//!
//! SQLite non plus : le balayage veut les octets **contigus**, pour que le processeur lise en
//! avant. Une colonne `BLOB` par message ferait 73 658 lectures de lignes à chaque requête, et
//! la comparaison n'a besoin d'aucune des garanties transactionnelles qu'on paierait.
//!
//! ## Le format, et ce qu'il rend possible
//!
//! ```text
//! en-tête  16 octets : "MAILVEC1" | version u32 | dimension u32
//! record   8 + 4×dim : identifiant i64 | dim × f32, tous en petit-boutiste
//! ```
//!
//! Il est **ajout seul**. Un message vectorisé deux fois écrit deux records, et la relecture
//! garde le dernier : c'est ce qui rend l'écriture idempotente, donc ce qui rend sûr l'ordre
//! « vecteur durable **puis** drapeau posé » — la leçon du 2026-09-11 sur l'index plein texte.
//! Une passe tuée entre les deux recalcule et réécrit, sans conséquence.
//!
//! Il est **reconstructible**, comme l'index tantivy : les blobs sont la source de vérité. Le
//! perdre coûte une passe, jamais une donnée.

mod scan;
mod store;

pub use scan::{Hit, Vectors};
pub use store::VectorWriter;

/// Ce qui identifie le format, en tête de fichier.
///
/// Huit octets lisibles dans un éditeur hexadécimal : un fichier de 72 Mio qu'on retrouve sans
/// savoir d'où il vient doit pouvoir se nommer lui-même.
pub(crate) const MAGIC: [u8; 8] = *b"MAILVEC1";

/// La version du format. Un fichier d'une autre version est refusé, jamais deviné.
pub(crate) const VERSION: u32 = 1;

/// La taille de l'en-tête : magie, version, dimension.
pub(crate) const HEADER_LEN: u64 = 16;

/// Les octets d'un record, pour une dimension donnée.
pub(crate) const fn record_len(dim: usize) -> u64 {
    8 + 4 * dim as u64
}
