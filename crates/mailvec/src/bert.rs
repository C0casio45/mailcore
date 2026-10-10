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
//!
//! ## D'autres modèles que `e5`, depuis le 2026-10-09
//!
//! Les deux règles ci-dessus sont celles de **`e5`**, pas de tous les modèles. Un autre producteur
//! entraîne avec d'autres préfixes — ou aucun — et une autre mise en commun ; les appliquer à son
//! modèle rendrait des vecteurs bien formés et faux, sans rien signaler. Elles se lisent donc
//! dans les fichiers que le producteur publie à côté des poids, au format de
//! `sentence-transformers` :
//!
//! - **l'architecture** dans `config.json` — `bert`, ou `xlm-roberta` dont les positions
//!   commencent après le remplissage ;
//! - **la mise en commun** dans `1_Pooling/config.json` — moyenne ou jeton `CLS` ;
//! - **les préfixes** dans `config_sentence_transformers.json`, clé `prompts`.
//!
//! **Un fichier absent garde la règle de `e5`** : c'est le modèle installé jusqu'ici, et ses trois
//! fichiers ne portent ni l'un ni l'autre. Un réglage présent mais inconnu — une mise en commun par
//! le maximum — est **refusé** plutôt qu'approché.

use camino::{Utf8Path, Utf8PathBuf};
use candle_core::{DType, Device, IndexOp, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::bert::{BertModel, Config};
use candle_transformers::models::xlm_roberta::{Config as XlmConfig, XLMRobertaModel};

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

/// Les préfixes qu'un modèle a vus à l'entraînement, par rôle.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Prompts {
    query: String,
    passage: String,
}

impl Prompts {
    /// Ceux de `e5`, la règle quand le producteur n'en publie pas.
    fn e5() -> Self {
        Self {
            query: "query: ".to_owned(),
            passage: "passage: ".to_owned(),
        }
    }

    fn of(&self, role: Role) -> &str {
        match role {
            Role::Query => &self.query,
            Role::Passage => &self.passage,
        }
    }

    /// Lit `config_sentence_transformers.json`, ou rend ceux de `e5` s'il n'existe pas.
    ///
    /// Le document s'appelle `passage` chez `e5` et `document` chez d'autres ; une clé absente
    /// veut dire « pas de préfixe pour ce rôle », c'est ce que fait `sentence-transformers`.
    fn read(dir: &Utf8Path) -> Result<Self> {
        Self::parse(dir, read_setting(dir, SETTINGS[1])?.as_deref())
    }

    /// Le même, sur le texte du fichier — `None` s'il n'existe pas.
    fn parse(dir: &Utf8Path, text: Option<&str>) -> Result<Self> {
        let Some(text) = text else {
            return Ok(Self::e5());
        };
        let value: serde_json::Value = serde_json::from_str(text).map_err(|e| {
            model_error(
                dir,
                format!("config_sentence_transformers.json illisible : {e}"),
            )
        })?;
        let prompts = value.get("prompts");
        let pick = |keys: &[&str]| {
            keys.iter()
                .find_map(|key| {
                    prompts
                        .and_then(|it| it.get(*key))
                        .and_then(serde_json::Value::as_str)
                })
                .unwrap_or_default()
                .to_owned()
        };
        Ok(Self {
            query: pick(&["query"]),
            passage: pick(&["passage", "document"]),
        })
    }
}

/// Comment les états cachés d'un texte deviennent un vecteur.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pooling {
    /// La moyenne pondérée par le masque — `e5`.
    Mean,
    /// L'état du premier jeton — `granite-embedding`, entre autres.
    Cls,
}

impl Pooling {
    /// Lit `1_Pooling/config.json`, ou rend la moyenne s'il n'existe pas.
    fn read(dir: &Utf8Path) -> Result<Self> {
        Self::parse(dir, read_setting(dir, SETTINGS[0])?.as_deref())
    }

    /// Le même, sur le texte du fichier — `None` s'il n'existe pas.
    fn parse(dir: &Utf8Path, text: Option<&str>) -> Result<Self> {
        let Some(text) = text else {
            return Ok(Self::Mean);
        };
        let value: serde_json::Value = serde_json::from_str(text)
            .map_err(|e| model_error(dir, format!("1_Pooling/config.json illisible : {e}")))?;
        let on = |key: &str| value.get(key).and_then(serde_json::Value::as_bool) == Some(true);
        // Un seul mode, et l'un des deux que ce crate calcule. Plusieurs à la fois — que
        // `sentence-transformers` concatène — ou un autre seraient approchés, donc faux.
        let modes: Vec<&str> = [
            "pooling_mode_cls_token",
            "pooling_mode_mean_tokens",
            "pooling_mode_max_tokens",
            "pooling_mode_mean_sqrt_len_tokens",
            "pooling_mode_weightedmean_tokens",
            "pooling_mode_lasttoken",
        ]
        .into_iter()
        .filter(|key| on(key))
        .collect();
        match modes.as_slice() {
            ["pooling_mode_cls_token"] => Ok(Self::Cls),
            ["pooling_mode_mean_tokens"] => Ok(Self::Mean),
            other => Err(model_error(
                dir,
                format!("mise en commun non gérée : {other:?}"),
            )),
        }
    }
}

/// L'empreinte qu'aurait un modèle de ces poids avec les réglages de `dir`, sans le charger —
/// `declared` remplaçant ceux de `dir` qu'il nomme.
///
/// Les mêmes analyses que [`Bert::load`], donc la même réponse : c'est ce qui permet à
/// l'installation de refuser des réglages **avant** d'écrire dans le store, où le démon chargerait
/// le modèle pendant qu'on le vérifie. `declared` est ce que l'installation écrira elle-même à
/// côté des fichiers reçus — les préfixes que le producteur ne publie pas.
///
/// # Errors
///
/// [`Error::Model`] si un fichier de réglages de `dir` est illisible ou demande une mise en commun
/// que ce crate ne calcule pas.
pub(crate) fn settled_in(
    dir: &Utf8Path,
    weights: &str,
    declared: &[(&str, &str)],
) -> Result<String> {
    let setting = |name: &str| -> Result<Option<String>> {
        match declared.iter().find(|(declared, _)| *declared == name) {
            Some((_, text)) => Ok(Some((*text).to_owned())),
            None => read_setting(dir, name),
        }
    };
    Ok(settled(
        weights.to_owned(),
        Pooling::parse(dir, setting(SETTINGS[0])?.as_deref())?,
        &Prompts::parse(dir, setting(SETTINGS[1])?.as_deref())?,
    ))
}

/// L'empreinte d'un modèle : celle de ses poids, et ce qui en fait des vecteurs s'il s'écarte de
/// `e5`.
///
/// **La provenance nomme tout ce qui fait le vecteur.** Les mêmes poids, encodés avec deux
/// préfixes ou deux mises en commun, rendent deux familles de vecteurs qu'on ne compare pas — et
/// sur l'empreinte des seuls poids, elles portaient le même nom. Trouvé le 2026-10-10 :
/// `granite-embedding` a tourné une nuit avec les préfixes de `e5`, faute de fichier qui dise
/// les siens, et ses vecteurs seraient passés pour ceux du même modèle sans préfixe.
///
/// Les réglages de `e5` laissent l'empreinte **inchangée** : c'est le modèle installé, et un
/// magasin écrit avant ce jour doit rester le sien.
fn settled(weights: String, pooling: Pooling, prompts: &Prompts) -> String {
    if pooling == Pooling::Mean && *prompts == Prompts::e5() {
        return weights;
    }
    let mut hasher = blake3::Hasher::new();
    for part in [
        weights.as_str(),
        match pooling {
            Pooling::Mean => "mean",
            Pooling::Cls => "cls",
        },
        &prompts.query,
        &prompts.passage,
    ] {
        // La longueur avant chaque morceau : deux découpages du même texte ne se confondent pas.
        hasher.update(&(part.len() as u64).to_le_bytes());
        hasher.update(part.as_bytes());
    }
    hasher.finalize().to_hex()[..16].to_owned()
}

/// Le réseau, selon l'architecture que nomme `config.json`.
enum Network {
    Bert(BertModel),
    XlmRoberta(XLMRobertaModel),
}

impl Network {
    fn forward(&self, ids: &Tensor, types: &Tensor, mask: &Tensor) -> candle_core::Result<Tensor> {
        match self {
            Self::Bert(model) => model.forward(ids, types, Some(mask)),
            Self::XlmRoberta(model) => model.forward(ids, mask, types, None, None, None),
        }
    }
}

/// Ce que `config.json` dit de l'architecture, avant de charger quoi que ce soit.
enum Architecture {
    Bert(Config),
    XlmRoberta(XlmConfig),
}

impl Architecture {
    fn read(dir: &Utf8Path) -> Result<Self> {
        let text = read(dir, "config.json")?;
        let illegible =
            |e: serde_json::Error| model_error(dir, format!("config.json illisible : {e}"));
        let value: serde_json::Value = serde_json::from_str(&text).map_err(illegible)?;
        match value.get("model_type").and_then(serde_json::Value::as_str) {
            Some("xlm-roberta") => Ok(Self::XlmRoberta(
                serde_json::from_str(&text).map_err(illegible)?,
            )),
            Some("bert") | None => Ok(Self::Bert(serde_json::from_str(&text).map_err(illegible)?)),
            Some(other) => Err(model_error(
                dir,
                format!("architecture non gérée : {other}"),
            )),
        }
    }

    const fn hidden(&self) -> usize {
        match self {
            Self::Bert(config) => config.hidden_size,
            Self::XlmRoberta(config) => config.hidden_size,
        }
    }

    fn pad(&self) -> Option<u32> {
        match self {
            Self::Bert(config) => u32::try_from(config.pad_token_id).ok(),
            Self::XlmRoberta(config) => Some(config.pad_token_id),
        }
    }

    /// Combien de jetons la table de positions sait placer.
    ///
    /// Les positions d'un `xlm-roberta` **commencent après celle du remplissage** — c'est
    /// `padding_idx + 1`, et `candle` le calcule ainsi — : sur 514 positions, 512 sont utilisables.
    /// Prendre 514 ferait déborder la table au premier long texte, à chaque passe.
    fn positions(&self) -> usize {
        match self {
            Self::Bert(config) => config.max_position_embeddings,
            Self::XlmRoberta(config) => config
                .max_position_embeddings
                .saturating_sub(config.pad_token_id as usize + 1),
        }
    }
}

/// Le transformeur chargé.
pub struct Bert {
    model: Network,
    pooling: Pooling,
    prompts: Prompts,
    tokenizer: tokenizers::Tokenizer,
    device: Device,
    dim: usize,
    /// La pièce de remplissage, telle que la configuration du modèle la nomme. Sa valeur ne
    /// change aucun vecteur — le masque exclut ces positions —, mais c'est la sienne.
    pad: u32,
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

        let architecture = Architecture::read(&dir)?;
        let pooling = Pooling::read(&dir)?;
        let prompts = Prompts::read(&dir)?;

        let mut tokenizer = tokenizers::Tokenizer::from_file(dir.join("tokenizer.json").as_str())
            .map_err(|source| {
            model_error(&dir, format!("tokenizer.json illisible : {source}"))
        })?;
        // La troncature et le remplissage sont réglés ici plutôt que dans le fichier : le fichier
        // vient du producteur du modèle et dit ce que *lui* faisait, pas ce que cette passe
        // demande.
        //
        // **Une fois, au chargement**, et plus à chaque appel. Les régler demande un `&mut`, et
        // `encode_as` les réglait sur une **copie** du tokeniseur faite à chaque lot : une copie
        // profonde — vocabulaire, table inverse, arbre de préfixes — de 249 Mio, mesurée le
        // 2026-09-25, vivante pendant toute la passe avant. Payée à chaque lot de la passe et à
        // chaque requête de recherche, pour régler deux paramètres qui ne changent jamais.
        //
        // **Jamais au-delà des positions du modèle.** Un jeton de plus que ce que la table de
        // positions connaît fait échouer le passage avant — pour ce message, à chaque passe, sans
        // fin. `multilingual-e5-small` en a 512, donc rien ne change pour lui ; un modèle qui en
        // aurait moins que [`MAX_TOKENS`] échouait sur tout courrier un peu long. Trouvé le
        // 2026-10-08 en écrivant le test du critère 6 sur le transformeur minuscule des tests.
        tokenizer
            .with_truncation(Some(tokenizers::TruncationParams {
                max_length: MAX_TOKENS.min(architecture.positions()),
                ..Default::default()
            }))
            .map_err(|e| model_error(&dir, format!("troncature : {e}")))?;
        // **Pas de remplissage par le tokeniseur** : il remplirait tout l'appel à la longueur de
        // son plus long texte. Les lots sont formés après la tokenisation, par longueur, et
        // chacun est rempli à la sienne — voir [`plan`].
        tokenizer.with_padding(None);

        let device = Device::Cpu;
        // `f32` et non `f16` : le processeur n'a pas d'unité `f16`, donc la demi-précision y
        // coûterait une conversion par opération sans rien économiser en calcul. Ce qu'elle
        // économiserait — la moitié de la mémoire — se gagne bien mieux en taillant le
        // vocabulaire, qui est où va l'essentiel du poids de ce modèle. Et `candle` 0.9 rend des
        // vecteurs faits de `NaN` en `f16` : son masque d'attention multiplie zéro par l'infini.
        let weights = dir.join("model.safetensors");
        let (builder, digest) = load_weights(&weights, &device)?;
        let unusable = |source: candle_core::Error| {
            model_error(&dir, format!("poids inexploitables : {source}"))
        };
        let model = match &architecture {
            Architecture::Bert(config) => {
                Network::Bert(BertModel::load(builder, config).map_err(unusable)?)
            }
            Architecture::XlmRoberta(config) => {
                Network::XlmRoberta(XLMRobertaModel::new(config, builder).map_err(unusable)?)
            }
        };

        let dim = architecture.hidden();
        let pad = architecture
            .pad()
            .ok_or_else(|| model_error(&dir, "pad_token_id hors limites".to_owned()))?;
        let identity = crate::pass::Identity {
            name: crate::pass::Identity::name_of(&dir),
            family: "transformeur".to_owned(),
            digest: settled(digest, pooling, &prompts),
        };

        tracing::info!(
            dir = %dir,
            dim,
            ?pooling,
            digest = %identity.digest,
            "transformeur chargé"
        );
        Ok(Self {
            model,
            pooling,
            prompts,
            tokenizer,
            device,
            dim,
            pad,
            identity,
        })
    }

    /// La dimension des vecteurs.
    #[must_use]
    pub const fn dim(&self) -> usize {
        self.dim
    }

    /// Les pièces que le modèle reçoit pour un texte, préfixe et jetons spéciaux compris, après
    /// troncature — exactement ce que [`Bert::encode_as`] lui donnerait.
    ///
    /// Pour vérifier une taille de vocabulaire sans rien encoder : deux modèles qui donnent les
    /// mêmes pièces à un texte, et dont les lignes de table sont celles de ces pièces, donnent le
    /// même vecteur.
    ///
    /// ## La pièce du vocabulaire, pas le texte qu'elle couvre
    ///
    /// Chaque identifiant est rendu par **son entrée de vocabulaire**, et non par le texte qu'il
    /// couvre — ce que `get_tokens` donne. La différence ne se voit que sur un caractère inconnu :
    /// le tokeniseur le rend sous sa forme d'origine, avec l'identifiant de `<unk>`. Comparés par
    /// texte, un `Ж` inconnu et une pièce `Ж` connue étaient égaux ; le premier relevé du
    /// 2026-09-25 annonçait zéro `<unk>` sur treize millions de pièces à côté de 859 messages dont
    /// les pièces avaient changé — un chiffre impossible, donc l'instrument.
    ///
    /// # Errors
    ///
    /// [`Error::Encode`] si la tokenisation échoue.
    pub fn tokens(&self, text: &str, role: Role) -> Result<Vec<String>> {
        let encoding = self
            .tokenizer
            .encode(format!("{}{text}", self.prompts.of(role)), true)
            .map_err(|e| Error::Encode(format!("tokenisation : {e}")))?;
        encoding
            .get_ids()
            .iter()
            .map(|id| {
                self.tokenizer
                    .id_to_token(*id)
                    .ok_or_else(|| Error::Encode(format!("identifiant {id} hors du vocabulaire")))
            })
            .collect()
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
            .map(|text| format!("{}{text}", self.prompts.of(role)))
            .collect();

        let encodings = self
            .tokenizer
            .encode_batch(prefixed, true)
            .map_err(|e| Error::Encode(format!("tokenisation : {e}")))?;
        let lengths: Vec<usize> = encodings.iter().map(|e| e.get_ids().len()).collect();
        if lengths.contains(&0) {
            return Err(Error::Encode("un texte ne produit aucun jeton".to_owned()));
        }

        let mut vectors: Vec<Option<Vec<f32>>> = vec![None; texts.len()];
        for group in plan(&lengths) {
            let cols = group
                .iter()
                .filter_map(|index| lengths.get(*index))
                .copied()
                .max()
                .unwrap_or(0);
            let mut ids = Vec::with_capacity(group.len() * cols);
            let mut mask = Vec::with_capacity(group.len() * cols);
            for index in &group {
                let encoding = encodings
                    .get(*index)
                    .ok_or_else(|| Error::Encode(format!("texte {index} hors du lot")))?;
                let own = encoding.get_ids();
                ids.extend_from_slice(own);
                ids.extend(std::iter::repeat_n(self.pad, cols - own.len()));
                mask.extend(std::iter::repeat_n(1u32, own.len()));
                mask.extend(std::iter::repeat_n(0u32, cols - own.len()));
            }

            let ids = tensor(&ids, group.len(), cols, &self.device)?;
            let mask = tensor(&mask, group.len(), cols, &self.device)?;
            let types = ids
                .zeros_like()
                .map_err(|e| Error::Encode(format!("types de jetons : {e}")))?;
            let hidden = self
                .model
                .forward(&ids, &types, &mask)
                .map_err(|e| Error::Encode(format!("passe avant : {e}")))?;
            let pooled = match self.pooling {
                Pooling::Mean => mean_pool(&hidden, &mask, self.dim)?,
                Pooling::Cls => cls_pool(&hidden, self.dim)?,
            };

            for (index, vector) in group.iter().zip(pooled) {
                if let Some(slot) = vectors.get_mut(*index) {
                    *slot = Some(vector);
                }
            }
        }

        vectors
            .into_iter()
            .enumerate()
            .map(|(index, vector)| {
                vector.ok_or_else(|| Error::Encode(format!("texte {index} sans vecteur")))
            })
            .collect()
    }
}

/// Ce qu'un lot peut porter : **textes × longueur du plus long**, soit trois textes de la longueur
/// maximale — 768 pièces, remplissage compris.
///
/// ## Pourquoi un budget, et pas un nombre de textes
///
/// La mémoire d'un lot a deux termes. L'un est **quadratique** en longueur : douze couches de
/// matrices `pièces × pièces` par texte. L'autre est **linéaire** en nombre de pièces : les couches
/// denses, les états cachés. Mesuré le 2026-09-25 hors du crate : +290 Mio pour 16 × 256, +140 pour
/// 8 × 256, +84 pour 16 × 128, +27 pour 16 × 64. Un lot de seize textes était donc le **pire cas**
/// de seize longs textes, et rien ne le bornait sinon la troncature.
///
/// Et le temps suit la même loi, avec un défaut de plus : un lot formé dans l'ordre d'arrivée se
/// remplit à la longueur de son plus long texte. Un message de trois lignes rangé à côté d'une
/// lettre de deux pages paie les deux pages — d'où le débit qui variait d'un facteur 3,5 à
/// l'intérieur d'une passe (2026-09-22).
///
/// Les textes sont donc **triés par longueur**, puis regroupés tant que le lot tient dans ce
/// budget : trois textes de 256 pièces, douze de 64, et presque aucun remplissage.
///
/// ## Pourquoi un budget de pièces, et pas d'attention
///
/// La première version bornait le seul terme quadratique, textes × pièces². Une relecture l'a
/// réfuté le jour même, mesure à l'appui : à budget quadratique égal, soixante-quatre textes de
/// soixante-quatre pièces passaient d'un bloc — 4 096 pièces —, et le terme linéaire, que ce
/// budget ne voyait pas, en faisait **le pire lot permis** : 125,5 Mio de tas pour la passe avant,
/// contre 88,2 pour quatre textes de 256. Trente-sept de plus, soit davantage que la marge du
/// critère 5. « Borné par construction » était faux.
///
/// Borner le produit textes × longueur borne **les deux termes à la fois** : le linéaire à
/// 768 pièces, le quadratique à 768 × 256 — exactement le cas de trois longs textes, qui devient
/// le pire lot, quel que soit le corpus.
///
/// ## Pourquoi trois, et pas huit ni quatre
///
/// Huit a été mesuré d'abord, sur le modèle taillé : crête de passe de 517 à 525 Mio, soit 182
/// au-dessus des 343 du modèle chargé — et le seuil du critère 5 est de 500 Mo, 476,8 Mio.
/// Quatre divise par deux ce qui dépassait. Le prix est un lot plus petit pour les seuls longs
/// textes, là où le calcul est de toute façon dominé par leurs propres pièces.
///
/// Quatre a tenu sur mille messages, à 451–456 Mio, puis la passe complète du 2026-10-08 a crevé
/// le seuil de 5 Mio : 481,9. Le corpus entier coûte une quinzaine de mébioctets de plus que son
/// début, et l'essentiel de l'écart est un **reste** — ce que la passe laisse résident, 33 Mio
/// après 5 000 messages, 47 après 73 658 —, pas un lot plus lourd. Trois retire un long texte du
/// pire lot, environ seize mébioctets d'après la pente de huit à quatre, et ne touche pas aux
/// vecteurs : un lot ne change pas ce que chaque texte devient. Le reste, lui, n'est pas borné
/// par ce réglage — voir docs/PHASE-4.md, 2026-10-08.
const PIECE_BUDGET: usize = 3 * MAX_TOKENS;

/// Plafond de textes par lot, même très courts, et taille de la fenêtre que la passe remet d'un
/// coup — voir [`crate::pass::Encoder::batch`].
const MAX_BATCH: usize = 64;

/// Répartit des textes en lots, par longueur croissante.
///
/// Rend des indices dans la liste d'origine : chaque texte apparaît dans exactement un lot, et
/// chaque lot tient dans [`PIECE_BUDGET`] — sauf un texte seul, qu'on ne peut pas couper.
/// L'ordre est entièrement déterminé par les longueurs et les positions : le même lot donne
/// toujours les mêmes lots.
fn plan(lengths: &[usize]) -> Vec<Vec<usize>> {
    let mut order: Vec<usize> = (0..lengths.len()).collect();
    order.sort_by_key(|index| (lengths.get(*index).copied().unwrap_or(0), *index));

    let mut groups = Vec::new();
    let mut current: Vec<usize> = Vec::new();
    for index in order {
        // Par longueur croissante : le texte qui arrive est le plus long du lot en cours.
        let longest = lengths.get(index).copied().unwrap_or(0);
        let cost = (current.len() + 1).saturating_mul(longest);
        if !current.is_empty() && (cost > PIECE_BUDGET || current.len() >= MAX_BATCH) {
            groups.push(std::mem::take(&mut current));
        }
        current.push(index);
    }
    if !current.is_empty() {
        groups.push(current);
    }
    groups
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

/// L'état caché du premier jeton de chaque texte.
///
/// Le remplissage est **à droite** — voir [`Bert::encode_as`] —, donc le premier jeton est
/// toujours un vrai jeton, le `<s>` ou `[CLS]` que le tokeniseur pose en tête : le masque n'a
/// rien à exclure ici.
fn cls_pool(hidden: &Tensor, dim: usize) -> Result<Vec<Vec<f32>>> {
    let vectors = hidden
        .i((.., 0, ..))
        .and_then(|first| first.to_vec2::<f32>())
        .map_err(|e| Error::Encode(format!("mise en commun : {e}")))?;
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

/// Lit un fichier de réglages, `None` s'il n'existe pas : chacun est facultatif.
fn read_setting(dir: &Utf8Path, name: &str) -> Result<Option<String>> {
    if dir.join(name).is_file() {
        read(dir, name).map(Some)
    } else {
        Ok(None)
    }
}

/// Monte les poids `safetensors`, et rend leur empreinte.
///
/// ## Lus, et non projetés en mémoire
///
/// `candle` propose `from_mmaped_safetensors`, plus économe — les pages arrivent à la demande —
/// et **`unsafe`** : le modèle deviendrait incohérent si quelqu'un réécrivait le fichier pendant
/// qu'on s'en sert. Ce crate porte `#![forbid(unsafe_code)]`, et la règle du projet demande une
/// justification écrite pour la lever. Elle ne vaut pas d'être levée ici.
///
/// ## Lus par [`crate::weights`], et non par `candle`
///
/// Le lecteur de `candle` lit le fichier entier **puis** copie chaque tenseur : les poids
/// existaient deux fois pendant le chargement, soit une crête de 998 à 1 066 Mio pour un modèle
/// qui en occupe 725 chargé (2026-09-25). Celui du crate lit chaque tenseur directement dans son
/// tampon, et calcule l'empreinte sur la même lecture — qui était jusque-là une seconde lecture
/// complète du fichier.
fn load_weights(path: &Utf8Path, device: &Device) -> Result<(VarBuilder<'static>, String)> {
    if !path.is_file() {
        return Err(model_error(path, "model.safetensors absent".to_owned()));
    }
    let loaded = crate::weights::load(path, device)
        .map_err(|reason| model_error(path, format!("poids illisibles : {reason}")))?;
    Ok((
        VarBuilder::from_tensors(loaded.tensors, DType::F32, device),
        loaded.digest,
    ))
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

    /// Soixante-quatre textes par appel, que [`Bert::encode_as`] redécoupe en lots par longueur.
    ///
    /// Ce n'est plus la taille d'un lot de calcul — c'est [`PIECE_BUDGET`] qui la borne,
    /// depuis le 2026-09-25 —, mais celle de la **fenêtre** dans laquelle les textes sont triés.
    /// Plus grande, elle trierait mieux ; elle retarderait aussi d'autant l'écriture durable et
    /// l'arrêt demandé, que la passe ne regarde qu'entre deux appels. Soixante-quatre messages,
    /// c'est une dizaine de secondes de travail au pire.
    ///
    /// Le premier essai, à cinq cents textes d'un seul lot, avait demandé 4 557 Mio avant
    /// d'écrire un seul vecteur : c'est la raison d'être de cette méthode.
    fn batch(&self) -> usize {
        MAX_BATCH
    }

    fn encode(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        self.encode_as(texts, Role::Passage)
    }
}

/// Les fichiers qui disent comment le modèle fait un vecteur, chacun facultatif : sans eux, les
/// réglages de `e5`.
pub const SETTINGS: [&str; 2] = ["1_Pooling/config.json", "config_sentence_transformers.json"];

/// Tous les fichiers dont un changement change le modèle chargé, ou s'il se charge.
///
/// **Une liste, pour tous ceux qui surveillent un modèle.** Le démon retient un refus tant que ces
/// fichiers n'ont pas bougé ; il ne regardait que les trois premiers, et une mise en commun
/// corrigée — ou un fichier de préfixes ajouté puis retiré, qui fait basculer l'empreinte — laissait
/// la passe et la recherche refusées jusqu'au redémarrage. Trouvé par une relecture le 2026-10-10.
pub const MODEL_FILES: [&str; 5] = [
    "model.safetensors",
    "tokenizer.json",
    "config.json",
    SETTINGS[0],
    SETTINGS[1],
];

/// Le répertoire où ce modèle s'installe, sous la racine du store.
#[must_use]
pub fn default_dir(store_root: &Utf8Path) -> Utf8PathBuf {
    store_root.join("model-bert")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::{Bert, MAX_BATCH, MAX_TOKENS, PIECE_BUDGET, Role, plan};
    use crate::pass::Encoder;
    use crate::testing::{TINY_PIECES, tiny_bert};

    fn tiny() -> (tempfile::TempDir, Bert) {
        let dir = tempfile::tempdir().unwrap();
        let root = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        tiny_bert(&root, TINY_PIECES);
        let bert = Bert::load(&root).unwrap();
        (dir, bert)
    }

    fn cost(group: &[usize], lengths: &[usize]) -> usize {
        let longest = group.iter().map(|index| lengths[*index]).max().unwrap();
        group.len() * longest
    }

    #[test]
    fn every_text_is_in_exactly_one_batch_and_every_batch_fits() {
        let lengths: Vec<usize> = (0..300).map(|k| 3 + (k * 37) % MAX_TOKENS).collect();
        let groups = plan(&lengths);
        let mut seen: Vec<usize> = groups.iter().flatten().copied().collect();
        seen.sort_unstable();
        assert_eq!(seen, (0..lengths.len()).collect::<Vec<_>>());
        for group in &groups {
            assert!(group.len() <= MAX_BATCH, "{} textes", group.len());
            assert!(
                group.len() == 1 || cost(group, &lengths) <= PIECE_BUDGET,
                "un lot de {} coûte {}",
                group.len(),
                cost(group, &lengths)
            );
        }
    }

    #[test]
    fn sixteen_long_texts_are_split_where_they_were_computed_together() {
        // Le pire cas d'avant le 2026-09-25 : seize textes à la longueur maximale, un seul lot.
        let groups = plan(&[MAX_TOKENS; 16]);
        assert_eq!(
            groups.iter().map(Vec::len).collect::<Vec<_>>(),
            vec![3, 3, 3, 3, 3, 1]
        );
        // Et des textes courts se regroupent jusqu'au plafond.
        assert!(
            plan(&[10; 200])
                .iter()
                .all(|group| group.len() <= MAX_BATCH)
        );
        assert_eq!(plan(&[10; 64]).len(), 1);
    }

    #[test]
    fn no_batch_costs_more_than_three_long_texts_on_either_term() {
        // Le défaut que la relecture du 2026-09-25 a mesuré : un budget sur le seul terme
        // quadratique laissait passer soixante-quatre textes de soixante-quatre pièces d'un bloc,
        // le pire lot permis. Les deux termes sont vérifiés ici, sur toutes les longueurs.
        for length in 1..=MAX_TOKENS {
            for group in plan(&vec![length; MAX_BATCH]) {
                let pieces = group.len() * length;
                assert!(
                    pieces <= PIECE_BUDGET,
                    "{} × {length} = {pieces} pièces",
                    group.len()
                );
                assert!(
                    pieces * length <= 3 * MAX_TOKENS * MAX_TOKENS,
                    "{} × {length}² dépasse trois longs textes",
                    group.len()
                );
            }
        }
        // Le cas qui passait : il est maintenant coupé en lots de douze.
        assert_eq!(
            plan(&[64; 64]).iter().map(Vec::len).collect::<Vec<_>>(),
            vec![12, 12, 12, 12, 12, 4]
        );
    }

    #[test]
    fn a_window_split_into_several_batches_keeps_every_vector_in_its_place() {
        // Les tests d'encodage précédents tiennent en un seul lot. Celui-ci en force plusieurs, de
        // longueurs différentes, pour que la reconstruction par lot — longueur, remplissage,
        // masque — et le retour dans l'ordre d'origine soient exercés au-delà du premier.
        let (_dir, bert) = tiny();
        let mut texts: Vec<String> = (0..70).map(|_| "bon".to_owned()).collect();
        texts.insert(35, "bonjour été bonjour été bonjour été 😀".to_owned());
        texts.insert(3, "été bonjour".to_owned());
        let lengths: Vec<usize> = texts
            .iter()
            .map(|text| bert.tokens(text, Role::Passage).unwrap().len())
            .collect();
        assert!(
            plan(&lengths).len() >= 2,
            "un seul lot : le test ne prouverait rien"
        );

        let together = bert.encode_as(&texts, Role::Passage).unwrap();
        assert_eq!(together.len(), texts.len());
        for (text, vector) in texts.iter().zip(&together) {
            let alone = bert
                .encode_as(std::slice::from_ref(text), Role::Passage)
                .unwrap();
            let gap = alone[0]
                .iter()
                .zip(vector)
                .map(|(a, b)| (a - b).abs())
                .fold(0f32, f32::max);
            assert!(gap < 1e-5, "{text:?} : écart de {gap}");
        }
    }

    #[test]
    fn a_text_longer_than_the_budget_still_gets_its_own_batch() {
        assert_eq!(plan(&[100_000]), vec![vec![0]]);
        assert!(plan(&[]).is_empty());
    }

    #[test]
    fn the_same_lengths_always_give_the_same_batches() {
        let lengths = [40, 3, 40, 200, 3, 17];
        assert_eq!(plan(&lengths), plan(&lengths));
        // Par longueur croissante, à égalité dans l'ordre d'arrivée.
        assert_eq!(plan(&lengths).concat(), vec![1, 4, 5, 0, 2, 3]);
    }

    #[test]
    fn a_text_gets_the_same_vector_alone_or_beside_longer_texts() {
        // La propriété que le masque doit garantir, et que le tri par longueur met à l'épreuve :
        // un texte court rangé avec des longs est rempli, et ce remplissage ne doit rien changer.
        let (_dir, bert) = tiny();
        let short = "bon".to_owned();
        let long = "bonjour été bonjour été bonjour été 😀".to_owned();
        let alone = bert
            .encode_as(std::slice::from_ref(&short), Role::Passage)
            .unwrap();
        let beside = bert
            .encode_as(&[long.clone(), short, long], Role::Passage)
            .unwrap();
        let gap = alone[0]
            .iter()
            .zip(&beside[1])
            .map(|(a, b)| (a - b).abs())
            .fold(0f32, f32::max);
        assert!(gap < 1e-5, "écart de {gap}");
    }

    #[test]
    fn vectors_come_back_in_the_order_of_the_texts() {
        // Le tri par longueur réordonne le calcul ; il ne doit pas réordonner le résultat.
        let (_dir, bert) = tiny();
        let texts = vec![
            "bonjour été bonjour été".to_owned(),
            "bon".to_owned(),
            "été 😀".to_owned(),
        ];
        let together = bert.encode_as(&texts, Role::Passage).unwrap();
        for (text, vector) in texts.iter().zip(&together) {
            let alone = bert
                .encode_as(std::slice::from_ref(text), Role::Passage)
                .unwrap();
            let gap = alone[0]
                .iter()
                .zip(vector)
                .map(|(a, b)| (a - b).abs())
                .fold(0f32, f32::max);
            assert!(gap < 1e-5, "{text:?} : écart de {gap}");
        }
    }

    #[test]
    fn the_query_and_the_passage_roles_give_different_vectors() {
        // Les préfixes de `e5` : les intervertir ou les omettre dégrade sans rien signaler.
        let (_dir, bert) = tiny();
        let text = vec!["bonjour".to_owned()];
        assert_ne!(
            bert.encode_as(&text, Role::Query).unwrap(),
            bert.encode_as(&text, Role::Passage).unwrap()
        );
        assert_eq!(bert.tokens("bonjour", Role::Query).unwrap()[1], "▁query");
    }

    /// Le transformeur minuscule, déclaré `xlm-roberta` : mêmes poids, même tokeniseur — le
    /// remplissage y est déjà `<pad>`, identifiant 1, comme chez `granite-embedding`.
    fn tiny_xlm(extra: &[(&str, serde_json::Value)]) -> (tempfile::TempDir, Bert) {
        let dir = tempfile::tempdir().unwrap();
        let root = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        tiny_bert(&root, TINY_PIECES);
        let path = root.join("config.json");
        let mut config: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(path.as_std_path()).unwrap()).unwrap();
        config["model_type"] = serde_json::json!("xlm-roberta");
        config["architectures"] = serde_json::json!(["XLMRobertaModel"]);
        config["pad_token_id"] = serde_json::json!(1);
        std::fs::write(path.as_std_path(), config.to_string()).unwrap();
        for (name, value) in extra {
            let file = root.join(name);
            std::fs::create_dir_all(file.parent().unwrap().as_std_path()).unwrap();
            std::fs::write(file.as_std_path(), value.to_string()).unwrap();
        }
        let bert = Bert::load(&root).unwrap();
        (dir, bert)
    }

    #[test]
    fn an_xlm_roberta_model_is_cut_before_its_positions_overflow() {
        // Ses positions commencent après celle du remplissage : sur 64, 62 sont utilisables. Une
        // fenêtre à 64 ferait déborder la table au premier long texte — à chaque passe, sur
        // chaque long message. Le contrôle inverse est `a_long_text_is_cut_at_the_window` : le
        // même modèle déclaré `bert` coupe à 64.
        let (_dir, bert) = tiny_xlm(&[]);
        let long = "bon ".repeat(2 * MAX_TOKENS);
        assert_eq!(bert.tokens(&long, Role::Passage).unwrap().len(), 62);
        assert_eq!(bert.encode_as(&[long], Role::Passage).unwrap().len(), 1);
    }

    #[test]
    fn the_first_token_pooling_does_not_depend_on_the_batch() {
        // Lu dans `1_Pooling/config.json`. La propriété du masque, pour cette mise en commun :
        // le remplissage est à droite, donc le premier jeton d'un texte court rangé à côté d'un
        // long reste le sien.
        let cls =
            serde_json::json!({"pooling_mode_cls_token": true, "pooling_mode_mean_tokens": false});
        let (_dir, bert) = tiny_xlm(&[("1_Pooling/config.json", cls)]);
        let short = "bon".to_owned();
        let long = "bonjour été bonjour été bonjour été 😀".to_owned();
        let alone = bert
            .encode_as(std::slice::from_ref(&short), Role::Passage)
            .unwrap();
        let beside = bert
            .encode_as(&[long.clone(), short.clone(), long], Role::Passage)
            .unwrap();
        let gap = alone[0]
            .iter()
            .zip(&beside[1])
            .map(|(a, b)| (a - b).abs())
            .fold(0f32, f32::max);
        assert!(gap < 1e-5, "écart de {gap}");

        // Et ce n'est pas la moyenne : sans le fichier, le même modèle rend autre chose.
        let (_mean_dir, mean) = tiny_xlm(&[]);
        assert_ne!(
            mean.encode_as(std::slice::from_ref(&short), Role::Passage)
                .unwrap(),
            alone
        );
    }

    #[test]
    fn the_prompts_come_from_the_producer_file_and_default_to_e5() {
        // Sans fichier, les préfixes de `e5` — c'est le modèle installé jusqu'ici.
        let (_dir, e5) = tiny_xlm(&[]);
        assert_eq!(e5.tokens("bonjour", Role::Query).unwrap()[1], "▁query");
        // Un fichier sans préfixe : rien n'est ajouté, dans aucun rôle.
        let none = serde_json::json!({"prompts": {}});
        let (_dir2, bare) = tiny_xlm(&[("config_sentence_transformers.json", none)]);
        assert_ne!(bare.tokens("bonjour", Role::Query).unwrap()[1], "▁query");
        assert_eq!(
            bare.tokens("bonjour", Role::Query).unwrap(),
            bare.tokens("bonjour", Role::Passage).unwrap()
        );
    }

    #[test]
    fn the_same_weights_with_other_settings_are_another_model() {
        // Les réglages de `e5` gardent l'empreinte des poids : un magasin déjà écrit reste le sien.
        let (dir, e5) = tiny_xlm(&[]);
        let weights = camino::Utf8Path::from_path(dir.path())
            .unwrap()
            .join("model.safetensors");
        assert_eq!(
            e5.identity().digest,
            crate::pass::Identity::digest_of(&weights)
        );
        // Sans préfixe, ou au premier jeton : mêmes poids, autres vecteurs, autre nom.
        let none = serde_json::json!({"prompts": {}});
        let (_d1, bare) = tiny_xlm(&[("config_sentence_transformers.json", none)]);
        let cls =
            serde_json::json!({"pooling_mode_cls_token": true, "pooling_mode_mean_tokens": false});
        let (_d2, first) = tiny_xlm(&[("1_Pooling/config.json", cls)]);
        let digests = [
            e5.identity().digest,
            bare.identity().digest,
            first.identity().digest,
        ];
        for (i, a) in digests.iter().enumerate() {
            for b in &digests[i + 1..] {
                assert_ne!(a, b);
            }
        }
        // Et un fichier qui redit les préfixes de `e5` ne change rien : c'est le réglage qui compte,
        // pas le fichier.
        let same = serde_json::json!({"prompts": {"query": "query: ", "passage": "passage: "}});
        let (_d3, explicit) = tiny_xlm(&[("config_sentence_transformers.json", same)]);
        assert_eq!(explicit.identity().digest, digests[0]);
    }

    #[test]
    fn a_pooling_or_an_architecture_this_crate_does_not_compute_is_refused() {
        // Approcher une mise en commun par une autre rendrait des vecteurs bien formés et faux.
        let refused = |name: &str, value: serde_json::Value| {
            let dir = tempfile::tempdir().unwrap();
            let root = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
            tiny_bert(&root, TINY_PIECES);
            let file = root.join(name);
            std::fs::create_dir_all(file.parent().unwrap().as_std_path()).unwrap();
            if name == "config.json" {
                let mut config: serde_json::Value =
                    serde_json::from_str(&std::fs::read_to_string(file.as_std_path()).unwrap())
                        .unwrap();
                config["model_type"] = value;
                std::fs::write(file.as_std_path(), config.to_string()).unwrap();
            } else {
                std::fs::write(file.as_std_path(), value.to_string()).unwrap();
            }
            Bert::load(&root).expect_err("accepté").to_string()
        };
        let max = refused(
            "1_Pooling/config.json",
            serde_json::json!({"pooling_mode_max_tokens": true}),
        );
        assert!(max.contains("mise en commun"), "{max}");
        let both = refused(
            "1_Pooling/config.json",
            serde_json::json!({"pooling_mode_cls_token": true, "pooling_mode_mean_tokens": true}),
        );
        assert!(both.contains("mise en commun"), "{both}");
        let arch = refused("config.json", serde_json::json!("gpt2"));
        assert!(arch.contains("architecture"), "{arch}");
    }

    #[test]
    fn a_long_text_is_cut_at_the_window() {
        let (_dir, bert) = tiny();
        let long = "bon ".repeat(2 * MAX_TOKENS);
        // Le transformeur minuscule n'a que soixante-quatre positions : la fenêtre est la plus
        // petite des deux bornes, et un texte long **s'encode** au lieu de faire échouer la passe.
        assert_eq!(bert.tokens(&long, Role::Passage).unwrap().len(), 64);
        assert_eq!(bert.encode_as(&[long], Role::Passage).unwrap().len(), 1);
    }
}
