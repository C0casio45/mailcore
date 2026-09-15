//! Stockage des blobs : écriture atomique, lecture, ramasse-miettes.
//!
//! Trois invariants, et tout le reste en découle :
//!
//! 1. **Un blob n'est jamais modifié.** Seulement créé, ou supprimé quand plus aucune
//!    ligne `refs` ne pointe vers lui. C'est ce qui fait qu'il n'y a pas de compactage.
//! 2. **La clé est le contenu.** [`BlobHash`] = BLAKE3 des octets RFC 5322 bruts. Deux
//!    copies du même message donnent le même chemin, donc un seul fichier.
//! 3. **Écriture atomique.** Fichier temporaire, `fsync`, `rename`. Un plantage laisse
//!    soit rien, soit un blob complet — jamais un blob tronqué.
//!
//! ## Le temporaire n'est pas dans `blobs/`, mais il est sur le même volume
//!
//! `docs/ARCHITECTURE.md` dit « temporaire hors store ». À prendre au sens de « hors de
//! l'arborescence des blobs », pas « dans le répertoire temporaire du système » : un
//! `rename` n'est atomique qu'à l'intérieur d'un même système de fichiers. Le temporaire
//! va donc dans `<root>/tmp/`, voisin de `blobs/` et sur le même volume. L'énumération des
//! blobs ne le voit jamais, et le `rename` reste atomique.
//!
//! ## Durabilité : ce qui est garanti et ce qui ne l'est pas
//!
//! Le `fsync` porte sur le fichier temporaire avant le `rename`, ce qui garantit qu'un blob
//! visible est un blob complet. Le répertoire parent n'est pas synchronisé : après une
//! coupure d'alimentation, un blob tout juste écrit peut manquer alors que l'index le
//! référence. C'est un état **détectable** (`mail doctor`) et **réparable** (réimport du
//! message), et le coût d'un `fsync` de répertoire par message sur un import de plusieurs
//! centaines de milliers de messages ne le justifie pas. Choix assumé, pas oubli.

use std::io::{Read, Write};

use camino::{Utf8Path, Utf8PathBuf};

use crate::error::{Error, Result};
use crate::model::BlobHash;

/// Niveau de compression zstd.
///
/// 3 est le défaut de zstd, et le bon compromis ici : le texte d'un mail compresse d'un
/// facteur 3 à 4, les pièces jointes déjà compressées ne compressent pas du tout, et
/// monter le niveau coûte du temps d'import pour quelques pour-cent sur la moitié du
/// corpus qui compresse déjà bien. À réévaluer avec les mesures de l'étape 4, pas avant.
const ZSTD_LEVEL: i32 = 3;

/// Ce qu'une écriture a réellement fait.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PutOutcome {
    /// L'identité du contenu.
    pub hash: BlobHash,
    /// Faux si le blob existait déjà — c'est la mesure du taux de dédup (critère 6).
    pub created: bool,
    /// Octets écrits sur le disque **par cet appel**, après compression. Zéro si le blob
    /// existait déjà : c'est exactement l'espace que la dédup a économisé.
    pub stored_len: u64,
}

/// L'accès aux blobs.
///
/// Un trait et non une structure concrète parce qu'un blob immuable adressé par contenu est
/// exactement le modèle d'accès d'un stockage objet : une implémentation S3 est possible
/// plus tard sans toucher au reste. Le système de fichiers reste le défaut — l'import écrit
/// des centaines de milliers de petits objets, et le surcoût par requête y déciderait.
pub trait BlobStore: std::fmt::Debug + Send + Sync {
    /// Écrit un message RFC 5322 brut et rend son identité.
    ///
    /// Idempotent : réécrire le même contenu ne fait rien et rend `created: false`.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] si l'écriture échoue.
    fn put(&self, rfc822: &[u8]) -> Result<PutOutcome>;

    /// Range un contenu **lu d'un flux**, sans le tenir en mémoire.
    ///
    /// Pour ce qui n'est pas déjà en mémoire : une pièce jointe de 25 Mo se range sans être
    /// chargée. Voir l'implémentation pour ce que ça coûte — le court-circuit sur doublon
    /// n'est pas possible, la clé n'étant connue qu'à la fin.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] si la lecture, la compression ou le renommage échoue.
    fn put_reader(&self, source: &mut dyn Read) -> Result<PutOutcome>;

    /// Ouvre un blob en écriture, dont la clé sera celle de ce qu'on y écrit.
    ///
    /// Pour ce qui n'existe pas encore : un message **assemblé**, que
    /// `mailsmtp::compose::Draft::write_to` produit en écrivant. Voir [`BlobSink`].
    ///
    /// # Errors
    ///
    /// [`Error::Io`] si le temporaire ne peut pas être créé.
    fn put_writer(&self) -> Result<Box<dyn BlobSink>>;

    /// Vrai si le blob est présent.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] si le store est illisible.
    fn contains(&self, hash: BlobHash) -> Result<bool>;

    /// Lit un message en entier.
    ///
    /// Un message unitaire tient en mémoire — c'est un mbox entier qui n'y tient pas.
    ///
    /// # Errors
    ///
    /// [`Error::BlobNotFound`] si le blob est absent, [`Error::Io`] si la lecture ou la
    /// décompression échoue.
    fn read(&self, hash: BlobHash) -> Result<Vec<u8>>;

    /// Ouvre un flux de lecture décompressé sur un message.
    ///
    /// # Errors
    ///
    /// [`Error::BlobNotFound`] si le blob est absent, [`Error::Io`] sinon.
    fn open(&self, hash: BlobHash) -> Result<Box<dyn Read + Send>>;

    /// Supprime un blob. Rend faux s'il n'était pas là.
    ///
    /// À n'appeler que quand plus aucune ligne `refs` ne pointe vers lui.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] si la suppression échoue.
    fn delete(&self, hash: BlobHash) -> Result<bool>;
}

/// Implémentation sur le système de fichiers : `<root>/blobs/<aa>/<bb>/<hash>`.
#[derive(Debug, Clone)]
pub struct FsBlobStore {
    blobs: Utf8PathBuf,
    tmp: Utf8PathBuf,
}

impl FsBlobStore {
    /// Ouvre — et crée si besoin — un store de blobs sous `root`.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] si les répertoires ne peuvent pas être créés.
    pub fn open(root: &Utf8Path) -> Result<Self> {
        let blobs = root.join("blobs");
        let tmp = root.join("tmp");
        for dir in [&blobs, &tmp] {
            std::fs::create_dir_all(dir).map_err(|e| io_err(dir, e))?;
        }
        Ok(Self { blobs, tmp })
    }

    /// Le chemin absolu d'un blob.
    #[must_use]
    pub fn path_of(&self, hash: BlobHash) -> Utf8PathBuf {
        self.blobs.join(hash.shard_path())
    }
}

impl BlobStore for FsBlobStore {
    fn put(&self, rfc822: &[u8]) -> Result<PutOutcome> {
        let hash = BlobHash::of(rfc822);
        let final_path = self.path_of(hash);

        // Sortie anticipée sur un doublon : ni compression, ni écriture, ni `fsync`. Sur un
        // corpus dont une bonne part est dupliquée, c'est le chemin le plus fréquent.
        if final_path.exists() {
            return Ok(PutOutcome {
                hash,
                created: false,
                stored_len: 0,
            });
        }

        let mut tmp =
            tempfile::NamedTempFile::new_in(&self.tmp).map_err(|e| io_err(&self.tmp, e))?;

        // **La compression va droit dans le fichier**, sans copie compressée en mémoire.
        //
        // `zstd::encode_all` alloue tout le résultat avant d'en écrire un octet : pour le plus
        // gros message du corpus réel — 48 Mio, mesurés le 2026-09-09 — ça faisait 48 Mio de
        // corps **plus** 47,5 Mio de compressé vivants en même temps. C'est ce qui expliquait
        // les 168,9 Mio de crête d'une moisson complète, là où la borne du lot est de 32 Mio :
        // un message plus gros que la borne part seul, et il payait alors le double de sa
        // taille.
        //
        // C'est aussi la règle 4 du `CLAUDE.md` prise au mot un cran plus bas : rien ne charge
        // un message entier **deux fois**.
        {
            let mut sink = std::io::BufWriter::new(&mut tmp);
            zstd::stream::copy_encode(rfc822, &mut sink, ZSTD_LEVEL)
                .map_err(|e| io_err(&final_path, e))?;
            sink.flush().map_err(|e| io_err(&self.tmp, e))?;
        }
        tmp.flush().map_err(|e| io_err(&self.tmp, e))?;
        // La taille est relue du fichier plutôt que comptée : `copy_encode` ne la rend pas, et
        // un compteur maison serait une deuxième vérité sur la même donnée.
        let stored_len = tmp
            .as_file()
            .metadata()
            .map_err(|e| io_err(&self.tmp, e))?
            .len();
        // Avant le rename : un blob visible doit être un blob complet.
        tmp.as_file().sync_all().map_err(|e| io_err(&self.tmp, e))?;

        if let Some(parent) = final_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| io_err(parent, e))?;
        }

        // `persist_noclobber` plutôt que `persist` : si un autre écrivain a gagné la course,
        // le contenu est identique par construction — c'est la même clé, donc les mêmes
        // octets. Il n'y a rien à écraser, et rien à signaler comme erreur.
        match tmp.persist_noclobber(final_path.as_std_path()) {
            Ok(_) => Ok(PutOutcome {
                hash,
                created: true,
                stored_len,
            }),
            Err(e) if e.error.kind() == std::io::ErrorKind::AlreadyExists => Ok(PutOutcome {
                hash,
                created: false,
                stored_len: 0,
            }),
            Err(e) => Err(io_err(&final_path, e.error)),
        }
    }

    /// Range un contenu **lu d'un flux**, sans le tenir en mémoire.
    ///
    /// ## Pourquoi c'est une méthode de plus et pas un remplacement
    ///
    /// [`BlobStore::put`] prend une tranche, et c'est le bon appel pour un message reçu : il est
    /// déjà en mémoire, l'IMAP l'y a mis. Cette variante-ci sert à ce qui **n'y est pas** — un
    /// fichier de 25 Mo que l'utilisateur joint, et qu'il ne faut pas charger pour le ranger.
    ///
    /// ## Le hachage est calculé au passage
    ///
    /// C'est ce qui rend la fonction possible : la clé est le hachage du contenu, donc la
    /// destination n'est connue qu'après avoir tout lu. L'écriture va dans un temporaire pendant
    /// que le hachage avance, et le renommage final n'a lieu qu'à la fin.
    ///
    /// Conséquence à connaître : **le doublon n'est pas court-circuité.** [`BlobStore::put`]
    /// sort avant toute écriture quand le blob existe déjà ; ici, on ne le sait qu'après avoir
    /// tout compressé. Sur un corpus très dupliqué, c'est le mauvais appel ; sur une pièce
    /// jointe, le contenu est neuf presque à coup sûr.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] si la lecture, la compression ou le renommage échoue.
    fn put_reader(&self, source: &mut dyn Read) -> Result<PutOutcome> {
        let mut tmp =
            tempfile::NamedTempFile::new_in(&self.tmp).map_err(|e| io_err(&self.tmp, e))?;

        // Le hachage avance **pendant** que zstd lit. Un second passage sur le fichier
        // temporaire donnerait le même résultat au prix d'une relecture complète, et il
        // faudrait décompresser pour hacher le contenu brut.
        let mut hashing = Hashing {
            source,
            hasher: blake3::Hasher::new(),
        };
        {
            let mut sink = std::io::BufWriter::new(&mut tmp);
            zstd::stream::copy_encode(&mut hashing, &mut sink, ZSTD_LEVEL)
                .map_err(|e| io_err(&self.tmp, e))?;
            sink.flush().map_err(|e| io_err(&self.tmp, e))?;
        }
        let hash = BlobHash::from_bytes(*hashing.hasher.finalize().as_bytes());
        let final_path = self.path_of(hash);

        tmp.flush().map_err(|e| io_err(&self.tmp, e))?;
        let stored_len = tmp
            .as_file()
            .metadata()
            .map_err(|e| io_err(&self.tmp, e))?
            .len();
        // Avant le rename : un blob visible doit être un blob complet.
        tmp.as_file().sync_all().map_err(|e| io_err(&self.tmp, e))?;

        if let Some(parent) = final_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| io_err(parent, e))?;
        }
        match tmp.persist_noclobber(final_path.as_std_path()) {
            Ok(_) => Ok(PutOutcome {
                hash,
                created: true,
                stored_len,
            }),
            // Le contenu était déjà là. C'est la même clé, donc les mêmes octets : il n'y a
            // rien à écraser et rien à signaler.
            Err(e) if e.error.kind() == std::io::ErrorKind::AlreadyExists => Ok(PutOutcome {
                hash,
                created: false,
                stored_len: 0,
            }),
            Err(e) => Err(io_err(&final_path, e.error)),
        }
    }

    fn put_writer(&self) -> Result<Box<dyn BlobSink>> {
        let tmp = tempfile::NamedTempFile::new_in(&self.tmp).map_err(|e| io_err(&self.tmp, e))?;
        let encoder = zstd::stream::write::Encoder::new(std::io::BufWriter::new(tmp), ZSTD_LEVEL)
            .map_err(|e| io_err(&self.tmp, e))?;
        Ok(Box::new(FsBlobSink {
            encoder: Some(encoder),
            hasher: blake3::Hasher::new(),
            blobs: self.blobs.clone(),
            tmp: self.tmp.clone(),
        }))
    }

    fn contains(&self, hash: BlobHash) -> Result<bool> {
        Ok(self.path_of(hash).exists())
    }

    fn read(&self, hash: BlobHash) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        self.open(hash)?
            .read_to_end(&mut out)
            .map_err(|e| io_err(&self.path_of(hash), e))?;
        Ok(out)
    }

    fn open(&self, hash: BlobHash) -> Result<Box<dyn Read + Send>> {
        let path = self.path_of(hash);
        let file = match std::fs::File::open(&path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(Error::BlobNotFound(hash));
            }
            Err(e) => return Err(io_err(&path, e)),
        };
        let decoder = zstd::Decoder::new(file).map_err(|e| io_err(&path, e))?;
        Ok(Box::new(decoder))
    }

    fn delete(&self, hash: BlobHash) -> Result<bool> {
        let path = self.path_of(hash);
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(io_err(&path, e)),
        }
    }
}

/// Construit une [`Error::Io`] en gardant le chemin : un message d'erreur sans le chemin
/// concerné est inexploitable sur un store de plusieurs centaines de milliers de fichiers.
fn io_err(path: &Utf8Path, source: std::io::Error) -> Error {
    Error::Io {
        path: path.to_owned(),
        source,
    }
}

impl FsBlobStore {
    /// Passe chaque empreinte présente sur le disque à `on`, **sans lire aucun contenu**.
    ///
    /// ## Pourquoi les noms suffisent
    ///
    /// Un blob est nommé par son empreinte — c'est ce que veut dire « adressé par contenu ».
    /// Recenser ce qui est stocké ne demande donc que de lire des noms de fichiers, pas 3,5 Gio
    /// de zstd. Un fichier dont le nom n'est pas une empreinte valide est **ignoré** : ce n'est
    /// pas un blob, et refuser tout l'inventaire à cause de lui rendrait l'inventaire inutile
    /// le jour où un outil extérieur laisse traîner quelque chose.
    ///
    /// Par rappel plutôt qu'en rendant une liste : le corpus réel en a 48 532, et un `Vec` ne
    /// coûterait qu'un mégaoctet et demi — mais rien n'oblige un appelant à les tenir tous, et
    /// la règle 4 du `CLAUDE.md` dit de ne pas charger ce qu'on peut parcourir.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] si un répertoire de l'arborescence est illisible, ou ce que `on` rend.
    pub fn for_each_hash(&self, on: &mut dyn FnMut(crate::BlobHash) -> Result<()>) -> Result<()> {
        // Deux niveaux de partition, `<aa>/<bb>`, puis les fichiers. Le parcours est explicite
        // plutôt que récursif : la profondeur est une propriété du schéma de nommage, et un
        // parcours récursif descendrait aussi dans ce qu'un jour quelqu'un aurait posé là.
        let shards = match std::fs::read_dir(&self.blobs) {
            Ok(shards) => shards,
            // Un store neuf n'a pas encore de répertoire : zéro blob, pas une erreur.
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(source) => return Err(io_err(&self.blobs, source)),
        };
        for shard in shards.flatten() {
            let shard = shard.path();
            let Ok(inner) = std::fs::read_dir(&shard) else {
                continue;
            };
            for sub in inner.flatten() {
                let Ok(files) = std::fs::read_dir(sub.path()) else {
                    continue;
                };
                for file in files.flatten() {
                    let name = file.file_name();
                    let Some(name) = name.to_str() else {
                        continue;
                    };
                    if let Ok(hash) = crate::BlobHash::from_hex(name) {
                        on(hash)?;
                    }
                }
            }
        }
        Ok(())
    }
}

/// Un blob en cours d'écriture.
///
/// ## Pourquoi ce type existe
///
/// [`BlobStore::put`] veut une tranche, [`BlobStore::put_reader`] veut un flux à lire. Un
/// message **assemblé** n'est ni l'un ni l'autre : il n'existe pas encore, et
/// `mailsmtp::compose::Draft::write_to` le produit en écrivant.
///
/// Sans ce type, il faudrait un fichier temporaire de plus : assembler dedans, puis le relire
/// pour le ranger. Or le magasin en écrit déjà un — c'est comme ça que la clé, qui est le
/// hachage, peut être connue à la fin. Donner accès à celui-là évite la copie.
///
/// ## Le blob n'existe pas avant [`BlobSink::finish`]
///
/// Un puits abandonné ne laisse rien : son temporaire est supprimé par son destructeur. C'est
/// ce qui fait qu'un assemblage interrompu — un fichier joint arraché en cours de lecture — ne
/// range pas un message tronqué sous la clé de sa troncature.
pub trait BlobSink: Write {
    /// Ferme le blob et le range sous la clé de son contenu.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] si la compression, la synchronisation ou le renommage échoue.
    fn finish(self: Box<Self>) -> Result<PutOutcome>;
}

/// Un blob en écriture sur le système de fichiers.
///
/// Le hachage avance au fil des écritures, la compression aussi : rien de la taille du contenu
/// n'existe en mémoire, et c'est ce qui tient le critère 3 de `docs/PHASE-3.md`.
struct FsBlobSink {
    /// La compression, qui écrit dans le temporaire.
    ///
    /// `Option` parce que [`BlobSink::finish`] doit la **conclure** — un flux zstd a un pied de
    /// trame — et que `finish` consomme l'encodeur. Toujours `Some` avant `finish`.
    encoder:
        Option<zstd::stream::write::Encoder<'static, std::io::BufWriter<tempfile::NamedTempFile>>>,
    hasher: blake3::Hasher,
    blobs: Utf8PathBuf,
    tmp: Utf8PathBuf,
}

impl Write for FsBlobSink {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        let Some(encoder) = self.encoder.as_mut() else {
            return Err(std::io::Error::other("blob déjà conclu"));
        };
        encoder.write_all(buffer)?;
        // **Le hachage porte sur le contenu brut**, pas sur le compressé : c'est l'identité du
        // message, et deux niveaux de compression différents donneraient deux clés pour le même
        // message.
        self.hasher.update(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self.encoder.as_mut() {
            Some(encoder) => encoder.flush(),
            None => Ok(()),
        }
    }
}

impl BlobSink for FsBlobSink {
    fn finish(mut self: Box<Self>) -> Result<PutOutcome> {
        let encoder = self
            .encoder
            .take()
            .ok_or_else(|| io_err(&self.tmp, std::io::Error::other("blob déjà conclu")))?;
        let mut buffered = encoder.finish().map_err(|e| io_err(&self.tmp, e))?;
        buffered.flush().map_err(|e| io_err(&self.tmp, e))?;
        let mut tmp = buffered
            .into_inner()
            .map_err(|e| io_err(&self.tmp, e.into_error()))?;
        tmp.flush().map_err(|e| io_err(&self.tmp, e))?;

        let hash = BlobHash::from_bytes(*self.hasher.finalize().as_bytes());
        let final_path = self.blobs.join(hash.shard_path());
        let stored_len = tmp
            .as_file()
            .metadata()
            .map_err(|e| io_err(&self.tmp, e))?
            .len();
        // Avant le rename : un blob visible doit être un blob complet.
        tmp.as_file().sync_all().map_err(|e| io_err(&self.tmp, e))?;

        if let Some(parent) = final_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| io_err(parent, e))?;
        }
        match tmp.persist_noclobber(final_path.as_std_path()) {
            Ok(_) => Ok(PutOutcome {
                hash,
                created: true,
                stored_len,
            }),
            Err(e) if e.error.kind() == std::io::ErrorKind::AlreadyExists => Ok(PutOutcome {
                hash,
                created: false,
                stored_len: 0,
            }),
            Err(e) => Err(io_err(&final_path, e.error)),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// Un store jetable, plus le répertoire temporaire qui le maintient en vie.
    fn store() -> (tempfile::TempDir, FsBlobStore) {
        let dir = tempfile::tempdir().unwrap();
        let root = Utf8Path::from_path(dir.path()).unwrap().to_owned();
        let store = FsBlobStore::open(&root).unwrap();
        (dir, store)
    }

    const MSG: &[u8] = b"From: a@b.c\r\nSubject: essai\r\n\r\nUn corps de message.\r\n";

    #[test]
    fn roundtrips_exactly() {
        let (_dir, s) = store();
        let out = s.put(MSG).unwrap();
        assert!(out.created);
        assert_eq!(s.read(out.hash).unwrap(), MSG);
    }

    #[test]
    fn same_content_is_stored_once() {
        // La thèse du projet : le même message dans deux dossiers, un seul fichier.
        let (_dir, s) = store();
        let first = s.put(MSG).unwrap();
        let second = s.put(MSG).unwrap();

        assert_eq!(first.hash, second.hash);
        assert!(first.created);
        assert!(!second.created, "le doublon a été réécrit");
        assert_eq!(second.stored_len, 0, "le doublon a consommé du disque");
    }

    #[test]
    fn different_content_gives_different_blobs() {
        let (_dir, s) = store();
        let a = s.put(b"contenu a").unwrap();
        let b = s.put(b"contenu b").unwrap();
        assert_ne!(a.hash, b.hash);
        assert!(a.created && b.created);
    }

    #[test]
    fn stores_binary_payloads_byte_for_byte() {
        // Une pièce jointe n'est pas de l'UTF-8. Octets nuls, séquences invalides, tout
        // doit ressortir identique.
        let (_dir, s) = store();
        let hostile: Vec<u8> = (0u8..=255).cycle().take(10_000).collect();
        let out = s.put(&hostile).unwrap();
        assert_eq!(s.read(out.hash).unwrap(), hostile);
    }

    #[test]
    fn empty_blob_is_a_valid_blob() {
        // Un message vide entre deux séparateurs mbox existe dans la nature.
        let (_dir, s) = store();
        let out = s.put(b"").unwrap();
        assert!(out.created);
        assert_eq!(s.read(out.hash).unwrap(), Vec::<u8>::new());
    }

    #[test]
    fn compresses_compressible_content() {
        let (_dir, s) = store();
        let repetitive = "Re: Re: Re: le meme texte encore et encore. ".repeat(500);
        let out = s.put(repetitive.as_bytes()).unwrap();
        assert!(
            out.stored_len < repetitive.len() as u64 / 2,
            "{} octets stockés pour {} octets d'entrée",
            out.stored_len,
            repetitive.len()
        );
    }

    #[test]
    fn lands_on_the_sharded_path() {
        let (_dir, s) = store();
        let out = s.put(MSG).unwrap();
        let hex = out.hash.to_hex();
        let path = s.path_of(out.hash);

        assert!(path.exists());
        assert!(path.as_str().contains(&hex[0..2]));
        assert_eq!(path.file_name(), Some(hex.as_str()));
    }

    #[test]
    fn leaves_no_temporary_behind() {
        let (_dir, s) = store();
        s.put(MSG).unwrap();
        s.put(b"un autre").unwrap();

        let leftovers = std::fs::read_dir(&s.tmp).unwrap().count();
        assert_eq!(leftovers, 0, "des fichiers temporaires sont restés");
    }

    #[test]
    fn temporary_never_lands_in_the_blob_tree() {
        // Un temporaire dans `blobs/` serait pris pour un blob par l'énumération.
        let (_dir, s) = store();
        assert!(!s.tmp.starts_with(&s.blobs));
    }

    #[test]
    fn missing_blob_is_an_error_not_a_panic() {
        let (_dir, s) = store();
        let absent = BlobHash::of(b"jamais ecrit");

        assert!(!s.contains(absent).unwrap());
        assert!(matches!(s.read(absent), Err(Error::BlobNotFound(h)) if h == absent));
        assert!(matches!(s.open(absent), Err(Error::BlobNotFound(_))));
    }

    #[test]
    fn delete_is_idempotent() {
        let (_dir, s) = store();
        let out = s.put(MSG).unwrap();

        assert!(s.contains(out.hash).unwrap());
        assert!(s.delete(out.hash).unwrap());
        assert!(!s.contains(out.hash).unwrap());
        assert!(!s.delete(out.hash).unwrap(), "seconde suppression signalée");
    }

    #[test]
    fn corrupted_blob_errors_instead_of_panicking() {
        // Un fichier qui n'est pas du zstd — antivirus, disque abîmé, écriture d'un tiers.
        let (_dir, s) = store();
        let hash = BlobHash::of(b"peu importe");
        let path = s.path_of(hash);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"ce ne sont pas des octets zstd").unwrap();

        assert!(s.read(hash).is_err());
    }

    #[test]
    fn reopening_finds_existing_blobs() {
        let dir = tempfile::tempdir().unwrap();
        let root = Utf8Path::from_path(dir.path()).unwrap().to_owned();

        let hash = FsBlobStore::open(&root).unwrap().put(MSG).unwrap().hash;
        let reopened = FsBlobStore::open(&root).unwrap();

        assert!(reopened.contains(hash).unwrap());
        assert_eq!(reopened.read(hash).unwrap(), MSG);
    }

    #[test]
    fn is_usable_as_a_trait_object() {
        // `maild` partagera le store derrière un `dyn BlobStore` entre plusieurs tâches.
        let (_dir, s) = store();
        let dynamic: &dyn BlobStore = &s;
        let out = dynamic.put(MSG).unwrap();
        assert_eq!(dynamic.read(out.hash).unwrap(), MSG);
    }
}

/// Un lecteur qui hache ce qu'il laisse passer.
///
/// Sert à `put_reader` : la clé d'un blob est le hachage de son contenu, donc la destination
/// n'est connue qu'après avoir tout lu. Hacher **pendant** que zstd lit évite un second
/// passage — qui devrait en plus décompresser pour retrouver le contenu brut.
struct Hashing<'a> {
    source: &'a mut dyn Read,
    hasher: blake3::Hasher,
}

impl Read for Hashing<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let read = self.source.read(buffer)?;
        // **Seuls les octets réellement lus.** Hacher tout le tampon inclurait les octets
        // laissés du passage précédent, et le hachage ne correspondrait à aucun contenu.
        self.hasher.update(&buffer[..read]);
        Ok(read)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod stream_tests {
    use super::{BlobStore, FsBlobStore};
    use crate::model::BlobHash;

    fn store() -> (tempfile::TempDir, FsBlobStore) {
        let dir = tempfile::tempdir().unwrap();
        let root = camino::Utf8Path::from_path(dir.path()).unwrap();
        let store = FsBlobStore::open(root).unwrap();
        (dir, store)
    }

    #[test]
    fn a_streamed_blob_has_the_same_key_as_a_buffered_one() {
        // **La propriété qui rend les deux chemins interchangeables.** Si les clés
        // divergeaient, un contenu rangé par un chemin serait introuvable par l'autre — et la
        // dédup par contenu s'arrêterait de fonctionner sans rien signaler.
        let (_dir, store) = store();
        let content = b"des octets quelconques, et un peu plus\r\n";

        let buffered = store.put(content).unwrap();
        let streamed = store
            .put_reader(&mut std::io::Cursor::new(content.to_vec()))
            .unwrap();

        assert_eq!(buffered.hash, streamed.hash);
        assert_eq!(buffered.hash, BlobHash::of(content));
        // Le second n'a rien créé : le contenu était déjà là.
        assert!(buffered.created);
        assert!(!streamed.created);
    }

    #[test]
    fn a_streamed_blob_round_trips_byte_for_byte() {
        let (_dir, store) = store();
        // Assez gros pour traverser plusieurs tampons de `copy_encode`, et non compressible :
        // un contenu répétitif masquerait une erreur de longueur.
        let mut content = Vec::with_capacity(300_000);
        let mut seed = 0x1234_5678_u32;
        for _ in 0..300_000 {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            content.push((seed >> 16) as u8);
        }

        let put = store
            .put_reader(&mut std::io::Cursor::new(content.clone()))
            .unwrap();
        assert!(put.created);
        assert_eq!(store.read(put.hash).unwrap(), content);
        assert_eq!(put.hash, BlobHash::of(&content));
    }

    #[test]
    fn an_empty_stream_is_a_valid_blob() {
        let (_dir, store) = store();
        let put = store.put_reader(&mut std::io::empty()).unwrap();
        assert_eq!(put.hash, BlobHash::of(b""));
        assert!(store.read(put.hash).unwrap().is_empty());
    }

    #[test]
    fn a_reader_that_fails_midway_leaves_no_blob_behind() {
        // Un fichier arraché en cours de lecture. Le temporaire est jeté par son destructeur,
        // et **rien** n'apparaît sous une clé : un blob partiel serait indistinguable d'un
        // blob complet, et il porterait la clé de son contenu tronqué.
        struct Failing(usize);
        impl std::io::Read for Failing {
            fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
                if self.0 == 0 {
                    return Err(std::io::Error::other("disque arraché"));
                }
                let take = self.0.min(buffer.len());
                buffer[..take].fill(b'x');
                self.0 -= take;
                Ok(take)
            }
        }

        let (dir, store) = store();
        let outcome = store.put_reader(&mut Failing(50_000));
        assert!(outcome.is_err());

        // Aucun blob dans l'arborescence — seuls les répertoires de service subsistent.
        let mut found = 0;
        store
            .for_each_hash(&mut |_| {
                found += 1;
                Ok(())
            })
            .unwrap();
        assert_eq!(found, 0, "un blob partiel a été rangé");
        drop(dir);
    }
}
