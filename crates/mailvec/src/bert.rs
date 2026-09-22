//! Le modèle transformeur, la branche reprise le 2026-09-21.
//!
//! ## Pourquoi il existe alors qu'un modèle marchait déjà
//!
//! `model2vec` marchait au sens où il rendait des vecteurs, et le 2026-09-21 a dit de combien il
//! ne suffisait pas : rang médian **1 992 sur 73 658** pour les cibles du sous-ensemble dur, là
//! où le hasard donne 36 829. Le signal est dix-huit fois meilleur que le hasard et il lui manque
//! deux ordres de grandeur pour entrer dans dix résultats.
//!
//! Ce qui distingue ce modèle-ci, et qui explique l'écart attendu :
//!
//! - **il a du contexte.** Un embedding statique donne le même vecteur à « vol » dans « vol de
//!   nuit » et dans « vol à l'étalage ». Douze couches d'attention n'ont pas ce problème ;
//! - **il est entraîné pour la recherche**, par contraste avec des négatifs difficiles. C'est un
//!   objectif d'entraînement, pas une propriété émergente : un modèle distillé sans cet objectif
//!   n'a aucune raison de séparer une bonne réponse d'une réponse plausible.
//!
//! ## Les deux choses qu'il faut faire exactement, sous peine de chiffres médiocres inexplicables
//!
//! **Les préfixes.** La famille `e5` est entraînée avec `query: ` devant une requête et
//! `passage: ` devant un document. Ce n'est pas une convention décorative : les deux rôles
//! occupent des régions différentes de l'espace, et les intervertir — ou les omettre — dégrade
//! sans rien signaler. C'est le genre de détail qui se paie en une journée de recherche d'un
//! défaut qui n'existe pas.
//!
//! **La mise en commun par la moyenne**, pondérée par le masque d'attention, et **pas** le jeton
//! `CLS`. C'est ce que fait l'entraînement de `e5` ; prendre `CLS` rendrait des vecteurs
//! parfaitement formés et sans rapport avec ce que le modèle a appris.

use camino::{Utf8Path, Utf8PathBuf};
use candle_core::{DType, Device, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::bert::{BertModel, Config};

use crate::{Error, Result};

/// Le nombre maximal de jetons donnés au modèle.
///
/// ## Pourquoi c'est une vraie fenêtre, cette fois
///
/// Contrairement à l'embedding statique, un transformeur a un coût **quadratique** en longueur et
/// une position maximale au-delà de laquelle il n'a rien appris. 512 est celle de ce modèle ; on
/// s'arrête avant, à 256, parce que le coût compte pour une passe de 73 658 messages et que le
/// budget de texte de [`crate::text`] tient largement dedans.
///
/// La valeur est donc un arbitrage mesurable et non une limite subie — et elle se remesurera au
/// même endroit que le budget de texte, contre le jeu de requêtes.
const MAX_TOKENS: usize = 256;

/// Ce qu'on encode : une requête ou un document.
///
/// Un type et non un booléen, parce que le préfixe qu'il gouverne est exactement le genre de
/// chose qu'on intervertit une fois et qu'on cherche pendant une journée.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Ce que l'utilisateur tape.
    Query,
    /// Un message du corpus.
    Passage,
}

impl Role {
    /// Le préfixe que l'entraînement de `e5` attend.
    const fn prefix(self) -> &'static str {
        match self {
            Self::Query => "query: ",
            Self::Passage => "passage: ",
        }
    }
}

/// Le transformeur chargé.
pub struct Bert {
    model: BertModel,
    tokenizer: tokenizers::Tokenizer,
    device: Device,
    dim: usize,
    /// Ce qui le nomme et le distingue dans la provenance d'un magasin.
    identity: crate::pass::Identity,
}

// Le modèle porte des dizaines de mébioctets de poids : les afficher n'aiderait personne. Même
// raisonnement que pour `Model`, et que pour le `Debug` écrit à la main de `Credential`.
impl std::fmt::Debug for Bert {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Bert")
            .field("dim", &self.dim)
            .field("max_tokens", &MAX_TOKENS)
            .finish()
    }
}

impl Bert {
    /// Charge le modèle depuis un répertoire : `config.json`, `tokenizer.json`,
    /// `model.safetensors`.
    ///
    /// # Errors
    ///
    /// [`Error::Model`] si un des trois fichiers manque ou n'est pas lisible.
    pub fn load(dir: impl AsRef<Utf8Path>) -> Result<Self> {
        let dir = dir.as_ref().to_owned();
        if !dir.is_dir() {
            return Err(Error::Model {
                path: dir,
                reason: "le répertoire n'existe pas".to_owned(),
            });
        }

        let config: Config = serde_json::from_str(&read(&dir, "config.json")?)
            .map_err(|source| model_error(&dir, format!("config.json illisible : {source}")))?;

        let tokenizer = tokenizers::Tokenizer::from_file(dir.join("tokenizer.json").as_str())
            .map_err(|source| model_error(&dir, format!("tokenizer.json illisible : {source}")))?;

        let device = Device::Cpu;
        // `f32` et non `f16` : le processeur n'a pas d'unité `f16`, donc la demi-précision y
        // coûterait une conversion par opération sans rien économiser en calcul. Ce qu'elle
        // économiserait — la moitié de la mémoire — se gagne bien mieux en taillant le
        // vocabulaire, qui est où va l'essentiel du poids de ce modèle.
        let weights = dir.join("model.safetensors");
        let builder = load_weights(&weights, &device)?;
        let model = BertModel::load(builder, &config)
            .map_err(|source| model_error(&dir, format!("poids inexploitables : {source}")))?;

        let dim = config.hidden_size;
        let identity = crate::pass::Identity {
            name: crate::pass::Identity::name_of(&dir),
            family: "transformeur".to_owned(),
            digest: crate::pass::Identity::digest_of(&weights),
        };

        tracing::info!(dir = %dir, dim, digest = %identity.digest, "transformeur chargé");
        Ok(Self {
            model,
            tokenizer,
            device,
            dim,
            identity,
        })
    }

    /// La dimension des vecteurs.
    #[must_use]
    pub const fn dim(&self) -> usize {
        self.dim
    }

    /// Encode un lot de textes dans un rôle donné.
    ///
    /// # Errors
    ///
    /// [`Error::Encode`] si la tokenisation ou le calcul échoue.
    pub fn encode_as(&self, texts: &[String], role: Role) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }

        let prefixed: Vec<String> = texts
            .iter()
            .map(|text| format!("{}{text}", role.prefix()))
            .collect();

        let mut tokenizer = self.tokenizer.clone();
        // La troncature et le remplissage sont réglés ici plutôt que dans le fichier : le fichier
        // vient du producteur du modèle et dit ce que *lui* faisait, pas ce que cette passe
        // demande. Les régler à l'appel les rend visibles à côté de leur raison.
        tokenizer
            .with_truncation(Some(tokenizers::TruncationParams {
                max_length: MAX_TOKENS,
                ..Default::default()
            }))
            .map_err(|e| Error::Encode(format!("troncature : {e}")))?;
        tokenizer.with_padding(Some(tokenizers::PaddingParams {
            strategy: tokenizers::PaddingStrategy::BatchLongest,
            ..Default::default()
        }));

        let encodings = tokenizer
            .encode_batch(prefixed, true)
            .map_err(|e| Error::Encode(format!("tokenisation : {e}")))?;

        let rows = encodings.len();
        let cols = encodings.first().map_or(0, |e| e.get_ids().len());
        if cols == 0 {
            return Err(Error::Encode("le lot ne produit aucun jeton".to_owned()));
        }

        let ids: Vec<u32> = encodings
            .iter()
            .flat_map(|e| e.get_ids().to_vec())
            .collect();
        let mask: Vec<u32> = encodings
            .iter()
            .flat_map(|e| e.get_attention_mask().to_vec())
            .collect();

        let ids = tensor(&ids, rows, cols, &self.device)?;
        let mask = tensor(&mask, rows, cols, &self.device)?;
        let types = ids
            .zeros_like()
            .map_err(|e| Error::Encode(format!("types de jetons : {e}")))?;

        let hidden = self
            .model
            .forward(&ids, &types, Some(&mask))
            .map_err(|e| Error::Encode(format!("passe avant : {e}")))?;

        mean_pool(&hidden, &mask, self.dim)
    }
}

/// La moyenne des états cachés, pondérée par le masque d'attention.
///
/// **Pondérée**, et c'est tout le sujet : sans le masque, les jetons de remplissage d'un texte
/// court entreraient dans sa moyenne, et son vecteur dépendrait de la longueur du plus long texte
/// de son lot. Le même message rangé dans deux lots différents aurait deux vecteurs — un défaut
/// qui ne se verrait que sur la qualité des résultats, et jamais en test unitaire sur un seul
/// texte.
fn mean_pool(hidden: &Tensor, mask: &Tensor, dim: usize) -> Result<Vec<Vec<f32>>> {
    let pool = || -> candle_core::Result<Vec<Vec<f32>>> {
        let mask = mask.to_dtype(DType::F32)?.unsqueeze(2)?;
        let masked = hidden.broadcast_mul(&mask)?;
        let summed = masked.sum(1)?;
        let counts = mask.sum(1)?.clamp(1e-9, f32::INFINITY)?;
        summed.broadcast_div(&counts)?.to_vec2::<f32>()
    };
    let vectors = pool().map_err(|e| Error::Encode(format!("mise en commun : {e}")))?;
    if let Some(found) = vectors.iter().map(Vec::len).find(|len| *len != dim) {
        return Err(Error::Encode(format!(
            "vecteur de dimension {found}, attendu {dim}"
        )));
    }
    Ok(vectors)
}

/// Un tenseur entier de forme `rows × cols`.
fn tensor(values: &[u32], rows: usize, cols: usize, device: &Device) -> Result<Tensor> {
    Tensor::from_slice(values, (rows, cols), device)
        .map_err(|e| Error::Encode(format!("tenseur {rows}×{cols} : {e}")))
}

/// Lit un fichier du répertoire du modèle.
fn read(dir: &Utf8Path, name: &str) -> Result<String> {
    std::fs::read_to_string(dir.join(name).as_std_path())
        .map_err(|source| model_error(dir, format!("{name} : {source}")))
}

/// Monte les poids `safetensors`.
///
/// ## Lus, et non projetés en mémoire
///
/// `candle` propose `from_mmaped_safetensors`, plus économe — les pages arrivent à la demande —
/// et **`unsafe`** : le modèle deviendrait incohérent si quelqu'un réécrivait le fichier pendant
/// qu'on s'en sert. Ce crate porte `#![forbid(unsafe_code)]`, et la règle du projet demande une
/// justification écrite pour la lever. Elle ne vaut pas d'être levée ici : le chemin sûr lit le
/// fichier, ce qui coûte sa taille en mémoire **une fois**, et la taille de ce fichier est
/// précisément ce que la taille du vocabulaire va réduire d'un facteur quatre.
///
/// Autrement dit, on paierait une entorse à une règle de sûreté pour économiser une mémoire qu'on
/// s'apprête à ne plus consommer.
fn load_weights(path: &Utf8Path, device: &Device) -> Result<VarBuilder<'static>> {
    if !path.is_file() {
        return Err(model_error(path, "model.safetensors absent".to_owned()));
    }
    let tensors = candle_core::safetensors::load(path.as_std_path(), device)
        .map_err(|source| model_error(path, format!("poids illisibles : {source}")))?;
    Ok(VarBuilder::from_tensors(tensors, DType::F32, device))
}

/// Une erreur de modèle avec son chemin.
fn model_error(path: impl AsRef<Utf8Path>, reason: String) -> Error {
    Error::Model {
        path: path.as_ref().to_owned(),
        reason,
    }
}

/// Un transformeur vu comme l'encodeur de la passe.
///
/// Le rôle est **`Passage`**, toujours : cette implémentation sert la vectorisation du corpus. Une
/// requête passe par [`Bert::encode_as`] avec `Role::Query`, et c'est
/// [`crate::search::Semantic`] qui le fait. Séparer les deux au niveau du type est ce qui empêche
/// de les intervertir.
impl crate::pass::Encoder for Bert {
    fn dim(&self) -> usize {
        Self::dim(self)
    }

    fn identity(&self) -> crate::pass::Identity {
        self.identity.clone()
    }

    /// Seize, contre cinq cents pour un embedding statique.
    ///
    /// Le coût mémoire d'un lot est ici **quadratique en longueur et linéaire en taille de
    /// lot** : douze couches d'attention de jetons × jetons par texte. Le premier essai à cinq
    /// cents a demandé 4 557 Mio avant d'écrire un seul vecteur. Seize tient dans quelques
    /// centaines de mébioctets et amortit encore l'appel.
    fn batch(&self) -> usize {
        16
    }

    fn encode(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        self.encode_as(texts, Role::Passage)
    }
}

/// Le répertoire où ce modèle s'installe, sous la racine du store.
#[must_use]
pub fn default_dir(store_root: &Utf8Path) -> Utf8PathBuf {
    store_root.join("model-bert")
}
