//! Lecture de l'index plein texte.
//!
//! Ouvert en lecture concurrente, jamais bloqué par l'écrivain : tantivy travaille sur des
//! segments immuables, et un `IndexReader` continue de servir l'ancien état pendant qu'une
//! réindexation écrit le nouveau. C'est le pendant du WAL de SQLite, et la même raison —
//! l'UI ne bloque jamais.
//!
//! ## Ce que la recherche rend, et ce qu'elle ne rend pas
//!
//! Des identifiants et des scores, rien d'autre. Les métadonnées se relisent dans SQLite,
//! le corps dans le blob. L'index ne stocke pas le contenu, donc il ne peut pas le rendre —
//! et c'est voulu : une seule source de vérité par donnée.

use tantivy::collector::TopDocs;
use tantivy::query::QueryParser;
use tantivy::{Index, IndexReader};

use crate::error::Result;
use crate::index::schema::{self, Fields};
use crate::model::MessageId;

/// Un résultat de recherche : un message et sa pertinence.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Hit {
    /// Le message trouvé.
    pub id: MessageId,
    /// Le score BM25. Comparable entre résultats d'une même requête, pas entre requêtes.
    pub score: f32,
}

/// Un index ouvert en lecture.
///
/// À garder vivant : construire un `Searcher` ouvre les segments et monte les structures de
/// lecture. Le recréer à chaque requête ferait payer ce coût à chaque frappe et rendrait le
/// critère 4 inatteignable.
pub struct Searcher {
    reader: IndexReader,
    fields: Fields,
    parser: QueryParser,
}

impl Searcher {
    /// Ouvre un index existant en lecture.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Tantivy`] si l'index est absent, illisible, ou d'un schéma
    /// incompatible.
    pub fn open(index: &Index) -> Result<Self> {
        let fields = schema::resolve(&index.schema())?;
        let reader = index.reader()?;

        // Champs interrogés par défaut, quand la requête ne nomme pas de champ. Le sujet et
        // l'expéditeur pèsent plus que le corps : chercher « facture plombier » doit
        // remonter le mail intitulé « facture » avant celui qui contient le mot au détour
        // d'une signature.
        let mut parser = QueryParser::for_index(index, fields.searched().to_vec());
        parser.set_field_boost(fields.subject, 3.0);
        parser.set_field_boost(fields.from, 2.0);

        Ok(Self {
            reader,
            fields,
            parser,
        })
    }

    /// Recherche, du plus pertinent au moins pertinent.
    ///
    /// La syntaxe est celle de tantivy : `facture`, `"phrase exacte"`, `from:plombier`,
    /// `subject:facture AND body:devis`.
    ///
    /// ## Une phrase française n'est pas une requête mal formée
    ///
    /// **L'apostrophe ouvre une chaîne dans la grammaire de tantivy**, donc `l'achat qui
    /// m'attend` n'a pas de fin de chaîne et se fait refuser. En français, ça vise un utilisateur
    /// sur deux : `l'équipe`, `aujourd'hui`, `qu'il`. Le banc du critère 1 de la phase 4 l'a
    /// trouvé au premier relevé, sur la deuxième requête du jeu.
    ///
    /// La règle est donc : on essaie la **grammaire** d'abord, ce qui préserve `from:` et les
    /// phrases exactes de qui les connaît ; si elle refuse **à cause d'une apostrophe**, on
    /// réessaie sans elles. Tout autre refus reste un refus.
    ///
    /// ## Pourquoi le repli est étroit, et pourquoi c'est un test qui l'a resserré
    ///
    /// Le premier jet neutralisait toute la syntaxe. `a_malformed_query_is_an_error_not_a_silent_empty_result`
    /// est alors tombé — et sa justification écrite, « une erreur, pas un silence », **couvrait**
    /// le cas : `subject:(` est une grammaire qu'on a voulue et ratée, et lui rendre les messages
    /// contenant le mot « subject » n'est pas un silence, c'est pire. Un refus se corrige ; une
    /// liste sans rapport se croit.
    ///
    /// C'est la règle du 2026-09-10 appliquée dans l'autre sens : quand un test gêne, on relit sa
    /// justification — et ici elle tenait, donc c'est le code qui a bougé.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Tantivy`] si l'index est illisible, ou si la saisie est refusée pour autre
    /// chose qu'une apostrophe.
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<Hit>> {
        // Une requête vide ne remonte rien plutôt que tout : renvoyer 73 000 résultats
        // parce que l'utilisateur a effacé sa saisie serait le pire des comportements.
        if query.trim().is_empty() {
            return Ok(Vec::new());
        }
        let parsed = match self.parser.parse_query(query) {
            Ok(parsed) => parsed,
            Err(refused) => {
                // **Le repli est étroit, et il doit le rester.** Neutraliser toute la syntaxe
                // ferait rendre des résultats à `subject:(` — une saisie où quelqu'un a
                // *voulu* la grammaire et s'est trompé. Il y lirait des messages contenant le
                // mot « subject », ce qui est pire qu'un refus : un refus se corrige, une
                // liste sans rapport se croit.
                //
                // Seule l'apostrophe est reprise, parce qu'elle seule apparaît dans une phrase
                // que personne n'a voulue comme une requête.
                let Some(plain) = without_apostrophes(query) else {
                    return Err(refused.into());
                };
                tracing::debug!(%query, "requête relue sans ses apostrophes");
                self.parser.parse_query(&plain)?
            }
        };
        // Zéro résultat demandé, zéro rendu. Sans ce retour, `TopDocs::with_limit(0)` **panique**
        // dans tantivy (`assert_ne!(limit, 0)`) — et la limite vient d'un client, donc d'une entrée
        // qu'on ne maîtrise pas. Trouvé par la relecture du 2026-09-23.
        //
        // **Après** l'analyse et non avant : une requête mal formée reste une erreur quelle que soit
        // la limite. La placer en tête — ce que faisait la première version — faisait taire le refus
        // de `subject:(` dès que la limite valait zéro.
        if limit == 0 {
            return Ok(Vec::new());
        }
        let searcher = self.reader.searcher();
        // `.order_by_score()` : depuis tantivy 0.26, `TopDocs` est un constructeur, pas un
        // collecteur. Sans lui, ça ne compile pas — et avec un autre `order_by_*`, le tri
        // ne serait plus par pertinence.
        let collector = TopDocs::with_limit(limit).order_by_score();
        let found = searcher.search(&parsed, &collector)?;

        let mut hits = Vec::with_capacity(found.len());
        for (score, address) in found {
            let segment = searcher.segment_reader(address.segment_ord);
            let ids = segment.fast_fields().i64("id")?;
            // `first` et non une itération : `id` est mono-valué par construction du schéma.
            if let Some(id) = ids.first(address.doc_id) {
                hits.push(Hit {
                    id: MessageId(id),
                    score,
                });
            }
        }
        Ok(hits)
    }

    /// Nombre de documents dans l'index.
    ///
    /// À ne **pas** confronter au nombre de messages du store : un message dont le blob manque est
    /// marqué indexé sans document. La comparaison juste est [`crate::index::Coverage`].
    #[must_use]
    pub fn document_count(&self) -> u64 {
        self.reader.searcher().num_docs()
    }

    /// Les champs du schéma, pour les requêtes construites à la main.
    #[must_use]
    pub const fn fields(&self) -> Fields {
        self.fields
    }

    /// Combien de documents contiennent ce mot, dans les champs interrogés par défaut.
    ///
    /// ## À quoi ça sert, et pourquoi ce n'est pas une statistique de plus
    ///
    /// C'est ce qui permet de dire **si un moteur de mots peut atteindre une cible**, sans
    /// interroger aucun moteur. Une requête réduite à ce seul mot a au plus `doc_freq`
    /// candidats ; si ce nombre tient dans les dix premiers résultats, le mot suffit à
    /// remonter la cible, sinon il ne fait que la ranger dans un groupe.
    ///
    /// Le banc du critère 1 en avait besoin le 2026-09-23 : il partitionnait ses requêtes selon
    /// qu'elles **partagent** un mot avec leur cible, ce qui teste l'existence d'un mot commun
    /// et non son **pouvoir discriminant**. « club » est partagé et ne discrimine rien ;
    /// « delacroix » est partagé et discrimine à quelques dizaines près. La fréquence
    /// documentaire distingue les deux, et elle ne regarde le classement d'aucun moteur — donc
    /// elle ne peut pas rendre circulaire une comparaison entre moteurs.
    ///
    /// ## L'union des champs, ni leur somme ni leur maximum
    ///
    /// Ce qu'on veut est un **nombre de documents** : combien de messages une requête réduite à ce
    /// mot rendrait. Deux raccourcis s'offrent, et ils se trompent dans deux sens opposés :
    ///
    /// - la **somme** des fréquences par champ compte deux fois un message qui porte le mot dans
    ///   son sujet et son corps — elle fait paraître un mot plus **fréquent** qu'il n'est ;
    /// - le **maximum** oublie les messages qui ne le portent que dans un autre champ — il le fait
    ///   paraître plus **rare**, jusqu'à un facteur égal au nombre de champs.
    ///
    /// La première version prenait le maximum, avec un commentaire qui affirmait qu'il « ne fera
    /// jamais croire qu'un mot est plus rare qu'il n'est ». C'était exactement l'inverse, et une
    /// relecture l'a trouvé le 2026-09-23. L'erreur n'était pas anodine pour ce qui s'en sert :
    /// un mot sous-compté peut passer sous le seuil de dix du banc du critère 1, et ranger une
    /// requête parmi celles « qu'un mot suffit à résoudre » alors qu'il ne suffit pas.
    ///
    /// L'union se calcule donc pour de bon : une requête booléenne `OU` d'un terme par champ,
    /// comptée par le collecteur [`tantivy::collector::Count`]. C'est ce que ferait la vraie
    /// recherche, restreinte à ce mot, et elle ignore au passage les documents supprimés que
    /// `Searcher::doc_freq` de tantivy compte encore jusqu'à la prochaine fusion de segments.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Tantivy`] si l'index est illisible.
    pub fn doc_freq(&self, word: &str) -> Result<u64> {
        let found = self
            .reader
            .searcher()
            .search(&self.word_query(word), &tantivy::collector::Count)?;
        Ok(found as u64)
    }

    /// Ce document porte-t-il ce mot, dans l'un des champs interrogés par défaut ?
    ///
    /// ## Pourquoi demander à l'index plutôt que relire le message
    ///
    /// Le banc du critère 1 répondait à cette question en recomposant à la main « le texte que
    /// l'index voit » — sujet, expéditeur, corps rendu. Une relecture contradictoire a trouvé le
    /// 2026-09-23 qu'il en oubliait **quatre** sources : les destinataires, les parties HTML quand
    /// une partie texte existe, le texte des pièces jointes indexé depuis le même jour, et la fin
    /// des corps coupés à [`crate::query::MAX_BODY_TEXT`]. Une requête qui partageait un mot avec
    /// sa cible par l'une de ces voies était rangée « sans mot commun » — dans le sous-ensemble
    /// même de la comparaison que le critère 1 déclare.
    ///
    /// Recomposer le texte de l'index, c'est écrire une deuxième copie de l'indexeur, et elle
    /// dérive dès que le premier change. Demander à l'index garantit deux choses : **le même
    /// texte** — ce que l'indexeur a réellement écrit, y compris ce qu'on y ajoutera demain — et
    /// **les mêmes champs**, puisque le parseur et cette requête lisent la même liste,
    /// [`Fields::searched`].
    ///
    /// Ce qu'il ne garantit pas, et qu'il faut dire : le mot demandé arrive **déjà découpé** par
    /// l'appelant. `Term::from_field_text` ne passe pas par l'analyseur du champ, donc un
    /// découpage qui diffère de celui de tantivy — un mot de quarante octets ou plus, que
    /// l'analyseur jette ; un sigma final, que son repli en minuscules ignore — rendrait « absent »
    /// un mot qui est là. Sur du français et de l'anglais ordinaires, les deux découpages coïncident.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Tantivy`] si l'index est illisible.
    pub fn contains(&self, id: MessageId, word: &str) -> Result<bool> {
        use tantivy::query::{BooleanQuery, Occur, Query, TermQuery};
        use tantivy::schema::IndexRecordOption;

        let this_message: Box<dyn Query> = Box::new(TermQuery::new(
            tantivy::Term::from_field_i64(self.fields.id, id.0),
            IndexRecordOption::Basic,
        ));
        let this_word: Box<dyn Query> = Box::new(self.word_query(word));
        let query = BooleanQuery::new(vec![(Occur::Must, this_message), (Occur::Must, this_word)]);
        let found = self
            .reader
            .searcher()
            .search(&query, &tantivy::collector::Count)?;
        Ok(found > 0)
    }

    /// Un mot dans l'un des champs interrogés par défaut : un `OU` d'un terme par champ.
    ///
    /// Partagé par [`Self::doc_freq`] et [`Self::contains`], pour que « combien de messages
    /// portent ce mot » et « ce message porte-t-il ce mot » ne puissent pas désigner deux
    /// ensembles de champs différents.
    fn word_query(&self, word: &str) -> tantivy::query::BooleanQuery {
        use tantivy::query::{BooleanQuery, Occur, Query, TermQuery};
        use tantivy::schema::IndexRecordOption;

        let clauses: Vec<(Occur, Box<dyn Query>)> = self
            .fields
            .searched()
            .into_iter()
            .map(|field| {
                // Le terme est construit sur le mot **déjà replié en minuscules** par l'appelant :
                // `Term::from_field_text` ne passe pas par l'analyseur du champ, donc un mot capital
                // n'y trouverait rien. C'est le genre de silence qu'on préfère écrire que découvrir.
                let term = tantivy::Term::from_field_text(field, word);
                let query: Box<dyn Query> =
                    Box::new(TermQuery::new(term, IndexRecordOption::Basic));
                (Occur::Should, query)
            })
            .collect();
        BooleanQuery::new(clauses)
    }
}

/// `IndexReader` et `QueryParser` de tantivy n'implémentent pas `Debug`. Le workspace exige
/// `missing_debug_implementations` : on donne une empreinte utile plutôt que de lever la
/// règle.
impl std::fmt::Debug for Searcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Searcher")
            .field("documents", &self.document_count())
            .finish_non_exhaustive()
    }
}

/// Remplace les apostrophes par des espaces, ou rend `None` s'il n'y en avait pas.
///
/// `None` veut dire « ce refus ne vient pas d'une apostrophe » : l'appelant garde alors
/// l'erreur d'origine, parce que la saisie était une tentative de grammaire et non une phrase.
///
/// Les deux apostrophes comptent : la droite `U+0027` et la typographique `U+2019`, que les
/// claviers et les correcteurs français produisent sans qu'on le demande. N'en traiter qu'une
/// laisserait la moitié des saisies réelles se faire refuser.
///
/// Elles deviennent des **espaces** et non rien du tout : `l'achat` donne `l achat`, ce qui est
/// déjà la façon dont le texte a été découpé à l'indexation. Les coller donnerait `lachat`, un
/// mot qui n'existe dans aucun message.
fn without_apostrophes(query: &str) -> Option<String> {
    const APOSTROPHES: [char; 2] = ['\'', '\u{2019}'];
    if !query.contains(APOSTROPHES) {
        return None;
    }
    Some(
        query
            .chars()
            .map(|c| if APOSTROPHES.contains(&c) { ' ' } else { c })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::without_apostrophes;

    #[test]
    fn an_apostrophe_becomes_a_space_and_not_nothing() {
        // `lachat` ne serait dans aucun message : c'est la seule chose que ce remplacement ne
        // doit pas faire.
        assert_eq!(
            without_apostrophes("l'achat qui m'attend").as_deref(),
            Some("l achat qui m attend")
        );
    }

    #[test]
    fn the_typographic_apostrophe_counts_too() {
        // Celle que Word, les téléphones et la moitié des sites produisent sans prévenir.
        assert_eq!(
            without_apostrophes("aujourd\u{2019}hui").as_deref(),
            Some("aujourd hui")
        );
    }

    #[test]
    fn a_query_without_an_apostrophe_is_left_alone() {
        // **Le contrôle qui protège le refus.** `subject:(` est une grammaire ratée, pas une
        // phrase : rendre `Some` ici ferait chercher le mot « subject » à quelqu'un qui a
        // voulu filtrer sur un champ, et une liste sans rapport se croit là où un refus se
        // corrige.
        assert!(without_apostrophes("subject:(").is_none());
        assert!(without_apostrophes("facture").is_none());
    }
}
