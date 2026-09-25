//! Les poids d'un modèle, lus tenseur par tenseur.
//!
//! ## Pourquoi ne pas laisser `candle` les lire
//!
//! `candle_core::safetensors::load` lit **tout le fichier** en mémoire, puis copie chaque tenseur
//! dans son propre tampon. Pendant le chargement, les poids existent donc deux fois : 470 Mo de
//! fichier et 448 Mio de tenseurs, et la crête mesurée le 2026-09-25 est de 998 à 1 066 Mio pour
//! un modèle qui en occupe 725 une fois chargé. Le critère 5 porte sur la crête, pas sur le
//! régime : ce doublement suffirait à le crever même avec un vocabulaire taillé.
//!
//! La projection en mémoire l'éviterait, et elle est `unsafe` — voir `bert::load_weights`. Ce
//! module fait l'autre chose possible : il lit l'en-tête, puis chaque tenseur **directement dans
//! le tampon qui deviendra le sien**, par morceaux d'un mébioctet. La crête du chargement devient
//! la taille des poids plus un morceau.
//!
//! ## L'empreinte, au passage
//!
//! La provenance d'un magasin porte l'empreinte BLAKE3 du fichier de poids, et elle était
//! calculée par une **seconde** lecture complète — 470 Mo relus à chaque chargement. Le fichier
//! est lu ici en entier et dans l'ordre, octets de remplissage compris : l'empreinte se calcule
//! sur la même lecture, et elle est la même que `pass::Identity::digest_of` — un test le vérifie,
//! parce qu'une empreinte qui changerait de calcul rendrait tous les magasins existants
//! « d'un autre modèle ».
//!
//! ## Une entrée hostile
//!
//! Un modèle est un fichier venu d'ailleurs et lu en entier. Chaque longueur et chaque décalage de
//! l'en-tête est vérifié **contre la taille du fichier** avant qu'un octet ne soit alloué : un
//! en-tête qui annonce un tenseur de mille milliards d'éléments est refusé, pas tenté.

use std::collections::HashMap;
use std::io::Read;

use camino::Utf8Path;
use candle_core::{Device, Tensor};

/// La taille d'un morceau de lecture.
///
/// Multiple de huit, pour qu'un morceau ne coupe jamais un élément en deux, quelle que soit la
/// largeur du type.
const CHUNK: usize = 1 << 20;

/// Le plafond de l'en-tête : celui que le format `safetensors` fixe lui-même.
const HEADER_CAP: u64 = 100 * 1024 * 1024;

/// Ce que le chargement rend.
#[derive(Debug)]
pub(crate) struct Loaded {
    /// Les tenseurs, par nom.
    pub tensors: HashMap<String, Tensor>,
    /// L'empreinte du fichier, au format de `pass::Identity::digest_of`.
    pub digest: String,
}

/// Les types d'éléments que ce lecteur sait lire.
///
/// Deux, parce que ce sont les deux que les modèles du projet portent : les poids en `F32` et les
/// positions en `I64`. Un autre type est refusé avec son nom plutôt que lu de travers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    F32,
    I64,
}

impl Kind {
    fn parse(name: &str) -> Option<Self> {
        match name {
            "F32" => Some(Self::F32),
            "I64" => Some(Self::I64),
            _ => None,
        }
    }

    /// Le nom du type dans l'en-tête.
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::F32 => "F32",
            Self::I64 => "I64",
        }
    }

    pub(crate) const fn width(self) -> u64 {
        match self {
            Self::F32 => 4,
            Self::I64 => 8,
        }
    }
}

/// Un tenseur tel que l'en-tête le décrit, vérifié.
#[derive(Debug, Clone)]
pub(crate) struct Entry {
    pub name: String,
    pub kind: Kind,
    pub shape: Vec<usize>,
    /// Décalages dans la zone de données, `begin..end`.
    pub begin: u64,
    pub end: u64,
}

/// Un lecteur qui hache tout ce qu'il lit.
struct Hashing<R> {
    inner: R,
    hasher: blake3::Hasher,
}

impl<R: Read> Read for Hashing<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let read = self.inner.read(buf)?;
        if let Some(bytes) = buf.get(..read) {
            self.hasher.update(bytes);
        }
        Ok(read)
    }
}

/// Charge tous les tenseurs d'un fichier `safetensors`, et son empreinte.
///
/// # Errors
///
/// Une phrase qui dit ce qui ne va pas, si le fichier est illisible, tronqué, ou si son en-tête
/// décrit autre chose que ce que le fichier contient.
pub(crate) fn load(path: &Utf8Path, device: &Device) -> Result<Loaded, String> {
    let file = std::fs::File::open(path.as_std_path()).map_err(|e| format!("ouverture : {e}"))?;
    let length = file
        .metadata()
        .map_err(|e| format!("taille du fichier : {e}"))?
        .len();
    let mut reader = Hashing {
        inner: std::io::BufReader::with_capacity(CHUNK, file),
        hasher: blake3::Hasher::new(),
    };

    let Layout {
        entries, data_len, ..
    } = read_layout(&mut reader, length)?;

    let mut position = 0u64;
    let mut tensors = HashMap::with_capacity(entries.len());
    let mut buffer = vec![0u8; CHUNK];
    for entry in entries {
        skip(&mut reader, entry.begin - position, &mut buffer)?;
        let tensor = read_tensor(&mut reader, &entry, &mut buffer, device)?;
        position = entry.end;
        tensors.insert(entry.name, tensor);
    }
    // Ce qui suit le dernier tenseur est lu aussi : l'empreinte porte sur tout le fichier.
    skip(&mut reader, data_len - position, &mut buffer)?;

    let digest = reader.hasher.finalize().to_hex()[..16].to_owned();
    Ok(Loaded { tensors, digest })
}

/// Ce que l'en-tête d'un fichier `safetensors` décrit, vérifié contre la taille du fichier.
#[derive(Debug)]
pub(crate) struct Layout {
    /// Les tenseurs, **dans l'ordre du fichier**, sans chevauchement.
    pub entries: Vec<Entry>,
    /// Où commence la zone de données, depuis le début du fichier.
    pub data_start: u64,
    /// La taille de la zone de données.
    pub data_len: u64,
    /// Les métadonnées libres de l'en-tête, s'il y en a — gardées telles quelles par qui réécrit.
    pub metadata: Option<serde_json::Value>,
}

/// Lit le préfixe et l'en-tête, et laisse `reader` au début de la zone de données.
///
/// Partagé par le chargement et par la taille du vocabulaire : deux lecteurs du même format
/// auraient deux idées de ce qu'est un fichier valide.
pub(crate) fn read_layout<R: Read>(reader: &mut R, length: u64) -> Result<Layout, String> {
    let mut prefix = [0u8; 8];
    reader
        .read_exact(&mut prefix)
        .map_err(|_| "fichier trop court pour porter un en-tête".to_owned())?;
    let header_len = u64::from_le_bytes(prefix);
    if header_len > HEADER_CAP {
        return Err(format!(
            "en-tête de {header_len} octets annoncé, au-delà des {HEADER_CAP} du format"
        ));
    }
    let data_len = length
        .checked_sub(8)
        .and_then(|rest| rest.checked_sub(header_len))
        .ok_or_else(|| {
            format!("en-tête de {header_len} octets annoncé dans un fichier de {length}")
        })?;

    // Borné par `HEADER_CAP` juste au-dessus, et par la taille réelle du fichier.
    let mut header = vec![0u8; usize::try_from(header_len).map_err(|e| e.to_string())?];
    reader
        .read_exact(&mut header)
        .map_err(|e| format!("lecture de l'en-tête : {e}"))?;
    let (mut entries, metadata) = parse_header(&header, data_len)?;
    drop(header);

    // Dans l'ordre du fichier : c'est ce qui permet de tout lire d'une traite, donc de hacher en
    // passant, et de refuser deux tenseurs qui se chevauchent.
    // Par début, puis par fin : un tenseur vide qui partage son décalage avec un voisin passe
    // **avant** lui, sans quoi il semblerait commencer au milieu de ce voisin.
    entries.sort_by_key(|entry| (entry.begin, entry.end));
    let mut position = 0u64;
    for entry in &entries {
        if entry.begin < position {
            return Err(format!(
                "le tenseur {} chevauche le précédent (décalage {} < {position})",
                entry.name, entry.begin
            ));
        }
        position = entry.end;
    }

    Ok(Layout {
        entries,
        data_start: 8 + header_len,
        data_len,
        metadata,
    })
}

/// Lit et vérifie l'en-tête JSON.
fn parse_header(
    header: &[u8],
    data_len: u64,
) -> Result<(Vec<Entry>, Option<serde_json::Value>), String> {
    let value: serde_json::Value =
        serde_json::from_slice(header).map_err(|e| format!("en-tête illisible : {e}"))?;
    let map = value
        .as_object()
        .ok_or_else(|| "en-tête qui n'est pas un objet".to_owned())?;

    let mut entries = Vec::with_capacity(map.len());
    let mut metadata = None;
    for (name, description) in map {
        if name == "__metadata__" {
            metadata = Some(description.clone());
            continue;
        }
        entries.push(parse_entry(name, description, data_len)?);
    }
    Ok((entries, metadata))
}

/// Lit et vérifie la description d'un tenseur.
fn parse_entry(
    name: &str,
    description: &serde_json::Value,
    data_len: u64,
) -> Result<Entry, String> {
    let field = |key: &str| {
        description
            .get(key)
            .ok_or_else(|| format!("le tenseur {name} n'a pas de champ {key}"))
    };

    let dtype = field("dtype")?
        .as_str()
        .ok_or_else(|| format!("le type du tenseur {name} n'est pas une chaîne"))?;
    let kind =
        Kind::parse(dtype).ok_or_else(|| format!("le tenseur {name} est en {dtype}, non géré"))?;

    let shape = field("shape")?
        .as_array()
        .ok_or_else(|| format!("la forme du tenseur {name} n'est pas une liste"))?
        .iter()
        .map(|dim| {
            dim.as_u64()
                .and_then(|dim| usize::try_from(dim).ok())
                .ok_or_else(|| format!("une dimension du tenseur {name} n'est pas un entier"))
        })
        .collect::<Result<Vec<usize>, String>>()?;

    let offsets = field("data_offsets")?
        .as_array()
        .ok_or_else(|| format!("les décalages du tenseur {name} ne sont pas une liste"))?;
    let [begin, end] = offsets.as_slice() else {
        return Err(format!(
            "le tenseur {name} n'a pas exactement deux décalages"
        ));
    };
    let (Some(begin), Some(end)) = (begin.as_u64(), end.as_u64()) else {
        return Err(format!(
            "les décalages du tenseur {name} ne sont pas des entiers"
        ));
    };
    if begin > end || end > data_len {
        return Err(format!(
            "le tenseur {name} occupe {begin}..{end} dans une zone de {data_len} octets"
        ));
    }

    // La forme doit décrire **exactement** les octets annoncés. C'est ce qui borne l'allocation :
    // un tenseur ne peut pas demander plus que ce que le fichier contient.
    // Le produit des dimensions **non nulles** doit tenir, lui aussi : `candle` calcule les pas du
    // tenseur en multipliant les dimensions depuis la droite, zéros compris, et une forme
    // `[0, 2^40, 2^40]` — zéro élément, donc zéro octet, donc acceptée par le contrôle qui suit —
    // y débordait. Trouvé par une relecture le 2026-09-25 : une panique en profil de débogage.
    shape
        .iter()
        .filter(|dim| **dim != 0)
        .try_fold(1usize, |acc, dim| acc.checked_mul(*dim))
        .ok_or_else(|| format!("la forme du tenseur {name} déborde"))?;
    let elements = shape
        .iter()
        .try_fold(1u64, |acc, dim| acc.checked_mul(*dim as u64))
        .ok_or_else(|| format!("la forme du tenseur {name} déborde"))?;
    let bytes = elements
        .checked_mul(kind.width())
        .ok_or_else(|| format!("la taille du tenseur {name} déborde"))?;
    if bytes != end - begin {
        return Err(format!(
            "le tenseur {name} annonce {elements} éléments de {} octets sur {} octets",
            kind.width(),
            end - begin
        ));
    }

    Ok(Entry {
        name: name.to_owned(),
        kind,
        shape,
        begin,
        end,
    })
}

/// Lit `count` octets sans les garder — l'empreinte, elle, les voit passer.
fn skip<R: Read>(reader: &mut R, mut count: u64, buffer: &mut [u8]) -> Result<(), String> {
    while count > 0 {
        let take = usize::try_from(count.min(buffer.len() as u64)).unwrap_or(buffer.len());
        let chunk = buffer
            .get_mut(..take)
            .ok_or_else(|| "tampon de lecture trop court".to_owned())?;
        reader
            .read_exact(chunk)
            .map_err(|_| "fichier tronqué : les données s'arrêtent avant l'en-tête".to_owned())?;
        count -= take as u64;
    }
    Ok(())
}

/// Lit un tenseur, morceau par morceau, dans son tampon définitif.
fn read_tensor<R: Read>(
    reader: &mut R,
    entry: &Entry,
    buffer: &mut [u8],
    device: &Device,
) -> Result<Tensor, String> {
    let bytes = entry.end - entry.begin;
    let elements = usize::try_from(bytes / entry.kind.width())
        .map_err(|_| format!("le tenseur {} ne tient pas en mémoire", entry.name))?;
    let shape = entry.shape.clone();
    let made = match entry.kind {
        Kind::F32 => {
            let mut values: Vec<f32> = Vec::with_capacity(elements);
            fill(reader, bytes, buffer, |chunk| {
                values.extend(chunk.chunks_exact(4).map(|b| {
                    let mut raw = [0u8; 4];
                    raw.copy_from_slice(b);
                    f32::from_le_bytes(raw)
                }));
            })?;
            Tensor::from_vec(values, shape, device)
        }
        Kind::I64 => {
            let mut values: Vec<i64> = Vec::with_capacity(elements);
            fill(reader, bytes, buffer, |chunk| {
                values.extend(chunk.chunks_exact(8).map(|b| {
                    let mut raw = [0u8; 8];
                    raw.copy_from_slice(b);
                    i64::from_le_bytes(raw)
                }));
            })?;
            Tensor::from_vec(values, shape, device)
        }
    };
    made.map_err(|e| format!("tenseur {} : {e}", entry.name))
}

/// Lit `bytes` octets par morceaux et les passe à `sink`.
fn fill<R: Read>(
    reader: &mut R,
    mut bytes: u64,
    buffer: &mut [u8],
    mut sink: impl FnMut(&[u8]),
) -> Result<(), String> {
    while bytes > 0 {
        let take = usize::try_from(bytes.min(buffer.len() as u64)).unwrap_or(buffer.len());
        let chunk = buffer
            .get_mut(..take)
            .ok_or_else(|| "tampon de lecture trop court".to_owned())?;
        reader
            .read_exact(chunk)
            .map_err(|_| "fichier tronqué au milieu d'un tenseur".to_owned())?;
        sink(chunk);
        bytes -= take as u64;
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::load;
    use camino::Utf8PathBuf;
    use candle_core::{DType, Device, Tensor};
    use std::collections::HashMap;

    fn dir() -> (tempfile::TempDir, Utf8PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        (dir, path)
    }

    /// Un petit modèle écrit par `candle` lui-même : c'est la référence à laquelle ce lecteur
    /// doit être égal.
    fn written_by_candle(path: &Utf8PathBuf) -> HashMap<String, Tensor> {
        let device = Device::Cpu;
        let mut tensors = HashMap::new();
        tensors.insert(
            "embeddings.word_embeddings.weight".to_owned(),
            Tensor::arange(0f32, 3_000f32, &device)
                .unwrap()
                .reshape((1_000, 3))
                .unwrap(),
        );
        tensors.insert(
            "encoder.layer.0.bias".to_owned(),
            Tensor::new(&[0.5f32, -1.25, 3.0e-7], &device).unwrap(),
        );
        tensors.insert(
            "embeddings.position_ids".to_owned(),
            Tensor::arange(0i64, 512, &device)
                .unwrap()
                .reshape((1, 512))
                .unwrap(),
        );
        // Un tenseur plus grand qu'un morceau de lecture, pour que la frontière entre deux
        // morceaux soit traversée.
        tensors.insert(
            "encoder.layer.0.weight".to_owned(),
            Tensor::arange(0f32, 400_000f32, &device)
                .unwrap()
                .reshape((1_000, 400))
                .unwrap(),
        );
        candle_core::safetensors::save(&tensors, path.as_std_path()).unwrap();
        tensors
    }

    #[test]
    fn every_tensor_is_read_as_candle_wrote_it() {
        let (_guard, root) = dir();
        let path = root.join("model.safetensors");
        let written = written_by_candle(&path);

        let loaded = load(&path, &Device::Cpu).unwrap();
        assert_eq!(loaded.tensors.len(), written.len());
        for (name, expected) in &written {
            let found = &loaded.tensors[name];
            assert_eq!(found.dims(), expected.dims(), "{name}");
            assert_eq!(found.dtype(), expected.dtype(), "{name}");
            let equal = match expected.dtype() {
                DType::I64 => {
                    found.flatten_all().unwrap().to_vec1::<i64>().unwrap()
                        == expected.flatten_all().unwrap().to_vec1::<i64>().unwrap()
                }
                _ => {
                    found.flatten_all().unwrap().to_vec1::<f32>().unwrap()
                        == expected.flatten_all().unwrap().to_vec1::<f32>().unwrap()
                }
            };
            assert!(equal, "{name} diffère de ce que candle a écrit");
        }
    }

    #[test]
    fn the_digest_is_the_one_the_provenance_already_uses() {
        // Une empreinte qui changerait de calcul rendrait tous les magasins existants « d'un
        // autre modèle », et `VectorWriter::open` refuserait d'y ajouter.
        let (_guard, root) = dir();
        let path = root.join("model.safetensors");
        written_by_candle(&path);
        let loaded = load(&path, &Device::Cpu).unwrap();
        assert_eq!(loaded.digest, crate::pass::Identity::digest_of(&path));
    }

    /// Un fichier `safetensors` écrit à la main, pour fabriquer ce que `candle` refuserait
    /// d'écrire.
    fn raw(path: &Utf8PathBuf, header: &str, data: &[u8]) {
        let mut bytes = (header.len() as u64).to_le_bytes().to_vec();
        bytes.extend_from_slice(header.as_bytes());
        bytes.extend_from_slice(data);
        std::fs::write(path.as_std_path(), bytes).unwrap();
    }

    fn refused(header: &str, data: &[u8]) -> String {
        let (_guard, root) = dir();
        let path = root.join("model.safetensors");
        raw(&path, header, data);
        load(&path, &Device::Cpu).expect_err("un fichier malformé a été accepté")
    }

    #[test]
    fn a_well_formed_hand_written_file_is_accepted() {
        // Le contrôle inverse des refus qui suivent : sans lui, un lecteur qui refuse tout les
        // passerait tous.
        let (_guard, root) = dir();
        let path = root.join("model.safetensors");
        raw(
            &path,
            r#"{"a":{"dtype":"F32","shape":[2],"data_offsets":[0,8]}}"#,
            &[0, 0, 128, 63, 0, 0, 0, 64],
        );
        let loaded = load(&path, &Device::Cpu).unwrap();
        assert_eq!(
            loaded.tensors["a"].to_vec1::<f32>().unwrap(),
            vec![1.0, 2.0]
        );
    }

    #[test]
    fn a_header_longer_than_the_file_is_refused_before_allocating() {
        let (_guard, root) = dir();
        let path = root.join("model.safetensors");
        let mut bytes = (50_000_000u64).to_le_bytes().to_vec();
        bytes.extend_from_slice(b"{}");
        std::fs::write(path.as_std_path(), bytes).unwrap();
        let error = load(&path, &Device::Cpu).unwrap_err();
        assert!(error.contains("50000000"), "{error}");
    }

    #[test]
    fn an_absurd_header_length_is_refused() {
        let (_guard, root) = dir();
        let path = root.join("model.safetensors");
        std::fs::write(path.as_std_path(), u64::MAX.to_le_bytes()).unwrap();
        let error = load(&path, &Device::Cpu).unwrap_err();
        assert!(error.contains("au-delà"), "{error}");
    }

    #[test]
    fn a_file_shorter_than_its_prefix_is_refused() {
        let (_guard, root) = dir();
        let path = root.join("model.safetensors");
        std::fs::write(path.as_std_path(), [1, 2, 3]).unwrap();
        assert!(load(&path, &Device::Cpu).is_err());
    }

    #[test]
    fn malformed_headers_are_refused_with_a_reason() {
        for (header, data, expected) in [
            ("pas du json", &[][..], "illisible"),
            ("[1,2]", &[][..], "pas un objet"),
            (
                r#"{"a":{"shape":[1],"data_offsets":[0,4]}}"#,
                &[0; 4][..],
                "dtype",
            ),
            (
                r#"{"a":{"dtype":"F16","shape":[2],"data_offsets":[0,4]}}"#,
                &[0; 4][..],
                "non géré",
            ),
            (
                r#"{"a":{"dtype":"F32","shape":"2","data_offsets":[0,8]}}"#,
                &[0; 8][..],
                "forme",
            ),
            (
                r#"{"a":{"dtype":"F32","shape":[-2],"data_offsets":[0,8]}}"#,
                &[0; 8][..],
                "entier",
            ),
            (
                r#"{"a":{"dtype":"F32","shape":[2],"data_offsets":[0]}}"#,
                &[0; 8][..],
                "deux décalages",
            ),
            (
                r#"{"a":{"dtype":"F32","shape":[2],"data_offsets":[8,0]}}"#,
                &[0; 8][..],
                "occupe",
            ),
            (
                r#"{"a":{"dtype":"F32","shape":[2],"data_offsets":[0,16]}}"#,
                &[0; 8][..],
                "occupe",
            ),
            (
                r#"{"a":{"dtype":"F32","shape":[3],"data_offsets":[0,8]}}"#,
                &[0; 8][..],
                "annonce",
            ),
            (
                r#"{"a":{"dtype":"F32","shape":[4294967296,4294967296,4294967296],"data_offsets":[0,8]}}"#,
                &[0; 8][..],
                "déborde",
            ),
            (
                r#"{"a":{"dtype":"F32","shape":[2],"data_offsets":[0,8]},"b":{"dtype":"F32","shape":[1],"data_offsets":[4,8]}}"#,
                &[0; 8][..],
                "chevauche",
            ),
        ] {
            let error = refused(header, data);
            assert!(error.contains(expected), "{header} → {error}");
        }
    }

    #[test]
    fn a_shape_with_a_zero_does_not_hide_an_overflow() {
        // Zéro élément, donc zéro octet — et des pas qui débordent dans `candle`.
        let error = refused(
            r#"{"a":{"dtype":"F32","shape":[0,1099511627776,1099511627776],"data_offsets":[0,0]}}"#,
            &[],
        );
        assert!(error.contains("déborde"), "{error}");
    }

    #[test]
    fn an_empty_tensor_beside_its_neighbour_is_valid() {
        // Ce que `candle` écrit lui-même : `z` vide, au même décalage que le début de `a`.
        let (_guard, root) = dir();
        let path = root.join("model.safetensors");
        raw(
            &path,
            r#"{"p":{"dtype":"I64","shape":[1],"data_offsets":[0,8]},"z":{"dtype":"I64","shape":[1,0],"data_offsets":[8,8]},"a":{"dtype":"F32","shape":[2],"data_offsets":[8,16]}}"#,
            &[1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 128, 63, 0, 0, 0, 64],
        );
        let loaded = load(&path, &Device::Cpu).unwrap();
        assert_eq!(loaded.tensors.len(), 3);
        assert_eq!(
            loaded.tensors["a"].to_vec1::<f32>().unwrap(),
            vec![1.0, 2.0]
        );
    }

    #[test]
    fn metadata_is_skipped_not_read_as_a_tensor() {
        let (_guard, root) = dir();
        let path = root.join("model.safetensors");
        raw(
            &path,
            r#"{"__metadata__":{"format":"pt"},"a":{"dtype":"I64","shape":[1],"data_offsets":[0,8]}}"#,
            &7i64.to_le_bytes(),
        );
        let loaded = load(&path, &Device::Cpu).unwrap();
        assert_eq!(loaded.tensors.len(), 1);
        assert_eq!(loaded.tensors["a"].to_vec1::<i64>().unwrap(), vec![7]);
    }
}
