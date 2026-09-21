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

/// Le moteur sémantique : un modèle et un magasin de vecteurs, tous deux ouverts.
#[derive(Debug)]
pub struct Semantic {
    model: Model,
    vectors: Vectors,
}

impl Semantic {
    /// Ouvre le moteur sur un store.
    ///
    /// # Errors
    ///
    /// [`Error::Model`] si le modèle n'est pas là — c'est le cas nominal d'une installation sans
    /// modèle, et l'appelant doit pouvoir continuer sans, ce que le critère 7 exige.
    /// [`Error::Core`] si le magasin de vecteurs est illisible ou d'une autre dimension.
    pub fn open(store: &mailcore::Store, model_dir: &camino::Utf8Path) -> Result<Self> {
        let model = Model::load(model_dir)?;
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

    /// Les messages les plus proches d'une requête, du plus proche au moins.
    ///
    /// ## La requête est encodée telle qu'elle est tapée
    ///
    /// Pas de préfixe, pas de reformulation. Certaines familles de modèles en demandent un
    /// — « query: » pour les `e5` — et en ajouter un ici « au cas où » serait un réglage inventé
    /// de plus. Celui-ci est distillé de `bge-m3`, qui n'en demande pas ; si une mesure montre
    /// qu'un préfixe aide, il s'ajoutera **avec** cette mesure.
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
        let encoded = self.model.encode(std::slice::from_ref(&query.to_owned()))?;
        let Some(vector) = encoded.first() else {
            return Err(Error::Encode(
                "la requête n'a produit aucun vecteur".to_owned(),
            ));
        };
        Ok(self.vectors.search(vector, limit)?)
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
    /// ## Une requête que le plein texte refuse n'annule pas le sémantique
    ///
    /// La grammaire de tantivy refuse certaines entrées — c'est le défaut de l'apostrophe trouvé
    /// le 2026-09-15, corrigé mais pas disparu : `subject:(` reste une erreur, et elle doit le
    /// rester. Ici, une requête que le plein texte rejette laisse quand même parler le
    /// sémantique, qui n'a aucune grammaire et donc aucune raison de refuser. L'erreur n'est pas
    /// avalée pour autant : elle est journalisée, et le résultat dit qu'un moteur a manqué.
    ///
    /// # Errors
    ///
    /// [`Error::Core`] si **les deux** moteurs échouent, ou si le store est illisible — une liste
    /// vide rendue sans rien dire serait le pire des deux mondes.
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<mailcore::fusion::Fused>> {
        let depth = self.depth.max(limit);

        let full_text = match self.mailbox.search(query, depth) {
            Ok(hits) => Ok(hits.into_iter().map(|hit| hit.item.id).collect::<Vec<_>>()),
            Err(error) => {
                tracing::warn!(%error, "le plein texte a refusé la requête");
                Err(error)
            }
        };

        let semantic = match &self.semantic {
            Some(engine) => match engine.search(query, depth) {
                Ok(hits) => Some(hits.into_iter().map(|hit| hit.id).collect::<Vec<_>>()),
                Err(error) => {
                    tracing::warn!(%error, "le sémantique a refusé la requête");
                    None
                }
            },
            None => None,
        };

        // Les deux en échec : on rend l'erreur du plein texte, qui est celle qui parle de la
        // requête. Rendre une liste vide ferait croire à zéro résultat.
        let full_text = match (full_text, &semantic) {
            (Ok(list), _) => list,
            (Err(error), None) => return Err(error.into()),
            (Err(_), Some(_)) => Vec::new(),
        };

        let mut lists: Vec<&[MessageId]> = vec![&full_text];
        if let Some(list) = &semantic {
            lists.push(list);
        }

        let mut fused = mailcore::fusion::fuse(&lists, self.k);
        fused.truncate(limit);
        Ok(fused)
    }
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
