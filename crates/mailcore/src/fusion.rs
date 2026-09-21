//! La règle de fusion des deux moteurs, écrite **à un seul endroit**.
//!
//! La recherche plein texte et la recherche sémantique ne répondent pas à la même question, et
//! l'utilisateur voit une seule liste. Ce module décide comment les deux n'en font qu'une, et
//! `docs/PHASE-4.md` avait posé la contrainte avant qu'on ait un moteur : la règle est un choix
//! explicite, écrit une fois, et mesurable.
//!
//! ## Pourquoi les scores ne sont pas additionnés, ni normalisés
//!
//! Le score de tantivy est un **BM25** : une statistique de corpus, sans borne supérieure, dont
//! la valeur dépend de la rareté des mots de *cette* requête. Le score du vectoriel est un
//! **cosinus** : un angle, borné dans `[-1, 1]`, comparable d'une requête à l'autre. Les
//! additionner n'a aucun sens dimensionnel.
//!
//! Les normaliser en a moins qu'il n'y paraît, et c'est le piège qui mérite d'être écrit : une
//! normalisation min-max par requête met le **meilleur résultat à 1,0 quelles que soient ses
//! qualités**. Sur une requête dont aucun message ne répond — le cas où l'on veut justement que
//! le moteur se taise —, elle promeut le moins mauvais au rang d'excellent. Elle fabrique de la
//! confiance là où il n'y en a pas.
//!
//! ## Ce qu'on fusionne à la place : les rangs
//!
//! Un rang est la seule chose que les deux moteurs produisent dans la même unité. La règle est la
//! **fusion par rangs réciproques** (RRF, Cormack, Clarke & Buettcher, 2009) :
//!
//! ```text
//! score(m) = Σ   1 / (K + rang de m dans la liste i)
//!          i
//! ```
//!
//! Elle a trois propriétés qui comptent ici, et aucune n'est une coïncidence heureuse — ce sont
//! celles pour lesquelles elle a été retenue :
//!
//! - **elle n'a besoin d'aucune comparabilité entre moteurs.** Ajouter un troisième moteur, ou
//!   changer celui de dessous, ne demande pas de rejauger quoi que ce soit ;
//! - **un message trouvé par un seul moteur remonte quand même.** C'est tout l'intérêt de la
//!   phase : le sémantique existe pour faire entrer dans la liste ce que le plein texte ne peut
//!   pas voir. Une règle qui exigerait un accord des deux annulerait exactement le gain qu'on
//!   cherche ;
//! - **un accord des deux vaut mieux qu'un premier rang seul.** Avec `K = 60`, deux septièmes
//!   places battent une première place isolée — ce qui est la définition raisonnable de « les
//!   deux moteurs sont d'accord ».
//!
//! ## Ce qu'il n'y a **pas**, et pourquoi
//!
//! **Pas de poids par moteur.** Un coefficient deviné est exactement ce que `docs/PHASE-4.md`
//! refuse : « une moyenne pondérée par des coefficients devinés est le genre de chose qui donne
//! un banc à 78 % qu'on n'arrive plus à améliorer sans savoir pourquoi ». Les deux listes pèsent
//! donc pareil tant que personne n'a **mesuré** qu'il faut autre chose. Le jour où ce sera
//! mesuré, le poids s'ajoute ici et nulle part ailleurs.
//!
//! `K` est le seul réglage, il porte la valeur publiée par les auteurs, et le banc
//! `measure-recall` sait le faire varier — voir l'étape 6.

use std::collections::HashMap;

use crate::MessageId;

/// L'amortissement des premiers rangs.
///
/// Soixante, la valeur de l'article d'origine. Ce qu'elle règle, en une phrase : plus `K` est
/// petit, plus la première place d'un seul moteur écrase tout le reste ; plus il est grand, plus
/// les rangs s'égalisent et plus la fusion revient à compter les moteurs qui ont vu le message.
///
/// Elle n'est pas choisie par nous, et c'est volontaire : un réglage repris d'une publication est
/// un point de départ défendable, là où un réglage inventé demande d'être justifié avant même
/// d'avoir un chiffre. Le banc le fait varier ; s'il faut en changer, ce sera avec une mesure.
pub const K: f32 = 60.0;

/// Un message et ce que la fusion lui a donné.
#[derive(Debug, Clone, PartialEq)]
pub struct Fused {
    /// Le message.
    pub id: MessageId,
    /// Le score de fusion. Comparable **au sein d'une requête** et pas entre requêtes : c'est une
    /// somme de rangs réciproques, pas une probabilité.
    pub score: f32,
    /// Le rang de ce message dans chaque liste d'entrée, dans l'ordre où elles ont été données.
    /// `None` quand la liste ne l'a pas rendu.
    ///
    /// Gardé parce que c'est ce qui rend un résultat **explicable** : « ce message est là parce
    /// que le sémantique l'a mis premier et que le plein texte ne l'a pas vu » est une phrase
    /// qu'une interface peut dire, et un banc diagnostiquer.
    pub ranks: Vec<Option<usize>>,
}

/// Fusionne des listes classées en une seule.
///
/// Chaque liste est donnée **dans l'ordre du moteur**, le meilleur en premier. Un message présent
/// deux fois dans la même liste ne compte que pour sa meilleure position — un moteur ne devrait
/// pas le faire, et l'ignorer ici évite qu'un défaut d'en dessous double un score.
///
/// Le résultat est trié par score décroissant. À score égal — le cas se produit dès que deux
/// messages occupent des rangs symétriques — l'ordre est **déterministe** : le meilleur rang
/// obtenu d'abord, puis l'identifiant. Un banc qui rejouerait deux fois la même requête doit lire
/// deux fois le même classement.
#[must_use]
pub fn fuse(lists: &[&[MessageId]], k: f32) -> Vec<Fused> {
    let mut seen: HashMap<MessageId, Fused> = HashMap::new();

    for (which, list) in lists.iter().enumerate() {
        for (index, id) in list.iter().enumerate() {
            let rank = index + 1;
            let entry = seen.entry(*id).or_insert_with(|| Fused {
                id: *id,
                score: 0.0,
                ranks: vec![None; lists.len()],
            });
            // Un doublon dans la même liste ne recompte pas : seule la meilleure position vaut.
            if entry.ranks[which].is_some() {
                continue;
            }
            entry.ranks[which] = Some(rank);
            #[allow(clippy::cast_precision_loss)]
            {
                entry.score += 1.0 / (k + rank as f32);
            }
        }
    }

    let mut fused: Vec<Fused> = seen.into_values().collect();
    fused.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| best_rank(a).cmp(&best_rank(b)))
            .then_with(|| a.id.0.cmp(&b.id.0))
    });
    fused
}

/// Le meilleur rang obtenu, pour départager deux scores égaux.
fn best_rank(fused: &Fused) -> usize {
    fused
        .ranks
        .iter()
        .filter_map(|rank| *rank)
        .min()
        .unwrap_or(usize::MAX)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn ids(values: &[i64]) -> Vec<MessageId> {
        values.iter().copied().map(MessageId).collect()
    }

    fn order(fused: &[Fused]) -> Vec<i64> {
        fused.iter().map(|it| it.id.0).collect()
    }

    /// **La propriété qui justifie la phase.** Un message que seul le sémantique a vu doit entrer
    /// dans la liste. Une règle qui exigerait un accord des deux moteurs annulerait exactement le
    /// gain qu'on cherche — et le ferait sans rien signaler.
    #[test]
    fn a_message_only_one_engine_found_still_enters_the_list() {
        let plein = ids(&[1, 2]);
        let semantique = ids(&[9]);
        let fused = fuse(&[&plein, &semantique], K);
        assert!(order(&fused).contains(&9), "{:?}", order(&fused));
    }

    /// Le contrôle inverse : il entre, mais il ne passe pas devant un message que **les deux**
    /// moteurs ont bien classé. Sans cette moitié, la règle promouvrait n'importe quel bruit
    /// sémantique au-dessus d'un résultat sur lequel les deux s'accordent.
    #[test]
    fn agreement_beats_a_single_first_place() {
        let plein = ids(&[7, 1]);
        let semantique = ids(&[7, 9]);
        let fused = fuse(&[&plein, &semantique], K);
        assert_eq!(order(&fused)[0], 7);
    }

    /// Avec `K = 60`, deux septièmes places battent une première place isolée. C'est la
    /// définition opérationnelle de « les deux moteurs sont d'accord », et c'est ce que le
    /// réglage `K` gouverne — le vérifier ici fige ce que la valeur veut dire.
    #[test]
    fn two_seventh_places_beat_a_lone_first() {
        let solitaire = ids(&[1]);
        let accord_a = ids(&[0, 0, 0, 0, 0, 0, 5]);
        let accord_b = ids(&[0, 0, 0, 0, 0, 0, 5]);

        let lone = fuse(&[&solitaire], K)[0].score;
        let pair = fuse(&[&accord_a, &accord_b], K)
            .into_iter()
            .find(|it| it.id.0 == 5)
            .unwrap()
            .score;
        assert!(pair > lone, "accord {pair} contre solitaire {lone}");
    }

    /// Ce que `K` règle, dans l'autre sens : très petit, la première place d'un seul moteur
    /// l'emporte sur un accord plus bas. Le test dit ce que le réglage fait, ce qui est la seule
    /// façon de le régler plus tard sans tâtonner.
    #[test]
    fn a_small_k_lets_a_lone_first_place_win() {
        let solitaire = ids(&[1]);
        let accord_a = ids(&[0, 0, 0, 0, 0, 0, 5]);
        let accord_b = ids(&[0, 0, 0, 0, 0, 0, 5]);

        let lone = fuse(&[&solitaire], 0.5)[0].score;
        let pair = fuse(&[&accord_a, &accord_b], 0.5)
            .into_iter()
            .find(|it| it.id.0 == 5)
            .unwrap()
            .score;
        assert!(pair < lone, "accord {pair} contre solitaire {lone}");
    }

    #[test]
    fn the_ranks_say_which_engine_saw_what() {
        let plein = ids(&[1, 2]);
        let semantique = ids(&[2]);
        let fused = fuse(&[&plein, &semantique], K);

        let two = fused.iter().find(|it| it.id.0 == 2).unwrap();
        assert_eq!(two.ranks, vec![Some(2), Some(1)]);
        let one = fused.iter().find(|it| it.id.0 == 1).unwrap();
        assert_eq!(one.ranks, vec![Some(1), None]);
    }

    /// Un moteur qui rendrait deux fois le même message ne doit pas lui donner deux fois son
    /// score. Ce n'est pas une hypothèse gratuite : le magasin de vecteurs a une règle exprès —
    /// le dernier record gagne — pour que ça n'arrive pas, et une barrière par défaut vaut mieux
    /// qu'un appelant vigilant.
    #[test]
    fn a_duplicate_inside_one_list_counts_once_at_its_best_rank() {
        let doublon = ids(&[3, 3, 3]);
        let fused = fuse(&[&doublon], K);
        assert_eq!(fused.len(), 1);
        assert_eq!(fused[0].ranks, vec![Some(1)]);
        assert!((fused[0].score - 1.0 / (K + 1.0)).abs() < f32::EPSILON);
    }

    /// Deux exécutions de la même fusion doivent rendre le même ordre, y compris quand des scores
    /// sont exactement égaux. Sans départage déterministe, l'ordre viendrait du parcours d'une
    /// table de hachage et un banc rendrait des chiffres qui bougent.
    #[test]
    fn equal_scores_are_broken_deterministically() {
        let a = ids(&[4, 5, 6, 7, 8, 9, 10, 11]);
        let b = ids(&[11, 10, 9, 8, 7, 6, 5, 4]);
        let first = order(&fuse(&[&a, &b], K));
        for _ in 0..20 {
            assert_eq!(order(&fuse(&[&a, &b], K)), first);
        }
    }

    #[test]
    fn an_empty_engine_does_not_change_the_other() {
        let plein = ids(&[1, 2, 3]);
        let vide: Vec<MessageId> = Vec::new();
        assert_eq!(order(&fuse(&[&plein, &vide], K)), vec![1, 2, 3]);
    }

    #[test]
    fn fusing_nothing_gives_nothing() {
        assert!(fuse(&[], K).is_empty());
        let vide: Vec<MessageId> = Vec::new();
        assert!(fuse(&[&vide, &vide], K).is_empty());
    }

    /// L'ordre d'une seule liste doit survivre intact à la fusion. C'est le contrôle qui dit que
    /// la règle n'abîme pas ce qu'elle reçoit — un moteur absent ne doit rien coûter à l'autre.
    #[test]
    fn a_single_list_comes_back_in_its_own_order() {
        let plein = ids(&[42, 7, 13, 1]);
        assert_eq!(order(&fuse(&[&plein], K)), vec![42, 7, 13, 1]);
    }
}
