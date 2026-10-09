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

use mailcore::vectors::{VectorLock, VectorWriter};
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
    /// Combien ont été sautés parce que leur blob n'a pas pu être lu — fichier abîmé,
    /// décompression impossible.
    ///
    /// **Marqués quand même**, depuis le 2026-10-08 : l'erreur arrêtait la passe, et comme les
    /// lignes reviennent dans l'ordre des identifiants, le même message en tête de chaque passe
    /// l'aurait arrêtée pour toujours. Une passe de fond ne doit pas s'enrayer sur un message.
    pub unreadable: u64,
}

/// Vectorise ce qui ne l'est pas encore, et s'arrête là.
///
/// # Errors
///
/// [`Error::Model`] si le modèle n'est pas chargeable, [`Error::Core`] si le store ou le magasin
/// est illisible — ou si une autre passe tient le verrou, `mailcore::Error::VectorsBusy` —,
/// [`Error::Encode`] si la bibliothèque rend autre chose que ce qu'on lui a demandé.
pub fn update(
    store: &Store,
    model_dir: &camino::Utf8Path,
    budget: crate::text::Budget,
    progress: &Progress,
) -> Result<Stats> {
    // **La question la moins chère en premier.** Charger le modèle coûte une cinquantaine de
    // millisecondes et une centaine de mébioctets ; le `COUNT` qui dit s'il y a du travail coûte
    // des microsecondes. Et ce qui est là doit encore être valable : voir [`Pending`].
    //
    // Le verrou d'abord : la réponse à « que reste-t-il à faire » ne vaut que tant que personne
    // d'autre n'écrit.
    let lock = VectorLock::try_acquire(&store.vectors_path())?;
    if pending(store, budget)?.is_empty() {
        return Ok(Stats::default());
    }
    let model = Model::load(model_dir)?;
    update_locked(store, &model, budget, progress, &lock)
}

/// [`update`] avec un encodeur déjà construit.
///
/// Sert les tests, le banc et `mail vectors --bert`. Le raccourci du `COUNT` n'est **pas** ici :
/// il a pour but d'éviter de charger le modèle, et à ce point il l'est déjà.
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
    let lock = VectorLock::try_acquire(&store.vectors_path())?;
    update_locked(store, model, budget, progress, &lock)
}

/// Le corps de [`update_with`], verrou tenu.
fn update_locked(
    store: &Store,
    model: &impl Encoder,
    budget: crate::text::Budget,
    progress: &Progress,
    _lock: &VectorLock,
) -> Result<Stats> {
    let path = store.vectors_path();
    let found = pending(store, budget)?;

    // **Le critère 9 : les deux magasins ne doivent pas se contredire.** Le drapeau est dans
    // SQLite, les vecteurs dans un fichier. Ils divergent quand le fichier disparaît — disque
    // remplacé, répertoire nettoyé —, quand une copie plus ancienne le remplace, ou quand il est
    // tronqué ; des drapeaux qui disent « vectorisé » feraient alors sauter ces messages pour
    // toujours, sans que rien ne le signale.
    //
    // Le contrôle est ici, au point d'usage, et non à l'ouverture du magasin : celle-ci sert
    // aussi les chemins de **lecture** — une recherche — qui ne doivent pas écrire. C'est la
    // même répartition que pour l'index plein texte, et elle a la même raison.
    //
    // **Le second cas de divergence, trouvé le 2026-09-23** : un magasin *présent* mais écrit par
    // une autre composition de texte. Même conséquence, même réponse.
    //
    // Ce chemin est **explicite** — `mail vectors`, un banc. Le démon ne le prend jamais : il
    // passe par [`follow`], qui refuse de remettre quoi que ce soit à zéro.
    if found.diverged || found.stale {
        if found.diverged {
            tracing::warn!(
                "le magasin de vecteurs a moins de records que de messages marqués vectorisés : \
                 passe complète"
            );
        } else {
            tracing::warn!(
                "le magasin de vecteurs a été écrit par une autre composition de texte ou un \
                 autre budget : passe complète"
            );
        }
        // **Le fichier d'abord, les drapeaux ensuite** — l'ordre de [`rebuild_with`]. Dans
        // l'ordre inverse, une coupure entre les deux laissait des drapeaux remis à zéro à côté
        // d'un magasin ancien, que la passe suivante aurait continué. Dans celui-ci, elle laisse
        // des drapeaux sans records, ce que le contrôle de divergence voit.
        remove_store(&path)?;
        store.reset_vectorised()?;
    }

    advance(store, model, budget, progress, None)
}

/// Retire le magasin, s'il existe.
fn remove_store(path: &camino::Utf8Path) -> Result<()> {
    if path.exists() {
        std::fs::remove_file(path).map_err(|source| {
            Error::Core(mailcore::Error::Io {
                path: path.to_owned(),
                source,
            })
        })?;
    }
    Ok(())
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
    let lock = VectorLock::try_acquire(&store.vectors_path())?;
    let model = Model::load(model_dir)?;
    rebuild_locked(store, &model, budget, progress, &lock)
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
    let lock = VectorLock::try_acquire(&store.vectors_path())?;
    rebuild_locked(store, model, budget, progress, &lock)
}

/// Le corps de [`rebuild_with`], verrou tenu.
fn rebuild_locked(
    store: &Store,
    model: &impl Encoder,
    budget: crate::text::Budget,
    progress: &Progress,
    _lock: &VectorLock,
) -> Result<Stats> {
    // Vider le magasin **et** remettre les drapeaux : les deux vont ensemble. N'en faire qu'un
    // donnerait soit un magasin vide que rien ne remplit, soit une revectorisation qui ne se
    // déclenche jamais.
    remove_store(&store.vectors_path())?;
    store.reset_vectorised()?;
    advance(store, model, budget, progress, None)
}

/// Combien de messages une passe de fond accepte de faire d'elle-même.
///
/// Mille, soit trois à quatre minutes au débit mesuré de la passe complète. Au-delà, ce n'est
/// plus suivre la moisson, c'est vectoriser un corpus : une passe de plusieurs heures, qui se
/// décide — `mail vectors` —, et ne démarre pas parce qu'une fenêtre s'est ouverte.
pub const FOLLOW_LIMIT: u64 = 1_000;

/// Pourquoi une passe de fond refuse, et demande une passe explicite.
///
/// Chaque raison est un cas où continuer **effacerait ou mélangerait** : la passe explicite
/// remet tout à zéro, ce qu'une passe de fond ne fait jamais — le 2026-09-25, le passage de la
/// composition à sa révision 3 aurait fait effacer 108 Mio de vecteurs et relancer quatre heures
/// de calcul au premier démarrage du démon, sans que personne ne le sache.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Explicit {
    /// Plus de [`FOLLOW_LIMIT`] messages à faire — la première passe, un rattrapage.
    Backlog,
    /// Le magasin a été écrit par une composition de texte plus ancienne, ou un autre budget.
    Stale,
    /// Le magasin a été écrit par une composition **plus récente** que ce binaire : on n'y touche
    /// pas, ni pour le continuer ni pour le refaire.
    Newer,
    /// Moins de records que de messages marqués : les deux magasins se contredisent.
    Diverged,
    /// Un autre modèle — famille, poids ou dimension — que celui qui l'a écrit.
    ModelChanged,
    /// Un magasin de version 1, sans provenance : impossible de savoir ce qu'on continuerait.
    Legacy,
}

impl Explicit {
    /// Un mot qui le désigne, pour un journal ou un protocole — jamais un chemin.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::Backlog => "backlog",
            Self::Stale => "stale",
            Self::Newer => "newer",
            Self::Diverged => "diverged",
            Self::ModelChanged => "model-changed",
            Self::Legacy => "legacy",
        }
    }

    /// L'inverse de [`Self::code`].
    #[must_use]
    pub fn from_code(code: &str) -> Option<Self> {
        [
            Self::Backlog,
            Self::Stale,
            Self::Newer,
            Self::Diverged,
            Self::ModelChanged,
            Self::Legacy,
        ]
        .into_iter()
        .find(|reason| reason.code() == code)
    }
}

/// Ce qu'une passe de fond ferait maintenant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Plan {
    /// Rien : tout est vectorisé, et le magasin est valable.
    Nothing,
    /// Suivre : `pending` messages au plus, ajoutés au magasin tel qu'il est.
    Follow {
        /// Combien de messages attendent.
        pending: u64,
    },
    /// Refuser, et demander une passe explicite.
    Explicit {
        /// Pourquoi.
        reason: Explicit,
        /// Combien de messages attendent.
        pending: u64,
    },
}

/// Ce qu'une passe de fond ferait maintenant — **le seul endroit qui en décide**.
///
/// Le démon le demande avant de lancer quoi que ce soit, sans modèle chargé (`model` à `None`) ;
/// la passe de fond le redemande verrou tenu, modèle chargé, pour vérifier aussi que le magasin
/// est bien le sien. Deux appels, une seule règle.
///
/// # Errors
///
/// [`Error::Core`] si le store ou l'en-tête du magasin est illisible.
pub fn plan(
    store: &Store,
    budget: crate::text::Budget,
    model: Option<(&Identity, usize)>,
) -> Result<Plan> {
    let counts = store.vector_counts()?;
    let summary = mailcore::vectors::summary_of(&store.vectors_path())?;
    let pending = counts.unvectorised;
    let explicit = |reason| Ok(Plan::Explicit { reason, pending });

    if let Some(summary) = &summary {
        let Some(found) = &summary.provenance else {
            return explicit(Explicit::Legacy);
        };
        if found.revision > crate::text::REVISION {
            return explicit(Explicit::Newer);
        }
        if stale_against(found, budget) {
            return explicit(Explicit::Stale);
        }
        if let Some((identity, dim)) = model {
            let theirs = mailcore::vectors::Provenance {
                model: identity.name.clone(),
                family: identity.family.clone(),
                digest: identity.digest.clone(),
                budget: budget.body,
                revision: crate::text::REVISION,
                written: String::new(),
            };
            if summary.dim != dim || !found.comparable_to(&theirs) {
                return explicit(Explicit::ModelChanged);
            }
        }
    }
    let records = summary.as_ref().map_or(0, |summary| summary.records);
    if records < counts.marked() {
        return explicit(Explicit::Diverged);
    }
    if pending == 0 {
        return Ok(Plan::Nothing);
    }
    if pending > FOLLOW_LIMIT {
        return explicit(Explicit::Backlog);
    }
    Ok(Plan::Follow { pending })
}

/// Ce qu'une passe de fond a fait.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Followed {
    /// Elle a suivi — `Stats::default()` s'il n'y avait rien.
    Done(Stats),
    /// Elle a refusé, sans rien écrire.
    Explicit {
        /// Pourquoi.
        reason: Explicit,
        /// Combien de messages attendent.
        pending: u64,
    },
    /// Une autre passe tient le verrou ; rien n'a été fait, et ce n'est pas une panne.
    Busy,
}

/// La passe de fond : ajoute au magasin ce qui manque, **sans jamais rien effacer ni remettre à
/// zéro**, et refuse tout ce qui le demanderait. Voir [`plan`].
///
/// # Errors
///
/// Les mêmes que [`update_with`], hors verrou tenu, qui rend [`Followed::Busy`].
pub fn follow(
    store: &Store,
    model: &impl Encoder,
    budget: crate::text::Budget,
    progress: &Progress,
) -> Result<Followed> {
    let lock = match VectorLock::try_acquire(&store.vectors_path()) {
        Ok(lock) => lock,
        Err(mailcore::Error::VectorsBusy { .. }) => return Ok(Followed::Busy),
        Err(other) => return Err(other.into()),
    };
    follow_locked(store, model, budget, progress, &lock)
}

/// [`follow`], verrou déjà tenu — pour qui le prend **avant** de charger le modèle, et ne paie
/// donc pas 343 Mio pour découvrir qu'une autre passe tourne.
///
/// # Errors
///
/// Les mêmes que [`follow`].
pub fn follow_locked(
    store: &Store,
    model: &impl Encoder,
    budget: crate::text::Budget,
    progress: &Progress,
    _lock: &VectorLock,
) -> Result<Followed> {
    let identity = model.identity();
    match plan(store, budget, Some((&identity, model.dim())))? {
        Plan::Nothing => Ok(Followed::Done(Stats::default())),
        Plan::Explicit { reason, pending } => Ok(Followed::Explicit { reason, pending }),
        Plan::Follow { .. } => Ok(Followed::Done(advance(
            store, model, budget, progress, None,
        )?)),
    }
}

/// [`follow_locked`], borné à `at_most` messages : ce qui reste attendra le passage suivant.
///
/// Pour un processus qui a autre chose à faire entre deux lots — l'enfant de la recherche par le
/// sens, qui sert des requêtes et ne doit pas les faire attendre derrière mille messages. Une
/// requête qui arrive pendant un lot attend ce lot, et seulement lui.
///
/// # Errors
///
/// Les mêmes que [`follow`].
pub fn follow_some(
    store: &Store,
    model: &impl Encoder,
    budget: crate::text::Budget,
    progress: &Progress,
    _lock: &VectorLock,
    at_most: u64,
) -> Result<Followed> {
    let identity = model.identity();
    match plan(store, budget, Some((&identity, model.dim())))? {
        Plan::Nothing => Ok(Followed::Done(Stats::default())),
        Plan::Explicit { reason, pending } => Ok(Followed::Explicit { reason, pending }),
        Plan::Follow { .. } => Ok(Followed::Done(advance(
            store,
            model,
            budget,
            progress,
            Some(at_most),
        )?)),
    }
}

/// Ce qu'une passe aurait à faire si on la lançait maintenant.
///
/// ## Pourquoi cette question a une seule réponse, et une seule porte
///
/// Elle en avait **trois** avant le 2026-09-23, et c'est ce qui a laissé passer le défaut du jour :
/// `update` court-circuitait sur un `COUNT`, `mail vectors` refaisait le même `COUNT` pour son
/// affichage — et retournait avant même d'appeler la passe —, et la vérification de provenance
/// vivait encore ailleurs. Corriger la première n'a rien corrigé du tout, parce que la commande
/// n'y arrivait jamais.
///
/// C'est la leçon du carnet d'adresses, dans l'autre sens : là-bas, deux chemins vers la même
/// donnée devaient partager leur **code** et pas seulement leur intention ; ici, deux endroits
/// posaient la même **question** et l'un des deux ignorait la moitié de la réponse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pending {
    /// Combien de messages n'ont pas encore de vecteur.
    pub messages: u64,
    /// Le magasin a-t-il été écrit par une autre composition de texte, ou un autre budget ?
    ///
    /// Quand c'est vrai, `messages` ne dit plus rien d'utile : la passe remettra tous les
    /// drapeaux à zéro, donc le travail réel est le corpus entier.
    pub stale: bool,
    /// Le magasin a-t-il **moins de records que de messages marqués vectorisés** ?
    ///
    /// Le critère 9 : chaque message marqué a au moins un record, puisque le drapeau n'est posé
    /// qu'après l'écriture durable. Moins de records prouve que les deux magasins se contredisent
    /// — fichier supprimé, remplacé par une copie plus ancienne, tronqué. Comme `stale`, il rend
    /// `messages` sans objet : la passe repartira de zéro.
    pub diverged: bool,
}

impl Pending {
    /// Vrai s'il n'y a rien à faire — **les trois** conditions, pas seulement le compte.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.messages == 0 && !self.stale && !self.diverged
    }
}

/// Ce qu'une passe aurait à faire, sans rien charger de cher.
///
/// Deux `COUNT`, un en-tête de vingt octets et la taille d'un fichier. C'est ce qui permet de la
/// poser après chaque moisson sans payer le chargement d'un modèle — la leçon du 2026-09-11, et
/// elle tient toujours : ce contrôle-ci ne l'a pas rendue plus chère.
///
/// ## Le contrôle du critère 9, ici plutôt qu'après
///
/// Jusqu'au 2026-09-25, la divergence n'était vue que par [`update_with`], en relisant le magasin
/// entier — et seulement quand il y avait déjà du travail. Un magasin supprimé pendant que les
/// drapeaux disaient « fait » donnait donc « rien à faire » à `mail vectors`, et le restait
/// jusqu'à l'arrivée d'un message. Le nombre de records se lit dans la taille du fichier : la
/// question coûte le même prix que les autres, donc elle se pose avec elles.
///
/// # Errors
///
/// [`Error::Core`] si le store ou l'en-tête du magasin est illisible.
pub fn pending(store: &Store, budget: crate::text::Budget) -> Result<Pending> {
    // **Les comptes d'abord, dans un même instantané, et la taille du fichier ensuite.** Deux
    // `COUNT` séparés laissaient une moisson s'intercaler et fabriquer une divergence — voir
    // `Store::vector_counts`. Et dans cet ordre, la taille lue est toujours au moins celle qui
    // correspondait aux drapeaux comptés : pendant une passe, les records ne font que croître, et
    // un drapeau n'est posé qu'après que son record est durable.
    let counts = store.vector_counts()?;
    let summary = mailcore::vectors::summary_of(&store.vectors_path())?;
    let records = summary.as_ref().map_or(0, |summary| summary.records);
    Ok(Pending {
        messages: counts.unvectorised,
        stale: summary
            .as_ref()
            .and_then(|summary| summary.provenance.as_ref())
            .is_some_and(|found| stale_against(found, budget)),
        diverged: records < counts.marked(),
    })
}

/// Le magasin existant a-t-il été écrit par autre chose que ce que cette passe écrirait ?
///
/// Ne compare que **ce qui se sait sans charger le modèle** — le budget et la révision de la
/// composition. Le nom et l'empreinte du modèle demanderaient de lire 470 Mo de poids, ce qui
/// coûterait plus cher que le raccourci qu'on cherche à protéger ; ils restent vérifiés au moment
/// d'ouvrir le magasin en écriture, où le modèle est de toute façon chargé.
///
/// Autrement dit : le contrôle bon marché attrape le cas bon marché, le contrôle cher attrape le
/// reste, et aucun des deux ne dispense de l'autre.
///
/// Un magasin absent ou sans provenance n'est **pas** périmé : le premier n'existe pas encore, le
/// second est un magasin de version 1 qu'on ne peut pas juger. Les déclarer périmés déclencherait
/// une passe complète de quatre heures à chaque démarrage sur une installation ancienne.
const fn stale_against(found: &mailcore::vectors::Provenance, budget: crate::text::Budget) -> bool {
    found.budget != budget.body || found.revision != crate::text::REVISION
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
        revision: crate::text::REVISION,
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

/// Ce que la passe tire d'une ligne.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Composed {
    /// Le blob est absent : rien à lire.
    Missing,
    /// Le blob est là et ne se lit pas. Rendu par la boucle seulement ; [`compose`] rend l'erreur.
    Unreadable,
    /// Le texte que le modèle recevra — vide, parfois.
    Text(String),
}

/// Le texte qu'un message donne au modèle.
///
/// **Le même pour la passe et pour tout banc** qui veut savoir ce que la passe verrait : la leçon
/// du 2026-09-10, un banc qui réimplémente ce qu'il vérifie ne vérifie rien. `model-bench`
/// composait jusqu'ici son propre texte — « sujet adresse corps » — qui n'était pas celui de la
/// passe.
///
/// # Errors
///
/// [`Error::Core`] si le blob est illisible pour une autre raison que son absence.
pub fn compose(
    store: &Store,
    row: &mailcore::store::read::VectorRow,
    budget: crate::text::Budget,
) -> Result<Composed> {
    compose_with(store, row, &mail_parser::MessageParser::default(), budget)
}

/// [`compose`] avec un analyseur déjà construit, pour la boucle.
fn compose_with(
    store: &Store,
    row: &mailcore::store::read::VectorRow,
    parser: &mail_parser::MessageParser,
    budget: crate::text::Budget,
) -> Result<Composed> {
    // Les premiers [`crate::text::MESSAGE_CAP`] octets, lus en flux : `mail-parser` décode tout ce
    // qu'on lui donne, et un message de 124 Mio coûtait 209 Mio le temps de sa lecture.
    let mut reader = match store.blobs().open(row.blob) {
        Ok(reader) => reader,
        Err(mailcore::Error::BlobNotFound(_)) => return Ok(Composed::Missing),
        Err(other) => return Err(other.into()),
    };
    let mut raw = Vec::new();
    std::io::Read::read_to_end(
        &mut std::io::Read::take(&mut reader, crate::text::MESSAGE_CAP),
        &mut raw,
    )
    .map_err(|source| {
        // Le chemin du blob n'est pas exposé par le trait ; son empreinte le désigne.
        Error::Core(mailcore::Error::Io {
            path: camino::Utf8PathBuf::from(format!("blob {}", row.blob)),
            source,
        })
    })?;
    Ok(Composed::Text(match parser.parse(&raw) {
        Some(parsed) => {
            crate::text::from_message(&parsed, &row.subject, row.from_name.as_deref(), budget)
        }
        // En-têtes illisibles : on vectorise sur ce que SQLite sait déjà. Un message
        // introuvable serait pire qu'un message mal placé.
        None => crate::text::compose(&row.subject, row.from_name.as_deref(), ""),
    }))
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
    cap: Option<u64>,
) -> Result<Stats> {
    let mut stats = Stats::default();
    let parser = mail_parser::MessageParser::default();
    let batch = model.batch().max(1);
    let provenance = provenance_of(model, budget);
    let mut writer = VectorWriter::open(store.vectors_path(), model.dim(), &provenance)?;

    // Borné quand on le demande : voir [`follow_some`].
    let total = store.unvectorised_total()?;
    let pending = cap.map_or(total, |cap| total.min(cap));
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
            match compose_with(store, row, &parser, budget).unwrap_or_else(|error| {
                tracing::warn!(%error, id = row.id.0, "blob illisible, message non vectorisé");
                Composed::Unreadable
            }) {
                Composed::Unreadable => {
                    stats.unreadable += 1;
                    absent += 1;
                    ids.push(row.id);
                    texts.push(String::new());
                }
                Composed::Missing => {
                    // Un blob absent est diagnostiquable par `mail doctor`. Ce n'est pas de quoi
                    // interrompre une passe de soixante-treize mille messages.
                    tracing::warn!(hash = %row.blob, id = row.id.0, "blob absent, message non vectorisé");
                    stats.missing_blobs += 1;
                    absent += 1;
                    // **Marqué quand même.** Sinon la ligne revient à chaque passe, pour toujours,
                    // et `unvectorised_total` ne retombe jamais à zéro : la passe la moins chère
                    // du projet redeviendrait la plus chère, et le raccourci du haut ne servirait
                    // plus jamais. Un blob qui réapparaît est un cas pour une reconstruction.
                    ids.push(row.id);
                    texts.push(String::new());
                }
                Composed::Text(text) => {
                    if text.trim().is_empty() {
                        stats.empty += 1;
                    }
                    ids.push(row.id);
                    texts.push(text);
                }
            }
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

    /// Le défaut que `pending` avait jusqu'au 2026-09-25 : un magasin supprimé pendant que les
    /// drapeaux disaient « fait » rendait « rien à faire », donc `mail vectors` s'arrêtait et une
    /// moisson sans nouveau message ne voyait rien.
    #[test]
    fn the_cheap_question_already_sees_a_vanished_store() {
        let (_dir, store) = corpus(3);
        let fake = Fake::new(8);
        update_with(&store, &fake, budget(), &Progress::new()).unwrap();
        assert!(pending(&store, budget()).unwrap().is_empty());

        std::fs::remove_file(store.vectors_path()).unwrap();
        let found = pending(&store, budget()).unwrap();
        assert!(found.diverged, "{found:?}");
        assert!(!found.is_empty());
    }

    /// Un magasin **plus court** que les drapeaux — une copie ancienne remise en place, un
    /// fichier tronqué : ni vide ni absent, donc invisible au contrôle d'avant.
    #[test]
    fn a_store_shorter_than_its_flags_is_redone_from_scratch() {
        let (_dir, store) = corpus(4);
        let fake = Fake::new(8);
        let progress = Progress::new();
        update_with(&store, &fake, budget(), &progress).unwrap();

        // Deux records retirés à la main, en fin de fichier.
        let path = store.vectors_path();
        let length = std::fs::metadata(path.as_std_path()).unwrap().len();
        let record = 8 + 4 * 8;
        std::fs::OpenOptions::new()
            .write(true)
            .open(path.as_std_path())
            .unwrap()
            .set_len(length - 2 * record)
            .unwrap();
        assert!(pending(&store, budget()).unwrap().diverged);

        let redone = update_with(&store, &fake, budget(), &progress).unwrap();
        assert_eq!(redone.vectorised, 4, "le corpus n'a pas été refait");
        assert_eq!(Vectors::load(&path, 8).unwrap().len(), 4);
        assert!(pending(&store, budget()).unwrap().is_empty());
    }

    /// Un fichier vide est ce que laisse une passe tuée avant l'en-tête. La lecture le refusait,
    /// et la passe avec elle, pour toujours.
    #[test]
    fn an_empty_store_file_is_an_absent_store_not_an_error() {
        let (_dir, store) = corpus(2);
        std::fs::write(store.vectors_path().as_std_path(), []).unwrap();
        let found = pending(&store, budget()).unwrap();
        assert_eq!(found.messages, 2);
        assert!(!found.diverged && !found.stale);

        let fake = Fake::new(8);
        let done = update_with(&store, &fake, budget(), &Progress::new()).unwrap();
        assert_eq!(done.vectorised, 2);
    }

    /// Les octets du magasin, pour vérifier qu'un refus n'a rien écrit.
    fn snapshot(store: &Store) -> (Option<Vec<u8>>, u64) {
        (
            std::fs::read(store.vectors_path().as_std_path()).ok(),
            store.unvectorised_total().unwrap(),
        )
    }

    #[test]
    fn the_background_pass_follows_a_valid_store() {
        let (_dir, store) = corpus(5);
        let fake = Fake::new(8);
        update_with(&store, &fake, budget(), &Progress::new()).unwrap();
        // Les drapeaux remis, les records gardés : cinq messages à suivre, sur un magasin valable.
        store.reset_vectorised().unwrap();
        assert_eq!(
            plan(&store, budget(), None).unwrap(),
            Plan::Follow { pending: 5 }
        );
        match follow(&store, &fake, budget(), &Progress::new()).unwrap() {
            Followed::Done(stats) => assert_eq!(stats.vectorised, 5),
            other => panic!("{other:?}"),
        }
        assert_eq!(plan(&store, budget(), None).unwrap(), Plan::Nothing);
    }

    #[test]
    fn a_bounded_follow_stops_at_its_bound_and_the_next_one_continues() {
        // Le lot de l'enfant de recherche : il ne doit pas faire attendre une requête derrière
        // tout l'arriéré. Le contrôle inverse est le test précédent — sans borne, tout passe.
        let (_dir, store) = corpus(5);
        let fake = Fake::new(8);
        update_with(&store, &fake, budget(), &Progress::new()).unwrap();
        store.reset_vectorised().unwrap();

        let lock = VectorLock::try_acquire(&store.vectors_path()).unwrap();
        let once =
            |at_most| match follow_some(&store, &fake, budget(), &Progress::new(), &lock, at_most)
                .unwrap()
            {
                Followed::Done(stats) => stats.vectorised,
                other => panic!("{other:?}"),
            };
        assert_eq!(once(2), 2);
        assert_eq!(
            plan(&store, budget(), None).unwrap(),
            Plan::Follow { pending: 3 }
        );
        assert_eq!(once(2), 2);
        assert_eq!(once(2), 1);
        assert_eq!(plan(&store, budget(), None).unwrap(), Plan::Nothing);
        // Rien à faire : un lot vide, pas une erreur.
        assert_eq!(once(2), 0);
    }

    #[test]
    fn a_bounded_follow_refuses_what_the_unbounded_one_refuses() {
        // Une borne ne doit pas ouvrir une porte que `follow` tient fermée : même `plan`, même
        // refus, rien d'écrit.
        let (_dir, store) = corpus(3);
        update_with(&store, &Fake::new(8), budget(), &Progress::new()).unwrap();
        store.reset_vectorised().unwrap();
        let before = snapshot(&store);
        let lock = VectorLock::try_acquire(&store.vectors_path()).unwrap();
        let other = Fake::named(8, "un-autre-modele");
        assert!(matches!(
            follow_some(&store, &other, budget(), &Progress::new(), &lock, 2).unwrap(),
            Followed::Explicit {
                reason: Explicit::ModelChanged,
                ..
            }
        ));
        assert_eq!(snapshot(&store), before);
    }

    #[test]
    fn the_background_pass_refuses_whatever_would_erase_or_mix() {
        // Chaque cas, préparé par un chemin explicite, puis soumis à la passe de fond : elle doit
        // refuser **sans rien écrire ni encoder**. Le contrôle inverse est l'existant :
        // `update_with` refait ces magasins-là.
        let other_budget = crate::text::Budget::of(150);

        // Périmé : écrit avec un autre budget.
        let (_a, stale) = corpus(4);
        update_with(&stale, &Fake::new(8), other_budget, &Progress::new()).unwrap();
        stale.reset_vectorised().unwrap();

        // Divergent : moins de records que de drapeaux.
        let (_b, diverged) = corpus(4);
        update_with(&diverged, &Fake::new(8), budget(), &Progress::new()).unwrap();
        std::fs::write(diverged.vectors_path().as_std_path(), []).unwrap();

        // Un autre modèle.
        let (_c, other_model) = corpus(4);
        update_with(
            &other_model,
            &Fake::named(8, "autre"),
            budget(),
            &Progress::new(),
        )
        .unwrap();
        other_model.reset_vectorised().unwrap();

        // Plus récent que ce binaire.
        let (_d, newer) = corpus(4);
        let ahead = mailcore::vectors::Provenance {
            revision: crate::text::REVISION + 1,
            ..provenance_of(&Fake::new(8), budget())
        };
        drop(VectorWriter::open(newer.vectors_path(), 8, &ahead).unwrap());

        // De version 1, sans provenance.
        let (_e, legacy) = corpus(4);
        let mut v1 = b"MAILVEC1".to_vec();
        v1.extend_from_slice(&1u32.to_le_bytes());
        v1.extend_from_slice(&8u32.to_le_bytes());
        std::fs::write(legacy.vectors_path().as_std_path(), v1).unwrap();

        for (store, expected) in [
            (&stale, Explicit::Stale),
            (&diverged, Explicit::Diverged),
            (&other_model, Explicit::ModelChanged),
            (&newer, Explicit::Newer),
            (&legacy, Explicit::Legacy),
        ] {
            let fake = Fake::new(8);
            let before = snapshot(store);
            let followed = follow(store, &fake, budget(), &Progress::new()).unwrap();
            assert!(
                matches!(followed, Followed::Explicit { reason, .. } if reason == expected),
                "{expected:?} : {followed:?}"
            );
            assert_eq!(
                snapshot(store),
                before,
                "{expected:?} : la passe de fond a écrit"
            );
            assert_eq!(
                fake.encoded.get(),
                0,
                "{expected:?} : la passe de fond a encodé"
            );
        }
    }

    #[test]
    fn a_backlog_is_left_to_an_explicit_pass_from_one_message_over_the_limit() {
        let limit = usize::try_from(FOLLOW_LIMIT).unwrap();
        let (_dir, store) = corpus(limit);
        assert_eq!(
            plan(&store, budget(), None).unwrap(),
            Plan::Follow {
                pending: FOLLOW_LIMIT
            }
        );
        let (_d2, over) = corpus(limit + 1);
        assert_eq!(
            plan(&over, budget(), None).unwrap(),
            Plan::Explicit {
                reason: Explicit::Backlog,
                pending: FOLLOW_LIMIT + 1
            }
        );
    }

    #[test]
    fn a_pass_holding_the_lock_makes_the_others_wait_without_writing() {
        let (_dir, store) = corpus(3);
        let held = VectorLock::try_acquire(&store.vectors_path()).unwrap();
        let fake = Fake::new(8);
        assert_eq!(
            follow(&store, &fake, budget(), &Progress::new()).unwrap(),
            Followed::Busy
        );
        assert!(matches!(
            update_with(&store, &fake, budget(), &Progress::new()),
            Err(Error::Core(mailcore::Error::VectorsBusy { .. }))
        ));
        assert!(matches!(
            rebuild_with(&store, &fake, budget(), &Progress::new()),
            Err(Error::Core(mailcore::Error::VectorsBusy { .. }))
        ));
        assert_eq!(fake.encoded.get(), 0);
        assert!(!store.vectors_path().exists());
        // Le contrôle inverse : rendu, la passe se fait.
        drop(held);
        assert!(matches!(
            follow(&store, &fake, budget(), &Progress::new()).unwrap(),
            Followed::Done(stats) if stats.vectorised == 3
        ));
    }

    #[test]
    fn one_unreadable_message_does_not_stop_the_pass() {
        let (_dir, store) = corpus(4);
        // Le blob du premier message, remplacé par des octets qu'aucun décompresseur ne lit.
        let first = store.unvectorised(1).unwrap().remove(0);
        let path = store.root().join("blobs").join(first.blob.shard_path());
        std::fs::write(path.as_std_path(), b"pas du zstd").unwrap();

        let stats = update_with(&store, &Fake::new(8), budget(), &Progress::new()).unwrap();
        assert_eq!(stats.unreadable, 1);
        assert_eq!(stats.vectorised, 3);
        // Marqué, pour ne pas revenir en tête de chaque passe.
        assert_eq!(store.unvectorised_total().unwrap(), 0);
    }

    /// Un message plus long que la borne de lecture arrive **tronqué** à `mail-parser` : c'est une
    /// entrée malformée, et le corps, qui la précède, doit en sortir intact.
    #[test]
    fn a_message_cut_at_the_reading_cap_still_gives_its_body() {
        let (_dir, store) = corpus(0);
        let attachment = "QUFB".repeat(usize::try_from(crate::text::MESSAGE_CAP).unwrap() / 3);
        let raw = format!(
            "From: Plomberie <devis@exemple.invalid>\r\n\
             Subject: devis\r\n\
             Content-Type: multipart/mixed; boundary=\"b\"\r\n\
             \r\n\
             --b\r\n\
             Content-Type: text/plain; charset=utf-8\r\n\
             \r\n\
             Voici le devis pour la chaudiere.\r\n\
             --b\r\n\
             Content-Type: application/octet-stream\r\n\
             Content-Transfer-Encoding: base64\r\n\
             \r\n\
             {attachment}\r\n\
             --b--\r\n"
        );
        assert!(raw.len() as u64 > crate::text::MESSAGE_CAP);
        let blob = store.blobs().put(raw.as_bytes()).unwrap().hash;
        let row = mailcore::store::read::VectorRow {
            id: mailcore::MessageId(1),
            blob,
            subject: "devis".to_owned(),
            from_name: None,
        };
        match compose(&store, &row, budget()).unwrap() {
            Composed::Text(text) => assert!(text.contains("chaudiere"), "{text}"),
            Composed::Missing | Composed::Unreadable => panic!("le blob est là"),
        }
    }

    /// Le défaut que la relecture du 2026-09-25 a trouvé dans le contrôle ajouté le matin même :
    /// deux `COUNT` pris dans deux instantanés, et une moisson qui valide entre les deux fabrique
    /// une divergence — donc l'effacement d'un magasin sain et une passe complète de quatre
    /// heures. Elle en avait provoqué 112 sur 23 198 appels en quatre secondes.
    #[test]
    fn a_harvest_committing_during_the_question_is_not_a_divergence() {
        use mailcore::store::write::NewMessage;
        use std::sync::atomic::{AtomicBool, Ordering};

        let (dir, store) = corpus(20);
        let fake = Fake::new(8);
        update_with(&store, &fake, budget(), &Progress::new()).unwrap();
        let root = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();

        let stop = AtomicBool::new(false);
        let (diverged, calls, inserted) = std::thread::scope(|scope| {
            let harvest = scope.spawn(|| {
                // Une seconde connexion, comme une moisson du démon pendant `mail vectors`.
                let other = Store::open(&root).unwrap();
                let mut inserted = 0u64;
                while !stop.load(Ordering::Relaxed) {
                    let raw = format!("Subject: arrivée {inserted}\r\n\r\ncorps {inserted}\r\n");
                    let blob = other.blobs().put(raw.as_bytes()).unwrap().hash;
                    let writer = other.writer().unwrap();
                    writer
                        .insert_message(&NewMessage {
                            blob,
                            rfc822_id: None,
                            date: 1_800_000_000,
                            from_addr: "e@exemple.invalid",
                            from_name: None,
                            subject: "arrivée",
                            size: raw.len() as u64,
                            has_attachments: false,
                        })
                        .unwrap();
                    writer.commit().unwrap();
                    inserted += 1;
                }
                inserted
            });
            let deadline = std::time::Instant::now() + std::time::Duration::from_millis(1_500);
            let (mut diverged, mut calls) = (0u64, 0u64);
            while std::time::Instant::now() < deadline {
                if pending(&store, budget()).unwrap().diverged {
                    diverged += 1;
                }
                calls += 1;
            }
            stop.store(true, Ordering::Relaxed);
            (diverged, calls, harvest.join().unwrap())
        });

        // Le contrôle positif : la moisson a bien tourné pendant la question.
        assert!(
            inserted > 10,
            "{inserted} messages insérés : la course n'a pas eu lieu"
        );
        assert_eq!(
            diverged, 0,
            "{diverged} fausses divergences sur {calls} appels"
        );
    }

    /// Le contrôle inverse : un magasin complet, et même un magasin qui porte **plus** de records
    /// que de messages — des doublons après une reprise —, ne divergent pas.
    #[test]
    fn a_complete_store_with_duplicates_does_not_diverge() {
        let (_dir, store) = corpus(3);
        let fake = Fake::new(8);
        update_with(&store, &fake, budget(), &Progress::new()).unwrap();
        // Une reprise après coupure réécrit des records déjà là : l'ajout seul les garde.
        store.reset_vectorised().unwrap();
        update_with(&store, &fake, budget(), &Progress::new()).unwrap();
        let found = pending(&store, budget()).unwrap();
        assert!(!found.diverged, "{found:?}");
        assert!(found.is_empty());
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
    /// ## Ce test a changé de conclusion le 2026-09-23, et pourquoi
    ///
    /// Il vérifiait que la passe **échoue** quand le budget change. Son motif écrit était « le
    /// budget change les vecteurs autant que le modèle, et le balayage à 2 000, 400, 150 et 0 a
    /// écrit quatre magasins que rien ne séparait » — c'est-à-dire : *deux budgets ne doivent pas
    /// partager un magasin*.
    ///
    /// Ce motif est maintenant mieux tenu qu'avant : la passe **détecte** la divergence et refait
    /// le magasin, au lieu de rendre une erreur que l'appelant devait comprendre. La garantie est
    /// la même, le remède est meilleur, donc c'est le test qui bouge. La barrière d'origine, elle,
    /// n'a pas disparu — `mailcore::vectors::store` la vérifie toujours à son niveau, qui est
    /// celui où elle vit.
    ///
    /// Ce qu'on vérifie ici est donc ce que l'ancien test ne disait pas : après un changement de
    /// budget, le magasin porte **exactement** les nouveaux vecteurs, sans garder les anciens à
    /// côté.
    #[test]
    fn changing_the_text_budget_replaces_the_store_rather_than_mixing_two() {
        let (_dir, store) = corpus(4);
        let progress = Progress::new();
        let fake = Fake::new(8);

        update_with(&store, &fake, crate::text::Budget::of(2_000), &progress).unwrap();
        let redone = update_with(&store, &fake, crate::text::Budget::of(150), &progress).unwrap();

        assert_eq!(redone.vectorised, 4, "le changement de budget a été ignoré");
        assert_eq!(
            Vectors::load(store.vectors_path(), 8).unwrap().len(),
            4,
            "les vecteurs des deux budgets cohabitent dans le magasin"
        );
        let provenance = mailcore::vectors::provenance_of(&store.vectors_path())
            .unwrap()
            .expect("le magasin refait porte sa provenance");
        assert_eq!(provenance.budget, 150, "la provenance dit l'ancien budget");
    }

    /// Le défaut du 2026-09-23 : un magasin périmé que le raccourci du `COUNT` rendait invisible.
    ///
    /// Tous les messages étaient marqués vectorisés, donc `unvectorised_total` rendait zéro, donc
    /// `update` annonçait « rien à faire » — sur des vecteurs écrits par une autre composition de
    /// texte. Le refus de `VectorWriter::open` existait déjà et ne servait à rien : il n'était
    /// jamais atteint.
    #[test]
    fn a_store_written_by_another_composition_is_redone_and_not_declared_up_to_date() {
        let (_dir, store) = corpus(3);
        let progress = Progress::new();
        let fake = Fake::new(8);

        update_with(&store, &fake, crate::text::Budget::of(2_000), &progress).unwrap();
        assert_eq!(
            store.unvectorised_total().unwrap(),
            0,
            "la passe a tout fait"
        );

        // Un budget différent tient lieu de composition différente : c'est le même champ de
        // provenance, et il se règle sans toucher à la constante du crate.
        let redone = update_with(&store, &fake, crate::text::Budget::of(150), &progress).unwrap();
        assert_eq!(
            redone.vectorised, 3,
            "le magasin périmé a été annoncé à jour"
        );
    }

    /// Le contrôle inverse du précédent, et il compte autant : une passe qui se croirait toujours
    /// périmée revectoriserait le corpus entier à chaque moisson — quatre heures, en boucle.
    #[test]
    fn a_store_written_by_the_same_composition_is_left_alone() {
        let (_dir, store) = corpus(3);
        let progress = Progress::new();
        let fake = Fake::new(8);
        let budget = crate::text::Budget::of(2_000);

        update_with(&store, &fake, budget, &progress).unwrap();
        let again = update_with(&store, &fake, budget, &progress).unwrap();

        assert_eq!(again.vectorised, 0, "une passe à jour a tout refait");
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
