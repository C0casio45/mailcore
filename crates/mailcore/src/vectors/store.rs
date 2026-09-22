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
use crate::vectors::{
    HEADER_LEN_V1, HEADER_LEN_V2, MAGIC, PROVENANCE_MAX, Provenance, VERSION,
    VERSION_WITHOUT_PROVENANCE, record_len,
};

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
    /// ## Et pourquoi la dimension ne suffisait pas
    ///
    /// C'est le refus ci-dessus qui a manqué le 2026-09-21 : le corpus porte **deux** modèles
    /// `potion` de dimension 256, et un balayage de budget a écrit quatre magasins que rien ne
    /// distinguait. La dimension ne les séparait pas, donc elle ne protégeait rien. La
    /// provenance est comparée au même endroit et de la même façon, et la date en est exclue
    /// pour qu'une passe reprise le lendemain reste la même passe.
    ///
    /// Un magasin de version 1, qui n'a pas de provenance, est **refusé en écriture** alors
    /// qu'il reste lisible : on ne peut pas vérifier qu'on le continue avec ce qui l'a commencé,
    /// et poursuivre sans le savoir est exactement ce qu'on cherche à empêcher.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] si le fichier n'est pas ouvrable, [`Error::VectorFormat`] si son en-tête
    /// n'est pas celui d'un magasin de cette version, de cette dimension ou de cette provenance.
    pub fn open(path: impl AsRef<Utf8Path>, dim: usize, provenance: &Provenance) -> Result<Self> {
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

        let header_len = if len == 0 {
            write_header(&mut file, &path, dim, provenance)?;
            HEADER_LEN_V2 + provenance.encode().len() as u64
        } else {
            let found = read_header(&mut file, &path)?;
            if found.dim != dim {
                return Err(Error::VectorFormat {
                    path: path.clone(),
                    reason: format!("dimension {} sur disque, {dim} demandée", found.dim),
                });
            }
            match &found.provenance {
                None => {
                    return Err(Error::VectorFormat {
                        path: path.clone(),
                        reason: "magasin sans provenance (format de version 1) : impossible de \
                                 vérifier qu'on le continue avec ce qui l'a écrit. Relancer la \
                                 passe avec --rebuild"
                            .to_owned(),
                    });
                }
                Some(disk) if !disk.comparable_to(provenance) => {
                    return Err(Error::VectorFormat {
                        path: path.clone(),
                        reason: format!(
                            "magasin écrit par {}, passe lancée avec {}. Deux modèles ne se \
                             comparent pas, même à dimension égale. Relancer avec --rebuild",
                            disk.summary(),
                            provenance.summary()
                        ),
                    });
                }
                Some(_) => {}
            }
            found.len
        };

        // Se placer au bout, et **sur une frontière de record**. Un processus tué au milieu
        // d'une écriture laisse une queue partielle : la garder ferait du record suivant un
        // mélange de deux, donc un vecteur faux qu'aucune relecture ne saurait distinguer d'un
        // vrai. La tronquer coûte un message à recalculer, ce que le drapeau fera de toute
        // façon puisqu'il n'a pas été posé.
        let complete = len.saturating_sub(header_len) / record_len(dim);
        let end = header_len + complete * record_len(dim);
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
fn write_header(
    file: &mut File,
    path: &Utf8Path,
    dim: usize,
    provenance: &Provenance,
) -> Result<()> {
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

    let block = provenance.encode();
    let block_len = u32::try_from(block.len()).unwrap_or(u32::MAX);
    if block_len > PROVENANCE_MAX {
        return Err(Error::VectorFormat {
            path: path.to_owned(),
            reason: format!("provenance de {block_len} octets, plafond {PROVENANCE_MAX}"),
        });
    }

    let mut header = [0u8; HEADER_LEN_V2 as usize];
    header[..8].copy_from_slice(&MAGIC);
    header[8..12].copy_from_slice(&VERSION.to_le_bytes());
    header[12..16].copy_from_slice(&declared.to_le_bytes());
    header[16..20].copy_from_slice(&block_len.to_le_bytes());

    file.write_all(&header).map_err(|e| Error::Io {
        path: path.to_owned(),
        source: e,
    })?;
    file.write_all(block.as_bytes()).map_err(|e| Error::Io {
        path: path.to_owned(),
        source: e,
    })
}

/// Ce qu'un en-tête déclare.
#[derive(Debug, Clone)]
pub(crate) struct Header {
    /// La dimension des vecteurs.
    pub(crate) dim: usize,
    /// Ce qui a écrit le magasin — `None` pour un magasin de version 1.
    pub(crate) provenance: Option<Provenance>,
    /// Où commencent les records.
    pub(crate) len: u64,
}

/// Relit l'en-tête.
///
/// Laisse le curseur juste après l'en-tête, donc au premier record.
pub(crate) fn read_header(file: &mut File, path: &Utf8Path) -> Result<Header> {
    file.seek(SeekFrom::Start(0)).map_err(|e| Error::Io {
        path: path.to_owned(),
        source: e,
    })?;

    let mut fixed = [0u8; HEADER_LEN_V1 as usize];
    file.read_exact(&mut fixed)
        .map_err(|_| Error::VectorFormat {
            path: path.to_owned(),
            reason: "fichier trop court pour porter un en-tête".to_owned(),
        })?;

    if fixed[..8] != MAGIC {
        return Err(Error::VectorFormat {
            path: path.to_owned(),
            reason: "ce n'est pas un magasin de vecteurs".to_owned(),
        });
    }

    // Le tableau fait seize octets et les tranches quatre : ces conversions ne peuvent pas
    // échouer. Le repli sur zéro évite un `expect` interdit par `CLAUDE.md`, et il tombe dans
    // les refus qui suivent — une version nulle et une dimension nulle sont l'une et l'autre
    // rejetées, donc l'invariant est tenu par le refus plutôt que par la panique.
    let version = u32::from_le_bytes(fixed[8..12].try_into().unwrap_or([0; 4]));
    if version != VERSION && version != VERSION_WITHOUT_PROVENANCE {
        return Err(Error::VectorFormat {
            path: path.to_owned(),
            reason: format!(
                "version {version}, ce binaire lit la {VERSION_WITHOUT_PROVENANCE} et la {VERSION}"
            ),
        });
    }

    let dim = u32::from_le_bytes(fixed[12..16].try_into().unwrap_or([0; 4])) as usize;
    if dim == 0 {
        return Err(Error::VectorFormat {
            path: path.to_owned(),
            reason: "dimension nulle".to_owned(),
        });
    }

    if version == VERSION_WITHOUT_PROVENANCE {
        return Ok(Header {
            dim,
            provenance: None,
            len: HEADER_LEN_V1,
        });
    }

    let mut length = [0u8; 4];
    file.read_exact(&mut length)
        .map_err(|_| Error::VectorFormat {
            path: path.to_owned(),
            reason: "en-tête tronqué avant la longueur de provenance".to_owned(),
        })?;
    let block_len = u32::from_le_bytes(length);
    // Le plafond est vérifié **avant** d'allouer : ce champ vient du fichier, donc d'une entrée
    // qu'on n'a pas écrite, et lui faire confiance ferait demander quatre gibioctets pour un
    // octet corrompu.
    if block_len > PROVENANCE_MAX {
        return Err(Error::VectorFormat {
            path: path.to_owned(),
            reason: format!("provenance annoncée à {block_len} octets, plafond {PROVENANCE_MAX}"),
        });
    }

    let mut block = vec![0u8; block_len as usize];
    file.read_exact(&mut block)
        .map_err(|_| Error::VectorFormat {
            path: path.to_owned(),
            reason: "en-tête tronqué dans la provenance".to_owned(),
        })?;
    let raw = String::from_utf8_lossy(&block);

    Ok(Header {
        dim,
        provenance: Some(Provenance::decode(&raw)),
        len: HEADER_LEN_V2 + u64::from(block_len),
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::vectors::Vectors;

    /// Une provenance quelconque, pour les tests qui ne portent pas sur elle.
    fn prov() -> Provenance {
        named("jouet")
    }

    /// Une provenance qui se distingue d'une autre par son modèle.
    fn named(model: &str) -> Provenance {
        Provenance {
            model: model.to_owned(),
            family: "jouet".to_owned(),
            digest: format!("{model:_<16}"),
            budget: 2_000,
            written: "2026-09-22".to_owned(),
        }
    }

    /// Un magasin jetable, et son chemin.
    fn scratch() -> (tempfile::TempDir, Utf8PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(dir.path().join("vectors.bin")).unwrap();
        (dir, path)
    }

    #[test]
    fn what_is_written_comes_back_normalised() {
        let (_dir, path) = scratch();
        let mut writer = VectorWriter::open(&path, 3, &prov()).unwrap();
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
        let mut writer = VectorWriter::open(&path, 2, &prov()).unwrap();
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
        VectorWriter::open(&path, 2, &prov())
            .unwrap()
            .append(MessageId(1), &[1.0, 0.0])
            .unwrap();

        let mut writer = VectorWriter::open(&path, 2, &prov()).unwrap();
        writer.append(MessageId(2), &[0.0, 1.0]).unwrap();
        writer.sync().unwrap();

        assert_eq!(Vectors::load(&path, 2).unwrap().len(), 2);
    }

    #[test]
    fn a_record_cut_in_half_is_dropped_and_the_rest_survives() {
        // Ce que laisse un processus tué en pleine écriture. Le corpus déjà écrit doit
        // survivre, et la queue doit disparaître — pas l'inverse.
        let (_dir, path) = scratch();
        let mut writer = VectorWriter::open(&path, 4, &prov()).unwrap();
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
        let mut writer = VectorWriter::open(&path, 4, &prov()).unwrap();
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
        VectorWriter::open(&path, 8, &prov())
            .unwrap()
            .sync()
            .unwrap();

        assert!(matches!(
            VectorWriter::open(&path, 16, &prov()),
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
        let mut writer = VectorWriter::open(&path, 3, &prov()).unwrap();
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
            VectorWriter::open(&path, 4, &prov()),
            Err(Error::VectorFormat { .. })
        ));
    }

    #[test]
    fn a_store_of_a_future_version_is_refused_rather_than_read() {
        // Le contrôle inverse du précédent : la magie est bonne, donc rien ne signalerait le
        // problème si la version n'était pas regardée.
        let (_dir, path) = scratch();
        let mut header = [0u8; HEADER_LEN_V2 as usize];
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
        let mut writer = VectorWriter::open(&path, 3, &prov()).unwrap();
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

    #[test]
    fn a_store_says_what_wrote_it() {
        let (_dir, path) = scratch();
        let mut writer = VectorWriter::open(&path, 3, &named("e5-small")).unwrap();
        writer.append(MessageId(1), &[1.0, 0.0, 0.0]).unwrap();
        writer.sync().unwrap();

        let found = Vectors::load(&path, 3).unwrap();
        let provenance = found
            .provenance()
            .expect("la version 2 porte sa provenance");
        assert_eq!(provenance.model, "e5-small");
        assert_eq!(provenance.budget, 2_000);
        assert_eq!(provenance.written, "2026-09-22");
    }

    /// Le refus qui manquait le 2026-09-21.
    ///
    /// Deux modèles de **même dimension** — c'est le cas des deux `potion` du corpus — ne se
    /// comparent pas, et jusqu'ici rien ne l'aurait dit : la dimension passait, les records
    /// s'empilaient, et le magasin rendait un classement dont chaque moitié venait d'un autre
    /// espace.
    #[test]
    fn a_pass_refuses_to_continue_a_store_that_another_model_began() {
        let (_dir, path) = scratch();
        let mut writer = VectorWriter::open(&path, 3, &named("potion-taille")).unwrap();
        writer.append(MessageId(1), &[1.0, 0.0, 0.0]).unwrap();
        writer.sync().unwrap();
        drop(writer);

        let refused = VectorWriter::open(&path, 3, &named("potion-brut"));
        assert!(
            matches!(refused, Err(Error::VectorFormat { .. })),
            "un autre modèle à la même dimension doit être refusé"
        );
    }

    /// Le contrôle inverse, sans lequel le précédent serait tenu par un refus systématique.
    #[test]
    fn a_pass_continues_a_store_that_it_began_itself() {
        let (_dir, path) = scratch();
        let mut writer = VectorWriter::open(&path, 3, &named("e5-small")).unwrap();
        writer.append(MessageId(1), &[1.0, 0.0, 0.0]).unwrap();
        writer.sync().unwrap();
        drop(writer);

        let mut again = VectorWriter::open(&path, 3, &named("e5-small"))
            .expect("la même provenance doit être acceptée");
        again.append(MessageId(2), &[0.0, 1.0, 0.0]).unwrap();
        again.sync().unwrap();

        assert_eq!(Vectors::load(&path, 3).unwrap().len(), 2);
    }

    /// La date ne fait pas partie de l'identité, et c'est le critère 3 qui l'exige.
    ///
    /// Une passe de quatre heures coupée à minuit reprend le lendemain. Comparer sur la date
    /// rendrait « tuée au milieu, elle repart où elle en était » intenable un jour sur — au
    /// hasard — mille.
    #[test]
    fn a_pass_resumed_the_next_day_is_still_the_same_pass() {
        let (_dir, path) = scratch();
        let veille = Provenance {
            written: "2026-09-21".to_owned(),
            ..named("e5-small")
        };
        let lendemain = Provenance {
            written: "2026-09-22".to_owned(),
            ..named("e5-small")
        };

        let mut writer = VectorWriter::open(&path, 3, &veille).unwrap();
        writer.append(MessageId(1), &[1.0, 0.0, 0.0]).unwrap();
        writer.sync().unwrap();
        drop(writer);

        assert!(
            VectorWriter::open(&path, 3, &lendemain).is_ok(),
            "seule la date change : c'est la même passe"
        );
    }

    /// Un magasin de version 1 se lit, et ne s'écrit plus.
    ///
    /// Les deux moitiés comptent. Refuser à la lecture obligerait à recalculer des heures de
    /// vecteurs parfaitement valides ; accepter à l'écriture laisserait continuer un magasin
    /// sans pouvoir vérifier avec quoi il a commencé, ce qui est exactement le trou qu'on
    /// bouche.
    #[test]
    fn a_store_without_provenance_is_read_but_not_extended() {
        let (_dir, path) = scratch();

        // Un magasin de version 1, écrit à la main : le format n'a plus de quoi en produire.
        let mut bytes = Vec::new();
        let mut header = [0u8; HEADER_LEN_V1 as usize];
        header[..8].copy_from_slice(&MAGIC);
        header[8..12].copy_from_slice(&VERSION_WITHOUT_PROVENANCE.to_le_bytes());
        header[12..16].copy_from_slice(&3u32.to_le_bytes());
        bytes.extend_from_slice(&header);
        bytes.extend_from_slice(&7i64.to_le_bytes());
        for value in [1.0f32, 0.0, 0.0] {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        std::fs::write(&path, &bytes).unwrap();

        let found = Vectors::load(&path, 3).expect("un magasin de version 1 se lit encore");
        assert_eq!(found.len(), 1);
        assert!(
            found.provenance().is_none(),
            "et il dit qu'il ne sait pas d'où il vient"
        );

        assert!(
            matches!(
                VectorWriter::open(&path, 3, &prov()),
                Err(Error::VectorFormat { .. })
            ),
            "mais on ne le continue pas"
        );
    }

    /// La provenance vient du fichier, donc c'est une entrée hostile.
    #[test]
    fn an_absurd_provenance_length_is_refused_before_it_is_allocated() {
        let (_dir, path) = scratch();
        let mut header = [0u8; HEADER_LEN_V2 as usize];
        header[..8].copy_from_slice(&MAGIC);
        header[8..12].copy_from_slice(&VERSION.to_le_bytes());
        header[12..16].copy_from_slice(&4u32.to_le_bytes());
        header[16..20].copy_from_slice(&u32::MAX.to_le_bytes());
        std::fs::write(&path, header).unwrap();

        assert!(matches!(
            Vectors::load(&path, 4),
            Err(Error::VectorFormat { .. })
        ));
    }

    #[test]
    fn a_provenance_survives_the_round_trip_and_ignores_what_it_does_not_know() {
        let written = named("e5-small");
        let read = Provenance::decode(&written.encode());
        assert_eq!(read, written);

        // Une clé de plus — un magasin écrit par une version ultérieure — ne casse rien.
        let extended = format!("{}futur=quelque chose\n", written.encode());
        assert_eq!(Provenance::decode(&extended), written);
    }

    /// Un retour à la ligne dans une valeur couperait le bloc en deux à la relecture.
    #[test]
    fn a_newline_in_a_value_cannot_break_the_block() {
        let hostile = Provenance {
            model: "e5\nbudget=9999".to_owned(),
            ..named("x")
        };
        let read = Provenance::decode(&hostile.encode());
        assert_eq!(
            read.budget, 2_000,
            "la valeur injectée n'a pas pris la main"
        );
    }
}
