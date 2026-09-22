//! La passe de vectorisation : elle avance, elle se reprend, elle s'arrête.
//!
//! La même forme que `mailcore::index::writer`, et ce n'est pas une coïncidence : c'est le
//! quatrième dérivé du store après le carnet, l'index plein texte et les fils, et les trois
//! précédents ont payé les leçons.
//!
//! ## Ce qui rend une coupure sans conséquence
//!
//! Trois propriétés, et il faut les trois :
//!
//! 1. **un drapeau par ligne** (`messages.vectorised`, `SCHEMA_V14`) et non un filigrane sur un
//!    identifiant — `messages.id` est un alias de `rowid`, que SQLite réutilise ;
//! 2. **une écriture idempotente** : le magasin est en ajout seul et sa relecture garde le
//!    dernier record d'un message, donc revectoriser ne laisse pas de doublon ;
//! 3. **l'ordre « durable, puis marqué »** : `sync` sur le fichier, *ensuite* `mark_vectorised`.
//!    Une coupure entre les deux fait recalculer un lot, ce qui ne coûte que du temps. L'ordre
//!    inverse laisserait des messages sans vecteur marqués comme en ayant un, donc introuvables
//!    pour toujours par la recherche sémantique.
//!
//! ## La borne, qui n'est pas une précaution
//!
//! La boucle est bornée par ce qui était en attente à son début. Sans ça, elle ne s'arrête que
//! parce que le marquage retire les lignes de la requête — et un marquage défaillant ferait
//! tourner un job de fond pour toujours, affiché « en cours », donc invisible. C'est le contrôle
//! négatif du 2026-09-11, qui n'avait fait échouer aucun test mais les avait fait **pendre**.

use mailcore::vectors::VectorWriter;
use mailcore::{Progress, Store};

use crate::model::Model;
use crate::{Error, Result};

/// Combien de messages sont lus, encodés et écrits d'un coup, par défaut.
///
/// Le lot sert deux choses opposées, d'où l'arbitrage. Trop petit, on paie un appel de
/// bibliothèque et une transaction SQLite par message. Trop grand, on garde en mémoire autant de
/// textes de deux mille caractères — et le critère 5 compte la mémoire de la passe, pas
/// seulement celle du modèle. Cinq cents textes font au pire un mébioctet, ce qui ne pèse rien à
/// côté des cent du modèle.
///
/// **Cette valeur suppose un modèle sans attention**, et c'est pour ça que [`Encoder::batch`]
/// existe — voir la leçon qui l'accompagne.
pub const BATCH: usize = 500;

/// Comment un modèle se nomme et se reconnaît.
///
/// Trois champs, et pas un chemin : un chemin est une propriété de la machine, et deux machines
/// qui rangent le même modèle ailleurs doivent pouvoir se passer un magasin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    /// Le dernier segment du répertoire du modèle.
    pub name: String,
    /// Sa famille, telle qu'un lecteur la comprend.
    pub family: String,
    /// L'empreinte de ses poids, tronquée à seize caractères hexadécimaux.
    pub digest: String,
}

impl Identity {
    /// L'empreinte du fichier de poids d'un modèle.
    ///
    /// Elle lit le fichier par morceaux : celui du transformeur fait 470 Mo, et le charger
    /// entier pour le hacher doublerait la crête mémoire d'une passe qui crève déjà le
    /// critère 5.
    ///
    /// Un fichier illisible rend `inconnue` plutôt qu'une erreur. Refuser de vectoriser parce
    /// qu'on n'a pas pu calculer une **étiquette** serait disproportionné — le modèle, lui, est
    /// déjà chargé et fonctionne. La provenance dit alors qu'elle ne sait pas, ce qui est le
    /// comportement juste d'un champ dont tout l'intérêt est l'honnêteté.
    #[must_use]
    pub fn digest_of(path: &camino::Utf8Path) -> String {
        let Ok(file) = std::fs::File::open(path.as_std_path()) else {
            return "inconnue".to_owned();
        };
        let mut reader = std::io::BufReader::new(file);
        let mut hasher = blake3::Hasher::new();
        let mut buffer = vec![0u8; 1 << 20];
        loop {
            match std::io::Read::read(&mut reader, &mut buffer) {
                Ok(0) => break,
                Ok(read) => hasher.update(&buffer[..read]),
                Err(_) => return "inconnue".to_owned(),
            };
        }
        hasher.finalize().to_hex()[..16].to_owned()
    }

    /// Le dernier segment d'un répertoire, ou son chemin s'il n'en a pas.
    #[must_use]
    pub fn name_of(dir: &camino::Utf8Path) -> String {
        dir.file_name().unwrap_or(dir.as_str()).to_owned()
    }
}

/// Ce que la passe attend de ce qui fabrique les vecteurs.
///
/// ## Pourquoi un trait plutôt que [`Model`] en dur
///
/// Le modèle n'est pas dans le dépôt : il s'installe. Une passe qui l'exigerait n'aurait
/// **aucun test** — et c'est précisément la fonction du projet où une coupure mal gérée perd des
/// messages en silence, donc la dernière qu'on veut laisser sans contrôle négatif.
///
/// Le trait ne cache pas une abstraction spéculative : il a exactement les deux méthodes dont la
/// boucle se sert, et le seul autre implémenteur est un encodeur déterministe de test. Un
/// encodeur distant ne pourrait pas s'y glisser — il n'y a personne pour l'écrire, et
/// `docs/PHASE-4.md` l'interdit.
pub trait Encoder {
    /// La dimension des vecteurs produits.
    fn dim(&self) -> usize;

    /// Ce qui identifie ce modèle dans la provenance du magasin.
    ///
    /// ## Pourquoi c'est l'encodeur qui le dit
    ///
    /// La passe ne reçoit pas toujours un répertoire — `update_with` prend un encodeur déjà
    /// construit — et elle n'a de toute façon aucun moyen de savoir ce qui distingue deux
    /// modèles. Seul celui qui a lu les fichiers peut le dire.
    ///
    /// Le nom **et** l'empreinte des poids, parce que les deux répondent à des questions
    /// différentes : le nom sert à lire un relevé, l'empreinte sert à refuser. Un répertoire
    /// dont on remplace le contenu garde son nom, et c'est exactement le genre de chose qui a
    /// rendu un chiffre indéfendable le 2026-09-21.
    fn identity(&self) -> Identity;

    /// Combien de textes cet encodeur veut recevoir d'un coup.
    ///
    /// ## Pourquoi c'est au modèle de le dire, et pas à la passe
    ///
    /// Trouvé le 2026-09-21 en lançant la première passe transformeur : **4 557 Mio de mémoire
    /// résidente avant le premier vecteur écrit**, contre 117 pour l'embedding statique.
    ///
    /// Le lot de cinq cents avait été dimensionné pour un modèle **sans attention**, où le coût
    /// d'un texte est celui de ses jetons — linéaire, minuscule. Un transformeur calcule en plus,
    /// à chacune de ses douze couches, une matrice d'attention de `jetons × jetons` **par texte
    /// du lot**. Cinq cents textes de 256 jetons font quatre milliards de flottants
    /// d'intermédiaires.
    ///
    /// La leçon générale : **une constante de lot porte une hypothèse sur ce qu'elle nourrit.**
    /// Celle-ci était juste et elle est devenue fausse le jour où le modèle a changé de famille,
    /// sans qu'aucun test ne puisse le voir — les tests emploient un encodeur jouet dont le coût
    /// est linéaire, comme l'était l'ancien modèle.
    fn batch(&self) -> usize {
        BATCH
    }

    /// Encode un lot de textes, dans l'ordre.
    ///
    /// # Errors
    ///
    /// [`Error::Encode`] si le lot rendu ne correspond pas au lot demandé.
    fn encode(&self, texts: &[String]) -> Result<Vec<Vec<f32>>>;
}

impl Encoder for Model {
    fn dim(&self) -> usize {
        Self::dim(self)
    }

    fn identity(&self) -> Identity {
        Self::identity(self).clone()
    }

    fn encode(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        Self::encode(self, texts)
    }
}

/// Le bilan d'une passe.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stats {
    /// Combien de messages ont reçu un vecteur.
    pub vectorised: u64,
    /// Combien ont été sautés parce que leur blob est absent.
    pub missing_blobs: u64,
    /// Combien ont été sautés parce qu'il n'en restait aucun texte.
    ///
    /// Compté et non ignoré : un corpus où ce nombre est grand a un problème d'extraction, et
    /// sans le chiffre on l'attribuerait au modèle. Un message vide de texte existe pour de bon
    /// — une pièce jointe seule, un corps perdu au transport — mais il doit rester marginal.
    pub empty: u64,
}

/// Vectorise ce qui ne l'est pas encore, et s'arrête là.
///
/// # Errors
///
/// [`Error::Model`] si le modèle n'est pas chargeable, [`Error::Core`] si le store ou le magasin
/// est illisible, [`Error::Encode`] si la bibliothèque rend autre chose que ce qu'on lui a
/// demandé.
pub fn update(
    store: &Store,
    model_dir: &camino::Utf8Path,
    budget: crate::text::Budget,
    progress: &Progress,
) -> Result<Stats> {
    // **La question la moins chère en premier.** Charger le modèle coûte une cinquantaine de
    // millisecondes et une centaine de mébioctets ; le `COUNT` qui dit s'il y a du travail coûte
    // des microsecondes. La passe tourne après chaque moisson, et la grande majorité des
    // moissons n'apportent rien.
    if store.unvectorised_total()? == 0 {
        return Ok(Stats::default());
    }

    let model = Model::load(model_dir)?;
    update_with(store, &model, budget, progress)
}

/// [`update`] avec un encodeur déjà construit.
///
/// Sert les tests et le banc, qui ont besoin d'un encodeur qu'ils contrôlent. Le raccourci du
/// `COUNT` n'est **pas** ici : il a pour but d'éviter de charger le modèle, et à ce point il
/// l'est déjà.
///
/// # Errors
///
/// Les mêmes que [`update`], hors chargement du modèle.
pub fn update_with(
    store: &Store,
    model: &impl Encoder,
    budget: crate::text::Budget,
    progress: &Progress,
) -> Result<Stats> {
    let path = store.vectors_path();

    // **Le critère 9 : les deux magasins ne doivent pas se contredire.** Le drapeau est dans
    // SQLite, les vecteurs dans un fichier. Ils divergent quand le fichier disparaît — disque
    // remplacé, répertoire nettoyé — et des drapeaux qui disent « vectorisé » feraient alors
    // sauter tous les messages : la recherche sémantique resterait vide pour toujours, sans que
    // rien ne le signale.
    //
    // Le contrôle est ici, au point d'usage, et non à l'ouverture du magasin : celle-ci sert
    // aussi les chemins de **lecture** — une recherche — qui ne doivent pas écrire. C'est la
    // même répartition que pour l'index plein texte, et elle a la même raison.
    let stored = mailcore::vectors::Vectors::load(&path, model.dim())?;
    if stored.is_empty() && store.vectorised_count()? > 0 {
        tracing::warn!(
            "magasin de vecteurs vide alors que des messages se disent vectorisés : passe complète"
        );
        store.reset_vectorised()?;
    }
    drop(stored);

    advance(store, model, budget, progress)
}

/// Refait tous les vecteurs, depuis zéro.
///
/// Le geste à faire après un changement de modèle ou de budget de texte : les vecteurs existants
/// sont **comparables entre eux** et ne le seraient plus avec les nouveaux. Le fichier est retiré
/// plutôt que vidé, parce qu'un magasin en ajout seul n'a pas de « vider ».
///
/// # Errors
///
/// Les mêmes que [`update`], plus [`Error::Core`] si le fichier n'est pas supprimable.
pub fn rebuild(
    store: &Store,
    model_dir: &camino::Utf8Path,
    budget: crate::text::Budget,
    progress: &Progress,
) -> Result<Stats> {
    let model = Model::load(model_dir)?;
    rebuild_with(store, &model, budget, progress)
}

/// [`rebuild`] avec un encodeur déjà construit.
///
/// # Errors
///
/// Les mêmes que [`rebuild`], hors chargement du modèle.
pub fn rebuild_with(
    store: &Store,
    model: &impl Encoder,
    budget: crate::text::Budget,
    progress: &Progress,
) -> Result<Stats> {
    let path = store.vectors_path();

    if path.exists() {
        std::fs::remove_file(&path).map_err(|source| {
            Error::Core(mailcore::Error::Io {
                path: path.clone(),
                source,
            })
        })?;
    }
    // Vider le magasin **et** remettre les drapeaux : les deux vont ensemble. N'en faire qu'un
    // donnerait soit un magasin vide que rien ne remplit, soit une revectorisation qui ne se
    // déclenche jamais.
    store.reset_vectorised()?;

    advance(store, model, budget, progress)
}

/// Ce qui sera écrit dans l'en-tête du magasin.
///
/// Le budget en fait partie parce qu'il change les vecteurs autant que le modèle : c'est le
/// champ dont l'absence a rendu un relevé indéfendable le 2026-09-21, quand quatre passes de
/// budgets différents ont laissé quatre magasins indiscernables.
fn provenance_of(
    model: &impl Encoder,
    budget: crate::text::Budget,
) -> mailcore::vectors::Provenance {
    let identity = model.identity();
    mailcore::vectors::Provenance {
        model: identity.name,
        family: identity.family,
        digest: identity.digest,
        budget: budget.body,
        written: today(),
    }
}

/// La date du jour en `AAAA-MM-JJ`.
///
/// Sans dépendance de calendrier : le crate n'en a pas et n'en veut pas pour cinq champs. Le
/// calcul civil part de l'ère grégorienne proleptique, et il est testé sur des dates connues —
/// dont un 29 février, qui est la seule ligne où ce genre de code se trompe.
fn today() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let (year, month, day) = civil_from_days(i64::try_from(secs / 86_400).unwrap_or(0));
    format!("{year:04}-{month:02}-{day:02}")
}

/// Le jour civil correspondant à un nombre de jours depuis 1970-01-01.
///
/// Algorithme de Howard Hinnant, `civil_from_days`. Il est recopié plutôt qu'importé parce
/// qu'une dépendance de calendrier pour une ligne de provenance coûterait plus cher que ces
/// douze lignes — et il est testé, ce qui est la condition pour le dire.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (
        if m <= 2 { y + 1 } else { y },
        u32::try_from(m).unwrap_or(1),
        u32::try_from(d).unwrap_or(1),
    )
}

/// La boucle partagée par [`update`] et [`rebuild`].
///
/// Partagée et non recopiée : c'est la leçon du carnet, où deux boucles auraient divergé et où
/// un carnet reconstruit n'aurait pas donné le même classement qu'un carnet tenu à jour. Ici, un
/// magasin refait ne donnerait pas les mêmes vecteurs qu'un magasin suivi, et rien ne le dirait.
fn advance(
    store: &Store,
    model: &impl Encoder,
    budget: crate::text::Budget,
    progress: &Progress,
) -> Result<Stats> {
    let mut stats = Stats::default();
    let parser = mail_parser::MessageParser::default();
    let batch = model.batch().max(1);
    let provenance = provenance_of(model, budget);
    let mut writer = VectorWriter::open(store.vectors_path(), model.dim(), &provenance)?;

    let pending = store.unvectorised_total()?;
    progress.set_total(pending);
    let mut remaining = pending;

    while remaining > 0 {
        let rows = store.unvectorised(batch.min(usize::try_from(remaining).unwrap_or(batch)))?;
        if rows.is_empty() {
            break;
        }
        remaining = remaining.saturating_sub(rows.len() as u64);

        let mut texts: Vec<String> = Vec::with_capacity(rows.len());
        let mut ids = Vec::with_capacity(rows.len());
        let mut cancelled = false;
        // Combien de lignes de ce lot n'ont pas de contenu à vectoriser. Compté par lot et
        // retiré du total : elles sont **marquées** — sinon elles reviendraient à chaque passe —
        // mais les annoncer comme vectorisées serait faux, et c'est le genre de faux qui rend un
        // bilan rassurant.
        let mut absent = 0u64;

        for row in &rows {
            if progress.is_cancelled() {
                tracing::info!(vectorised = stats.vectorised, "vectorisation interrompue");
                cancelled = true;
                break;
            }
            let raw = match store.blobs().read(row.blob) {
                Ok(bytes) => bytes,
                Err(mailcore::Error::BlobNotFound(hash)) => {
                    // Un blob absent est diagnostiquable par `mail doctor`. Ce n'est pas de quoi
                    // interrompre une passe de soixante-treize mille messages.
                    tracing::warn!(%hash, id = row.id.0, "blob absent, message non vectorisé");
                    stats.missing_blobs += 1;
                    absent += 1;
                    // **Marqué quand même.** Sinon la ligne revient à chaque passe, pour toujours,
                    // et `unvectorised_total` ne retombe jamais à zéro : la passe la moins chère
                    // du projet redeviendrait la plus chère, et le raccourci du haut ne servirait
                    // plus jamais. Un blob qui réapparaît est un cas pour une reconstruction.
                    ids.push(row.id);
                    texts.push(String::new());
                    progress.advance(1);
                    continue;
                }
                Err(other) => return Err(other.into()),
            };

            let text = match parser.parse(&raw) {
                Some(parsed) => crate::text::from_message(
                    &parsed,
                    &row.subject,
                    row.from_name.as_deref(),
                    budget,
                ),
                // En-têtes illisibles : on vectorise sur ce que SQLite sait déjà. Un message
                // introuvable serait pire qu'un message mal placé.
                None => crate::text::compose(&row.subject, row.from_name.as_deref(), ""),
            };
            if text.trim().is_empty() {
                stats.empty += 1;
            }
            ids.push(row.id);
            texts.push(text);
            progress.advance(1);
        }

        if !ids.is_empty() {
            for (id, vector) in ids.iter().zip(model.encode(&texts)?) {
                writer.append(*id, &vector)?;
            }
            // **Durable, puis marqué.** L'ordre est ce qui rend la reprise sûre ; l'inverse
            // perdrait des messages en silence.
            writer.sync()?;
            store.mark_vectorised(&ids)?;
            stats.vectorised += (ids.len() as u64).saturating_sub(absent);
        }

        if cancelled {
            break;
        }
    }

    tracing::info!(
        vectorised = stats.vectorised,
        missing = stats.missing_blobs,
        empty = stats.empty,
        "vecteurs avancés"
    );
    Ok(stats)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use mailcore::vectors::Vectors;

    /// Un encodeur déterministe, sans modèle.
    ///
    /// Il ne prétend pas approcher un vrai modèle : il rend un vecteur qui **dépend du texte**,
    /// ce qui suffit à tout ce que cette boucle doit garantir — que chaque message reçoit son
    /// vecteur, dans le bon ordre, une seule fois, et que la reprise ne saute rien. La qualité
    /// des vecteurs est le sujet du critère 1, pas celui d'ici.
    struct Fake {
        dim: usize,
        /// Combien de textes ont été encodés depuis le début. C'est ce compteur qui rend visible
        /// le travail **refait** : une reprise qui recalculerait tout le ferait grimper.
        encoded: std::cell::Cell<usize>,
        /// Ce que la provenance du magasin retiendra de lui. Réglable, parce que la moitié de
        /// ce que la provenance garantit est un **refus**, et qu'un refus ne se teste qu'avec
        /// deux encodeurs différents.
        identity: Identity,
    }

    impl Fake {
        fn new(dim: usize) -> Self {
            Self::named(dim, "jouet")
        }

        fn named(dim: usize, name: &str) -> Self {
            Self {
                dim,
                encoded: std::cell::Cell::new(0),
                identity: Identity {
                    name: name.to_owned(),
                    family: "jouet".to_owned(),
                    digest: format!("{name:_<16}"),
                },
            }
        }
    }

    impl Encoder for Fake {
        fn dim(&self) -> usize {
            self.dim
        }

        fn identity(&self) -> Identity {
            self.identity.clone()
        }

        fn encode(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
            self.encoded.set(self.encoded.get() + texts.len());
            Ok(texts
                .iter()
                .map(|text| {
                    let mut vector = vec![0f32; self.dim];
                    for (index, byte) in text.bytes().enumerate() {
                        vector[index % self.dim] += f32::from(byte) / 255.0;
                    }
                    // Un vecteur nul serait refusé par la normalisation du magasin sur un texte
                    // vide ; une composante fixe garde le cas dégénéré représentable.
                    vector[0] += 1.0;
                    vector
                })
                .collect())
        }
    }

    use crate::testing::corpus;

    /// Le budget par défaut : ces tests portent sur la boucle, pas sur la longueur du texte.
    fn budget() -> crate::text::Budget {
        crate::text::Budget::default()
    }

    #[test]
    fn a_pass_vectorises_everything_and_the_second_has_nothing_to_do() {
        let (_dir, store) = corpus(7);
        let fake = Fake::new(8);
        let progress = Progress::new();

        let first = update_with(&store, &fake, budget(), &progress).unwrap();
        assert_eq!(first.vectorised, 7);
        assert_eq!(store.unvectorised_total().unwrap(), 0);

        let again = update_with(&store, &fake, budget(), &progress).unwrap();
        assert_eq!(again.vectorised, 0, "la seconde passe a refait du travail");
        assert_eq!(
            Vectors::load(store.vectors_path(), 8).unwrap().len(),
            7,
            "des doublons sont entrés dans le magasin"
        );
    }

    /// **Le critère 3.** Une passe coupée au milieu repart où elle en était, et ne recalcule pas
    /// ce qui est fait. La coupure est simulée par l'annulation, qui est le seul point d'arrêt
    /// que la boucle connaisse — un `Ctrl-C` ou une machine éteinte laissent le même état, à
    /// ceci près qu'ils peuvent tomber entre l'écriture et le marquage, ce que le test suivant
    /// couvre.
    #[test]
    fn a_cancelled_pass_resumes_where_it_stopped() {
        let (_dir, store) = corpus(4);
        let fake = Fake::new(8);

        let progress = Progress::new();
        progress.cancel();
        let stopped = update_with(&store, &fake, budget(), &progress).unwrap();
        assert_eq!(stopped.vectorised, 0);
        let done_first = fake.encoded.get();

        let progress = Progress::new();
        let resumed = update_with(&store, &fake, budget(), &progress).unwrap();
        assert_eq!(resumed.vectorised, 4);
        assert_eq!(
            fake.encoded.get() - done_first,
            4,
            "la reprise a réencodé autre chose que ce qui restait"
        );
    }

    /// **L'ordre « durable, puis marqué ».** Une coupure entre les deux laisse des vecteurs
    /// écrits et non marqués : la passe suivante les réécrit, et le magasin ne doit pas gagner de
    /// doublon pour autant. C'est l'idempotence qui rend cet ordre sûr, et c'est elle qu'on
    /// vérifie ici — en remettant les drapeaux à zéro sans toucher au fichier, ce qui est
    /// exactement l'état que laisse une machine éteinte au mauvais moment.
    #[test]
    fn vectors_written_but_not_marked_are_rewritten_without_duplicating() {
        let (_dir, store) = corpus(5);
        let fake = Fake::new(8);
        let progress = Progress::new();

        update_with(&store, &fake, budget(), &progress).unwrap();
        store.reset_vectorised().unwrap();
        update_with(&store, &fake, budget(), &progress).unwrap();

        assert_eq!(
            Vectors::load(store.vectors_path(), 8).unwrap().len(),
            5,
            "le magasin a gagné des doublons après une reprise"
        );
    }

    /// **Le critère 9.** Le fichier de vecteurs disparaît, les drapeaux disent « fait ». Sans le
    /// contrôle, la passe ne verrait aucun travail et la recherche sémantique resterait vide
    /// pour toujours, sans rien signaler.
    #[test]
    fn a_vanished_store_is_detected_rather_than_left_silent() {
        let (_dir, store) = corpus(3);
        let fake = Fake::new(8);
        let progress = Progress::new();

        update_with(&store, &fake, budget(), &progress).unwrap();
        std::fs::remove_file(store.vectors_path()).unwrap();

        // Le raccourci du `COUNT` ne se déclenche pas ici : `update_with` est appelée
        // directement, ce qui est le cas réel où le contrôle doit jouer — `update` ne
        // l'atteindrait qu'à l'arrivée d'un message, ce que le commentaire de `index::update`
        // avait déjà pesé.
        let after = update_with(&store, &fake, budget(), &progress).unwrap();
        assert_eq!(after.vectorised, 3, "la divergence n'a pas été vue");
        assert_eq!(Vectors::load(store.vectors_path(), 8).unwrap().len(), 3);
    }

    /// Le contrôle inverse du précédent : un magasin **présent** ne déclenche pas de passe
    /// complète. Sans lui, remettre les drapeaux à chaque appel passerait le test ci-dessus tout
    /// en revectorisant le corpus à chaque moisson.
    #[test]
    fn a_store_that_is_there_does_not_trigger_a_full_pass() {
        let (_dir, store) = corpus(3);
        let fake = Fake::new(8);
        let progress = Progress::new();

        update_with(&store, &fake, budget(), &progress).unwrap();
        let before = fake.encoded.get();
        update_with(&store, &fake, budget(), &progress).unwrap();

        assert_eq!(fake.encoded.get(), before, "le corpus a été revectorisé");
    }

    #[test]
    fn a_rebuild_redoes_everything_and_leaves_no_duplicate() {
        let (_dir, store) = corpus(4);
        let fake = Fake::new(8);
        let progress = Progress::new();

        update_with(&store, &fake, budget(), &progress).unwrap();
        let redone = rebuild_with(&store, &fake, budget(), &progress).unwrap();

        assert_eq!(redone.vectorised, 4);
        assert_eq!(Vectors::load(store.vectors_path(), 8).unwrap().len(), 4);
    }

    /// Un encodeur d'une autre dimension que le magasin existant doit être **refusé**, pas
    /// ajouté derrière. Deux moitiés correctes dont la comparaison ne veut rien dire est le pire
    /// résultat possible : elle rendrait des résultats, et ils seraient faux.
    #[test]
    fn changing_the_dimension_is_refused_rather_than_appended() {
        let (_dir, store) = corpus(2);
        let progress = Progress::new();

        update_with(&store, &Fake::new(8), budget(), &progress).unwrap();
        store.reset_vectorised().unwrap();
        let found = update_with(&store, &Fake::new(16), budget(), &progress);

        assert!(found.is_err(), "un magasin de dimension 8 a accepté du 16");
    }

    /// Le cas que la dimension ne couvrait pas, et qui a coûté un chiffre indéfendable.
    ///
    /// Deux modèles **de même dimension** — les deux `potion` du corpus — passaient le contrôle
    /// précédent sans encombre. Rien ne l'aurait dit : la passe aurait ajouté ses records
    /// derrière ceux de l'autre, et le magasin aurait rendu un classement dont chaque moitié
    /// vient d'un espace différent.
    #[test]
    fn changing_the_model_at_equal_dimension_is_refused_too() {
        let (_dir, store) = corpus(2);
        let progress = Progress::new();

        update_with(&store, &Fake::named(8, "premier"), budget(), &progress).unwrap();
        store.reset_vectorised().unwrap();
        let found = update_with(&store, &Fake::named(8, "second"), budget(), &progress);

        assert!(
            found.is_err(),
            "un autre modèle à dimension égale a été ajouté derrière le premier"
        );
    }

    /// Et le champ dont l'absence a rendu quatre magasins indiscernables le 2026-09-21.
    ///
    /// Le budget de texte change les vecteurs autant que le modèle : le balayage à 2 000, 400,
    /// 150 et 0 a écrit quatre magasins que ni la dimension ni le nom du modèle ne séparaient.
    #[test]
    fn changing_the_text_budget_is_refused_like_changing_the_model() {
        let (_dir, store) = corpus(2);
        let progress = Progress::new();
        let fake = Fake::new(8);

        update_with(&store, &fake, crate::text::Budget::of(2_000), &progress).unwrap();
        store.reset_vectorised().unwrap();
        let found = update_with(&store, &fake, crate::text::Budget::of(150), &progress);

        assert!(
            found.is_err(),
            "deux budgets ont écrit dans le même magasin sans que rien ne le dise"
        );
    }

    /// Le contrôle inverse des deux précédents : sans lui, un refus systématique les tiendrait.
    #[test]
    fn the_same_model_and_the_same_budget_continue_the_same_store() {
        let (_dir, store) = corpus(4);
        let progress = Progress::new();
        let fake = Fake::new(8);

        update_with(&store, &fake, budget(), &progress).unwrap();
        store.reset_vectorised().unwrap();
        let again = update_with(&store, &fake, budget(), &progress);

        assert!(again.is_ok(), "une passe a refusé de continuer la sienne");
    }

    /// La provenance écrite est celle qu'on relit.
    #[test]
    fn the_store_records_what_the_pass_used() {
        let (_dir, store) = corpus(3);
        let progress = Progress::new();

        update_with(
            &store,
            &Fake::named(8, "e5-jouet"),
            crate::text::Budget::of(400),
            &progress,
        )
        .unwrap();

        let found = Vectors::load(store.vectors_path(), 8).unwrap();
        let provenance = found.provenance().expect("la passe écrit sa provenance");
        assert_eq!(provenance.model, "e5-jouet");
        assert_eq!(provenance.budget, 400);
        // La date n'est pas comparée à une valeur fixe — elle serait fausse demain — mais sa
        // forme l'est, parce qu'un champ de date vide passerait sans ça.
        assert_eq!(provenance.written.len(), 10);
        assert_eq!(provenance.written.matches('-').count(), 2);
    }

    /// Le calcul de date est recopié, donc il se teste.
    ///
    /// Les trois lignes choisies sont celles où ce genre de code se trompe : le jour zéro, un
    /// 29 février, et le lendemain d'un 28 février d'année non bissextile.
    #[test]
    fn the_civil_date_is_right_where_that_calculation_usually_is_not() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        // 2024-02-29 : 19782 jours depuis l'époque.
        assert_eq!(civil_from_days(19_782), (2024, 2, 29));
        assert_eq!(civil_from_days(19_783), (2024, 3, 1));
        // 2023 n'est pas bissextile : le 28 février est suivi du 1er mars.
        assert_eq!(civil_from_days(19_416), (2023, 2, 28));
        assert_eq!(civil_from_days(19_417), (2023, 3, 1));
        // Et un 1er janvier de siècle non bissextile, le piège classique.
        assert_eq!(civil_from_days(11_323), (2001, 1, 1));
    }
}
