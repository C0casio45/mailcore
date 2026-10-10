//! Tailler le vocabulaire du transformeur — par écriture, et non par corpus.
//!
//! ## Ce qui pèse
//!
//! `multilingual-e5-small` connaît 250 002 pièces, pour une centaine de langues. Sa table de
//! plongements en porte une ligne chacune : 366 Mio des 448 que pèsent ses poids. Son tokeniseur,
//! une fois chargé, en occupe 250 de plus — un arbre de préfixes et deux tables par pièce. Le
//! courrier de ce corpus est écrit en français et en anglais.
//!
//! ## Pourquoi par écriture, et pas par corpus
//!
//! L'embedding statique a été taillé le 2026-09-15 sur **les pièces qu'un corpus employait**. La
//! méthode marche, et elle a trois défauts qui ne se voyaient pas alors :
//!
//! - **elle se fige à l'installation.** Le courrier qui arrive ensuite, et les requêtes qu'on
//!   tape, tombent sur `<unk>` dès qu'ils emploient une pièce que le corpus d'alors n'avait pas ;
//! - **elle est propre à la machine.** Deux machines taillent deux modèles, donc deux empreintes,
//!   et un magasin de vecteurs ne passe plus de l'une à l'autre — ce que `pass::Identity` promet ;
//! - **elle ne se vérifie que sur ce qui l'a servie.** `model-check` comparait les messages mêmes
//!   dont on avait tiré le vocabulaire.
//!
//! Garder les pièces **écrites en alphabet latin** — et avec lui la ponctuation, les chiffres, les
//! monnaies, les symboles et les émoji — n'en a aucun. Le résultat ne dépend que du fichier
//! source : son empreinte est une constante qu'on peut écrire et vérifier. Et pour tout texte
//! écrit dans ces blocs, la segmentation est **exactement** celle du modèle entier, par
//! construction : une pièce qui contient un caractère cyrillique ne peut pas apparaître dans la
//! segmentation d'un texte qui n'en contient pas. Ce qui disparaît ne pouvait pas servir.
//!
//! ## « Écrit dans ces blocs » veut dire **après normalisation**
//!
//! Le tokeniseur normalise avant de découper, et la normalisation fait sortir certains caractères
//! de leur bloc : le signe micro `µ` du Latin-1 devient le `μ` grec, le chevron `〉` des symboles
//! techniques devient celui de la ponctuation CJK. Trouvé le 2026-09-25 sur le corpus : dix-neuf
//! messages écrits entièrement dans les blocs gardés avaient perdu des pièces, et la vérification
//! par points de code a nommé les deux coupables — `U+3009` et `U+03BC`.
//!
//! D'où la seconde moitié de la règle : chaque caractère gardé est passé **par la normalisation du
//! modèle lui-même**, et les caractères qui en sortent sont gardés aussi. Pas une liste écrite à la
//! main — elle aurait oublié le troisième coupable —, mais ce que le fichier source dit de
//! lui-même. Le résultat en dépend toujours seul, donc l'empreinte reste une constante.
//!
//! Ce que ça coûte, et qui se mesure au lieu de se supposer : un caractère hors de ces blocs
//! devient `<unk>`. Un message écrit en grec perd son sens pour la recherche sémantique, et il le
//! garde pour la recherche plein texte, qui ne dépend d'aucun modèle.
//!
//! ## Ce qui est réécrit
//!
//! Trois fichiers, et chacun porte les identifiants des pièces à un endroit différent — c'est là
//! que se loge l'erreur d'un cran qui donne un modèle qui marche et dont chaque vecteur est faux :
//!
//! - `tokenizer.json` : la liste des pièces, l'identifiant de `<unk>`, ceux des jetons ajoutés,
//!   et ceux que le post-traitement écrit en dur autour de chaque texte ;
//! - `model.safetensors` : les lignes gardées de la table, **dans le même ordre** — la ligne *n*
//!   du fichier taillé est celle de la *n*-ième pièce gardée — et les autres tenseurs recopiés à
//!   l'octet ;
//! - `config.json` : la taille du vocabulaire, que `candle` compare à la forme de la table.
//!
//! Les pièces gardées restent dans leur ordre d'origine : les quatre jetons spéciaux du début
//! gardent donc leurs identifiants, 0 à 3, et le post-traitement de `e5` qui les écrit en dur
//! resterait juste même sans être réécrit. Il l'est quand même : c'est une propriété de cet
//! ordre, pas une garantie du format.

use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};

use camino::{Utf8Path, Utf8PathBuf};
use serde_json::Value;

use crate::{Error, Result};

/// Le nom de la table de plongements dans les poids de `e5` et de BERT.
const EMBEDDINGS: &str = "embeddings.word_embeddings.weight";

/// Les fichiers qu'un modèle transformeur doit porter.
const FILES: [&str; 3] = ["model.safetensors", "tokenizer.json", "config.json"];

/// Ceux qui disent comment le modèle fait un vecteur, recopiés tels quels s'ils existent.
///
/// Ils ne portent aucun identifiant de pièce, donc la taille n'a rien à y réécrire — mais les
/// **perdre** change chaque vecteur sans rien signaler : sans `1_Pooling/config.json`,
/// [`crate::bert::Bert`] retombe sur la moyenne, et `granite-embedding` met en commun au premier
/// jeton. Un modèle taillé qui aurait gardé ses poids et perdu sa mise en commun serait un autre
/// modèle sous le même nom.
pub(crate) const CARRIED: [&str; 2] = crate::bert::SETTINGS;

/// Ce qu'une taille a fait.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    /// Les pièces du vocabulaire d'origine.
    pub pieces: usize,
    /// Celles qui ont été gardées, jetons spéciaux compris.
    pub kept: usize,
    /// Les lignes de la table d'origine — parfois plus que de pièces : celle de `e5` en a 250 037
    /// pour 250 002 pièces, et les 35 dernières ne sont jamais lues.
    pub rows: usize,
    /// L'empreinte des poids écrits, au format de la provenance.
    pub digest: String,
    /// Les caractères hors des blocs gardés que la normalisation produit à partir d'un
    /// caractère gardé — `μ` depuis `µ` —, et qui sont donc gardés aussi.
    pub images: usize,
}

/// Les blocs que la taille garde, en intervalles de points de code.
///
/// La liste est **écrite**, bloc par bloc, plutôt que déduite d'une propriété Unicode : c'est ce
/// qui décide de ce qu'un modèle sait lire, et elle doit pouvoir se relire. Ce qui manque ici est
/// une écriture — grec, cyrillique, arabe, hébreu, écritures d'Asie —, et avec elles les symboles
/// qui n'existent qu'à l'intérieur de l'une d'elles : la ponctuation CJK, le `・` japonais.
///
/// La première version annonçait « jamais un symbole » et oubliait le caractère de remplacement
/// `�` — fréquent dans le courrier mal décodé —, la ponctuation supplémentaire `⸺`, les formes
/// verticales et compactes, les symboles hérités de l'informatique et les étiquettes des drapeaux
/// régionaux. Une relecture l'a relevé le jour même ; ils y sont.
const KEPT: &[(u32, u32)] = &[
    // Latin de base et Latin-1, Latin étendu A et B, alphabet phonétique, lettres modificatives,
    // diacritiques combinants.
    (0x0000, 0x036F),
    // Diacritiques combinants étendus et supplémentaires.
    (0x1AB0, 0x1AFF),
    (0x1DC0, 0x1DFF),
    // Latin étendu additionnel — le vietnamien, et les lettres rares du français ancien.
    (0x1E00, 0x1EFF),
    // Ponctuation générale, exposants, monnaies, symboles lettrés, formes numérales, flèches,
    // opérateurs, symboles techniques, pavés et formes géométriques, symboles divers et
    // dingbats — y compris `▁`, le marqueur d'espace du tokeniseur (U+2581).
    (0x2000, 0x2BFF),
    // Latin étendu C.
    (0x2C60, 0x2C7F),
    // Ponctuation supplémentaire — `⸺`, `⸻`, `⸮`.
    (0x2E00, 0x2E7F),
    // Les quatre émoji rangés dans des blocs CJK : `〰`, `〽`, `㊗`, `㊙`.
    (0x3030, 0x3030),
    (0x303D, 0x303D),
    (0x3297, 0x3297),
    (0x3299, 0x3299),
    // Latin étendu D et E.
    (0xA720, 0xA7FF),
    (0xAB30, 0xAB6F),
    // Ligatures latines, sélecteurs de variante, formes verticales, demi-signes combinants, formes
    // de compatibilité et petites formes de ponctuation.
    (0xFB00, 0xFB06),
    (0xFE00, 0xFE6F),
    // Caractères spéciaux, dont `�`, le caractère de remplacement.
    (0xFFF0, 0xFFFF),
    // Émoji et symboles, symboles hérités de l'informatique.
    (0x1F000, 0x1FBFF),
    // Étiquettes, qui composent les drapeaux régionaux.
    (0xE0000, 0xE007F),
];

/// Les signes combinants gardés : ceux qu'un texte gardé peut placer après une lettre, et que la
/// normalisation peut composer avec elle.
const MARKS: &[(u32, u32)] = &[
    (0x0300, 0x036F),
    (0x1AB0, 0x1AFF),
    (0x1DC0, 0x1DFF),
    (0x20D0, 0x20FF),
    (0xFE20, 0xFE2F),
];

/// Vrai si un caractère appartient aux blocs que la taille garde.
///
/// Ce n'est pas encore la règle entière : voir [`images`] pour ce que la normalisation y ajoute.
#[must_use]
pub fn is_kept(c: char) -> bool {
    let code = u32::from(c);
    KEPT.iter()
        .any(|(first, last)| (*first..=*last).contains(&code))
}

/// Les caractères hors des blocs gardés que la normalisation du tokeniseur produit à partir d'un
/// caractère gardé.
///
/// Chaque point de code des blocs est normalisé seul, par le normaliseur que le fichier déclare ;
/// ce qui en sort et n'est pas gardé est rendu.
///
/// ## Et chaque lettre suivie d'un signe combinant
///
/// La normalisation **compose** aussi : le signe ohm `Ω` des symboles lettrés suivi d'un accent
/// aigu combinant — deux caractères gardés — devient `Ώ`, une lettre grecque qu'aucun caractère
/// gardé ne produit seul. Une relecture l'a montré le 2026-09-25 sous NFC et NFKC : huit paires
/// sous NFKC, treize sous NFC, et aucune sous le normaliseur de `e5`, qui ne compose pas. Les
/// paires sont donc essayées aussi : chaque caractère gardé du plan de base, suivi de chaque signe
/// de [`MARKS`] — trois millions de normalisations, quelques secondes, à l'installation.
///
/// Ce qui reste hors de portée, écrit pour ne pas le croire couvert : une composition qui
/// demanderait **deux** signes à la suite pour sortir des blocs. Aucun n'est connu ; la
/// vérification sur le corpus — `cargo xtask model-check-bert` — est ce qui le dirait.
///
/// # Errors
///
/// Une phrase si le normaliseur déclaré n'est pas lisible.
fn images(
    normalizer: Option<&Value>,
) -> std::result::Result<std::collections::BTreeSet<char>, String> {
    use tokenizers::Normalizer;

    let mut found = std::collections::BTreeSet::new();
    let Some(normalizer) = normalizer.filter(|value| !value.is_null()) else {
        return Ok(found);
    };
    let normalizer: tokenizers::normalizers::NormalizerWrapper =
        serde_json::from_value(normalizer.clone())
            .map_err(|e| format!("normaliseur illisible : {e}"))?;
    let mut image = |text: &str| -> std::result::Result<(), String> {
        let mut normalized = tokenizers::NormalizedString::from(text);
        normalizer
            .normalize(&mut normalized)
            .map_err(|e| format!("normalisation de {text:?} : {e}"))?;
        found.extend(normalized.get().chars().filter(|c| !is_kept(*c)));
        Ok(())
    };

    let characters = |ranges: &'static [(u32, u32)]| {
        ranges
            .iter()
            .flat_map(|(first, last)| *first..=*last)
            .filter_map(char::from_u32)
    };
    let marks: Vec<char> = characters(MARKS).collect();
    for base in characters(KEPT) {
        let mut buffer = [0u8; 8];
        image(base.encode_utf8(&mut buffer))?;
        // Les paires, pour le plan de base seulement : un émoji ne se compose avec aucun accent.
        if u32::from(base) <= 0xFFFF {
            for mark in &marks {
                let pair: String = [base, *mark].iter().collect();
                image(&pair)?;
            }
        }
    }
    Ok(found)
}

/// Taille un modèle transformeur de `source` vers `target`.
///
/// `target` doit être vide de ces fichiers et de ceux de [`CARRIED`] : une taille n'écrase jamais
/// un modèle installé.
///
/// ## Tout ou rien
///
/// **Tout ce qui peut être refusé l'est avant la première écriture** : le tokeniseur et la
/// configuration sont réécrits en mémoire d'abord, et les fichiers recopiés lus. Puis tout est écrit
/// sous des noms provisoires, et renommé ensemble à la fin, `config.json` en dernier — c'est lui
/// que le chargement lit d'abord, donc un répertoire interrompu est un modèle absent, jamais un
/// modèle à moitié écrit. Un échec en route retire ce que l'appel avait écrit.
///
/// Les fichiers recopiés passent **avant** `config.json` pour la même raison, et elle y est plus
/// forte : un répertoire coupé entre `config.json` et la mise en commun se chargerait, ferait la
/// moyenne là où le modèle prend le premier jeton, et porterait l'empreinte des réglages de `e5`.
/// La première version les renommait après ; trouvé par une relecture le 2026-10-10.
///
/// La première version lisait `config.json` **après** avoir mis les poids en place : une
/// configuration illisible laissait un `model.safetensors` derrière elle, et la taille suivante
/// refusait d'écraser « un modèle » qui n'en était pas un. Trouvé par une relecture le 2026-09-25.
///
/// # Errors
///
/// [`Error::Model`] si la source est illisible ou d'un format que cette taille ne sait pas
/// réécrire — tout ce qui n'est pas un tokeniseur `Unigram` à post-traitement connu est refusé,
/// plutôt que réécrit de travers —, ou si la cible est occupée ou non inscriptible.
pub fn trim(source: &Utf8Path, target: &Utf8Path) -> Result<Report> {
    for name in FILES {
        if !source.join(name).is_file() {
            return Err(model_error(source, format!("{name} absent")));
        }
        if target.join(name).exists() {
            return Err(model_error(
                target,
                format!("{name} existe déjà : une taille n'écrase pas un modèle"),
            ));
        }
    }
    for name in CARRIED {
        if target.join(name).exists() {
            return Err(model_error(
                target,
                format!("{name} existe déjà : une taille n'écrase pas un modèle"),
            ));
        }
    }

    let tokenizer = std::fs::read_to_string(source.join("tokenizer.json").as_std_path())
        .map_err(|e| model_error(source, format!("tokenizer.json : {e}")))?;
    let rewritten = rewrite_tokenizer(&tokenizer).map_err(|reason| model_error(source, reason))?;
    let config = std::fs::read_to_string(source.join("config.json").as_std_path())
        .map_err(|e| model_error(source, format!("config.json : {e}")))?;
    let config =
        rewrite_config(&config, &rewritten).map_err(|reason| model_error(source, reason))?;
    let mut carried = Vec::new();
    for name in CARRIED {
        let from = source.join(name);
        if from.is_file() {
            let bytes = std::fs::read(from.as_std_path())
                .map_err(|e| model_error(source, format!("{name} : {e}")))?;
            carried.push((name, bytes));
        }
    }

    std::fs::create_dir_all(target.as_std_path())
        .map_err(|e| model_error(target, format!("création du répertoire : {e}")))?;
    let mut staged = Staged::default();
    let weights = target.join("model.safetensors");
    let (rows, digest) = rewrite_weights(
        &source.join("model.safetensors"),
        &staged.partial(&weights),
        &rewritten.kept,
    )
    .map_err(|reason| model_error(source, reason))?;
    staged.write(&target.join("tokenizer.json"), rewritten.text.as_bytes())?;
    for (name, bytes) in &carried {
        let to = target.join(name);
        if let Some(parent) = to.parent() {
            std::fs::create_dir_all(parent.as_std_path())
                .map_err(|e| model_error(target, format!("création de {parent} : {e}")))?;
        }
        staged.write(&to, bytes)?;
    }
    staged.write(&target.join("config.json"), config.as_bytes())?;
    // L'ordre d'écriture est l'ordre des renommages : vérifié ici, où une écriture ajoutée après
    // coup le casserait, et par chaque test qui taille un modèle porteur de réglages.
    debug_assert_eq!(
        staged.files.last().map(|(_, path)| path.as_path()),
        Some(target.join("config.json").as_path()),
        "config.json doit être renommé en dernier"
    );
    staged.commit()?;

    Ok(Report {
        pieces: rewritten.pieces,
        kept: rewritten.kept.len(),
        rows,
        digest,
        images: rewritten.images,
    })
}

/// Ce que la réécriture du tokeniseur rend.
struct Rewritten {
    /// Le nouveau `tokenizer.json`.
    text: String,
    /// Les identifiants d'origine gardés, dans l'ordre : la *n*-ième pièce gardée est `kept[n]`.
    kept: Vec<usize>,
    /// L'identifiant nouveau de chaque identifiant d'origine, s'il est gardé.
    map: Vec<Option<u64>>,
    /// Les pièces du vocabulaire d'origine.
    pieces: usize,
    /// Les caractères gardés par normalisation.
    images: usize,
}

impl Rewritten {
    /// L'identifiant nouveau d'un identifiant d'origine, ou une phrase qui dit qu'il a été retiré.
    fn remap(&self, old: u64) -> std::result::Result<u64, String> {
        remap_in(&self.map, old)
    }
}

fn remap_in(map: &[Option<u64>], old: u64) -> std::result::Result<u64, String> {
    usize::try_from(old)
        .ok()
        .and_then(|old| map.get(old).copied().flatten())
        .ok_or_else(|| format!("identifiant {old} sans pièce gardée"))
}

/// Réécrit le tokeniseur, en mémoire.
fn rewrite_tokenizer(text: &str) -> std::result::Result<Rewritten, String> {
    let mut root: Value =
        serde_json::from_str(text).map_err(|e| format!("tokenizer.json illisible : {e}"))?;

    let model = root
        .get("model")
        .ok_or_else(|| "tokenizer.json sans modèle".to_owned())?;
    let kind = model.get("type").and_then(Value::as_str).unwrap_or("?");
    if kind != "Unigram" {
        return Err(format!(
            "tokeniseur {kind} : seul un tokeniseur Unigram sait être taillé ici"
        ));
    }
    if model.get("byte_fallback").and_then(Value::as_bool) == Some(true) {
        return Err("tokeniseur à repli sur octets : non géré".to_owned());
    }
    let vocab = model
        .get("vocab")
        .and_then(Value::as_array)
        .ok_or_else(|| "tokenizer.json sans liste de pièces".to_owned())?;
    let pieces = vocab.len();

    // Les jetons spéciaux sont lus **dans le fichier**, jamais déduits : la leçon du
    // 2026-09-15, où une différence entre deux vocabulaires rendue vide les avait fait perdre.
    let unk = model
        .get("unk_id")
        .and_then(Value::as_u64)
        .and_then(|id| usize::try_from(id).ok());
    let added: Vec<usize> = root
        .get("added_tokens")
        .and_then(Value::as_array)
        .map(|tokens| {
            tokens
                .iter()
                .filter_map(|token| token.get("id").and_then(Value::as_u64))
                .filter_map(|id| usize::try_from(id).ok())
                .collect()
        })
        .unwrap_or_default();
    if let Some(beyond) = unk.iter().chain(&added).find(|id| **id >= pieces) {
        return Err(format!(
            "jeton spécial d'identifiant {beyond}, au-delà des {pieces} pièces"
        ));
    }

    // Ce que la normalisation fait sortir des blocs gardés : gardé aussi, sans quoi un `µ` tapé
    // deviendrait un `μ` inconnu — voir l'en-tête du module.
    let images = images(root.get("normalizer"))?;

    let mut kept = Vec::new();
    for (id, entry) in vocab.iter().enumerate() {
        let piece = entry
            .get(0)
            .and_then(Value::as_str)
            .ok_or_else(|| format!("pièce {id} illisible"))?;
        if unk == Some(id)
            || added.contains(&id)
            || piece.chars().all(|c| is_kept(c) || images.contains(&c))
        {
            kept.push(id);
        }
    }
    let mut map = vec![None; pieces];
    for (new, old) in kept.iter().enumerate() {
        if let Some(slot) = map.get_mut(*old) {
            *slot = Some(new as u64);
        }
    }
    let remap = |old: u64| remap_in(&map, old);

    let new_vocab: Vec<Value> = kept
        .iter()
        .filter_map(|id| vocab.get(*id).cloned())
        .collect();
    let model = root
        .get_mut("model")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| "tokenizer.json sans modèle".to_owned())?;
    model.insert("vocab".to_owned(), Value::Array(new_vocab));
    if let Some(unk) = unk {
        model.insert("unk_id".to_owned(), Value::from(remap(unk as u64)?));
    }

    if let Some(tokens) = root.get_mut("added_tokens").and_then(Value::as_array_mut) {
        for token in tokens {
            if let Some(id) = token.get("id").and_then(Value::as_u64) {
                let new = remap(id)?;
                if let Some(slot) = token.get_mut("id") {
                    *slot = Value::from(new);
                }
            }
        }
    }

    match root.get_mut("post_processor") {
        None | Some(Value::Null) => {}
        Some(processor) => remap_processor(processor, &remap)?,
    }

    if let Some(padding) = root.get_mut("padding").and_then(Value::as_object_mut)
        && let Some(id) = padding.get("pad_id").and_then(Value::as_u64)
    {
        padding.insert("pad_id".to_owned(), Value::from(remap(id)?));
    }

    let text = serde_json::to_string(&canonical(root))
        .map_err(|e| format!("écriture du tokeniseur : {e}"))?;
    Ok(Rewritten {
        text,
        kept,
        map,
        pieces,
        images: images.len(),
    })
}

/// Réécrit les identifiants que le post-traitement porte en dur.
///
/// Un seul type est connu, celui de `e5` ; un autre est **refusé** plutôt que recopié. Un
/// post-traitement recopié avec d'anciens identifiants encadrerait chaque texte par des pièces
/// qui ne sont plus les bonnes, et rien ne le dirait.
fn remap_processor(
    processor: &mut Value,
    remap: &impl Fn(u64) -> std::result::Result<u64, String>,
) -> std::result::Result<(), String> {
    let kind = processor
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("?")
        .to_owned();
    if kind != "TemplateProcessing" {
        return Err(format!("post-traitement {kind} : non géré"));
    }
    let Some(specials) = processor
        .get_mut("special_tokens")
        .and_then(Value::as_object_mut)
    else {
        return Ok(());
    };
    for special in specials.values_mut() {
        if let Some(ids) = special.get_mut("ids").and_then(Value::as_array_mut) {
            for id in ids {
                let old = id
                    .as_u64()
                    .ok_or_else(|| "identifiant de post-traitement illisible".to_owned())?;
                *id = Value::from(remap(old)?);
            }
        }
    }
    Ok(())
}

/// Les champs de `config.json` qui portent un identifiant de pièce.
///
/// `pad_token_id` est celui que [`crate::bert::Bert`] lit pour remplir un lot : un identifiant
/// resté ancien après la taille désignerait une autre pièce, ou une ligne hors de la table — et
/// chaque lot de longueurs mêlées échouerait.
const CONFIG_IDS: [&str; 3] = ["pad_token_id", "bos_token_id", "eos_token_id"];

/// Réécrit `config.json`, en mémoire : la taille du vocabulaire, et les identifiants qu'il porte.
fn rewrite_config(text: &str, rewritten: &Rewritten) -> std::result::Result<String, String> {
    let mut config: Value =
        serde_json::from_str(text).map_err(|e| format!("config.json illisible : {e}"))?;
    let object = config
        .as_object_mut()
        .ok_or_else(|| "config.json n'est pas un objet".to_owned())?;
    object.insert("vocab_size".to_owned(), Value::from(rewritten.kept.len()));
    for key in CONFIG_IDS {
        if let Some(old) = object.get(key).and_then(Value::as_u64) {
            let new = rewritten
                .remap(old)
                .map_err(|_| format!("config.json : {key} désigne la pièce {old}, retirée"))?;
            object.insert(key.to_owned(), Value::from(new));
        }
    }
    serde_json::to_string_pretty(&canonical(config))
        .map_err(|e| format!("écriture de config.json : {e}"))
}

/// Une valeur JSON dont chaque objet a ses clés **dans l'ordre**.
///
/// L'empreinte du modèle taillé est une constante — c'est ce qui permet de la vérifier. Elle
/// dépendait pourtant d'un détail de compilation : `serde_json` range les clés dans l'ordre sans la
/// fonction `preserve_order`, dans l'ordre d'insertion avec. Qu'un autre membre de l'espace de
/// travail l'active, et le même fichier source aurait donné une autre empreinte. Ranger les clés
/// soi-même donne le même texte dans les deux cas.
fn canonical(value: Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut entries: Vec<(String, Value)> = map.into_iter().collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            let mut sorted = serde_json::Map::new();
            for (key, value) in entries {
                sorted.insert(key, canonical(value));
            }
            Value::Object(sorted)
        }
        Value::Array(items) => Value::Array(items.into_iter().map(canonical).collect()),
        other => other,
    }
}

/// Un écrivain qui hache ce qu'il écrit.
struct Hashing<W> {
    inner: W,
    hasher: blake3::Hasher,
}

impl<W: Write> Write for Hashing<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let written = self.inner.write(buf)?;
        if let Some(bytes) = buf.get(..written) {
            self.hasher.update(bytes);
        }
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// Réécrit les poids dans `partial` : la table réduite aux lignes gardées, le reste recopié à
/// l'octet. Rend le nombre de lignes d'origine et l'empreinte de ce qui a été écrit.
///
/// **Par morceaux**, jamais en entier : une installation n'a pas de raison de coûter 470 Mo de
/// mémoire quand quelques mébioctets suffisent.
///
/// ## En avançant, sans revenir en arrière
///
/// La table se lit **dans l'ordre** : les identifiants gardés croissent, donc on saute les lignes
/// retirées vers l'avant et on lit les autres au passage. La première version se replaçait avant
/// chaque ligne gardée, et un repositionnement vide le tampon de lecture : cent quatorze mille
/// relectures d'un mébioctet pour écrire cent soixante-huit mébioctets de lignes.
fn rewrite_weights(
    source: &Utf8Path,
    partial: &Utf8Path,
    kept: &[usize],
) -> std::result::Result<(usize, String), String> {
    let file = std::fs::File::open(source.as_std_path()).map_err(|e| format!("poids : {e}"))?;
    let length = file.metadata().map_err(|e| format!("poids : {e}"))?.len();
    let mut reader = BufReader::with_capacity(1 << 20, file);
    let layout = crate::weights::read_layout(&mut reader, length)?;

    let table = layout
        .entries
        .iter()
        .find(|entry| entry.name == EMBEDDINGS)
        .ok_or_else(|| format!("pas de tenseur {EMBEDDINGS}"))?;
    let [rows, hidden] = table.shape.as_slice() else {
        return Err(format!("{EMBEDDINGS} n'est pas une matrice"));
    };
    let (rows, hidden) = (*rows, *hidden);
    // Les lignes sont recopiées à l'octet, donc le format importe peu tant qu'on sait sa largeur ;
    // `granite-embedding` est publié en BF16, et le chargement l'élargit.
    if !matches!(
        table.kind,
        crate::weights::Kind::F32 | crate::weights::Kind::BF16
    ) {
        return Err(format!("{EMBEDDINGS} n'est ni en F32 ni en BF16"));
    }
    if let Some(beyond) = kept.iter().find(|id| **id >= rows) {
        return Err(format!(
            "la pièce {beyond} n'a pas de ligne dans une table de {rows}"
        ));
    }
    if kept.windows(2).any(|pair| pair[0] >= pair[1]) {
        return Err("les pièces gardées ne sont pas dans l'ordre".to_owned());
    }
    let row_bytes = (hidden as u64)
        .checked_mul(table.kind.width())
        .ok_or_else(|| format!("une ligne de {EMBEDDINGS} déborde"))?;
    let kept_bytes = (kept.len() as u64)
        .checked_mul(row_bytes)
        .ok_or_else(|| format!("{EMBEDDINGS} taillée déborde"))?;

    // L'en-tête d'arrivée, dans l'ordre des noms : un fichier qui ne dépend que de sa source,
    // octet pour octet, donc une empreinte qu'on peut écrire d'avance.
    let mut entries = layout.entries.clone();
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    let mut header = serde_json::Map::new();
    if let Some(metadata) = &layout.metadata {
        header.insert("__metadata__".to_owned(), metadata.clone());
    }
    let mut offset = 0u64;
    for entry in &entries {
        let (shape, bytes) = if entry.name == EMBEDDINGS {
            (vec![kept.len(), hidden], kept_bytes)
        } else {
            (entry.shape.clone(), entry.end - entry.begin)
        };
        header.insert(
            entry.name.clone(),
            serde_json::json!({
                "dtype": entry.kind.name(),
                "shape": shape,
                "data_offsets": [offset, offset + bytes],
            }),
        );
        offset += bytes;
    }
    let mut header = serde_json::to_string(&canonical(Value::Object(header)))
        .map_err(|e| format!("en-tête : {e}"))?;
    // Aligné sur huit octets, comme le fait l'écrivain de référence du format.
    while header.len() % 8 != 0 {
        header.push(' ');
    }

    let out = std::fs::File::create(partial.as_std_path())
        .map_err(|e| format!("création de {partial} : {e}"))?;
    let mut writer = Hashing {
        inner: BufWriter::with_capacity(1 << 20, out),
        hasher: blake3::Hasher::new(),
    };
    let io = |e: std::io::Error| format!("écriture des poids : {e}");
    writer
        .write_all(&(header.len() as u64).to_le_bytes())
        .map_err(io)?;
    writer.write_all(header.as_bytes()).map_err(io)?;

    let mut buffer = vec![0u8; 1 << 20];
    for entry in &entries {
        let start = layout.data_start + entry.begin;
        reader
            .seek(SeekFrom::Start(start))
            .map_err(|e| format!("lecture de {} : {e}", entry.name))?;
        if entry.name == EMBEDDINGS {
            let row = buffer
                .get_mut(..usize::try_from(row_bytes).map_err(|e| e.to_string())?)
                .ok_or_else(|| "ligne plus longue que le tampon".to_owned())?;
            let mut next = 0usize;
            for id in kept {
                // Vers l'avant, dans le tampon quand c'est possible : `seek_relative` ne le vide
                // pas si la cible y tient.
                let gap = i64::try_from((*id - next) as u64 * row_bytes)
                    .map_err(|e| format!("saut dans la table : {e}"))?;
                reader
                    .seek_relative(gap)
                    .map_err(|e| format!("lecture de la table : {e}"))?;
                reader
                    .read_exact(row)
                    .map_err(|e| format!("lecture de la table : {e}"))?;
                writer.write_all(row).map_err(io)?;
                next = id + 1;
            }
        } else {
            let mut remaining = entry.end - entry.begin;
            while remaining > 0 {
                let take =
                    usize::try_from(remaining.min(buffer.len() as u64)).unwrap_or(buffer.len());
                let chunk = buffer
                    .get_mut(..take)
                    .ok_or_else(|| "tampon de lecture trop court".to_owned())?;
                reader
                    .read_exact(chunk)
                    .map_err(|e| format!("lecture de {} : {e}", entry.name))?;
                writer.write_all(chunk).map_err(io)?;
                remaining -= take as u64;
            }
        }
    }

    writer.flush().map_err(io)?;
    let digest = writer.hasher.finalize().to_hex()[..16].to_owned();
    let file = writer
        .inner
        .into_inner()
        .map_err(|e| format!("écriture des poids : {}", e.error()))?;
    file.sync_all().map_err(io)?;
    Ok((rows, digest))
}

/// Les fichiers qu'une taille écrit, sous leurs noms provisoires, jusqu'à ce qu'elle réussisse.
///
/// Détruit sans avoir été validé, il retire tout ce qu'il a écrit — provisoires et fichiers déjà
/// renommés : un échec ne laisse rien qui ressemble à un modèle, ni qui empêche la taille suivante.
#[derive(Default)]
struct Staged {
    /// `(provisoire, définitif)`, dans l'ordre où ils seront renommés.
    files: Vec<(Utf8PathBuf, Utf8PathBuf)>,
    /// Combien ont déjà été renommés.
    renamed: usize,
    committed: bool,
}

impl Staged {
    /// Réserve un nom provisoire pour `path`.
    fn partial(&mut self, path: &Utf8Path) -> Utf8PathBuf {
        let mut name = path.file_name().unwrap_or("fichier").to_owned();
        name.push_str(".partial");
        let partial = path.with_file_name(name);
        self.files.push((partial.clone(), path.to_owned()));
        partial
    }

    /// Écrit `bytes` sous un nom provisoire pour `path`.
    fn write(&mut self, path: &Utf8Path, bytes: &[u8]) -> Result<()> {
        let partial = self.partial(path);
        let mut file = std::fs::File::create(partial.as_std_path())
            .map_err(|e| model_error(path, format!("création : {e}")))?;
        file.write_all(bytes)
            .and_then(|()| file.sync_all())
            .map_err(|e| model_error(path, format!("écriture : {e}")))
    }

    /// Renomme tout, dans l'ordre.
    fn commit(&mut self) -> Result<()> {
        for (partial, path) in &self.files {
            std::fs::rename(partial.as_std_path(), path.as_std_path())
                .map_err(|e| model_error(path, format!("renommage : {e}")))?;
            self.renamed += 1;
        }
        self.committed = true;
        Ok(())
    }
}

impl Drop for Staged {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        for (index, (partial, path)) in self.files.iter().enumerate() {
            let written = if index < self.renamed { path } else { partial };
            if written.exists()
                && let Err(error) = std::fs::remove_file(written.as_std_path())
            {
                tracing::warn!(%written, %error, "fichier d'une taille échouée non retiré");
            }
        }
    }
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
    use super::{is_kept, trim};
    use crate::bert::{Bert, Role};
    use crate::testing::{TINY_PIECES, tiny_bert};
    use camino::Utf8PathBuf;

    struct Models {
        _dir: tempfile::TempDir,
        full: Utf8PathBuf,
        trimmed: Utf8PathBuf,
    }

    fn trimmed() -> (Models, super::Report) {
        let dir = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        let full = root.join("full");
        let trimmed = root.join("trimmed");
        tiny_bert(&full, TINY_PIECES);
        let report = trim(&full, &trimmed).unwrap();
        (
            Models {
                _dir: dir,
                full,
                trimmed,
            },
            report,
        )
    }

    fn vocabulary(dir: &Utf8PathBuf) -> Vec<String> {
        let tokenizer: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(dir.join("tokenizer.json").as_std_path()).unwrap(),
        )
        .unwrap();
        tokenizer["model"]["vocab"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry[0].as_str().unwrap().to_owned())
            .collect()
    }

    #[test]
    fn the_blocks_kept_are_latin_and_symbols_and_nothing_else() {
        for c in [
            'a', 'é', 'ç', 'œ', 'ấ', '€', '’', '«', '—', '😀', '▁', '½', '→',
        ] {
            assert!(is_kept(c), "{c:?} devrait être gardé");
        }
        for c in ['Ж', 'α', '中', 'ש', 'ع', 'ก', '한'] {
            assert!(!is_kept(c), "{c:?} ne devrait pas être gardé");
        }
    }

    #[test]
    fn a_bf16_model_keeps_its_format_and_the_files_that_say_how_it_pools() {
        // `granite-embedding` : poids en BF16, mise en commun au premier jeton, aucun préfixe.
        let dir = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        let full = root.join("full");
        tiny_bert(&full, TINY_PIECES);
        let weights = full.join("model.safetensors");
        let device = candle_core::Device::Cpu;
        let half: std::collections::HashMap<String, candle_core::Tensor> =
            candle_core::safetensors::load(weights.as_std_path(), &device)
                .unwrap()
                .into_iter()
                .map(|(name, t)| (name, t.to_dtype(candle_core::DType::BF16).unwrap()))
                .collect();
        candle_core::safetensors::save(&half, weights.as_std_path()).unwrap();
        let carried = [
            (
                "1_Pooling/config.json",
                r#"{"pooling_mode_cls_token": true, "pooling_mode_mean_tokens": false}"#,
            ),
            ("config_sentence_transformers.json", r#"{"prompts": {}}"#),
        ];
        for (name, text) in carried {
            let path = full.join(name);
            std::fs::create_dir_all(path.parent().unwrap().as_std_path()).unwrap();
            std::fs::write(path.as_std_path(), text).unwrap();
        }

        let trimmed = root.join("trimmed");
        trim(&full, &trimmed).unwrap();
        for (name, text) in carried {
            assert_eq!(
                std::fs::read_to_string(trimmed.join(name).as_std_path()).unwrap(),
                text,
                "{name}"
            );
        }
        let table = candle_core::safetensors::load(
            trimmed.join("model.safetensors").as_std_path(),
            &device,
        )
        .unwrap()
        .remove(super::EMBEDDINGS)
        .unwrap();
        assert_eq!(table.dtype(), candle_core::DType::BF16);

        // Mêmes vecteurs : sans la mise en commun recopiée, le taillé ferait la moyenne là où le
        // modèle entier prend le premier jeton, et ce test tomberait — c'est son contrôle inverse,
        // la différence entre les deux étant vérifiée par les tests de `bert`.
        let texts = vec!["bonjour été 😀".to_owned(), "bon jour".to_owned()];
        let full = Bert::load(&full).unwrap();
        let small = Bert::load(&trimmed).unwrap();
        for role in [Role::Passage, Role::Query] {
            assert_eq!(
                full.encode_as(&texts, role).unwrap(),
                small.encode_as(&texts, role).unwrap(),
                "{role:?}"
            );
        }
    }

    #[test]
    fn a_latin_text_gives_exactly_the_same_vector() {
        let (models, _) = trimmed();
        let full = Bert::load(&models.full).unwrap();
        let small = Bert::load(&models.trimmed).unwrap();
        let texts = vec!["bonjour été 😀".to_owned(), "bon jour".to_owned()];
        for role in [Role::Passage, Role::Query] {
            assert_eq!(
                full.encode_as(&texts, role).unwrap(),
                small.encode_as(&texts, role).unwrap(),
                "{role:?}"
            );
        }
    }

    #[test]
    fn a_text_in_another_script_is_what_the_trim_gives_up() {
        // Le contrôle inverse du précédent : sans lui, un test qui compare deux fois le même
        // modèle passerait aussi. Le russe retiré doit changer le vecteur.
        let (models, _) = trimmed();
        let full = Bert::load(&models.full).unwrap();
        let small = Bert::load(&models.trimmed).unwrap();
        let texts = vec!["Москва".to_owned()];
        assert_ne!(
            full.encode_as(&texts, Role::Passage).unwrap(),
            small.encode_as(&texts, Role::Passage).unwrap()
        );
        // Et l'inconnu se voit comme tel dans les pièces : c'est ce que la vérification sur le
        // corpus compte. Rendu par le texte qu'il couvre, il ressemblait à une pièce connue.
        assert!(
            small
                .tokens("Москва", Role::Passage)
                .unwrap()
                .contains(&"<unk>".to_owned())
        );
        assert!(
            !full
                .tokens("Москва", Role::Passage)
                .unwrap()
                .contains(&"<unk>".to_owned())
        );
    }

    /// Le défaut du 2026-09-25 : un texte écrit dans les blocs gardés, dont la normalisation fait
    /// sortir un caractère — le signe micro `µ` devient le `μ` grec. Sans la seconde moitié de la
    /// règle, la pièce `μ` était retirée et le texte tapé perdait son sens.
    #[test]
    fn what_normalisation_makes_of_a_kept_character_is_kept_too() {
        let dir = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        let mut pieces = TINY_PIECES.to_vec();
        let mask = pieces.pop().unwrap();
        pieces.extend([
            ("μ", -6.0),
            ("▁μg", -3.0),
            ("g", -6.0),
            ("Ж", -7.0),
            ("▁Ώ", -4.0),
            ("Ώ", -6.0),
        ]);
        pieces.push(mask);
        crate::testing::tiny_bert_with(
            &root.join("full"),
            &pieces,
            &serde_json::json!({"type": "NFKC"}),
        );
        let report = trim(&root.join("full"), &root.join("trimmed")).unwrap();
        assert!(report.images >= 1, "{report:?}");

        let small = vocabulary(&root.join("trimmed"));
        assert!(
            small.contains(&"μ".to_owned()),
            "la pièce grecque μ a été retirée"
        );
        assert!(small.contains(&"▁μg".to_owned()));
        // Le contrôle inverse : une lettre grecque ou russe qu'aucun caractère gardé ne produit
        // reste retirée — la règle n'a pas gardé tout ce qui passait.
        assert!(!small.contains(&"Ж".to_owned()));

        // Et le texte tel qu'on le tape, signe micro compris, rend le même vecteur.
        let full = Bert::load(root.join("full")).unwrap();
        let trimmed = Bert::load(root.join("trimmed")).unwrap();
        // Le signe ohm suivi d'un accent combinant — deux caractères gardés — se compose en `Ώ`,
        // que la normalisation d'un caractère seul ne produit pas.
        for text in ["5 µg", "\u{2126}\u{301}"] {
            let texts = vec![text.to_owned()];
            assert!(text.chars().all(is_kept), "{text:?}");
            assert_eq!(
                full.tokens(text, Role::Passage).unwrap(),
                trimmed.tokens(text, Role::Passage).unwrap(),
                "{text:?}"
            );
            assert_eq!(
                full.encode_as(&texts, Role::Passage).unwrap(),
                trimmed.encode_as(&texts, Role::Passage).unwrap(),
                "{text:?}"
            );
        }
        assert!(
            small.contains(&"Ώ".to_owned()),
            "la composition a été oubliée"
        );
    }

    #[test]
    fn the_pieces_kept_keep_their_order_and_the_others_are_gone() {
        let (models, report) = trimmed();
        let full = vocabulary(&models.full);
        let small = vocabulary(&models.trimmed);
        let expected: Vec<String> = full
            .iter()
            .filter(|piece| {
                (piece.starts_with('<') && piece.ends_with('>')) || piece.chars().all(is_kept)
            })
            .cloned()
            .collect();
        assert_eq!(small, expected);
        assert!(!small.iter().any(|piece| piece.contains('М')));
        assert_eq!(report.pieces, TINY_PIECES.len());
        assert_eq!(report.kept, small.len());
    }

    #[test]
    fn each_row_of_the_table_follows_its_piece() {
        // L'erreur d'un cran que rien d'autre ne verrait : un modèle qui charge, encode, et dont
        // chaque vecteur est faux.
        let (models, _) = trimmed();
        let device = candle_core::Device::Cpu;
        let full = crate::weights::load(&models.full.join("model.safetensors"), &device).unwrap();
        let small =
            crate::weights::load(&models.trimmed.join("model.safetensors"), &device).unwrap();
        let name = "embeddings.word_embeddings.weight";
        let full_rows = full.tensors[name].to_vec2::<f32>().unwrap();
        let small_rows = small.tensors[name].to_vec2::<f32>().unwrap();

        let full_vocab = vocabulary(&models.full);
        let small_vocab = vocabulary(&models.trimmed);
        assert_eq!(small_rows.len(), small_vocab.len());
        for (row, piece) in small_rows.iter().zip(&small_vocab) {
            let origin = full_vocab.iter().position(|p| p == piece).unwrap();
            assert_eq!(row, &full_rows[origin], "la ligne de {piece:?}");
        }
        // Les autres tenseurs, recopiés à l'identique.
        for (tensor_name, tensor) in &full.tensors {
            if tensor_name != name {
                assert_eq!(
                    tensor.flatten_all().unwrap().to_vec1::<f32>().unwrap(),
                    small.tensors[tensor_name]
                        .flatten_all()
                        .unwrap()
                        .to_vec1::<f32>()
                        .unwrap(),
                    "{tensor_name}"
                );
            }
        }
    }

    #[test]
    fn special_tokens_point_at_their_own_pieces() {
        let (models, _) = trimmed();
        let text =
            std::fs::read_to_string(models.trimmed.join("tokenizer.json").as_std_path()).unwrap();
        let tokenizer: serde_json::Value = serde_json::from_str(&text).unwrap();
        let vocab = vocabulary(&models.trimmed);
        let at =
            |id: &serde_json::Value| vocab[usize::try_from(id.as_u64().unwrap()).unwrap()].clone();

        assert_eq!(at(&tokenizer["model"]["unk_id"]), "<unk>");
        for token in tokenizer["added_tokens"].as_array().unwrap() {
            assert_eq!(at(&token["id"]), token["content"].as_str().unwrap());
        }
        for (name, special) in tokenizer["post_processor"]["special_tokens"]
            .as_object()
            .unwrap()
        {
            assert_eq!(&at(&special["ids"][0]), name);
        }
        let config: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(models.trimmed.join("config.json").as_std_path()).unwrap(),
        )
        .unwrap();
        assert_eq!(
            usize::try_from(config["vocab_size"].as_u64().unwrap()).unwrap(),
            vocab.len()
        );
    }

    #[test]
    fn the_same_source_always_gives_the_same_digest() {
        // Ce qui permet d'écrire l'empreinte d'un modèle taillé d'avance, et de la vérifier.
        let (models, first) = trimmed();
        let again = models.full.parent().unwrap().join("again");
        let second = trim(&models.full, &again).unwrap();
        assert_eq!(first.digest, second.digest);
        assert_eq!(
            first.digest,
            crate::pass::Identity::digest_of(&models.trimmed.join("model.safetensors"))
        );
        assert_eq!(
            std::fs::read(models.trimmed.join("tokenizer.json").as_std_path()).unwrap(),
            std::fs::read(again.join("tokenizer.json").as_std_path()).unwrap()
        );
    }

    #[test]
    fn an_installed_model_is_never_overwritten() {
        let (models, _) = trimmed();
        let before = std::fs::read(models.trimmed.join("model.safetensors").as_std_path()).unwrap();
        let error = trim(&models.full, &models.trimmed).unwrap_err();
        assert!(error.to_string().contains("existe déjà"), "{error}");
        assert_eq!(
            std::fs::read(models.trimmed.join("model.safetensors").as_std_path()).unwrap(),
            before
        );
    }

    #[test]
    fn a_trim_that_fails_leaves_nothing_behind_and_can_be_retried() {
        // Le défaut que la relecture du 2026-09-25 a reproduit : une configuration illisible,
        // lue après la mise en place des poids, laissait un `model.safetensors` — et la taille
        // suivante refusait d'écraser « un modèle ».
        let (models, _) = trimmed();
        let config = models.full.join("config.json");
        let original = std::fs::read(config.as_std_path()).unwrap();
        std::fs::write(config.as_std_path(), "[]").unwrap();

        let target = models.full.parent().unwrap().join("retried");
        assert!(trim(&models.full, &target).is_err());
        let left: Vec<String> = std::fs::read_dir(target.as_std_path())
            .map(|entries| {
                entries
                    .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        assert!(left.is_empty(), "une taille échouée a laissé {left:?}");

        std::fs::write(config.as_std_path(), original).unwrap();
        trim(&models.full, &target).unwrap();
    }

    #[test]
    fn the_padding_id_of_the_configuration_follows_its_piece() {
        let (models, _) = trimmed();
        let config = models.full.join("config.json");
        let mut value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(config.as_std_path()).unwrap()).unwrap();
        let full_vocab = vocabulary(&models.full);

        // Une pièce qui change d'identifiant avec la taille : `😀`, après le russe retiré.
        let emoji = full_vocab.iter().position(|piece| piece == "😀").unwrap();
        value["pad_token_id"] = serde_json::json!(emoji);
        std::fs::write(config.as_std_path(), value.to_string()).unwrap();
        let target = models.full.parent().unwrap().join("padded");
        trim(&models.full, &target).unwrap();
        let written: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(target.join("config.json").as_std_path()).unwrap(),
        )
        .unwrap();
        let new = usize::try_from(written["pad_token_id"].as_u64().unwrap()).unwrap();
        assert_eq!(vocabulary(&target)[new], "😀");
        assert_ne!(
            new, emoji,
            "l'identifiant aurait dû bouger — le test ne prouve rien"
        );

        // Et une pièce retirée ne peut pas servir de remplissage : refusé, rien d'écrit.
        let russian = full_vocab.iter().position(|piece| piece == "ква").unwrap();
        value["pad_token_id"] = serde_json::json!(russian);
        std::fs::write(config.as_std_path(), value.to_string()).unwrap();
        let refused = models.full.parent().unwrap().join("refused-pad");
        let error = trim(&models.full, &refused).unwrap_err().to_string();
        assert!(error.contains("pad_token_id"), "{error}");
        assert!(!refused.join("model.safetensors").exists());
    }

    #[test]
    fn a_tokenizer_this_trim_does_not_know_is_refused() {
        for (field, value, expected) in [
            ("/model/type", serde_json::json!("BPE"), "Unigram"),
            (
                "/post_processor/type",
                serde_json::json!("RobertaProcessing"),
                "non géré",
            ),
            ("/model/unk_id", serde_json::json!(9_999), "au-delà"),
        ] {
            let (models, _) = trimmed();
            let path = models.full.join("tokenizer.json");
            let mut tokenizer: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(path.as_std_path()).unwrap())
                    .unwrap();
            *tokenizer.pointer_mut(field).unwrap() = value;
            std::fs::write(path.as_std_path(), tokenizer.to_string()).unwrap();

            let target = models.full.parent().unwrap().join("refused");
            let error = trim(&models.full, &target).unwrap_err().to_string();
            assert!(error.contains(expected), "{field} → {error}");
            assert!(
                !target.join("config.json").exists(),
                "un modèle refusé a laissé un config.json, donc un modèle apparemment installé"
            );
        }
    }
}
