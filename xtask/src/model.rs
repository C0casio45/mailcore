//! Tailler le vocabulaire d'un modèle d'embeddings statiques sur le corpus qu'il servira.
//!
//! ## Pourquoi cette opération existe
//!
//! `potion-multilingual-128M` porte une table de **500 353 tokens × 256 dimensions en `f32`**,
//! soit 488 Mio — le vocabulaire de `bge-m3`, fait pour cent langues. Le critère 5 de
//! `docs/PHASE-4.md` accorde 500 Mo de RSS pour *toute* la passe de vectorisation, et
//! `model2vec-rs` matérialise la table en `f32` quoi qu'il arrive. Le critère est donc crevé
//! avant qu'un seul vecteur ait été calculé.
//!
//! Or un corpus de courrier français et anglais n'emploie qu'une fraction de ce vocabulaire. Ne
//! garder que les lignes qu'il utilise est une transformation **locale**, faite une fois à
//! l'installation, et son argument de justesse tient en une phrase : **un token qu'aucun message
//! ne contient ne peut faire remonter aucun message.**
//!
//! ## Le piège, et c'est lui qui dicte la forme du résultat
//!
//! `model2vec-rs::pool_ids` lit `mapping[token]` et, **à défaut, se rabat sur le numéro du token
//! lui-même comme index de ligne** :
//!
//! ```text
//! let row_idx = self.token_mapping.and_then(|m| m.get(tok)).copied().unwrap_or(tok);
//! let row = self.embeddings.row(row_idx);
//! ```
//!
//! Sur une table taillée à 60 000 lignes, un token numéroté 480 000 — parfaitement légal, il
//! suffit d'une langue que le corpus ne contient pas — sortirait de la table. `ndarray` panique.
//! La carte doit donc couvrir **tout le vocabulaire d'origine**, y compris ce qu'on jette :
//! 500 353 entrées de 4 octets, soit 2 Mio, ce qui ne se discute pas.
//!
//! Les tokens écartés pointent vers une **ligne nulle**, placée en tête. Ils diluent alors
//! légèrement la moyenne du message où ils apparaissent, au lieu de la fausser ou de planter —
//! et c'est le comportement qu'on veut pour un mot qui n'existe dans aucun message : il ne dit
//! rien, il ne doit rien peser.
//!
//! ## Ce que cette commande ne fait pas
//!
//! Elle n'écrit rien dans le store et ne touche pas au modèle d'origine : elle lit les deux et
//! écrit un modèle neuf ailleurs. Le modèle d'origine reste la référence pour comparer.

use std::collections::HashSet;

use anyhow::{Context, Result};
use camino::Utf8PathBuf;
use mailcore::{Mailbox, Store};
use safetensors::tensor::{Dtype, TensorView};

/// Taille le vocabulaire du modèle sur ce que le corpus emploie réellement.
///
/// # Errors
///
/// Si le store, le modèle ou le tokeniseur sont illisibles, ou si la table n'a pas la forme
/// attendue.
pub fn trim(store_root: &Utf8PathBuf, model: &Utf8PathBuf, out: &Utf8PathBuf) -> Result<()> {
    // Lu une fois, servi deux fois : au tokeniseur qui parcourt le corpus, et à la réécriture du
    // vocabulaire. Le relire ferait dix-huit mébioctets de plus pour le même contenu.
    let raw_tokenizer = std::fs::read_to_string(model.join("tokenizer.json").as_str())
        .with_context(|| format!("lecture de {model}/tokenizer.json"))?;
    let tokenizer = tokenizers::Tokenizer::from_bytes(raw_tokenizer.as_bytes())
        .map_err(|source| anyhow::anyhow!("tokeniseur illisible : {source}"))?;

    println!("Modèle d'origine   {model}");
    println!("Store              {store_root}");
    println!();

    // --- Ce que le corpus emploie ------------------------------------------------------------

    let store = Store::open(store_root).with_context(|| format!("ouverture de {store_root}"))?;
    let mailbox = Mailbox::open(store_root).with_context(|| format!("boîte {store_root}"))?;
    let rows = store.all_for_indexing()?;
    anyhow::ensure!(!rows.is_empty(), "aucun message dans {store_root}");

    let started = std::time::Instant::now();
    let mut used: HashSet<u32> = HashSet::new();
    let mut scanned = 0_usize;
    for row in &rows {
        // Le même texte que celui qui sera vectorisé : sujet, expéditeur, corps. Le tokeniser
        // sur autre chose ferait garder des lignes inutiles et en jeter d'utiles.
        let mut text = String::new();
        text.push_str(&row.subject);
        text.push(' ');
        if let Some(name) = &row.from_name {
            text.push_str(name);
            text.push(' ');
        }
        text.push_str(&row.from_addr);
        if let Ok(Some(detail)) = mailbox.message(row.id) {
            text.push(' ');
            text.push_str(&detail.body);
        }

        // **Le texte entier, pas les 512 premiers tokens.** La vectorisation tronquera ; la
        // collecte, non. Un token gardé pour rien coûte une ligne ; un token jeté à tort coûte
        // un mot qui ne veut plus rien dire, pour toujours.
        let encoded = tokenizer
            .encode_fast(text, false)
            .map_err(|source| anyhow::anyhow!("tokenisation : {source}"))?;
        used.extend(encoded.get_ids());
        scanned += 1;
    }

    // **Les tokens spéciaux se lisent là où ils sont écrits.**
    //
    // Le premier jet les cherchait dans la différence entre les deux vocabulaires du tokeniseur,
    // celui avec les tokens ajoutés et celui sans. Cette différence est **vide** ici : `[PAD]` et
    // `[UNK]` sont déclarés en tokens ajoutés *et* présents dans le vocabulaire du modèle. Ils
    // n'apparaissent dans aucun message, donc ils étaient écartés — et un tokeniseur sans son
    // token inconnu ne tokenise plus rien.
    let mut document: serde_json::Value =
        serde_json::from_str(&raw_tokenizer).context("analyse de tokenizer.json")?;
    if let Some(added) = document
        .get("added_tokens")
        .and_then(serde_json::Value::as_array)
    {
        for token in added {
            if let Some(id) = token.get("id").and_then(serde_json::Value::as_u64) {
                used.insert(u32::try_from(id).unwrap_or(u32::MAX));
            }
        }
    }
    if let Some(unk) = document
        .pointer("/model/unk_id")
        .and_then(serde_json::Value::as_u64)
    {
        used.insert(u32::try_from(unk).unwrap_or(u32::MAX));
    }

    let scan = started.elapsed();
    println!("Messages parcourus {scanned}");
    println!("Tokens employés    {}", used.len());
    println!("Durée du parcours  {scan:.1?}");
    println!();

    // --- La table taillée ---------------------------------------------------------------------

    // **Lu en entier, et c'est assumé.** Projeter le fichier en mémoire éviterait un `Vec<u8>`
    // de 488 Mio, mais `memmap2` demande un bloc `unsafe`, que `#![forbid(unsafe_code)]` refuse
    // dans ce crate — et la justification ne tient pas ici : ce coût est celui de
    // **l'installation**, une opération lancée à la main et faite une fois, pas celui de la passe
    // que le critère 5 mesure. Payer 488 Mio pendant vingt secondes pour en économiser 430 à
    // chaque démarrage est le bon sens de l'échange.
    let raw_model = std::fs::read(model.join("model.safetensors").as_str())
        .with_context(|| format!("lecture de {model}/model.safetensors"))?;
    let tensors =
        safetensors::SafeTensors::deserialize(&raw_model).context("lecture safetensors")?;
    let embeddings = tensors
        .tensor("embeddings")
        .context("tenseur `embeddings` absent")?;
    anyhow::ensure!(
        embeddings.dtype() == Dtype::F32,
        "table en {:?} : cette commande n'écrit que du f32",
        embeddings.dtype()
    );
    let [vocabulary, dimensions]: [usize; 2] = embeddings
        .shape()
        .try_into()
        .context("la table n'est pas à deux dimensions")?;

    // Ordonnés, pour que la table taillée soit reproductible d'une exécution à l'autre : un
    // `HashSet` ne garantit aucun ordre, et deux installations qui produiraient des fichiers
    // différents rendraient toute comparaison d'empreinte impossible.
    let mut kept: Vec<u32> = used
        .iter()
        .copied()
        .filter(|id| (*id as usize) < vocabulary)
        .collect();
    kept.sort_unstable();

    // **Les identifiants sont renumérotés, et c'est ce qui fait disparaître la carte.**
    //
    // Le premier jet gardait le vocabulaire d'origine et sa numérotation, avec une carte de
    // 500 353 entrées pour rattraper. Tailler *aussi* le tokeniseur change la donne : un token
    // qu'il ne connaît plus ne peut plus être produit, donc il n'y a plus rien à rattraper. Les
    // identifiants deviennent `0..n`, la table est dans cet ordre, et la correspondance est
    // l'identité.
    //
    // Ce qui rend la renumérotation sûre côté segmentation : Unigram choisit le découpage de
    // score maximal parmi les morceaux disponibles. Les morceaux retirés sont exactement ceux
    // qu'aucun message n'a produits, donc le chemin optimal d'un texte du corpus est toujours
    // là — retirer des options ne peut pas rendre meilleur un chemin qui ne l'était pas.
    // Pour un texte **neuf**, le découpage peut changer : c'est la contrepartie, et
    // `model-check` est ce qui la met à l'épreuve.
    let mut new_of = vec![u32::MAX; vocabulary];
    for (rank, id) in kept.iter().enumerate() {
        new_of[*id as usize] = u32::try_from(rank).context("vocabulaire trop grand pour un u32")?;
    }

    let raw = embeddings.data();
    let stride = dimensions * std::mem::size_of::<f32>();
    let mut table: Vec<u8> = Vec::with_capacity(kept.len() * stride);
    for id in &kept {
        let at = *id as usize * stride;
        table.extend_from_slice(&raw[at..at + stride]);
    }

    // --- Le tokeniseur, taillé de la même main --------------------------------------------------

    let entries = document
        .pointer_mut("/model/vocab")
        .and_then(serde_json::Value::as_array_mut)
        .context("`model.vocab` absent ou mal formé")?;
    anyhow::ensure!(
        entries.len() == vocabulary,
        "le tokeniseur annonce {} entrées, la table {vocabulary} : ils ne vont pas ensemble",
        entries.len()
    );
    let trimmed_vocabulary: Vec<serde_json::Value> = kept
        .iter()
        .map(|id| entries[*id as usize].clone())
        .collect();
    *entries = trimmed_vocabulary;

    // `unk_id` désigne le morceau rendu quand rien ne correspond. Le laisser pointer sur
    // l'ancienne numérotation donnerait un tokeniseur qui rend un mot au hasard à la place de
    // l'inconnu — et il faut le garder coûte que coûte, ce que la collecte a déjà assuré.
    if let Some(unk) = document
        .pointer("/model/unk_id")
        .and_then(serde_json::Value::as_u64)
    {
        let renumbered = new_of
            .get(usize::try_from(unk).unwrap_or(usize::MAX))
            .copied()
            .filter(|it| *it != u32::MAX)
            .context("le token inconnu a été écarté : le tokeniseur serait cassé")?;
        document["model"]["unk_id"] = serde_json::json!(renumbered);
    }

    // Les tokens ajoutés portent leur propre identifiant, en double de celui du vocabulaire.
    // Les deux doivent dire la même chose, sinon le tokeniseur en rend un et la table en lit un
    // autre.
    if let Some(added) = document
        .get_mut("added_tokens")
        .and_then(serde_json::Value::as_array_mut)
    {
        for token in added.iter_mut() {
            let old = token
                .get("id")
                .and_then(serde_json::Value::as_u64)
                .context("token ajouté sans identifiant")?;
            let renumbered = new_of
                .get(usize::try_from(old).unwrap_or(usize::MAX))
                .copied()
                .filter(|it| *it != u32::MAX)
                .context("un token ajouté a été écarté : il devait être gardé")?;
            token["id"] = serde_json::json!(renumbered);
        }
    }

    // --- Écriture ------------------------------------------------------------------------------

    std::fs::create_dir_all(out.as_str()).with_context(|| format!("création de {out}"))?;
    let views = vec![(
        "embeddings".to_owned(),
        TensorView::new(Dtype::F32, vec![kept.len(), dimensions], &table)
            .context("vue de la table taillée")?,
    )];
    safetensors::serialize_to_file(
        views,
        None,
        std::path::Path::new(out.join("model.safetensors").as_str()),
    )
    .context("écriture du modèle taillé")?;

    let written = serde_json::to_string(&document).context("sérialisation du tokeniseur")?;
    std::fs::write(out.join("tokenizer.json").as_str(), &written)
        .context("écriture du tokeniseur taillé")?;

    // La configuration est recopiée telle quelle : elle ne parle ni de vocabulaire ni d'index.
    std::fs::copy(
        model.join("config.json").as_str(),
        out.join("config.json").as_str(),
    )
    .context("copie de config.json")?;

    // --- Le relevé -----------------------------------------------------------------------------

    let before = vocabulary * stride;
    let after = kept.len() * stride;
    println!("Modèle taillé      {out}");
    println!(
        "Vocabulaire        {} gardés sur {vocabulary}   ({:.1} %)",
        kept.len(),
        percent(kept.len(), vocabulary)
    );
    println!("Table avant        {}", mib(before));
    println!("Table après        {}", mib(after));
    println!("Tokeniseur avant   {}", mib(raw_tokenizer.len()));
    println!("Tokeniseur après   {}", mib(written.len()));
    println!("Rapport            {:.1} ×", ratio(before, after));
    Ok(())
}

/// Un pourcentage, sans surprise sur zéro.
fn percent(part: usize, whole: usize) -> f64 {
    if whole == 0 {
        return 0.0;
    }
    #[allow(clippy::cast_precision_loss)]
    {
        part as f64 * 100.0 / whole as f64
    }
}

/// Un rapport, sans division par zéro.
fn ratio(before: usize, after: usize) -> f64 {
    if after == 0 {
        return 0.0;
    }
    #[allow(clippy::cast_precision_loss)]
    {
        before as f64 / after as f64
    }
}

/// Des octets en mébioctets, parce que c'est l'unité du critère.
fn mib(bytes: usize) -> String {
    #[allow(clippy::cast_precision_loss)]
    {
        format!("{:.1} Mio", bytes as f64 / (1024.0 * 1024.0))
    }
}

/// Vérifie qu'un modèle taillé rend **les mêmes vecteurs** que celui dont il vient.
///
/// ## Pourquoi cette vérification n'est pas optionnelle
///
/// Tailler un vocabulaire, c'est réécrire une table et une carte d'index. Une erreur d'un cran
/// dans la carte donne un modèle qui **marche** — il charge, il encode, il rend des vecteurs de
/// la bonne taille — et dont chaque vecteur est faux. Rien ne le signalerait : ni le chargement,
/// ni la recherche, qui rendrait simplement des résultats médiocres qu'on mettrait sur le compte
/// du modèle.
///
/// La comparaison porte sur de **vrais messages du corpus**, ceux-là mêmes qui ont servi à
/// choisir les lignes gardées. C'est le cas où l'égalité doit être exacte.
///
/// # Errors
///
/// Si l'un des deux modèles est illisible, ou si le store l'est.
pub fn check(
    store_root: &Utf8PathBuf,
    model: &Utf8PathBuf,
    trimmed: &Utf8PathBuf,
    count: usize,
) -> Result<()> {
    let store = Store::open(store_root).with_context(|| format!("ouverture de {store_root}"))?;
    let mailbox = Mailbox::open(store_root).with_context(|| format!("boîte {store_root}"))?;
    let rows = store.all_for_indexing()?;
    anyhow::ensure!(!rows.is_empty(), "aucun message dans {store_root}");

    let texts: Vec<String> = rows
        .iter()
        .take(count)
        .map(|row| {
            let body = mailbox
                .message(row.id)
                .ok()
                .flatten()
                .map(|it| it.body)
                .unwrap_or_default();
            format!("{} {} {}", row.subject, row.from_addr, body)
        })
        .collect();

    let before =
        model2vec_rs::model::StaticModel::from_pretrained(model.as_str(), None, None, None)
            .map_err(|source| anyhow::anyhow!("modèle d'origine : {source}"))?;
    let after =
        model2vec_rs::model::StaticModel::from_pretrained(trimmed.as_str(), None, None, None)
            .map_err(|source| anyhow::anyhow!("modèle taillé : {source}"))?;

    let reference = before.encode(&texts);
    let candidate = after.encode(&texts);
    anyhow::ensure!(
        reference.len() == candidate.len(),
        "les deux modèles ne rendent pas le même nombre de vecteurs"
    );

    let mut worst = 0.0_f32;
    let mut worst_at = 0_usize;
    for (rank, (left, right)) in reference.iter().zip(candidate.iter()).enumerate() {
        anyhow::ensure!(
            left.len() == right.len(),
            "vecteur {rank} : {} dimensions contre {}",
            left.len(),
            right.len()
        );
        for (a, b) in left.iter().zip(right.iter()) {
            let gap = (a - b).abs();
            if gap > worst {
                worst = gap;
                worst_at = rank;
            }
        }
    }

    println!("Messages comparés  {}", texts.len());
    println!(
        "Dimensions         {}",
        reference.first().map_or(0, Vec::len)
    );
    println!("Écart maximal      {worst:e}   (message #{worst_at})");
    println!();
    // Le seuil n'est pas zéro : la somme de `pool_ids` parcourt les lignes dans l'ordre des
    // tokens, qui est le même des deux côtés — mais la ligne nulle ajoutée en tête décale les
    // adresses, et une addition de flottants n'est pas associative. Un écart de l'ordre de
    // l'epsilon du `f32` est attendu ; au-delà, c'est la carte qui est fausse.
    if worst <= 1e-6 {
        println!("Verdict            identique — la carte est juste");
    } else {
        println!("Verdict            **DIVERGENT** — ne pas installer ce modèle taillé");
        anyhow::bail!("écart de {worst:e} entre les deux modèles");
    }
    Ok(())
}

/// Mesure ce que coûte la vectorisation : débit par message, et mémoire résidente.
///
/// ## Ce que cette mesure dit, et ce qu'elle ne dit pas
///
/// Elle porte sur le **moteur**, pas sur le résultat : rien n'est indexé, rien n'est rangé. C'est
/// l'étape 3 de `docs/PHASE-4.md`, et elle vient avant le magasin de vecteurs pour la même raison
/// que le banc vient avant le moteur — un coût qu'on découvre après avoir tout écrit est un coût
/// qu'on défend au lieu de le regarder.
///
/// La crête de RSS est échantillonnée pendant l'encodage, pas relevée à la fin : le modèle chargé
/// et le lot en cours ne sont pas résidents aux mêmes instants.
///
/// # Errors
///
/// Si le modèle ou le store sont illisibles.
pub fn bench(
    store_root: &Utf8PathBuf,
    model: &Utf8PathBuf,
    count: usize,
    batch: usize,
) -> Result<()> {
    // **Le RSS se décompose, sinon il accuse le mauvais coupable.** Un relevé unique après
    // chargement a donné 546 Mio pour une table de 36 : sans les paliers, on en aurait conclu
    // que le modèle taillé coûte quinze fois sa taille.
    let at_start = current_rss();

    let store = Store::open(store_root).with_context(|| format!("ouverture de {store_root}"))?;
    let mailbox = Mailbox::open(store_root).with_context(|| format!("boîte {store_root}"))?;
    let at_store = current_rss();

    let rows = store.all_for_indexing()?;
    anyhow::ensure!(!rows.is_empty(), "aucun message dans {store_root}");
    let count = count.min(rows.len());
    let at_rows = current_rss();

    // **Le tokeniseur, seul, avant le modèle.** La table taillée fait 36 Mio et le chargement en
    // coûte 530 : l'écart est ailleurs, et il faut savoir où avant de tailler autre chose.
    // Chargé puis relâché, pour que la suite parte du même palier.
    let at_tokenizer = {
        let alone = tokenizers::Tokenizer::from_file(model.join("tokenizer.json").as_str())
            .map_err(|source| anyhow::anyhow!("tokeniseur : {source}"))?;
        let seen = current_rss();
        drop(alone);
        seen
    };

    let sampler = crate::measure::RssSampler::start();

    let loading = std::time::Instant::now();
    let encoder =
        model2vec_rs::model::StaticModel::from_pretrained(model.as_str(), None, None, None)
            .map_err(|source| anyhow::anyhow!("modèle : {source}"))?;
    let load = loading.elapsed();
    let loaded_rss = current_rss();

    // La lecture des corps est chronométrée à part : c'est le coût de la moisson, pas celui du
    // modèle, et les mélanger ferait passer un parseur MIME pour un réseau de neurones.
    let reading = std::time::Instant::now();
    let texts: Vec<String> = rows
        .iter()
        .take(count)
        .map(|row| {
            let body = mailbox
                .message(row.id)
                .ok()
                .flatten()
                .map(|it| it.body)
                .unwrap_or_default();
            format!("{} {} {}", row.subject, row.from_addr, body)
        })
        .collect();
    let read = reading.elapsed();

    let encoding = std::time::Instant::now();
    let mut vectors = 0_usize;
    let mut dimensions = 0_usize;
    for chunk in texts.chunks(batch) {
        let encoded = encoder.encode(chunk);
        dimensions = encoded.first().map_or(dimensions, Vec::len);
        vectors += encoded.len();
    }
    let encode = encoding.elapsed();
    let peak = sampler.stop();

    println!("Modèle             {model}");
    println!("Messages           {vectors}");
    println!("Dimensions         {dimensions}");
    println!("Lot                {batch}");
    println!();
    println!("Chargement         {load:.2?}");
    println!();
    println!("Où va la mémoire, par palier :");
    println!("  au démarrage     {}", mib(at_start as usize));
    println!(
        "  store et index   {}   (+{})",
        mib(at_store as usize),
        mib(at_store.saturating_sub(at_start) as usize)
    );
    println!(
        "  lignes en RAM    {}   (+{})",
        mib(at_rows as usize),
        mib(at_rows.saturating_sub(at_store) as usize)
    );
    println!(
        "  tokeniseur seul  {}   (+{})",
        mib(at_tokenizer as usize),
        mib(at_tokenizer.saturating_sub(at_rows) as usize)
    );
    println!("  modèle chargé    {}", mib(loaded_rss as usize));
    println!("Lecture des corps  {read:.2?}");
    println!("Encodage           {encode:.2?}");
    #[allow(clippy::cast_precision_loss)]
    {
        let per = encode.as_secs_f64() / vectors.max(1) as f64;
        println!(
            "Par message        {:.3} ms   soit {:.0} messages/s",
            per * 1000.0,
            1.0 / per
        );
    }
    println!("RSS crête          {}", mib(peak as usize));
    println!();
    // Le magasin de vecteurs, qui n'existe pas encore mais dont la taille est déjà connue : elle
    // ne dépend que du nombre de messages et de la dimension.
    println!(
        "Vecteurs pour {} messages : {} en f32",
        rows.len(),
        mib(rows.len() * dimensions * std::mem::size_of::<f32>())
    );
    Ok(())
}

/// La mémoire résidente du processus, maintenant.
fn current_rss() -> u64 {
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};
    let pid = Pid::from_u32(std::process::id());
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[pid]),
        true,
        ProcessRefreshKind::nothing().with_memory(),
    );
    system.process(pid).map_or(0, sysinfo::Process::memory)
}
