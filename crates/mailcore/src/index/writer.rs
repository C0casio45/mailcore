//! Écriture dans l'index plein texte.
//!
//! Toujours appelé depuis un job de fond : indexer 73 000 messages prend des minutes, et
//! l'UI ne doit pas attendre (règle 3 du `CLAUDE.md`). L'index est **reconstructible** —
//! il se refait entièrement depuis les blobs, qui sont la source de vérité. Le perdre coûte
//! du temps, jamais une donnée.
//!
//! ## Pourquoi l'indexation lit les blobs et pas le profil
//!
//! L'import a déjà tout copié. Relire le profil ferait dépendre l'indexation d'une source
//! externe que l'utilisateur modifie en parallèle, et rendrait impossible de réindexer une
//! machine où le profil n'existe pas — le démon distant, par exemple.

use crate::progress::Progress;

use mail_parser::MessageParser;
use tantivy::{Index, IndexWriter, doc};

use crate::error::Result;
use crate::index::schema::{self, Fields};
use crate::model::MessageId;
use crate::store::Store;

/// Mémoire allouée à l'écrivain tantivy.
///
/// 128 Mio : tantivy accumule en mémoire puis écrit un segment quand le budget est plein.
/// Trop petit multiplie les segments et allonge la fusion ; trop grand pèse sur le RSS, que
/// le critère 3 plafonne. 128 Mio laisse de la marge sous les 500 Mo tout en produisant des
/// segments d'une taille raisonnable.
const WRITER_HEAP: usize = 128 * 1024 * 1024;

/// Messages indexés avant de rendre la main au compteur de progression.
const PROGRESS_EVERY: u64 = 5_000;

/// Ce qu'une indexation a fait.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IndexStats {
    /// Messages ajoutés à l'index.
    pub indexed: u64,
    /// Messages dont le blob était introuvable. Signalés, jamais fatals.
    pub missing_blobs: u64,
    /// Messages dont le MIME n'a pas pu être analysé : indexés sur leurs seules métadonnées.
    pub degraded: u64,
    /// Octets de texte envoyés à l'index.
    pub text_bytes: u64,
}

/// Reconstruit l'index plein texte depuis le store.
///
/// Efface l'index existant : c'est une reconstruction, pas une mise à jour incrémentale.
/// L'incrémental viendra avec la sync, quand il y aura des messages à ajouter un par un ;
/// pour un import en masse, tout refaire est plus simple et plus rapide.
///
/// `cancelled` permet à un job de fond d'être interrompu proprement — l'index reste
/// simplement incomplet, et une réindexation le refera.
///
/// # Errors
///
/// [`crate::Error::Tantivy`] si l'index ne peut pas être écrit, [`crate::Error::Sqlite`] si
/// le store est illisible. Un message qui échoue individuellement est compté, pas propagé.
pub fn rebuild(store: &Store, progress: &Progress) -> Result<IndexStats> {
    let (index, fields) = open_or_create(store)?;
    {
        let mut writer: IndexWriter = index.writer(WRITER_HEAP)?;
        writer.delete_all_documents()?;
        writer.commit()?;
    }
    // Vider l'index **et** remettre les drapeaux : les deux vont ensemble, comme pour le
    // carnet. N'en faire qu'un donnerait soit un index vide que rien ne remplit, soit une
    // réindexation qui ne se déclenche jamais.
    store.reset_indexed()?;
    advance(store, &index, fields, progress)
}

/// Indexe ce que l'index n'a pas encore vu, et s'arrête là.
///
/// ## Pourquoi elle existe
///
/// L'index ne suivait aucune moisson : le courrier arrivait, et la recherche l'ignorait jusqu'à
/// une réindexation complète lancée à la main. Enchaîner une reconstruction après chaque
/// moisson était exclu — `IDLE` met un `Kind::Sync` en file à chaque arrivée de courrier.
///
/// ## Le contrôle de cohérence, qui n'a pas d'équivalent côté carnet
///
/// Le drapeau vit dans SQLite, les documents dans tantivy : **deux magasins, donc deux vérités
/// possibles**. Elles divergent quand l'index est recréé sans que le store le sache — répertoire
/// supprimé, schéma illisible, disque remplacé. `open_or_create` rend alors un index vide, et
/// des drapeaux qui disent « indexé » feraient sauter tous les messages : la recherche resterait
/// vide **pour toujours**, sans que rien ne le signale.
///
/// Un index vide alors que des messages se disent indexés est donc traité comme le symptôme
/// qu'il est, et les drapeaux sont remis. Le contrôle est ici, au point d'usage, et non dans
/// `open_or_create` : cette fonction-là est appelée sur des chemins de **lecture** —
/// `Mailbox::open`, `mail search` — qui ne doivent pas écrire.
///
/// # Errors
///
/// Les mêmes que [`rebuild`].
pub fn update(store: &Store, progress: &Progress) -> Result<IndexStats> {
    // **La question la moins chère en premier.** Mesuré le 2026-09-11 : ouvrir l'index et son
    // lecteur coûte 11 à 15 ms, et c'était payé à **chaque** moisson — y compris les moissons
    // qui n'apportent rien, qui sont la grande majorité. Un `COUNT` sur une colonne indexée coûte
    // des microsecondes.
    //
    // Ce que ce raccourci retarde : le contrôle de divergence ci-dessous, qui a besoin de
    // l'index. Un index effacé n'est donc plus détecté tant qu'aucun message n'arrive — et à ce
    // moment-là il l'est. Entre-temps la recherche ne rend rien, ce que `mail doctor` signale et
    // qu'une réindexation corrige. Payer 15 ms à chaque moisson pour avancer ce diagnostic
    // d'un message n'en vaut pas le prix.
    if store.unindexed_total()? == 0 {
        return Ok(IndexStats::default());
    }

    let (index, fields) = open_or_create(store)?;

    let documents = index.reader()?.searcher().num_docs();
    if documents == 0 && store.indexed_count()? > 0 {
        tracing::warn!(
            "index vide alors que des messages se disent indexés : réindexation complète"
        );
        store.reset_indexed()?;
    }

    advance(store, &index, fields, progress)
}

/// La boucle partagée : indexe les messages non indexés, les marque, et rend le bilan.
///
/// ## Réindexer un message est sans effet, et c'est ce qui rend l'ordre sûr
///
/// Chaque écriture retire d'abord le document de même identifiant. Un message indexé deux fois
/// ne produit donc pas deux résultats de recherche — ce qui serait le défaut visible, et le plus
/// difficile à diagnostiquer. C'est cette idempotence qui permet de **valider l'index avant** de
/// marquer : une coupure entre les deux fait réindexer, sans conséquence.
///
/// L'ordre inverse — marquer puis indexer — rendrait un message introuvable pour toujours.
fn advance(
    store: &Store,
    index: &Index,
    fields: Fields,
    progress: &Progress,
) -> Result<IndexStats> {
    let mut stats = IndexStats::default();
    let parser = MessageParser::default();
    let mut text = String::with_capacity(64 * 1024);

    // Bornée par ce qui était en attente au départ, pour la même raison que le carnet : sans
    // borne, la boucle ne s'arrête que parce que le marquage retire les lignes de la requête,
    // et un marquage défaillant ferait tourner un job de fond pour toujours.
    let pending = store.unindexed_total()?;
    progress.set_total(pending);
    let mut remaining = pending;

    while remaining > 0 {
        let rows = store.unindexed(ROWS.min(remaining as usize))?;
        if rows.is_empty() {
            break;
        }
        remaining = remaining.saturating_sub(rows.len() as u64);

        let mut writer: IndexWriter = index.writer(WRITER_HEAP)?;
        let mut done: Vec<crate::MessageId> = Vec::with_capacity(rows.len());
        let mut cancelled = false;

        for row in &rows {
            if progress.is_cancelled() {
                tracing::info!(indexed = stats.indexed, "indexation interrompue");
                cancelled = true;
                break;
            }
            index_one(store, &writer, fields, &parser, row, &mut text, &mut stats)?;
            done.push(row.id);
            progress.advance(1);

            if stats.indexed % PROGRESS_EVERY == 0 && stats.indexed > 0 {
                tracing::debug!(indexed = stats.indexed, "indexation en cours");
            }
        }

        // Ce qui a été écrit est validé même sur une annulation : une interruption arrête le
        // travail, elle ne le jette pas.
        writer.commit()?;
        store.mark_indexed(&done)?;
        if cancelled {
            break;
        }
    }

    tracing::info!(
        indexed = stats.indexed,
        degraded = stats.degraded,
        missing = stats.missing_blobs,
        "index avancé"
    );
    Ok(stats)
}

/// Combien de lignes relire à la fois.
///
/// Le même arbitrage que pour le carnet : assez pour qu'une reconstruction complète ne paie pas
/// le coût par requête, assez peu pour qu'une moisson de trois messages ne charge pas le corpus.
/// Chaque paquet est un `commit` de tantivy, ce qui borne aussi la mémoire du writer.
const ROWS: usize = 2_000;

/// Ouvre l'index sur le disque, ou le crée s'il n'existe pas.
///
/// # Errors
///
/// [`crate::Error::Io`] si le répertoire ne peut pas être créé, [`crate::Error::Tantivy`]
/// si l'index existant est illisible ou son schéma incompatible.
pub fn open_or_create(store: &Store) -> Result<(Index, Fields)> {
    let dir = store.search_dir();
    std::fs::create_dir_all(&dir).map_err(|source| crate::Error::Io {
        path: dir.clone(),
        source,
    })?;

    match Index::open_in_dir(dir.as_std_path()) {
        Ok(index) => {
            let fields = schema::resolve(&index.schema())?;
            Ok((index, fields))
        }
        Err(_) => {
            // Index absent ou illisible : on le recrée. Il est reconstructible depuis les
            // blobs, donc l'écraser ne perd rien — au pire quelques minutes de calcul.
            let (schema, fields) = schema::build();
            let index = Index::create_in_dir(dir.as_std_path(), schema)?;
            Ok((index, fields))
        }
    }
}

/// Indexe un message. Une défaillance individuelle est comptée, jamais propagée.
fn index_one(
    store: &Store,
    writer: &IndexWriter,
    fields: Fields,
    parser: &MessageParser,
    row: &crate::store::read::IndexRow,
    text: &mut String,
    stats: &mut IndexStats,
) -> Result<()> {
    let raw = match store.blobs().read(row.blob) {
        Ok(bytes) => bytes,
        Err(crate::Error::BlobNotFound(hash)) => {
            // L'index référence un blob absent : diagnostiquable par `mail doctor`, pas de
            // quoi interrompre une indexation de 73 000 messages.
            tracing::warn!(%hash, id = row.id.0, "blob absent, message non indexé");
            stats.missing_blobs += 1;
            return Ok(());
        }
        Err(other) => return Err(other),
    };

    text.clear();
    let mut recipients = String::new();

    match parser.parse(&raw) {
        Some(parsed) => {
            body_text(&parsed, text);
            addresses(parsed.to(), &mut recipients);
            addresses(parsed.cc(), &mut recipients);
        }
        None => {
            // En-têtes illisibles : on indexe quand même sur ce que SQLite sait déjà. Un
            // message introuvable serait pire qu'un message mal indexé.
            stats.degraded += 1;
        }
    }

    let sender = match &row.from_name {
        Some(name) => format!("{name} {}", row.from_addr),
        None => row.from_addr.clone(),
    };

    stats.text_bytes += text.len() as u64;
    // **L'ancien document de même identifiant est retiré d'abord.** Sans ça, réindexer un
    // message en ajouterait un second exemplaire : la recherche rendrait deux fois la même
    // ligne, et c'est le genre de défaut qu'on met des semaines à rattacher à sa cause.
    //
    // C'est aussi ce qui rend l'ordre de la passe incrémentale sûr — valider l'index, puis
    // marquer : une coupure entre les deux fait réindexer, et réindexer ne fait rien de plus.
    // Sans objet lors d'une reconstruction, où l'index vient d'être vidé.
    writer.delete_term(tantivy::Term::from_field_i64(fields.id, row.id.0));
    writer.add_document(doc!(
        fields.id => row.id.0,
        fields.subject => row.subject.as_str(),
        fields.body => text.as_str(),
        fields.from => sender,
        fields.to => recipients,
        fields.folder => row.folders.as_str(),
        fields.date => row.date,
    ))?;
    stats.indexed += 1;
    Ok(())
}

/// Aplatit le corps du message en texte indexable.
///
/// Les parties texte sont prises telles quelles ; les parties HTML passent par
/// [`mailhtml::text`], qui retire le balisage, les URL et le contenu des `<script>`.
///
/// Les deux sont concaténées quand elles coexistent. Un `multipart/alternative` porte le
/// même contenu deux fois, donc on indexe deux fois les mêmes mots — sans conséquence pour
/// un moteur de recherche, et bien moins risqué que de choisir la « bonne » partie et de
/// rater le seul endroit où un mot apparaît.
fn body_text(parsed: &mail_parser::Message<'_>, out: &mut String) {
    for index in 0..parsed.text_body_count() {
        if let Some(part) = parsed.body_text(index) {
            if !out.is_empty() {
                out.push(' ');
            }
            out.push_str(&part);
        }
    }
    for index in 0..parsed.html_body_count() {
        if let Some(part) = parsed.body_html(index) {
            mailhtml::text::html_to_text(&part, out);
        }
    }
}

/// Ajoute les adresses et les noms d'un en-tête à `out`.
fn addresses(header: Option<&mail_parser::Address<'_>>, out: &mut String) {
    let Some(header) = header else { return };
    for addr in header.iter() {
        for value in [addr.name(), addr.address()].into_iter().flatten() {
            if !out.is_empty() {
                out.push(' ');
            }
            out.push_str(value);
        }
    }
}

/// L'identifiant d'un message tel que l'index le rend.
#[must_use]
pub const fn message_id(raw: i64) -> MessageId {
    MessageId(raw)
}
