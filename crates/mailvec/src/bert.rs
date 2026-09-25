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

        let config: Config = serde_json::from_str(&read(&dir, "config.json")?)
            .map_err(|source| model_error(&dir, format!("config.json illisible : {source}")))?;

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
        tokenizer
            .with_truncation(Some(tokenizers::TruncationParams {
                max_length: MAX_TOKENS,
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
        let model = BertModel::load(builder, &config)
            .map_err(|source| model_error(&dir, format!("poids inexploitables : {source}")))?;

        let dim = config.hidden_size;
        let pad = u32::try_from(config.pad_token_id)
            .map_err(|_| model_error(&dir, "pad_token_id hors limites".to_owned()))?;
        let identity = crate::pass::Identity {
            name: crate::pass::Identity::name_of(&dir),
            family: "transformeur".to_owned(),
            digest,
        };

        tracing::info!(dir = %dir, dim, digest = %identity.digest, "transformeur chargé");
        Ok(Self {
            model,
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
            .encode(format!("{}{text}", role.prefix()), true)
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
            .map(|text| format!("{}{text}", role.prefix()))
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
                .forward(&ids, &types, Some(&mask))
                .map_err(|e| Error::Encode(format!("passe avant : {e}")))?;

            for (index, vector) in group.iter().zip(mean_pool(&hidden, &mask, self.dim)?) {
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

/// Ce qu'un lot peut coûter en attention : textes × pièces², soit **quatre** textes de la longueur
/// maximale.
///
/// ## Pourquoi un budget, et pas un nombre de textes
///
/// La mémoire d'un lot est linéaire en nombre de textes et **quadratique** en longueur : douze
/// couches de matrices `pièces × pièces` par texte. Mesuré le 2026-09-25 hors du crate : +290 Mio
/// pour 16 × 256, +140 pour 8 × 256, +84 pour 16 × 128, +27 pour 16 × 64. Un lot de seize textes
/// était donc le **pire cas** de seize longs textes, et rien ne le bornait sinon la troncature.
///
/// Et le temps suit la même loi, avec un défaut de plus : un lot formé dans l'ordre d'arrivée se
/// remplit à la longueur de son plus long texte. Un message de trois lignes rangé à côté d'une
/// lettre de deux pages paie les deux pages — d'où le débit qui variait d'un facteur 3,5 à
/// l'intérieur d'une passe (2026-09-22).
///
/// Les textes sont donc **triés par longueur**, puis regroupés tant que le lot tient dans ce
/// budget : quatre longs textes, ou soixante-quatre courts, et presque aucun remplissage.
///
/// ## Pourquoi quatre, et pas huit
///
/// Huit a été mesuré d'abord, sur le modèle taillé : crête de passe de 517 à 525 Mio, soit
/// 182 au-dessus des 343 du modèle chargé — et le seuil du critère 5 est de 500 Mo, 476,8 Mio.
/// Quatre divise par deux ce qui dépassait. Le prix est un lot plus petit pour les seuls longs
/// textes, là où le calcul est de toute façon dominé par leurs propres pièces.
const ATTENTION_BUDGET: usize = 4 * MAX_TOKENS * MAX_TOKENS;

/// Plafond de textes par lot, même très courts : au-delà, le coût linéaire — les couches denses —
/// reprend le dessus sur l'attention, et le budget ne le voit plus.
const MAX_BATCH: usize = 64;

/// Répartit des textes en lots, par longueur croissante.
///
/// Rend des indices dans la liste d'origine : chaque texte apparaît dans exactement un lot, et
/// chaque lot tient dans [`ATTENTION_BUDGET`] — sauf un texte seul, qu'on ne peut pas couper.
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
        let cost = (current.len() + 1).saturating_mul(longest.saturating_mul(longest));
        if !current.is_empty() && (cost > ATTENTION_BUDGET || current.len() >= MAX_BATCH) {
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
    /// Ce n'est plus la taille d'un lot de calcul — c'est [`ATTENTION_BUDGET`] qui la borne,
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

/// Le répertoire où ce modèle s'installe, sous la racine du store.
#[must_use]
pub fn default_dir(store_root: &Utf8Path) -> Utf8PathBuf {
    store_root.join("model-bert")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::{ATTENTION_BUDGET, Bert, MAX_BATCH, MAX_TOKENS, Role, plan};
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
        group.len() * longest * longest
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
                group.len() == 1 || cost(group, &lengths) <= ATTENTION_BUDGET,
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
            vec![4, 4, 4, 4]
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

    #[test]
    fn a_long_text_is_cut_at_the_window() {
        let (_dir, bert) = tiny();
        let long = "bon ".repeat(2 * MAX_TOKENS);
        assert_eq!(bert.tokens(&long, Role::Passage).unwrap().len(), MAX_TOKENS);
    }
}
