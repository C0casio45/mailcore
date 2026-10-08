//! L'installation du modèle : vérifier ce qu'on a reçu, puis le tailler dans le store.
//!
//! ## Pourquoi le produit ne télécharge rien
//!
//! `docs/PHASE-4.md` n'accorde au projet qu'une requête réseau : l'installation du modèle, sur
//! une commande tapée par un humain, qui dit d'abord ce qu'elle va chercher, où, et combien ça
//! pèse. Ce module va plus loin que ce que la phase permet, et c'est voulu : **il ne télécharge
//! pas du tout.** Il dit quoi télécharger et d'où — trois fichiers d'une révision épinglée —, et
//! l'humain les récupère avec l'outil de son choix. Le produit n'embarque ainsi aucun client
//! réseau pour un fichier de 470 Mo, et « zéro requête » reste vrai par construction, comme dans
//! `mailcal`.
//!
//! ## Ce qui est vérifié
//!
//! **Chaque fichier reçu, octet par octet, contre une empreinte écrite ici.** Un modèle est du code
//! exécuté sur tout le corpus : un fichier altéré en route est le plus gros trou que le projet
//! puisse ouvrir. Les empreintes BLAKE3 ci-dessous ont été calculées le 2026-10-08 sur des fichiers
//! dont les SHA-256 ont été comparés, le même jour, à ceux que Hugging Face publie pour cette
//! révision — identiques pour les poids et le tokeniseur ; `config.json`, hors LFS, a le même
//! identifiant de contenu git.
//!
//! Puis **le modèle taillé** — voir [`crate::trim`] —, contre sa propre empreinte : la taille ne
//! dépend que de sa source, donc son résultat aussi est une constante. Une empreinte qui ne
//! correspond pas fait retirer ce qui a été écrit.

use std::io::Read;

use camino::{Utf8Path, Utf8PathBuf};

use crate::{Error, Result};

/// Le modèle installable, et la révision épinglée d'où viennent ses fichiers.
pub const REPOSITORY: &str = "intfloat/multilingual-e5-small";

/// La révision de [`REPOSITORY`], un commit : une branche bougerait sous l'empreinte.
pub const REVISION: &str = "614241f622f53c4eeff9890bdc4f31cfecc418b3";

/// Un fichier à recevoir.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Expected {
    /// Son nom, dans le répertoire source et dans le dépôt.
    pub name: &'static str,
    /// Sa taille exacte, en octets — vérifiée d'abord, parce qu'elle coûte un `stat`.
    pub size: u64,
    /// Son empreinte BLAKE3, en hexadécimal.
    pub blake3: &'static str,
}

/// Les trois fichiers de la révision épinglée.
pub const FILES: [Expected; 3] = [
    Expected {
        name: "config.json",
        size: 655,
        blake3: "008002d237332c95ffe7c26426ce14366bba14b85a7980f9c84992eb2c756242",
    },
    Expected {
        name: "tokenizer.json",
        size: 17_082_730,
        blake3: "8e30c0bbbb7ebc0f4b46034ecfcb413f69e2b1c3aef9dfbb421c607f2355bad2",
    },
    Expected {
        name: "model.safetensors",
        size: 470_641_600,
        blake3: "a02bcc5c7b413904935968d95ceea52053ade797b1d2081d590d72ba4b2471c3",
    },
];

/// L'empreinte des poids taillés, au format de la provenance : seize caractères BLAKE3.
///
/// **À mettre à jour en même temps que la règle de [`crate::trim`]** : `cargo xtask
/// model-trim-bert` l'imprime. Une règle changée sans elle fait refuser toutes les installations,
/// ce qui est le bon sens de l'échec.
pub const TRIMMED_DIGEST: &str = "a043ccf1d02843c5";

/// Ce que le modèle taillé occupe sur le disque, ses trois fichiers ensemble — pour le dire avant.
pub const TRIMMED_BYTES: u64 = 266_777_950;

/// L'adresse d'un fichier de la révision épinglée.
#[must_use]
pub fn url(name: &str) -> String {
    format!("https://huggingface.co/{REPOSITORY}/resolve/{REVISION}/{name}")
}

/// Ce qu'une installation a fait.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Installed {
    /// Où le modèle a été écrit.
    pub target: Utf8PathBuf,
    /// Le bilan de la taille.
    pub trim: crate::trim::Report,
}

/// L'empreinte BLAKE3 complète d'un fichier, lu par morceaux.
///
/// # Errors
///
/// [`Error::Model`] si le fichier n'est pas lisible.
pub fn blake3_of(path: &Utf8Path) -> Result<String> {
    let mut file = std::fs::File::open(path.as_std_path())
        .map_err(|e| model_error(path, format!("ouverture : {e}")))?;
    let mut hasher = blake3::Hasher::new();
    let mut buffer = vec![0u8; 1 << 20];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|e| model_error(path, format!("lecture : {e}")))?;
        if read == 0 {
            break;
        }
        hasher.update(buffer.get(..read).unwrap_or_default());
    }
    Ok(hasher.finalize().to_hex().to_string())
}

/// Vérifie les trois fichiers de `source` contre [`FILES`].
///
/// # Errors
///
/// [`Error::Model`] qui nomme le premier fichier absent, d'une autre taille ou d'une autre
/// empreinte — avec l'empreinte trouvée, pour qu'on puisse la comparer à la main.
pub fn verify(source: &Utf8Path, expected: &[Expected]) -> Result<()> {
    for file in expected {
        let path = source.join(file.name);
        let size = std::fs::metadata(path.as_std_path())
            .map_err(|_| model_error(&path, "absent".to_owned()))?
            .len();
        if size != file.size {
            return Err(model_error(
                &path,
                format!("{size} octets, {} attendus", file.size),
            ));
        }
        let found = blake3_of(&path)?;
        if found != file.blake3 {
            return Err(model_error(
                &path,
                format!(
                    "empreinte {found}, {} attendue : ce n'est pas le fichier de la révision \
                     {REVISION}",
                    file.blake3
                ),
            ));
        }
    }
    Ok(())
}

/// Installe le modèle depuis `source` dans le store `root`.
///
/// # Errors
///
/// [`Error::Model`] si un fichier source ne correspond pas, si un modèle est déjà installé, ou si
/// le modèle taillé n'a pas l'empreinte attendue — auquel cas ce qui a été écrit est retiré.
pub fn install(source: &Utf8Path, root: &Utf8Path) -> Result<Installed> {
    install_checked(source, root, &FILES, TRIMMED_DIGEST)
}

/// [`install`] contre des empreintes données — pour les tests, sur un modèle minuscule.
///
/// # Errors
///
/// Les mêmes que [`install`].
pub fn install_checked(
    source: &Utf8Path,
    root: &Utf8Path,
    expected: &[Expected],
    trimmed: &str,
) -> Result<Installed> {
    verify(source, expected)?;
    let target = crate::bert::default_dir(root);
    let trim = crate::trim::trim(source, &target)?;
    if trim.digest != trimmed {
        // Rien qui ressemble à un modèle ne doit rester : le démon le chargerait.
        for name in ["config.json", "tokenizer.json", "model.safetensors"] {
            let _ = std::fs::remove_file(target.join(name).as_std_path());
        }
        return Err(model_error(
            &target,
            format!(
                "modèle taillé d'empreinte {}, {trimmed} attendue : la règle de taille a changé \
                 sans que l'empreinte suive",
                trim.digest
            ),
        ));
    }
    Ok(Installed { target, trim })
}

fn model_error(path: &Utf8Path, reason: String) -> Error {
    Error::Model {
        path: path.to_owned(),
        reason,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::{Expected, blake3_of, install_checked};
    use crate::testing::{TINY_PIECES, tiny_bert};
    use camino::Utf8PathBuf;

    /// Le transformeur minuscule, et ses empreintes réelles.
    fn source() -> (tempfile::TempDir, Utf8PathBuf, Vec<Expected>) {
        let dir = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        let source = root.join("source");
        tiny_bert(&source, TINY_PIECES);
        let expected = ["config.json", "tokenizer.json", "model.safetensors"]
            .into_iter()
            .map(|name| {
                let path = source.join(name);
                Expected {
                    name,
                    size: std::fs::metadata(path.as_std_path()).unwrap().len(),
                    blake3: Box::leak(blake3_of(&path).unwrap().into_boxed_str()),
                }
            })
            .collect();
        (dir, root, expected)
    }

    fn trimmed_digest(root: &Utf8PathBuf) -> String {
        let probe = root.join("probe");
        crate::trim::trim(&root.join("source"), &probe)
            .unwrap()
            .digest
    }

    #[test]
    fn verified_files_are_installed_where_the_daemon_looks() {
        let (_dir, root, expected) = source();
        let digest = trimmed_digest(&root);
        let store = root.join("store");
        let installed = install_checked(&root.join("source"), &store, &expected, &digest).unwrap();
        assert_eq!(installed.target, crate::bert::default_dir(&store));
        crate::bert::Bert::load(&installed.target).unwrap();
    }

    #[test]
    fn a_file_altered_on_the_way_is_refused_before_anything_is_written() {
        let (_dir, root, expected) = source();
        let digest = trimmed_digest(&root);
        // Un octet changé, la taille gardée : seule l'empreinte peut le voir.
        let weights = root.join("source").join("model.safetensors");
        let mut bytes = std::fs::read(weights.as_std_path()).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 1;
        std::fs::write(weights.as_std_path(), bytes).unwrap();

        let store = root.join("store");
        let error = install_checked(&root.join("source"), &store, &expected, &digest)
            .unwrap_err()
            .to_string();
        assert!(error.contains("empreinte"), "{error}");
        assert!(!crate::bert::default_dir(&store).exists());
    }

    #[test]
    fn a_trim_that_drifted_from_its_digest_leaves_no_model() {
        let (_dir, root, expected) = source();
        let store = root.join("store");
        let error = install_checked(&root.join("source"), &store, &expected, "0000000000000000")
            .unwrap_err()
            .to_string();
        assert!(error.contains("règle de taille"), "{error}");
        assert!(
            !crate::bert::default_dir(&store)
                .join("config.json")
                .exists()
        );
    }
}
