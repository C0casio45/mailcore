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
        let mut parser = QueryParser::for_index(
            index,
            vec![fields.subject, fields.body, fields.from, fields.to],
        );
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
    /// `subject:facture AND body:devis`. Une requête que l'analyseur refuse rend une erreur
    /// plutôt qu'un silence — l'utilisateur doit savoir que sa requête est mal formée, pas
    /// croire qu'il n'y a aucun résultat.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Tantivy`] si la requête est mal formée ou l'index illisible.
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<Hit>> {
        // Une requête vide ne remonte rien plutôt que tout : renvoyer 73 000 résultats
        // parce que l'utilisateur a effacé sa saisie serait le pire des comportements.
        if query.trim().is_empty() {
            return Ok(Vec::new());
        }

        let parsed = self.parser.parse_query(query)?;
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
    /// À confronter au nombre de messages du store : un écart signale une indexation
    /// incomplète ou interrompue, ce que `mail doctor` doit savoir dire.
    #[must_use]
    pub fn document_count(&self) -> u64 {
        self.reader.searcher().num_docs()
    }

    /// Les champs du schéma, pour les requêtes construites à la main.
    #[must_use]
    pub const fn fields(&self) -> Fields {
        self.fields
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
