//! L'installation du modèle : vérifier ce qu'on a reçu, puis le tailler dans le store.
//!
//! ## Pourquoi le produit ne télécharge rien
//!
//! `docs/PHASE-4.md` n'accorde au projet qu'une requête réseau : l'installation du modèle, sur
//! une commande tapée par un humain, qui dit d'abord ce qu'elle va chercher, où, et combien ça
//! pèse. Ce module va plus loin que ce que la phase permet, et c'est voulu : **il ne télécharge
//! pas du tout.** Il dit quoi télécharger et d'où — quatre fichiers d'une révision épinglée —, et
//! l'humain les récupère avec l'outil de son choix. Le produit n'embarque ainsi aucun client
//! réseau pour un fichier de 214 Mo, et « zéro requête » reste vrai par construction, comme dans
//! `mailcal`.
//!
//! ## Quel modèle, depuis le 2026-10-10
//!
//! **`granite-embedding-107m-multilingual`**, d'IBM, à la place de `multilingual-e5-small`. Il
//! l'a remplacé selon une règle écrite avant la mesure — six conditions, six tenues : voir
//! l'entrée du 2026-10-10 de `docs/PHASE-4.md`. Deux choses le distinguent de `e5` pour
//! l'installation :
//!
//! - **il met en commun au premier jeton**, ce que dit `1_Pooling/config.json`. Ce fichier est
//!   donc un fichier **reçu**, épinglé et vérifié comme les poids : sans lui, le modèle ferait la
//!   moyenne et rendrait des vecteurs bien formés et faux ;
//! - **il n'a pas de préfixe**, et son producteur ne publie aucun fichier qui le dise. Sans
//!   fichier, [`crate::bert::Bert`] applique ceux de `e5`. La déclaration est donc **écrite par
//!   mailcore** — [`DECLARED`] —, d'après la fiche du modèle, qui encode requêtes et passages tels
//!   quels ; c'est le réglage mesuré.
//!
//! ## Ce qui est vérifié
//!
//! **Chaque fichier reçu, octet par octet, contre une empreinte écrite ici.** Un modèle est du code
//! exécuté sur tout le corpus : un fichier altéré en route est le plus gros trou que le projet
//! puisse ouvrir. Les empreintes BLAKE3 ci-dessous ont été calculées le 2026-10-10 sur des fichiers
//! comparés, le même jour, à ce que Hugging Face publie pour cette révision : le SHA-256 pour les
//! poids, l'identifiant de contenu git pour les trois autres, hors LFS — identiques tous les quatre.
//!
//! Puis **le modèle taillé** — voir [`crate::trim`] —, contre sa propre empreinte : la taille ne
//! dépend que de sa source, donc son résultat aussi est une constante. Une empreinte qui ne
//! correspond pas fait retirer ce qui a été écrit.
//!
//! Et **l'empreinte d'identité**, avant toute écriture : celle que porteront les vecteurs, qui
//! nomme les poids **et** ce qui en fait un vecteur — mise en commun et préfixes. Elle se calcule
//! sur la source, la déclaration de mailcore par-dessus, et se compare à [`IDENTITY`]. Elle couvre
//! un fichier de réglages qui n'est ni reçu ni déclaré, et que la taille recopierait tel quel —
//! trouvé par une relecture le 2026-10-10, quand le modèle installé était `e5`, qui n'épinglait
//! aucun des deux.
//!
//! **Avec granite, elle ne voit plus de fichier égaré**, et ce n'est pas elle qui le refuse : la
//! mise en commun est reçue, donc vérifiée par son empreinte, et un fichier de préfixes dans la
//! source est **masqué** par la déclaration dans ce calcul. C'est
//! [`crate::trim::trim_declaring`] qui le refuse, avant toute écriture — deux versions du même
//! réglage. Ce refus n'est donc pas redondant avec le contrôle d'identité : sans lui, le fichier
//! serait mis en place deux fois, et le second renommage échouerait après les premiers.
//!
//! **Sur la source, pas sur le modèle installé.** La première correction chargeait le modèle
//! taillé pour lire son empreinte — donc après l'avoir mis où le démon regarde, qui pouvait le
//! charger pendant les secondes de la vérification et vectoriser avec des réglages qu'on allait
//! refuser. La seconde relecture l'a vu ; la règle est celle de la taille, tout ce qui peut être
//! refusé l'est avant la première écriture. La déclaration suit la même règle : elle est écrite
//! **par** la taille, avant `config.json`, et pas après elle.

use std::io::Read;

use camino::{Utf8Path, Utf8PathBuf};

use crate::{Error, Result};

/// Le modèle installable, et la révision épinglée d'où viennent ses fichiers.
pub const REPOSITORY: &str = "ibm-granite/granite-embedding-107m-multilingual";

/// La révision de [`REPOSITORY`], un commit : une branche bougerait sous l'empreinte.
pub const REVISION: &str = "d6cffd338414d6a1c1f5decfad5fec62eebc90d5";

/// Un fichier à recevoir.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Expected {
    /// Son chemin, relatif au répertoire source comme au dépôt.
    pub name: &'static str,
    /// Sa taille exacte, en octets — vérifiée d'abord, parce qu'elle coûte un `stat`.
    pub size: u64,
    /// Son empreinte BLAKE3, en hexadécimal.
    pub blake3: &'static str,
}

/// Les quatre fichiers de la révision épinglée.
pub const FILES: [Expected; 4] = [
    Expected {
        name: "config.json",
        size: 697,
        blake3: "36eba5e7532f0c13224ded7f0f16808246f11f0683b49332f8fb9e1739b17dd7",
    },
    Expected {
        name: "tokenizer.json",
        size: 9_081_382,
        blake3: "de0fd147e1d2a523a8f5a0ffe7444463add507c8bc8c716be0d8bd21f966db82",
    },
    Expected {
        name: "model.safetensors",
        size: 214_000_232,
        blake3: "74309808c75696d692701ba2bf1fccc26102ce78f26c1796e99d16e98ccd9171",
    },
    Expected {
        name: "1_Pooling/config.json",
        size: 191,
        blake3: "a3f861aba8c4d402f4a127f6d019aa6e47475542024bd994c5f4a80dd354fb8d",
    },
];

/// Les réglages que mailcore écrit lui-même, faute de producteur qui les publie : `(chemin,
/// contenu)`.
///
/// Les préfixes vides de `granite-embedding`, octet pour octet le fichier avec lequel il a été
/// mesuré.
pub const DECLARED: [(&str, &str); 1] = [(
    "config_sentence_transformers.json",
    "{\n  \"_origine\": \"écrit par mailcore le 2026-10-10 d'après la fiche du modèle, qui encode \
     requêtes et passages sans préfixe ; le producteur ne publie pas ce fichier\",\n  \
     \"prompts\": {}\n}\n",
)];

/// L'empreinte des poids taillés, au format de la provenance : seize caractères BLAKE3.
///
/// **À mettre à jour en même temps que la règle de [`crate::trim`]** : `cargo xtask
/// model-trim-bert` l'imprime. Une règle changée sans elle fait refuser toutes les installations,
/// ce qui est le bon sens de l'échec.
pub const TRIMMED_DIGEST: &str = "a70907b53c80256d";

/// L'empreinte d'identité du modèle installé, celle que portent ses vecteurs : les poids taillés,
/// la mise en commun au premier jeton, aucun préfixe.
///
/// Elle n'est celle des poids seuls qu'avec les réglages de `e5` — c'était le cas jusqu'au
/// 2026-10-10, et ce ne l'est plus.
pub const IDENTITY: &str = "c7af5a7f438c4a3c";

/// Ce que le modèle taillé occupe sur le disque, ses cinq fichiers ensemble — pour le dire avant.
pub const TRIMMED_BYTES: u64 = 114_147_898;

/// Ce qu'une installation vérifie, et ce qu'elle doit obtenir.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pinned<'a> {
    /// Les fichiers reçus.
    pub files: &'a [Expected],
    /// Les réglages que mailcore écrit à côté : `(chemin, contenu)`.
    pub declared: &'a [(&'a str, &'a str)],
    /// L'empreinte des poids taillés.
    pub trimmed: &'a str,
    /// L'empreinte d'identité du modèle installé.
    pub identity: &'a str,
}

/// Le modèle que [`install`] installe.
pub const PINNED: Pinned<'static> = Pinned {
    files: &FILES,
    declared: &DECLARED,
    trimmed: TRIMMED_DIGEST,
    identity: IDENTITY,
};

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
    /// L'empreinte d'identité du modèle installé, celle que porteront ses vecteurs.
    pub identity: String,
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

/// Vérifie les fichiers de `source` contre `expected`.
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
/// [`Error::Model`] si un fichier source ne correspond pas, si les réglages de la source changent
/// le modèle, si un modèle est déjà installé, ou si le modèle taillé n'a pas l'empreinte attendue
/// — auquel cas ce qui a été écrit est retiré.
pub fn install(source: &Utf8Path, root: &Utf8Path) -> Result<Installed> {
    install_checked(source, root, &PINNED)
}

/// [`install`] contre un modèle épinglé donné — pour les tests, sur un modèle minuscule.
///
/// # Errors
///
/// Les mêmes que [`install`].
pub fn install_checked(
    source: &Utf8Path,
    root: &Utf8Path,
    pinned: &Pinned<'_>,
) -> Result<Installed> {
    verify(source, pinned.files)?;
    // Ce que porteront les vecteurs : les poids, et les réglages de la source sous la déclaration.
    let identity = crate::bert::settled_in(source, pinned.trimmed, pinned.declared)?;
    if identity != pinned.identity {
        return Err(model_error(
            source,
            format!(
                "les réglages de la source ({}) feraient un autre modèle que {REPOSITORY} — \
                 empreinte {identity}, {} attendue — et en changeraient chaque vecteur : la \
                 révision épinglée ne les publie pas ainsi, retirez-les — rien n'a été écrit",
                crate::bert::SETTINGS.join(", "),
                pinned.identity
            ),
        ));
    }
    let target = crate::bert::default_dir(root);
    let trim = crate::trim::trim_declaring(source, &target, pinned.declared)?;
    if trim.digest != pinned.trimmed {
        discard(&target);
        return Err(model_error(
            &target,
            format!(
                "modèle taillé d'empreinte {}, {} attendue : la règle de taille a changé sans que \
                 l'empreinte suive",
                trim.digest, pinned.trimmed
            ),
        ));
    }
    Ok(Installed {
        target,
        trim,
        identity,
    })
}

/// Retire tout ce qu'une installation refusée a écrit : rien qui ressemble à un modèle ne doit
/// rester, le démon le chargerait.
fn discard(target: &Utf8Path) {
    for name in crate::bert::MODEL_FILES {
        let _ = std::fs::remove_file(target.join(name).as_std_path());
    }
    // Vide, il ne gênerait rien ; retiré quand même, pour qu'un refus ne laisse aucune trace.
    let _ = std::fs::remove_dir(target.join("1_Pooling").as_std_path());
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
    use super::{DECLARED, Expected, FILES, IDENTITY, Pinned, TRIMMED_DIGEST, blake3_of};
    use super::{install_checked, verify};
    use crate::bert::{Bert, Role};
    use crate::pass::Encoder;
    use crate::testing::{TINY_PIECES, tiny_bert};
    use camino::{Utf8Path, Utf8PathBuf};

    /// Le `1_Pooling/config.json` de la révision épinglée, mot pour mot.
    const CLS: &str = "{\n  \"word_embedding_dimension\": 384,\n  \"pooling_mode_cls_token\": \
                       true,\n  \"pooling_mode_mean_tokens\": false,\n  \
                       \"pooling_mode_max_tokens\": false,\n  \
                       \"pooling_mode_mean_sqrt_len_tokens\": false\n}\n";

    /// Ajoute un fichier à la source.
    fn with_setting(root: &Utf8Path, name: &str, text: &str) {
        let path = root.join("source").join(name);
        std::fs::create_dir_all(path.parent().unwrap().as_std_path()).unwrap();
        std::fs::write(path.as_std_path(), text).unwrap();
    }

    /// Les empreintes réelles des fichiers `names` de la source.
    fn expected(root: &Utf8Path, names: &[&'static str]) -> Vec<Expected> {
        names
            .iter()
            .map(|name| {
                let path = root.join("source").join(name);
                Expected {
                    name,
                    size: std::fs::metadata(path.as_std_path()).unwrap().len(),
                    blake3: Box::leak(blake3_of(&path).unwrap().into_boxed_str()),
                }
            })
            .collect()
    }

    const RECEIVED: [&str; 3] = ["config.json", "tokenizer.json", "model.safetensors"];

    /// Le transformeur minuscule, aux réglages de `e5`, et ses empreintes réelles.
    fn source() -> (tempfile::TempDir, Utf8PathBuf, Vec<Expected>) {
        let dir = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        tiny_bert(&root.join("source"), TINY_PIECES);
        let expected = expected(&root, &RECEIVED);
        (dir, root, expected)
    }

    /// Le même, avec la mise en commun reçue et les préfixes déclarés de la révision épinglée :
    /// la forme de l'installation de `granite-embedding`.
    fn granite_like() -> (tempfile::TempDir, Utf8PathBuf, Vec<Expected>) {
        let (dir, root, _) = source();
        with_setting(&root, "1_Pooling/config.json", CLS);
        let mut names = RECEIVED.to_vec();
        names.push("1_Pooling/config.json");
        let expected = expected(&root, &names);
        (dir, root, expected)
    }

    fn trimmed_digest(root: &Utf8Path) -> String {
        let probe = root.join("probe");
        crate::trim::trim(&root.join("source"), &probe)
            .unwrap()
            .digest
    }

    /// Un modèle aux réglages de `e5` : son identité est l'empreinte de ses poids.
    fn e5_like<'a>(files: &'a [Expected], digest: &'a str) -> Pinned<'a> {
        Pinned {
            files,
            declared: &[],
            trimmed: digest,
            identity: digest,
        }
    }

    #[test]
    fn verified_files_are_installed_where_the_daemon_looks() {
        let (_dir, root, expected) = source();
        let digest = trimmed_digest(&root);
        let store = root.join("store");
        let installed =
            install_checked(&root.join("source"), &store, &e5_like(&expected, &digest)).unwrap();
        assert_eq!(installed.target, crate::bert::default_dir(&store));
        assert_eq!(installed.identity, digest);
        Bert::load(&installed.target).unwrap();
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
        let error = install_checked(&root.join("source"), &store, &e5_like(&expected, &digest))
            .unwrap_err()
            .to_string();
        assert!(error.contains("empreinte"), "{error}");
        assert!(!crate::bert::default_dir(&store).exists());
    }

    #[test]
    fn a_trim_that_drifted_from_its_digest_leaves_no_model() {
        let (_dir, root, expected) = source();
        let store = root.join("store");
        let drifted = "0000000000000000";
        let error = install_checked(&root.join("source"), &store, &e5_like(&expected, drifted))
            .unwrap_err()
            .to_string();
        assert!(error.contains("règle de taille"), "{error}");
        assert!(
            !crate::bert::default_dir(&store)
                .join("config.json")
                .exists()
        );
    }

    #[test]
    fn a_stray_settings_file_that_changes_the_vectors_is_refused() {
        // Les poids sont les bons — l'empreinte de la taille passe —, mais un fichier de préfixes
        // vide ôterait à `e5` « query: » et « passage: ». Rien ne doit rester.
        let (_dir, root, expected) = source();
        let digest = trimmed_digest(&root);
        with_setting(
            &root,
            "config_sentence_transformers.json",
            r#"{"prompts": {}}"#,
        );
        let store = root.join("store");
        let error = install_checked(&root.join("source"), &store, &e5_like(&expected, &digest))
            .unwrap_err()
            .to_string();
        assert!(error.contains("réglages"), "{error}");
        // Refusé **avant** d'écrire : un modèle posé puis retiré aurait pu être chargé par le démon
        // entre les deux.
        assert!(!crate::bert::default_dir(&store).exists());
    }

    #[test]
    fn an_unreadable_pooling_file_is_refused_before_anything_is_written() {
        let (_dir, root, expected) = source();
        let digest = trimmed_digest(&root);
        with_setting(
            &root,
            "1_Pooling/config.json",
            r#"{"pooling_mode_max_tokens": true}"#,
        );
        let store = root.join("store");
        let error = install_checked(&root.join("source"), &store, &e5_like(&expected, &digest))
            .unwrap_err()
            .to_string();
        assert!(error.contains("mise en commun"), "{error}");
        assert!(!crate::bert::default_dir(&store).exists());
    }

    #[test]
    fn a_settings_file_that_changes_nothing_is_accepted() {
        // Le contrôle inverse : la révision de `e5` publiait `1_Pooling/config.json`, en moyenne.
        // Un fichier qui redit le réglage par défaut ne change pas le modèle, et s'installe.
        let (_dir, root, expected) = source();
        let digest = trimmed_digest(&root);
        with_setting(
            &root,
            "1_Pooling/config.json",
            r#"{"pooling_mode_cls_token": false, "pooling_mode_mean_tokens": true}"#,
        );
        let store = root.join("store");
        let installed =
            install_checked(&root.join("source"), &store, &e5_like(&expected, &digest)).unwrap();
        assert!(installed.target.join("1_Pooling/config.json").is_file());
    }

    #[test]
    fn the_declared_prompts_are_installed_and_name_the_model() {
        let (_dir, root, expected) = granite_like();
        let digest = trimmed_digest(&root);
        let source = root.join("source");
        let identity = crate::bert::settled_in(&source, &digest, &DECLARED).unwrap();
        let pinned = Pinned {
            files: &expected,
            declared: &DECLARED,
            trimmed: &digest,
            identity: &identity,
        };
        let store = root.join("store");
        let installed = install_checked(&source, &store, &pinned).unwrap();

        // La déclaration est écrite telle quelle, à côté de la mise en commun reçue.
        let (name, text) = DECLARED[0];
        let written = std::fs::read_to_string(installed.target.join(name).as_std_path()).unwrap();
        assert_eq!(written, text);
        assert!(installed.target.join("1_Pooling/config.json").is_file());
        // Le modèle chargé — par l'autre chemin, celui du démon — est celui qu'on attendait : ni
        // les préfixes de `e5`, ni l'empreinte des poids seuls.
        let model = Bert::load(&installed.target).unwrap();
        assert_eq!(model.identity().digest, installed.identity);
        assert_ne!(installed.identity, digest);
        let tokens = model.tokens("bonjour", Role::Query).unwrap();
        assert_eq!(tokens[1], "▁bon", "{tokens:?}");
    }

    #[test]
    fn a_prompts_file_beside_the_declared_one_is_refused_before_anything_is_written() {
        // Même identique à la déclaration : deux versions du même fichier, et rien pour dire
        // laquelle est vraie. Le répertoire où granite a été mesuré en porte une — c'est le cas
        // le plus probable.
        let (_dir, root, expected) = granite_like();
        let digest = trimmed_digest(&root);
        let source = root.join("source");
        let identity = crate::bert::settled_in(&source, &digest, &DECLARED).unwrap();
        with_setting(&root, DECLARED[0].0, DECLARED[0].1);
        let pinned = Pinned {
            files: &expected,
            declared: &DECLARED,
            trimmed: &digest,
            identity: &identity,
        };
        let store = root.join("store");
        let error = install_checked(&source, &store, &pinned)
            .unwrap_err()
            .to_string();
        assert!(error.contains("retirez-le"), "{error}");
        assert!(!crate::bert::default_dir(&store).exists());
    }

    #[test]
    fn a_missing_pooling_file_is_refused() {
        // Sans `1_Pooling/config.json`, le modèle ferait la moyenne : c'est un fichier reçu, et
        // son absence se voit avant toute écriture.
        let (_dir, root, expected) = granite_like();
        let digest = trimmed_digest(&root);
        let source = root.join("source");
        let identity = crate::bert::settled_in(&source, &digest, &DECLARED).unwrap();
        std::fs::remove_file(source.join("1_Pooling/config.json").as_std_path()).unwrap();
        let pinned = Pinned {
            files: &expected,
            declared: &DECLARED,
            trimmed: &digest,
            identity: &identity,
        };
        let store = root.join("store");
        let error = install_checked(&source, &store, &pinned)
            .unwrap_err()
            .to_string();
        assert!(error.contains("absent"), "{error}");
        assert!(!crate::bert::default_dir(&store).exists());
    }

    #[test]
    fn the_pinned_constants_agree_with_each_other() {
        // La mise en commun de la révision épinglée est bien celle que `FILES` attend…
        let dir = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        with_setting(&root, "1_Pooling/config.json", CLS);
        let source = root.join("source");
        let pooling = FILES
            .iter()
            .filter(|file| file.name == "1_Pooling/config.json")
            .copied()
            .collect::<Vec<_>>();
        assert_eq!(pooling.len(), 1);
        verify(&source, &pooling).unwrap();
        // … et avec elle et la déclaration, les poids taillés donnent l'identité écrite : changer
        // l'une des trois sans les autres fait tomber ce test, pas une installation.
        assert_eq!(
            crate::bert::settled_in(&source, TRIMMED_DIGEST, &DECLARED).unwrap(),
            IDENTITY
        );
        assert_ne!(IDENTITY, TRIMMED_DIGEST);
    }
}
