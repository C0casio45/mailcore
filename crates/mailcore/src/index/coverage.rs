//! Ce que « l'index couvre le store » veut dire, écrit une fois.
//!
//! ## Pourquoi un module pour une soustraction
//!
//! Parce que la soustraction était fausse à deux endroits, de la même façon. `mail doctor` et le
//! banc du critère 1 comparaient tous deux le nombre de documents de l'index au nombre de messages
//! du store. Or l'indexeur marque indexé un message dont le blob manque **sans lui écrire de
//! document** — il n'a rien à indexer. Sur un store qui a perdu un seul blob, l'égalité ne tenait
//! jamais : `mail doctor` annonçait un index désynchronisé en permanence, le banc refusait de
//! tourner en permanence, et tous deux conseillaient `mail index`, qui saute les mêmes blobs et ne
//! pouvait donc rien réparer.
//!
//! Trois relectures contradictoires l'ont trouvé le 2026-09-23 : la première dans le banc, la
//! seconde dans `mail doctor`, **où la correction de la première renvoyait l'utilisateur**, la
//! troisième dans la correction elle-même — voir [`Coverage::is_complete`]. Une
//! règle corrigée à un endroit et laissée fausse à l'autre, c'est la leçon du carnet d'adresses —
//! deux chemins vers la même donnée partagent leur code, pas leur intention.

/// L'état de l'index plein texte face au store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Coverage {
    /// Les documents que porte l'index.
    pub documents: u64,
    /// Les messages du store.
    pub messages: u64,
    /// Les messages dont le blob est absent aujourd'hui. Selon que le blob a été perdu avant ou
    /// après leur indexation, ils ont un document ou n'en ont pas.
    pub missing_blobs: u64,
}

impl Coverage {
    /// Les messages que l'indexeur **peut** indexer : ceux dont le blob est là.
    #[must_use]
    pub const fn indexable(&self) -> u64 {
        self.messages.saturating_sub(self.missing_blobs)
    }

    /// Vrai si le nombre de documents est compatible avec un index complet.
    ///
    /// ## Un encadrement, et non une égalité
    ///
    /// Un message dont le blob est **là** a un document ; un message dont le blob manque **peut**
    /// en avoir un ou non, selon l'ordre des événements. Blob perdu **avant** l'indexation :
    /// l'indexeur n'a rien à lire et n'écrit pas de document. Blob perdu **après** — une coupure de
    /// courant, un disque qui lâche — : le document écrit plus tôt reste, parfaitement valide,
    /// puisque les métadonnées du message sont toujours dans SQLite.
    ///
    /// La deuxième version de cette règle — l'égalité avec les messages indexables — ne connaissait
    /// que le premier ordre. Une troisième relecture contradictoire a trouvé le second le
    /// 2026-09-23 : `mail doctor` y aurait annoncé un index désynchronisé, et conseillé une
    /// réindexation qui **effaçait** le document d'un message intact. Les deux bornes sont donc :
    ///
    /// - **au moins** un document par message indexable — en dessous, l'index est incomplet ;
    /// - **au plus** un document par message — au-dessus, il est en avance sur le store, un
    ///   message supprimé y traîne, et il rend des résultats fantômes.
    ///
    /// Ce que des comptes ne peuvent pas voir, et qu'il faut dire : un document manquant compensé
    /// par un document fantôme laisse les deux nombres en règle. Le démasquer demanderait de
    /// comparer les identifiants un à un ; ce n'est pas le rôle d'un diagnostic en deux comptes.
    #[must_use]
    pub const fn is_complete(&self) -> bool {
        self.indexable() <= self.documents && self.documents <= self.messages
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Le défaut que ce module corrige : un blob manquant n'est pas un index incomplet.
    #[test]
    fn a_missing_blob_does_not_make_a_complete_index_look_incomplete() {
        let coverage = Coverage {
            documents: 9,
            messages: 10,
            missing_blobs: 1,
        };
        assert_eq!(coverage.indexable(), 9);
        assert!(
            coverage.is_complete(),
            "neuf documents pour neuf messages indexables : l'index est complet"
        );
    }

    /// Le cas que la deuxième version ratait : le blob est perdu **après** l'indexation, et son
    /// document, toujours valide, est resté.
    #[test]
    fn a_blob_lost_after_indexing_leaves_a_valid_document_and_a_complete_index() {
        let coverage = Coverage {
            documents: 10,
            messages: 10,
            missing_blobs: 1,
        };
        assert!(
            coverage.is_complete(),
            "le document du message au blob perdu est légitime : rien à réindexer"
        );
    }

    /// Le contrôle inverse : un vrai manque reste un manque, dans les deux sens.
    #[test]
    fn a_real_gap_is_still_a_gap_in_both_directions() {
        let short = Coverage {
            documents: 8,
            messages: 10,
            missing_blobs: 1,
        };
        assert!(!short.is_complete(), "un document manque en plus du blob");
        let ahead = Coverage {
            documents: 11,
            messages: 10,
            missing_blobs: 0,
        };
        assert!(
            !ahead.is_complete(),
            "un index en avance sur le store n'est pas complet"
        );
        let empty = Coverage {
            documents: 0,
            messages: 2,
            missing_blobs: 0,
        };
        assert!(!empty.is_complete(), "un store importé mais pas indexé");
    }
}
