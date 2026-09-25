//! Lecture du magasin, et balayage complet.
//!
//! Le magasin est relu **une fois** en mémoire, et chaque requête le balaye entièrement. Les
//! deux moitiés de cette phrase ont leur raison.
//!
//! **Relu une fois** : 72 Mio pour le corpus réel. Les relire à chaque frappe ferait payer une
//! lecture de disque par requête et rendrait le critère 2 inatteignable. C'est la même règle
//! que [`crate::index::Searcher`], qui se garde vivant pour la même raison.
//!
//! **Balayé entièrement** : `docs/PHASE-4.md` refuse un index approché tant qu'un balayage n'a
//! pas été montré insuffisant. Un produit scalaire par message, sur des vecteurs déjà
//! normalisés à l'écriture — voir [`crate::vectors::VectorWriter::append`].

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, Read};

use camino::{Utf8Path, Utf8PathBuf};

use crate::error::{Error, Result};
use crate::model::MessageId;
use crate::vectors::record_len;
use crate::vectors::store::read_header;

/// Un voisin trouvé par le balayage : un message et sa proximité.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Hit {
    /// Le message trouvé.
    pub id: MessageId,
    /// Le cosinus entre la requête et le message, dans `[-1, 1]`.
    ///
    /// Contrairement au BM25 de [`crate::index::Hit`], il est comparable **entre requêtes** :
    /// c'est un angle, pas une statistique de corpus. La fusion des deux listes s'appuiera sur
    /// cette différence plutôt que de l'ignorer — voir l'étape 6 de `docs/PHASE-4.md`.
    pub score: f32,
}

/// Le magasin de vecteurs, en mémoire.
///
/// À garder vivant entre deux requêtes, comme [`crate::index::Searcher`].
#[derive(Debug)]
pub struct Vectors {
    dim: usize,
    ids: Vec<MessageId>,
    /// Les vecteurs bout à bout, `ids.len() × dim`. Contigus, parce que c'est ce qui permet au
    /// processeur de lire en avant pendant le balayage.
    data: Vec<f32>,
    /// Ce que l'en-tête déclare de son auteur. `None` pour un magasin de version 1.
    provenance: Option<crate::vectors::Provenance>,
}

impl Vectors {
    /// Relit le magasin en mémoire.
    ///
    /// ## Le dernier record d'un message gagne
    ///
    /// Le fichier est en ajout seul : un message vectorisé deux fois y apparaît deux fois. La
    /// relecture garde le dernier, ce qui fait de l'écriture une opération **idempotente** —
    /// et c'est précisément ce qui rend sûr l'ordre « vecteur durable, puis drapeau posé ». Une
    /// passe tuée entre les deux recalcule le message et le réécrit ; sans cette règle, elle
    /// laisserait un doublon qui remonterait deux fois dans les résultats.
    ///
    /// Un magasin absent n'est **pas** une erreur : c'est l'état normal avant la première
    /// passe, et la recherche plein texte doit continuer de répondre — critère 7. Il rend un
    /// magasin vide, de la dimension demandée.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] si le fichier existe mais n'est pas lisible, [`Error::VectorFormat`] si
    /// son en-tête n'est pas celui d'un magasin de cette version.
    pub fn load(path: impl AsRef<Utf8Path>, dim: usize) -> Result<Self> {
        let path: Utf8PathBuf = path.as_ref().to_owned();
        let mut file = match File::open(&path) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::empty(dim));
            }
            Err(e) => {
                return Err(Error::Io { path, source: e });
            }
        };
        // Vide : ce que laisse une passe tuée avant d'avoir écrit l'en-tête. L'écrivain le traite
        // comme neuf ; la lecture fait de même — voir `summary_of`.
        if file
            .metadata()
            .map_err(|e| Error::Io {
                path: path.clone(),
                source: e,
            })?
            .len()
            == 0
        {
            return Ok(Self::empty(dim));
        }

        let found = read_header(&mut file, &path)?;
        if found.dim != dim {
            return Err(Error::VectorFormat {
                path,
                reason: format!("dimension {} sur disque, {dim} demandée", found.dim),
            });
        }
        let provenance = found.provenance;

        let mut reader = BufReader::new(file);
        let mut record = vec![0u8; record_len(dim) as usize];
        let mut ids = Vec::new();
        let mut data: Vec<f32> = Vec::new();
        let mut rows: HashMap<i64, usize> = HashMap::new();

        loop {
            match read_record(&mut reader, &mut record) {
                // Une queue partielle est ce que laisse un processus tué en pleine écriture.
                // Elle s'ignore ici, et la prochaine ouverture en écriture la retire.
                RecordRead::Partial | RecordRead::End => break,
                RecordRead::Failed(e) => {
                    return Err(Error::Io {
                        path: path.clone(),
                        source: e,
                    });
                }
                RecordRead::Ok => {}
            }

            let id = i64::from_le_bytes(record[..8].try_into().unwrap_or([0; 8]));
            let row = match rows.get(&id) {
                Some(&row) => row,
                None => {
                    let row = ids.len();
                    ids.push(MessageId(id));
                    data.resize((row + 1) * dim, 0.0);
                    rows.insert(id, row);
                    row
                }
            };

            let slot = &mut data[row * dim..(row + 1) * dim];
            for (value, bytes) in slot.iter_mut().zip(record[8..].chunks_exact(4)) {
                *value = f32::from_le_bytes(bytes.try_into().unwrap_or([0; 4]));
            }
        }

        tracing::debug!(path = %path, vecteurs = ids.len(), dim, "magasin de vecteurs relu");
        Ok(Self {
            dim,
            ids,
            data,
            provenance,
        })
    }

    /// Un magasin vide de cette dimension.
    #[must_use]
    pub fn empty(dim: usize) -> Self {
        Self {
            dim,
            ids: Vec::new(),
            data: Vec::new(),
            provenance: None,
        }
    }

    /// Ce qui a écrit ce magasin, si le format le dit.
    ///
    /// `None` a deux causes qu'il ne distingue pas — un magasin absent, et un magasin écrit
    /// avant que le format porte sa provenance. Les deux appellent la même phrase dans un
    /// relevé : on ne sait pas d'où viennent ces nombres.
    #[must_use]
    pub const fn provenance(&self) -> Option<&crate::vectors::Provenance> {
        self.provenance.as_ref()
    }

    /// La dimension des vecteurs.
    #[must_use]
    pub fn dim(&self) -> usize {
        self.dim
    }

    /// Le vecteur rangé d'un message, normalisé, s'il y en a un.
    ///
    /// **Un parcours**, pas une table : la recherche balaie tout de toute façon, et garder une
    /// table d'identifiants pour les seuls outils de vérification coûterait sa taille à chaque
    /// processus qui cherche.
    #[must_use]
    pub fn get(&self, id: MessageId) -> Option<&[f32]> {
        let row = self.ids.iter().position(|found| *found == id)?;
        self.data.get(row * self.dim..(row + 1) * self.dim)
    }

    /// Combien de messages le magasin porte.
    #[must_use]
    pub fn len(&self) -> usize {
        self.ids.len()
    }

    /// Vrai si le magasin ne porte aucun vecteur.
    ///
    /// C'est la moitié du contrôle du critère 9 : un magasin vide alors que des drapeaux disent
    /// « vectorisé » est une divergence entre deux magasins, pas un corpus vide.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    /// Les `limit` messages les plus proches de cette requête, du plus proche au plus lointain.
    ///
    /// La requête est normalisée ici — les vecteurs du magasin l'ont été à l'écriture — donc le
    /// cosinus se réduit à un produit scalaire.
    ///
    /// # Errors
    ///
    /// [`Error::VectorDim`] si la requête n'a pas la dimension du magasin. C'est le cas d'un
    /// modèle changé sans refaire la passe, et le refuser vaut mieux que comparer des vecteurs
    /// qui n'ont rien à voir.
    pub fn search(&self, query: &[f32], limit: usize) -> Result<Vec<Hit>> {
        if query.len() != self.dim {
            return Err(Error::VectorDim {
                expected: self.dim,
                found: query.len(),
            });
        }
        if limit == 0 || self.ids.is_empty() {
            return Ok(Vec::new());
        }

        let norm = query.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm == 0.0 {
            // Une requête dont aucun token n'est connu du modèle. Rendre la liste des messages
            // les mieux classés par un score nul reviendrait à rendre les `limit` premiers du
            // fichier, c'est-à-dire un classement par ordre d'arrivée présenté comme une
            // pertinence.
            return Ok(Vec::new());
        }
        let unit: Vec<f32> = query.iter().map(|x| x / norm).collect();

        // Un tas borné plutôt qu'un tri complet : le corpus fait des dizaines de milliers de
        // messages, la liste dix. Trier 73 658 scores pour en garder dix coûterait le
        // logarithme de tout le corpus à chaque frappe.
        let mut best: Vec<Hit> = Vec::with_capacity(limit + 1);
        for (row, &id) in self.ids.iter().enumerate() {
            let score = dot(&unit, &self.data[row * self.dim..(row + 1) * self.dim]);

            // Le seuil est le pire des retenus, donc une comparaison suffit pour écarter
            // l'immense majorité des messages.
            if best.len() == limit
                && let Some(worst) = best.last()
                && score <= worst.score
            {
                continue;
            }

            let hit = Hit { id, score };
            let at = best.partition_point(|kept| kept.score.total_cmp(&score).is_ge());
            best.insert(at, hit);
            best.truncate(limit);
        }

        Ok(best)
    }
}

/// Le produit scalaire de deux vecteurs de même longueur.
///
/// ## Pourquoi il n'est pas déroulé à la main
///
/// L'addition flottante n'étant pas associative, un accumulateur unique impose à LLVM une
/// chaîne de dépendances sérielle : plusieurs accumulateurs indépendants lui rendraient le
/// droit de vectoriser. C'est la version qu'on écrirait par réflexe — et elle n'a pas été
/// écrite, parce que le balayage naïf tient le critère 2 avec de la marge (relevé du
/// 2026-09-17, voir `docs/PHASE-4.md`). Optimiser ici coûterait de la lisibilité et un test
/// d'équivalence pour un budget qu'on ne dépasse pas.
fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// Ce qu'une tentative de lecture d'un record a donné.
enum RecordRead {
    /// Un record complet est dans le tampon.
    Ok,
    /// Fin de fichier propre, sur une frontière de record.
    End,
    /// Le fichier s'arrête au milieu d'un record.
    Partial,
    /// La lecture elle-même a échoué.
    Failed(std::io::Error),
}

/// Lit un record, en distinguant la fin propre de la queue partielle.
fn read_record(reader: &mut impl Read, record: &mut [u8]) -> RecordRead {
    let mut filled = 0;
    while filled < record.len() {
        match reader.read(&mut record[filled..]) {
            Ok(0) => {
                return if filled == 0 {
                    RecordRead::End
                } else {
                    RecordRead::Partial
                };
            }
            Ok(n) => filled += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return RecordRead::Failed(e),
        }
    }
    RecordRead::Ok
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::vectors::VectorWriter;

    /// Un magasin jetable rempli de ces vecteurs.
    fn store(dim: usize, rows: &[(i64, Vec<f32>)]) -> (tempfile::TempDir, Vectors) {
        let dir = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(dir.path().join("vectors.bin")).unwrap();
        let provenance = crate::vectors::Provenance {
            model: "jouet".to_owned(),
            family: "jouet".to_owned(),
            digest: "jouet___________".to_owned(),
            budget: 0,
            revision: 1,
            written: "2026-09-22".to_owned(),
        };
        let mut writer = VectorWriter::open(&path, dim, &provenance).unwrap();
        for (id, vector) in rows {
            writer.append(MessageId(*id), vector).unwrap();
        }
        writer.sync().unwrap();
        let vectors = Vectors::load(&path, dim).unwrap();
        (dir, vectors)
    }

    /// Un vecteur unitaire d'angle `angle` dans le plan.
    fn ray(angle: f32) -> Vec<f32> {
        vec![angle.cos(), angle.sin()]
    }

    #[test]
    fn the_returned_order_is_the_order_of_the_angles() {
        // La propriété que le module affirme, vérifiée plutôt qu'affirmée : le classement est
        // celui des cosinus décroissants. Les vecteurs sont écrits dans le désordre exprès —
        // un balayage qui rendrait l'ordre du fichier passerait un test mal construit.
        let rows: Vec<(i64, Vec<f32>)> = vec![
            (30, ray(0.9)),
            (10, ray(0.1)),
            (40, ray(1.4)),
            (20, ray(0.5)),
        ];
        let (_dir, vectors) = store(2, &rows);

        let hits = vectors.search(&ray(0.0), 4).unwrap();
        let order: Vec<i64> = hits.iter().map(|hit| hit.id.0).collect();
        assert_eq!(order, vec![10, 20, 30, 40]);

        for pair in hits.windows(2) {
            assert!(
                pair[0].score >= pair[1].score,
                "{} avant {}",
                pair[0].score,
                pair[1].score
            );
        }
    }

    #[test]
    fn the_limit_keeps_the_best_and_not_the_first_seen() {
        // Le contrôle négatif du tas borné : la cible est écrite **en dernier**, donc un tas
        // qui refuserait d'y toucher une fois plein rendrait les trois premiers du fichier.
        let rows: Vec<(i64, Vec<f32>)> =
            vec![(1, ray(1.5)), (2, ray(1.4)), (3, ray(1.3)), (4, ray(0.0))];
        let (_dir, vectors) = store(2, &rows);

        let hits = vectors.search(&ray(0.0), 2).unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].id, MessageId(4));
        assert_eq!(hits[1].id, MessageId(3));
    }

    #[test]
    fn an_opposite_vector_scores_below_an_orthogonal_one() {
        // Le cosinus est signé, et il doit le rester : ramener les scores dans `[0, 1]` ferait
        // d'un message qui parle du contraire un voisin aussi bon qu'un message sans rapport.
        let rows: Vec<(i64, Vec<f32>)> = vec![
            (1, vec![-1.0, 0.0]),
            (2, vec![0.0, 1.0]),
            (3, vec![1.0, 0.0]),
        ];
        let (_dir, vectors) = store(2, &rows);

        let hits = vectors.search(&[1.0, 0.0], 3).unwrap();
        assert_eq!(hits[0].id, MessageId(3));
        assert!((hits[1].score - 0.0).abs() < 1e-6);
        assert!(hits[2].score < 0.0, "{}", hits[2].score);
    }

    #[test]
    fn a_missing_store_is_empty_and_not_an_error() {
        // Critère 7 : sans modèle et sans passe, la recherche plein texte doit continuer de
        // répondre. Un magasin absent qui ferait une erreur ferait tomber l'écran entier.
        let dir = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(dir.path().join("jamais-ecrit.bin")).unwrap();

        let vectors = Vectors::load(&path, 256).unwrap();
        assert!(vectors.is_empty());
        assert_eq!(vectors.dim(), 256);
        assert!(vectors.search(&vec![1.0; 256], 10).unwrap().is_empty());
    }

    #[test]
    fn a_query_of_another_dimension_is_refused() {
        let (_dir, vectors) = store(2, &[(1, vec![1.0, 0.0])]);
        assert!(matches!(
            vectors.search(&[1.0, 0.0, 0.0], 1),
            Err(Error::VectorDim {
                expected: 2,
                found: 3
            })
        ));
    }

    #[test]
    fn a_query_that_says_nothing_returns_nothing() {
        // Une requête dont aucun token n'est connu du modèle donne un vecteur nul. Rendre les
        // dix premiers messages, tous à zéro, présenterait l'ordre du fichier comme une
        // pertinence — le pire des deux, parce que ça ressemble à un résultat.
        let (_dir, vectors) = store(2, &[(1, vec![1.0, 0.0]), (2, vec![0.0, 1.0])]);
        assert!(vectors.search(&[0.0, 0.0], 10).unwrap().is_empty());
    }

    #[test]
    fn asking_for_nothing_returns_nothing() {
        let (_dir, vectors) = store(2, &[(1, vec![1.0, 0.0])]);
        assert!(vectors.search(&[1.0, 0.0], 0).unwrap().is_empty());
    }

    #[test]
    fn a_limit_above_the_corpus_is_not_padded() {
        let (_dir, vectors) = store(2, &[(1, vec![1.0, 0.0]), (2, vec![0.0, 1.0])]);
        assert_eq!(vectors.search(&[1.0, 0.0], 50).unwrap().len(), 2);
    }
}
