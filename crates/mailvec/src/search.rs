//! La recherche sémantique, et la recherche hybride qui la joint au plein texte.
//!
//! ## Pourquoi la composition est ici et pas dans le banc
//!
//! Le banc du critère 1 mesure ce que l'utilisateur obtiendra. S'il enchaînait lui-même les deux
//! moteurs et la fusion, il mesurerait **sa propre version** de la recherche, et le produit
//! pourrait en diverger sans que rien ne tombe. C'est exactement ce qui était arrivé au banc du
//! critère 1 de la phase 3, le 2026-09-10 : la règle de fil avait dû être sortie de la coquille
//! pour que le banc et l'envoi appellent la même fonction.
//!
//! [`Hybrid::search`] est donc la seule recherche hybride du projet. Le banc l'appelle, et
//! l'étape 8 l'exposera telle quelle.
//!
//! ## Ce que ça coûte de garder ouvert
//!
//! [`Semantic`] porte le modèle — une centaine de mébioctets — et les vecteurs — 72 Mio pour le
//! corpus réel. Les deux se construisent une fois et servent toutes les requêtes ; les rouvrir
//! par requête rendrait le critère 2 inatteignable, et c'est la même règle que pour le lecteur
//! tantivy et que pour `mailcore::vectors::Vectors`.

use mailcore::vectors::Vectors;
use mailcore::{Mailbox, MessageId};

use crate::model::Model;
use crate::{Error, Result};

/// Combien de candidats chaque moteur rend avant la fusion, au minimum.
///
/// ## Pourquoi c'est plus que ce que l'utilisateur voit
///
/// Fusionner deux top-10 ne ferait remonter aucun message que les deux moteurs classent onzième
/// — alors que c'est précisément le message sur lequel ils **s'accordent**, et donc celui que la
/// fusion existe pour faire apparaître. La profondeur d'entrée doit dépasser la profondeur de
/// sortie, sans quoi la règle n'a pas de matière.
///
/// Cinquante, la même valeur que le `--limit` du banc de l'étape 2 : assez pour que l'accord ait
/// lieu, assez peu pour que le balayage reste au budget du critère 2.
pub const DEPTH: usize = 50;

/// Ce qui sait encoder une **requête**.
///
/// ## Pourquoi ce n'est pas le trait de la passe
///
/// [`crate::pass::Encoder`] encode des **documents**. Pour un embedding statique la distinction
/// n'existe pas ; pour un `e5` elle est capitale — les deux rôles portent un préfixe différent,
/// et les intervertir dégrade sans rien signaler. Deux traits distincts sont ce qui empêche
/// d'appeler l'un à la place de l'autre, et ils sont implémentés séparément par chaque modèle.
pub trait QueryEncoder {
    /// La dimension des vecteurs produits.
    fn dim(&self) -> usize;

    /// Encode une requête telle que l'utilisateur l'a tapée.
    ///
    /// # Errors
    ///
    /// [`Error::Encode`] si la requête n'est pas encodable.
    fn encode_query(&self, query: &str) -> Result<Vec<f32>>;
}

impl QueryEncoder for Model {
    fn dim(&self) -> usize {
        Self::dim(self)
    }

    /// ## La requête est encodée telle qu'elle est tapée
    ///
    /// Pas de préfixe : ce modèle est distillé de `bge-m3`, qui n'en demande pas, et en ajouter
    /// un « au cas où » serait un réglage inventé de plus.
    fn encode_query(&self, query: &str) -> Result<Vec<f32>> {
        let encoded = Self::encode(self, std::slice::from_ref(&query.to_owned()))?;
        encoded
            .into_iter()
            .next()
            .ok_or_else(|| Error::Encode("la requête n'a produit aucun vecteur".to_owned()))
    }
}

impl QueryEncoder for crate::bert::Bert {
    fn dim(&self) -> usize {
        Self::dim(self)
    }

    /// Le préfixe `query: ` que l'entraînement de `e5` attend — voir [`crate::bert`].
    fn encode_query(&self, query: &str) -> Result<Vec<f32>> {
        let encoded = self.encode_as(
            std::slice::from_ref(&query.to_owned()),
            crate::bert::Role::Query,
        )?;
        encoded
            .into_iter()
            .next()
            .ok_or_else(|| Error::Encode("la requête n'a produit aucun vecteur".to_owned()))
    }
}

/// Le moteur sémantique : un modèle et un magasin de vecteurs, tous deux ouverts.
pub struct Semantic {
    model: Box<dyn QueryEncoder + Send + Sync>,
    vectors: Vectors,
}

impl std::fmt::Debug for Semantic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Semantic")
            .field("dim", &self.model.dim())
            .field("vectors", &self.vectors.len())
            .finish()
    }
}

impl Semantic {
    /// Ouvre le moteur sur un store, avec l'embedding statique.
    ///
    /// # Errors
    ///
    /// [`Error::Model`] si le modèle n'est pas là — c'est le cas nominal d'une installation sans
    /// modèle, et l'appelant doit pouvoir continuer sans, ce que le critère 7 exige.
    /// [`Error::Core`] si le magasin de vecteurs est illisible ou d'une autre dimension.
    pub fn open(store: &mailcore::Store, model_dir: &camino::Utf8Path) -> Result<Self> {
        Self::with(store, Box::new(Model::load(model_dir)?))
    }

    /// Ouvre le moteur avec le transformeur.
    ///
    /// # Errors
    ///
    /// Les mêmes que [`Self::open`].
    pub fn open_bert(store: &mailcore::Store, model_dir: &camino::Utf8Path) -> Result<Self> {
        Self::with(store, Box::new(crate::bert::Bert::load(model_dir)?))
    }

    /// Ouvre le moteur avec un encodeur déjà construit.
    ///
    /// # Errors
    ///
    /// [`Error::Core`] si le magasin de vecteurs est illisible ou d'une autre dimension — et
    /// c'est le contrôle qui compte : changer de modèle sans refaire la passe donnerait un
    /// magasin dont la dimension ne correspond plus, et il est refusé plutôt que lu de travers.
    pub fn with(
        store: &mailcore::Store,
        model: Box<dyn QueryEncoder + Send + Sync>,
    ) -> Result<Self> {
        let vectors = Vectors::load(store.vectors_path(), model.dim())?;
        Ok(Self { model, vectors })
    }

    /// Combien de messages le magasin porte.
    #[must_use]
    pub fn len(&self) -> usize {
        self.vectors.len()
    }

    /// Le magasin est-il vide ?
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.vectors.is_empty()
    }

    /// Ce qui a écrit le magasin sur lequel ce moteur répond.
    ///
    /// Exposée pour que les bancs l'impriment. Un relevé de rappel qui ne dit pas quel modèle,
    /// quel budget et quel jour ont produit les vecteurs qu'il a lus n'est pas défendable — le
    /// 2026-09-22 l'a appris en ne retrouvant pas un chiffre du journal.
    #[must_use]
    pub const fn provenance(&self) -> Option<&mailcore::vectors::Provenance> {
        self.vectors.provenance()
    }

    /// Les messages les plus proches d'une requête, du plus proche au moins.
    ///
    /// L'encodage de la requête appartient au modèle — voir [`QueryEncoder`] — parce que c'est là
    /// que vit la différence entre les familles : un `e5` veut un préfixe `query: `, un embedding
    /// statique n'en veut pas.
    ///
    /// Le texte des messages, lui, est composé par [`crate::text::compose`] : sujet, expéditeur,
    /// corps. L'asymétrie est assumée — une requête n'a pas de sujet ni d'expéditeur, et lui en
    /// fabriquer serait deviner.
    ///
    /// # Errors
    ///
    /// [`Error::Encode`] si la requête n'est pas encodable, [`Error::Core`] si le balayage
    /// refuse la dimension.
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<mailcore::vectors::Hit>> {
        let vector = self.model.encode_query(query)?;
        Ok(self.vectors.search(&vector, limit)?)
    }
}

/// La recherche telle que l'utilisateur la verra : les deux moteurs, fusionnés.
///
/// Le sémantique est **optionnel**, et c'est le critère 7 : sans modèle installé, la recherche
/// plein texte répond seule et rien ne casse. Ce n'est pas une dégradation silencieuse — l'appelant
/// sait lequel il a construit, et [`Hybrid::is_semantic`] le lui redit pour qu'une interface
/// puisse l'écrire à l'écran.
#[derive(Debug)]
pub struct Hybrid<'a> {
    mailbox: &'a Mailbox,
    semantic: Option<Semantic>,
    /// L'amortissement de la fusion. Réglable pour le banc, qui le fait varier.
    k: f32,
    /// La profondeur demandée à chaque moteur.
    depth: usize,
}

impl<'a> Hybrid<'a> {
    /// Assemble la recherche hybride.
    #[must_use]
    pub fn new(mailbox: &'a Mailbox, semantic: Option<Semantic>) -> Self {
        Self {
            mailbox,
            semantic,
            k: mailcore::fusion::K,
            depth: DEPTH,
        }
    }

    /// Change l'amortissement de la fusion. Pour le banc.
    #[must_use]
    pub fn with_k(mut self, k: f32) -> Self {
        self.k = k;
        self
    }

    /// Change la profondeur demandée à chaque moteur. Pour le banc.
    #[must_use]
    pub fn with_depth(mut self, depth: usize) -> Self {
        self.depth = depth;
        self
    }

    /// Le moteur sémantique est-il disponible ?
    #[must_use]
    pub fn is_semantic(&self) -> bool {
        self.semantic.is_some()
    }

    /// Le moteur sémantique seul, s'il y en a un.
    ///
    /// Sert au banc du critère 1, qui doit mesurer les **trois** bras : le plein texte seul, le
    /// sémantique seul, et la fusion. Sans le bras du milieu, un gain ne se distinguerait pas de
    /// ce que la règle de fusion apporte, et c'est la question de l'étape 6.
    #[must_use]
    pub const fn semantic(&self) -> Option<&Semantic> {
        self.semantic.as_ref()
    }

    /// Cherche, et rend au plus `limit` messages classés.
    ///
    /// ## Une requête que le plein texte refuse est une erreur, sémantique ou pas
    ///
    /// La grammaire de tantivy refuse certaines entrées — `subject:(` —, et c'est voulu : un refus
    /// se corrige, une liste sans rapport se croit. La première version laissait le sémantique
    /// répondre seul dans ce cas, en rendant une liste vide à la place du plein texte : la faute de
    /// frappe devenait une liste de résultats « par le sens » que rien ne signalait. Corrigé le
    /// 2026-10-08 : le refus remonte, que le sémantique soit là ou non.
    ///
    /// Une requête **à opérateurs** — un champ, des guillemets, `AND` — ne passe pas par le
    /// sémantique : voir [`uses_operators`].
    ///
    /// # Errors
    ///
    /// [`Error::Core`] si le plein texte refuse la requête ou si le store est illisible. Un échec
    /// du sémantique, lui, est journalisé et laisse le plein texte répondre seul.
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<mailcore::fusion::Fused>> {
        let depth = depth(self.depth, limit);

        // **Non plafonné, depuis le 2026-09-23.** `Mailbox::search` tronque à 500 pour protéger
        // l'API d'un client qui voudrait vider le store ; la fusion a besoin du rang vrai.
        let full_text: Vec<MessageId> = self
            .mailbox
            .search_deep(query, depth)?
            .into_iter()
            .map(|hit| hit.item.id)
            .collect();

        let semantic = match &self.semantic {
            Some(engine) if !uses_operators(query) => match engine.search(query, depth) {
                Ok(hits) => Some(hits.into_iter().map(|hit| hit.id).collect::<Vec<_>>()),
                Err(error) => {
                    tracing::warn!(%error, "le sémantique a refusé la requête");
                    None
                }
            },
            _ => None,
        };

        Ok(fuse_lists(&full_text, semantic.as_deref(), self.k, limit))
    }
}

/// La profondeur demandée à chaque moteur pour rendre `limit` résultats : jamais moins que la
/// limite, jamais moins que `minimum`.
#[must_use]
pub fn depth(minimum: usize, limit: usize) -> usize {
    minimum.max(limit)
}

/// **La règle de fusion de la recherche**, la seule du projet : le banc du critère 1 et le démon
/// l'appellent tous deux, le second avec une liste sémantique calculée dans un autre processus.
///
/// Sans liste sémantique, le résultat est exactement le plein texte, dans son ordre.
#[must_use]
pub fn fuse_lists(
    full_text: &[MessageId],
    semantic: Option<&[MessageId]>,
    k: f32,
    limit: usize,
) -> Vec<mailcore::fusion::Fused> {
    let mut lists: Vec<&[MessageId]> = vec![full_text];
    if let Some(list) = semantic {
        lists.push(list);
    }
    let mut fused = mailcore::fusion::fuse(&lists, k);
    fused.truncate(limit);
    fused
}

/// Vrai si la requête emploie la grammaire du plein texte : un champ (`from:`), une phrase entre
/// guillemets, un opérateur (`AND`, `OR`, `NOT`, `-mot`, `+mot`).
///
/// ## Pourquoi elle ne passe pas par le sémantique
///
/// Qui écrit `from:banque "relevé de compte"` demande un filtre précis, pas un voisinage de sens.
/// Le modèle encoderait la requête mot pour mot — `from:banque` compris — et la fusion diluerait
/// le filtre exact dans des messages « proches » : c'est le défaut de fusion relevé le 2026-09-21,
/// appliqué à la requête qui s'y prête le moins.
#[must_use]
pub fn uses_operators(query: &str) -> bool {
    query.contains('"')
        || query.split_whitespace().any(|word| {
            matches!(word, "AND" | "OR" | "NOT")
                || word.starts_with('-')
                || word.starts_with('+')
                || word
                    .split_once(':')
                    // Un nom de champ commence par une lettre : `14:30` est une heure.
                    .is_some_and(|(field, _)| {
                        field.chars().next().is_some_and(char::is_alphabetic)
                            && field.chars().all(char::is_alphanumeric)
                    })
        })
}
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use mailcore::Progress;

    /// Un store indexé en plein texte, sans vecteurs ni modèle.
    fn indexed(messages: &[(String, String)]) -> (tempfile::TempDir, Mailbox) {
        let (dir, store) = crate::testing::corpus_with(messages);
        mailcore::index::rebuild(&store, &Progress::new()).unwrap();
        let root = store.root().to_owned();
        drop(store);
        (dir, Mailbox::open(&root).unwrap())
    }

    fn sample() -> Vec<(String, String)> {
        vec![
            (
                "Devis chaudiere".to_owned(),
                "Voici le devis pour le remplacement.".to_owned(),
            ),
            (
                "Reservation train".to_owned(),
                "Votre billet pour Bordeaux est confirme.".to_owned(),
            ),
            (
                "Assemblee generale".to_owned(),
                "Convocation annuelle des adherents.".to_owned(),
            ),
        ]
    }

    /// **Le critère 7, du côté de la recherche.** Sans modèle, la recherche répond quand même, et
    /// elle répond exactement ce que le plein texte seul aurait répondu. Une recherche qui
    /// deviendrait vide ou qui échouerait parce qu'un fichier optionnel manque serait une panne
    /// causée par une fonctionnalité qu'on n'a pas installée.
    #[test]
    fn without_a_model_the_search_still_answers() {
        let (_dir, mailbox) = indexed(&sample());
        let hybrid = Hybrid::new(&mailbox, None);

        assert!(!hybrid.is_semantic());
        let found = hybrid.search("devis", 10).unwrap();
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].ranks, vec![Some(1)]);
    }

    /// Le contrôle inverse : la liste rendue est bien celle du plein texte, dans son ordre, et
    /// pas une liste vide qui passerait le test précédent si `len` avait été comparé à `0`.
    #[test]
    fn the_order_is_the_full_text_order_when_it_is_alone() {
        let (_dir, mailbox) = indexed(&sample());
        let hybrid = Hybrid::new(&mailbox, None);

        let direct: Vec<MessageId> = mailbox
            .search("convocation", 10)
            .unwrap()
            .into_iter()
            .map(|hit| hit.item.id)
            .collect();
        let fused: Vec<MessageId> = hybrid
            .search("convocation", 10)
            .unwrap()
            .into_iter()
            .map(|hit| hit.id)
            .collect();
        assert_eq!(fused, direct);
        assert!(!direct.is_empty(), "le contrôle ne contrôle rien");
    }

    /// Une requête que la grammaire de tantivy refuse — et qui doit continuer à être refusée,
    /// c'est le test `a_malformed_query_is_an_error_not_a_silent_empty_result` de la phase 1 —
    /// remonte comme une erreur quand il n'y a pas de sémantique pour la rattraper.
    #[test]
    fn a_query_the_grammar_refuses_is_an_error_and_not_an_empty_list() {
        let (_dir, mailbox) = indexed(&sample());
        let hybrid = Hybrid::new(&mailbox, None);
        assert!(hybrid.search("subject:(", 10).is_err());
    }

    /// Un encodeur de requête qui rend toujours le même vecteur : le sémantique est **là**, et
    /// c'est tout ce que les tests suivants demandent.
    struct Constant;

    impl QueryEncoder for Constant {
        fn dim(&self) -> usize {
            4
        }

        fn encode_query(&self, _query: &str) -> Result<Vec<f32>> {
            Ok(vec![1.0, 0.0, 0.0, 0.0])
        }
    }

    /// Le défaut corrigé le 2026-10-08 : avec un sémantique présent, une requête que la grammaire
    /// refuse rendait une liste « par le sens » au lieu de l'erreur.
    #[test]
    fn a_refused_query_stays_an_error_when_the_semantic_engine_is_there() {
        let (dir, mailbox) = indexed(&sample());
        let root = camino::Utf8Path::from_path(dir.path()).unwrap();
        let store = mailcore::Store::open(root).unwrap();
        let semantic = Semantic::with(&store, Box::new(Constant)).unwrap();
        let hybrid = Hybrid::new(&mailbox, Some(semantic));
        assert!(hybrid.is_semantic());
        assert!(hybrid.search("subject:(", 10).is_err());
        // Le contrôle inverse : une requête valable répond.
        assert!(hybrid.search("devis", 10).is_ok());
    }

    #[test]
    fn the_query_grammar_is_recognised_and_plain_words_are_not() {
        for query in [
            "from:banque",
            "subject:facture 2024",
            "\"relevé de compte\"",
            "chaudière AND devis",
            "facture -orange",
            "+urgent réunion",
        ] {
            assert!(uses_operators(query), "{query:?}");
        }
        // Le contrôle inverse : la langue courante, ponctuation comprise, passe par le sens.
        for query in [
            "la facture du plombier de l'an dernier",
            "rendez-vous à 14:30",
            "devis chaudière",
            "c'est quoi : la réponse",
            "and or not",
        ] {
            assert!(!uses_operators(query), "{query:?}");
        }
    }

    #[test]
    fn without_a_semantic_list_the_fusion_is_the_full_text_order() {
        let ids = [MessageId(3), MessageId(1), MessageId(2)];
        let fused: Vec<MessageId> = fuse_lists(&ids, None, mailcore::fusion::K, 10)
            .into_iter()
            .map(|hit| hit.id)
            .collect();
        assert_eq!(fused, ids);
    }

    #[test]
    fn a_query_that_matches_nothing_gives_an_empty_list_and_not_an_error() {
        let (_dir, mailbox) = indexed(&sample());
        let hybrid = Hybrid::new(&mailbox, None);
        assert!(hybrid.search("zzzzinexistant", 10).unwrap().is_empty());
    }

    /// La profondeur demandée aux moteurs ne doit pas descendre sous la limite réclamée : sinon
    /// un appelant qui veut cinquante résultats en recevrait `DEPTH`.
    #[test]
    fn asking_for_more_than_the_depth_does_not_truncate_the_engines() {
        let messages: Vec<(String, String)> = (0..60)
            .map(|i| (format!("commun sujet {i}"), format!("corps commun {i}")))
            .collect();
        let (_dir, mailbox) = indexed(&messages);
        let hybrid = Hybrid::new(&mailbox, None).with_depth(5);

        let found = hybrid.search("commun", 40).unwrap();
        assert_eq!(found.len(), 40, "la profondeur a bridé le résultat");
    }
}
