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
//! en-tête  20 octets : "MAILVEC1" | version u32 | dimension u32 | longueur de provenance u32
//!          puis N octets de provenance, en UTF-8, une ligne `clé=valeur` par fait
//! record   8 + 4×dim : identifiant i64 | dim × f32, tous en petit-boutiste
//! ```
//!
//! ## La provenance, et pourquoi elle est **dans** le fichier
//!
//! Ajoutée le 2026-09-22, après qu'un chiffre du journal de la phase 4 s'est révélé
//! irreproductible. Le 2026-09-21 avait enchaîné quatre passes avec des budgets de texte
//! différents ; chacune écrase le magasin ; le relevé qui a suivi a lu **celui qu'avait laissé
//! la dernière**, et personne n'a pu dire laquelle. Le chiffre était juste ou faux, on ne
//! saura pas : il n'est pas défendable, ce qui revient au même.
//!
//! La leçon est celle des cibles du jeu de requêtes, d'un cran plus loin : **un instrument dont
//! l'état n'est pas capturé rend des chiffres qu'on ne peut pas défendre.** D'où deux choix :
//!
//! - elle vit **dans l'en-tête**, pas dans un fichier à côté ni dans SQLite. Le projet a déjà
//!   payé une fois le prix de deux magasins qui se contredisent — les drapeaux d'indexation
//!   contre les documents tantivy, le 2026-09-11. Une provenance qui peut se désynchroniser de
//!   ce qu'elle décrit ne décrit rien ;
//! - elle est **opposable à l'écriture**. [`VectorWriter`] refuse d'ajouter à un magasin dont
//!   la provenance n'est pas la sienne, exactement comme il refuse déjà une autre dimension.
//!   Deux modèles de même dimension existent — c'est le cas des deux `potion` du corpus — et
//!   rien n'aurait signalé le mélange.
//!
//! Un magasin de version 1 se **lit** encore, et sa provenance est alors `None`. C'est
//! volontaire : refuser à la lecture obligerait à recalculer des heures de vecteurs pour lire
//! un fichier parfaitement valide. Mais une provenance absente est une information, pas un
//! détail — les bancs l'impriment comme telle.
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

/// Ce qui a écrit un magasin de vecteurs.
///
/// Elle répond à une seule question, celle que le 2026-09-22 n'a pas pu trancher :
/// **qu'est-ce qui a produit ces nombres ?** Chaque champ est là parce que le faire varier
/// change les vecteurs, donc change tout relevé lu dessus.
///
/// Elle ne porte **pas** de chemin de fichier. Un chemin est une propriété de la machine et non
/// du modèle ; deux machines qui rangent le même modèle ailleurs écriraient deux provenances
/// différentes pour un magasin identique, et l'égalité servirait alors à refuser ce qu'il
/// fallait accepter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Provenance {
    /// Le nom du modèle — le dernier segment de son répertoire, pas son chemin.
    pub model: String,
    /// Sa famille : un embedding statique et un transformeur ne se comparent pas.
    pub family: String,
    /// L'empreinte de ses poids, tronquée à seize caractères hexadécimaux.
    ///
    /// C'est le seul champ qui identifie le modèle **par son contenu** plutôt que par son
    /// étiquette, et c'est pour ça qu'il existe : un répertoire dont on remplace les poids
    /// garde son nom. Le projet désigne déjà ses messages et les cibles de son banc par leur
    /// contenu ; un modèle est ce qui mérite le plus ce traitement, puisqu'il s'exécute sur
    /// tout le corpus.
    pub digest: String,
    /// Le budget de texte donné à l'extraction, en caractères de corps.
    ///
    /// Le champ qui manquait le 2026-09-21 : le balayage à 2 000, 400, 150 et 0 a laissé quatre
    /// magasins indiscernables.
    pub budget: usize,
    /// Le jour où la passe a écrit ce magasin, en `AAAA-MM-JJ`.
    pub written: String,
}

impl Provenance {
    /// Sérialise en `clé=valeur`, une par ligne.
    ///
    /// Un format de trois lignes de code plutôt qu'un sérialiseur : le bloc doit rester lisible
    /// dans un éditeur hexadécimal, au même titre que la magie du fichier. Un magasin de
    /// 113 Mio qu'on retrouve sans savoir d'où il vient doit pouvoir se nommer lui-même — et
    /// maintenant se dater et se sourcer.
    #[must_use]
    pub fn encode(&self) -> String {
        // Les valeurs sont assainies : un retour à la ligne ou un `=` dans une valeur casserait
        // la relecture. Aucune des cinq n'en contient aujourd'hui, et c'est exactement pourquoi
        // la barrière est ici plutôt que chez l'appelant.
        let clean = |value: &str| {
            value
                .chars()
                .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
                .collect::<String>()
        };
        format!(
            "model={}\nfamily={}\ndigest={}\nbudget={}\nwritten={}\n",
            clean(&self.model),
            clean(&self.family),
            clean(&self.digest),
            self.budget,
            clean(&self.written),
        )
    }

    /// Relit ce qu'[`Self::encode`] a écrit.
    ///
    /// Une clé inconnue est **ignorée** et une clé absente rend un champ vide : un magasin
    /// écrit par une version ultérieure, qui aurait un champ de plus, reste lisible. C'est le
    /// contraire du choix fait pour la version du format — celle-ci se refuse — et la
    /// différence est que la version gouverne la **disposition des octets**, là où la
    /// provenance n'est qu'un commentaire opposable.
    #[must_use]
    pub fn decode(raw: &str) -> Self {
        let mut found = Self {
            model: String::new(),
            family: String::new(),
            digest: String::new(),
            budget: 0,
            written: String::new(),
        };
        for line in raw.lines() {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            match key {
                "model" => found.model = value.to_owned(),
                "family" => found.family = value.to_owned(),
                "digest" => found.digest = value.to_owned(),
                "budget" => found.budget = value.parse().unwrap_or(0),
                "written" => found.written = value.to_owned(),
                _ => {}
            }
        }
        found
    }

    /// Ce qui doit correspondre pour qu'on puisse ajouter à un magasin existant.
    ///
    /// **La date en est exclue**, et c'est tout l'intérêt de la méthode : une passe reprise le
    /// lendemain de celle qu'elle continue est la même passe, et refuser sur la date rendrait
    /// le critère 3 — « tuée au milieu, elle repart où elle en était » — impossible à tenir dès
    /// qu'une coupure passe minuit.
    #[must_use]
    pub fn comparable_to(&self, other: &Self) -> bool {
        self.model == other.model
            && self.family == other.family
            && self.digest == other.digest
            && self.budget == other.budget
    }

    /// Une ligne pour un relevé.
    #[must_use]
    pub fn summary(&self) -> String {
        format!(
            "{} ({}, {}) budget {} — écrit le {}",
            self.model, self.family, self.digest, self.budget, self.written
        )
    }
}

/// Ce qui identifie le format, en tête de fichier.
///
/// Huit octets lisibles dans un éditeur hexadécimal : un fichier de 72 Mio qu'on retrouve sans
/// savoir d'où il vient doit pouvoir se nommer lui-même.
pub(crate) const MAGIC: [u8; 8] = *b"MAILVEC1";

/// La version du format que ce binaire **écrit**.
///
/// La 2 ajoute le bloc de provenance. La 1 se lit encore — voir l'en-tête de module — et c'est
/// la seule tolérance : un fichier d'une version plus haute est refusé, jamais deviné.
pub(crate) const VERSION: u32 = 2;

/// La première version, sans provenance. Lue, jamais écrite.
pub(crate) const VERSION_WITHOUT_PROVENANCE: u32 = 1;

/// La partie fixe de l'en-tête de la version 1 : magie, version, dimension.
pub(crate) const HEADER_LEN_V1: u64 = 16;

/// La partie fixe de l'en-tête de la version 2 : la précédente, plus la longueur du bloc.
pub(crate) const HEADER_LEN_V2: u64 = 20;

/// Le plafond du bloc de provenance.
///
/// Il n'est pas là pour économiser des octets — cinq lignes en font deux cents — mais parce
/// qu'un `u32` lu dans un fichier abîmé demanderait sinon d'allouer quatre gibioctets avant de
/// découvrir que le fichier est trop court. C'est du parsing d'entrée hostile : le plafond est
/// vérifié **avant** l'allocation.
pub(crate) const PROVENANCE_MAX: u32 = 64 * 1024;

/// Les octets d'un record, pour une dimension donnée.
pub(crate) const fn record_len(dim: usize) -> u64 {
    8 + 4 * dim as u64
}
