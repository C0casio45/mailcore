//! Écriture du magasin de vecteurs.
//!
//! Ajout seul, et c'est tout ce que la passe a besoin de faire : un message vectorisé est un
//! record de plus au bout du fichier. Rien ne se réécrit en place, donc une coupure n'abîme
//! jamais ce qui était déjà là — au pire elle laisse un record tronqué au bout, que la
//! réouverture retire.

use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};

use camino::{Utf8Path, Utf8PathBuf};

use crate::error::{Error, Result};
use crate::model::MessageId;
use crate::vectors::{HEADER_LEN, MAGIC, VERSION, record_len};

/// Le magasin ouvert en écriture.
///
/// Un seul écrivain à la fois : la passe de vectorisation est un job de fond unique, comme
/// l'indexation. Rien n'empêche un lecteur de travailler en parallèle — il lit un préfixe du
/// fichier, et un préfixe d'un fichier en ajout seul est toujours cohérent.
#[derive(Debug)]
pub struct VectorWriter {
    path: Utf8PathBuf,
    file: BufWriter<File>,
    dim: usize,
    /// Tampon d'un record, réutilisé : la passe en écrit des dizaines de milliers.
    record: Vec<u8>,
}

impl VectorWriter {
    /// Ouvre le magasin, et le crée avec cet en-tête s'il n'existe pas.
    ///
    /// ## Pourquoi une dimension qui ne correspond pas est un refus
    ///
    /// La dimension vient du modèle. Deux modèles différents donnent des vecteurs qui ne se
    /// comparent pas, même quand ils ont la même taille : ajouter les seconds derrière les
    /// premiers ferait un magasin dont chaque moitié est correcte et dont la comparaison ne
    /// veut rien dire. On refuse, et l'appelant décide — refaire une passe complète est la
    /// seule réponse juste, et c'est à lui de la lancer.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] si le fichier n'est pas ouvrable, [`Error::VectorFormat`] si son en-tête
    /// n'est pas celui d'un magasin de cette version ou de cette dimension.
    pub fn open(path: impl AsRef<Utf8Path>, dim: usize) -> Result<Self> {
        let path = path.as_ref().to_owned();
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|e| Error::Io {
                path: path.clone(),
                source: e,
            })?;

        let len = file
            .metadata()
            .map_err(|e| Error::Io {
                path: path.clone(),
                source: e,
            })?
            .len();

        if len == 0 {
            write_header(&mut file, &path, dim)?;
        } else {
            let found = read_header(&mut file, &path)?;
            if found != dim {
                return Err(Error::VectorFormat {
                    path: path.clone(),
                    reason: format!("dimension {found} sur disque, {dim} demandée"),
                });
            }
        }

        // Se placer au bout, et **sur une frontière de record**. Un processus tué au milieu
        // d'une écriture laisse une queue partielle : la garder ferait du record suivant un
        // mélange de deux, donc un vecteur faux qu'aucune relecture ne saurait distinguer d'un
        // vrai. La tronquer coûte un message à recalculer, ce que le drapeau fera de toute
        // façon puisqu'il n'a pas été posé.
        let complete = len.saturating_sub(HEADER_LEN) / record_len(dim);
        let end = HEADER_LEN + complete * record_len(dim);
        if end < len {
            tracing::warn!(path = %path, octets = len - end, "record partiel retiré du magasin");
            file.set_len(end).map_err(|e| Error::Io {
                path: path.clone(),
                source: e,
            })?;
        }
        file.seek(SeekFrom::Start(end)).map_err(|e| Error::Io {
            path: path.clone(),
            source: e,
        })?;

        Ok(Self {
            path,
            file: BufWriter::new(file),
            dim,
            record: Vec::with_capacity(record_len(dim) as usize),
        })
    }

    /// La dimension du magasin.
    #[must_use]
    pub fn dim(&self) -> usize {
        self.dim
    }

    /// Ajoute le vecteur d'un message.
    ///
    /// ## La normalisation est faite ici, jamais chez l'appelant
    ///
    /// Le balayage compare par cosinus, et un cosinus entre vecteurs unitaires **est** un
    /// produit scalaire. Normaliser à l'écriture retire donc une racine carrée par message et
    /// par comparaison — mais surtout, c'est une barrière par défaut : trois appelants
    /// vigilants valent moins qu'un point d'écriture qui garantit la propriété. Même
    /// raisonnement que les chevrons d'un `msg-id`, remis en forme au point d'écriture depuis
    /// le 2026-09-10.
    ///
    /// Un vecteur de norme nulle est écrit tel quel, en zéros. C'est ce que rend un message
    /// dont aucun token n'est connu du modèle, et diviser par zéro y mettrait des `NaN` qui
    /// contamineraient tout le classement — pas seulement ce message. Un vecteur nul marque
    /// zéro contre n'importe quelle requête : il ne remonte pas, et il n'empêche personne de
    /// remonter.
    ///
    /// # Errors
    ///
    /// [`Error::VectorDim`] si le vecteur n'a pas la dimension du magasin, [`Error::Io`] si
    /// l'écriture échoue.
    pub fn append(&mut self, id: MessageId, vector: &[f32]) -> Result<()> {
        if vector.len() != self.dim {
            return Err(Error::VectorDim {
                expected: self.dim,
                found: vector.len(),
            });
        }

        let norm = vector.iter().map(|x| x * x).sum::<f32>().sqrt();
        let scale = if norm > 0.0 { 1.0 / norm } else { 0.0 };

        self.record.clear();
        self.record.extend_from_slice(&id.0.to_le_bytes());
        for x in vector {
            self.record.extend_from_slice(&(x * scale).to_le_bytes());
        }
        self.file.write_all(&self.record).map_err(|e| Error::Io {
            path: self.path.clone(),
            source: e,
        })
    }

    /// Rend durables les records écrits jusqu'ici.
    ///
    /// ## Pourquoi c'est l'appelant qui décide quand
    ///
    /// Le drapeau « vectorisé » se pose **après** que le vecteur est durable — sinon une
    /// coupure laisse un message marqué fait dont le vecteur n'existe pas, et rien ne le
    /// recalculera jamais. L'ordre juste est donc : ajouter le lot, `sync`, puis marquer le
    /// lot. Un `sync` par message coûterait une synchronisation de disque par message ; le
    /// choix du grain appartient à la passe, qui est ce qui sait combien de travail elle
    /// accepte de refaire après une coupure.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] si le vidage du tampon ou la synchronisation échouent.
    pub fn sync(&mut self) -> Result<()> {
        self.file.flush().map_err(|e| Error::Io {
            path: self.path.clone(),
            source: e,
        })?;
        self.file.get_ref().sync_data().map_err(|e| Error::Io {
            path: self.path.clone(),
            source: e,
        })
    }
}

/// Écrit l'en-tête d'un magasin neuf.
fn write_header(file: &mut File, path: &Utf8Path, dim: usize) -> Result<()> {
    let declared = u32::try_from(dim).map_err(|_| Error::VectorFormat {
        path: path.to_owned(),
        reason: format!("dimension {dim} hors des 32 bits de l'en-tête"),
    })?;
    if declared == 0 {
        return Err(Error::VectorFormat {
            path: path.to_owned(),
            reason: "dimension nulle".to_owned(),
        });
    }

    let mut header = [0u8; HEADER_LEN as usize];
    header[..8].copy_from_slice(&MAGIC);
    header[8..12].copy_from_slice(&VERSION.to_le_bytes());
    header[12..16].copy_from_slice(&declared.to_le_bytes());

    file.write_all(&header).map_err(|e| Error::Io {
        path: path.to_owned(),
        source: e,
    })
}

/// Relit l'en-tête et rend la dimension qu'il déclare.
///
/// Laisse le curseur juste après l'en-tête.
pub(crate) fn read_header(file: &mut File, path: &Utf8Path) -> Result<usize> {
    file.seek(SeekFrom::Start(0)).map_err(|e| Error::Io {
        path: path.to_owned(),
        source: e,
    })?;

    let mut header = [0u8; HEADER_LEN as usize];
    file.read_exact(&mut header)
        .map_err(|_| Error::VectorFormat {
            path: path.to_owned(),
            reason: "fichier trop court pour porter un en-tête".to_owned(),
        })?;

    if header[..8] != MAGIC {
        return Err(Error::VectorFormat {
            path: path.to_owned(),
            reason: "ce n'est pas un magasin de vecteurs".to_owned(),
        });
    }

    // Le tableau fait seize octets et les tranches quatre : ces conversions ne peuvent pas
    // échouer. Le repli sur zéro évite un `expect` interdit par `CLAUDE.md`, et il tombe dans
    // les refus qui suivent — une version nulle et une dimension nulle sont l'une et l'autre
    // rejetées, donc l'invariant est tenu par le refus plutôt que par la panique.
    let version = u32::from_le_bytes(header[8..12].try_into().unwrap_or([0; 4]));
    if version != VERSION {
        return Err(Error::VectorFormat {
            path: path.to_owned(),
            reason: format!("version {version}, ce binaire lit la {VERSION}"),
        });
    }

    let dim = u32::from_le_bytes(header[12..16].try_into().unwrap_or([0; 4])) as usize;
    if dim == 0 {
        return Err(Error::VectorFormat {
            path: path.to_owned(),
            reason: "dimension nulle".to_owned(),
        });
    }

    Ok(dim)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::vectors::Vectors;

    /// Un magasin jetable, et son chemin.
    fn scratch() -> (tempfile::TempDir, Utf8PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(dir.path().join("vectors.bin")).unwrap();
        (dir, path)
    }

    #[test]
    fn what_is_written_comes_back_normalised() {
        let (_dir, path) = scratch();
        let mut writer = VectorWriter::open(&path, 3).unwrap();
        writer.append(MessageId(7), &[3.0, 4.0, 0.0]).unwrap();
        writer.sync().unwrap();

        let vectors = Vectors::load(&path, 3).unwrap();
        assert_eq!(vectors.len(), 1);
        // 3-4-5 : la normalisation est vérifiable de tête, ce qui est tout l'intérêt de ce
        // triplet-là.
        let hits = vectors.search(&[1.0, 0.0, 0.0], 1).unwrap();
        assert_eq!(hits[0].id, MessageId(7));
        assert!((hits[0].score - 0.6).abs() < 1e-6, "{}", hits[0].score);
    }

    #[test]
    fn writing_the_same_message_twice_leaves_one_vector() {
        // La propriété qui rend sûr l'ordre « vecteur durable, puis drapeau posé » : une passe
        // tuée entre les deux réécrit, et ne doit pas laisser un doublon qui remonterait deux
        // fois dans les résultats.
        let (_dir, path) = scratch();
        let mut writer = VectorWriter::open(&path, 2).unwrap();
        writer.append(MessageId(1), &[1.0, 0.0]).unwrap();
        writer.append(MessageId(1), &[0.0, 1.0]).unwrap();
        writer.sync().unwrap();

        let vectors = Vectors::load(&path, 2).unwrap();
        assert_eq!(vectors.len(), 1, "un seul message, donc un seul vecteur");

        let hits = vectors.search(&[0.0, 1.0], 5).unwrap();
        assert_eq!(hits.len(), 1);
        assert!(
            (hits[0].score - 1.0).abs() < 1e-6,
            "le dernier écrit gagne, pas le premier : {}",
            hits[0].score
        );
    }

    #[test]
    fn a_vector_appended_after_a_reopen_lands_behind_the_first() {
        // Le contrôle qui vérifie qu'on se replace bien au bout : ouvrir un magasin existant
        // et écrire dessus ne doit rien recouvrir.
        let (_dir, path) = scratch();
        VectorWriter::open(&path, 2)
            .unwrap()
            .append(MessageId(1), &[1.0, 0.0])
            .unwrap();

        let mut writer = VectorWriter::open(&path, 2).unwrap();
        writer.append(MessageId(2), &[0.0, 1.0]).unwrap();
        writer.sync().unwrap();

        assert_eq!(Vectors::load(&path, 2).unwrap().len(), 2);
    }

    #[test]
    fn a_record_cut_in_half_is_dropped_and_the_rest_survives() {
        // Ce que laisse un processus tué en pleine écriture. Le corpus déjà écrit doit
        // survivre, et la queue doit disparaître — pas l'inverse.
        let (_dir, path) = scratch();
        let mut writer = VectorWriter::open(&path, 4).unwrap();
        writer.append(MessageId(1), &[1.0, 0.0, 0.0, 0.0]).unwrap();
        writer.append(MessageId(2), &[0.0, 1.0, 0.0, 0.0]).unwrap();
        writer.sync().unwrap();
        drop(writer);

        let full = std::fs::metadata(&path).unwrap().len();
        let file = OpenOptions::new().write(true).open(&path).unwrap();
        file.set_len(full - 5).unwrap();
        drop(file);

        // La relecture ignore la queue.
        let vectors = Vectors::load(&path, 4).unwrap();
        assert_eq!(vectors.len(), 1);
        assert_eq!(vectors.search(&[1.0, 0.0, 0.0, 0.0], 5).unwrap().len(), 1);

        // La réouverture en écriture la retire, et ce qui s'ajoute ensuite est lisible.
        let mut writer = VectorWriter::open(&path, 4).unwrap();
        writer.append(MessageId(3), &[0.0, 0.0, 1.0, 0.0]).unwrap();
        writer.sync().unwrap();

        let vectors = Vectors::load(&path, 4).unwrap();
        assert_eq!(vectors.len(), 2);
        let hits = vectors.search(&[0.0, 0.0, 1.0, 0.0], 5).unwrap();
        assert_eq!(
            hits[0].id,
            MessageId(3),
            "le record ajouté après la coupure"
        );
    }

    #[test]
    fn a_store_of_another_dimension_is_refused_both_ways() {
        // Le cas réel : un modèle changé sans refaire la passe. Les deux portes refusent, parce
        // qu'un magasin à moitié d'un modèle et à moitié d'un autre n'a aucun symptôme.
        let (_dir, path) = scratch();
        VectorWriter::open(&path, 8).unwrap().sync().unwrap();

        assert!(matches!(
            VectorWriter::open(&path, 16),
            Err(Error::VectorFormat { .. })
        ));
        assert!(matches!(
            Vectors::load(&path, 16),
            Err(Error::VectorFormat { .. })
        ));
    }

    #[test]
    fn a_vector_of_the_wrong_dimension_is_refused_at_the_door() {
        let (_dir, path) = scratch();
        let mut writer = VectorWriter::open(&path, 3).unwrap();
        assert!(matches!(
            writer.append(MessageId(1), &[1.0, 0.0]),
            Err(Error::VectorDim {
                expected: 3,
                found: 2
            })
        ));
    }

    #[test]
    fn what_is_not_a_vector_store_is_not_read_as_one() {
        let (_dir, path) = scratch();
        std::fs::write(&path, b"ceci n'est pas un magasin de vecteurs").unwrap();

        assert!(matches!(
            Vectors::load(&path, 4),
            Err(Error::VectorFormat { .. })
        ));
        assert!(matches!(
            VectorWriter::open(&path, 4),
            Err(Error::VectorFormat { .. })
        ));
    }

    #[test]
    fn a_store_of_a_future_version_is_refused_rather_than_read() {
        // Le contrôle inverse du précédent : la magie est bonne, donc rien ne signalerait le
        // problème si la version n'était pas regardée.
        let (_dir, path) = scratch();
        let mut header = [0u8; HEADER_LEN as usize];
        header[..8].copy_from_slice(&MAGIC);
        header[8..12].copy_from_slice(&(VERSION + 1).to_le_bytes());
        header[12..16].copy_from_slice(&4u32.to_le_bytes());
        std::fs::write(&path, header).unwrap();

        assert!(matches!(
            Vectors::load(&path, 4),
            Err(Error::VectorFormat { .. })
        ));
    }

    #[test]
    fn a_message_no_token_of_which_is_known_gives_zeros_and_not_nan() {
        // Un vecteur nul divisé par sa norme donnerait des `NaN`, qui ne contamineraient pas
        // que ce message : un `NaN` dans un classement rend toute comparaison fausse.
        let (_dir, path) = scratch();
        let mut writer = VectorWriter::open(&path, 3).unwrap();
        writer.append(MessageId(1), &[0.0, 0.0, 0.0]).unwrap();
        writer.append(MessageId(2), &[1.0, 0.0, 0.0]).unwrap();
        writer.sync().unwrap();

        let hits = Vectors::load(&path, 3)
            .unwrap()
            .search(&[1.0, 0.0, 0.0], 5)
            .unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].id, MessageId(2));
        assert!(hits.iter().all(|hit| hit.score.is_finite()));
        assert_eq!(hits[1].score, 0.0, "nul contre tout le monde, jamais NaN");
    }
}
