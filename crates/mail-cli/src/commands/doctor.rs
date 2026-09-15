//! `mail doctor` — diagnostic du store.
//!
//! Un store qui ment silencieusement est pire qu'un store cassé. Cette commande cherche les
//! incohérences qu'aucune contrainte SQL ne peut attraper, parce qu'elles vivent **entre**
//! les trois moteurs : le système de fichiers, SQLite et tantivy.

use anyhow::{Context, Result};
use camino::Utf8PathBuf;
use mailcore::Store;
use mailcore::index::{Searcher, open_or_create};

use mailapi::human;

/// Point d'entrée de la sous-commande.
///
/// # Errors
///
/// Si le store est introuvable ou illisible. Une incohérence trouvée n'est pas une erreur :
/// elle est rapportée, et le code de sortie reste zéro. Diagnostiquer n'est pas échouer.
pub fn run(store_root: Option<&Utf8PathBuf>, purge_orphans: bool) -> Result<()> {
    let root = crate::store_root(store_root)?;
    let store = Store::open(&root).with_context(|| format!("ouverture du store {root}"))?;
    let stats = store.stats().context("statistiques")?;

    println!("Store              {root}");
    println!("Contenus distincts {}", stats.messages);
    println!("Références         {}", stats.refs);
    println!("Octets RFC 5322    {}", human::bytes(stats.raw_bytes));

    let mut findings = Vec::new();

    // 1. Une référence vers un message absent. La clé étrangère l'interdit, mais un store
    //    ouvert par une version antérieure sans `foreign_keys` aurait pu en créer.
    let dangling = store.dangling_refs().context("références orphelines")?;
    if dangling > 0 {
        findings.push(format!("{dangling} références vers un message inexistant"));
    }

    // 2. Un message dont le blob a disparu. Personne ne l'interdit : le blob vit sur le
    //    système de fichiers, hors de portée de SQLite.
    let missing = store.missing_blobs().context("blobs manquants")?;
    if !missing.is_empty() {
        findings.push(format!(
            "{} messages dont le blob est absent (premier : {})",
            missing.len(),
            missing[0].0
        ));
    }

    // 3. Le symétrique du précédent : un blob que plus aucun message ne désigne. Un lot de
    //    moisson coupé entre l'écriture des blobs et celle des lignes en laisse un, et rien ne
    //    peut l'empêcher — un `ROLLBACK` n'efface pas un fichier.
    //
    //    **Ce n'est pas une perte de courrier, c'est de la place**, et c'est pour ça que le
    //    diagnostic le dit sans le proposer à la suppression : effacer demande de savoir
    //    qu'aucun import en cours ne va le réclamer.
    let orphans = store.orphan_blobs().context("blobs non référencés")?;
    if orphans > 0 && !purge_orphans {
        findings.push(format!(
            "{orphans} blobs que plus aucun message ne désigne — de la place, pas du courrier \
             perdu. `--purge-orphans` les supprime, **quand rien n'écrit**"
        ));
    }
    if purge_orphans {
        // **Le drapeau est explicite exprès.** Un import ou une moisson écrit ses blobs avant
        // les lignes qui les désignent, donc un blob fraîchement écrit ressemble à un orphelin.
        // Purger pendant un import supprimerait du courrier en cours d'arrivée, et c'est pour
        // ça que ce n'est ni automatique ni le défaut du diagnostic.
        let report = store.purge_orphan_blobs().context("purge des orphelins")?;
        println!();
        println!(
            "Purge              {} blobs supprimés, {} rendus",
            report.removed,
            human::bytes(report.freed)
        );
        if report.failed > 0 {
            findings.push(format!(
                "{} blobs orphelins n'ont pas pu être supprimés — voir le journal",
                report.failed
            ));
        }
    }

    // 4. La dérive entre SQLite et tantivy. C'est l'incohérence la plus probable des trois :
    //    l'index est reconstruit à part, et une indexation interrompue la produit.
    match open_or_create(&store).and_then(|(index, _)| Searcher::open(&index)) {
        Ok(searcher) => {
            let indexed = searcher.document_count();
            println!("Documents indexés  {indexed}");
            if indexed != stats.messages {
                findings.push(format!(
                    "index désynchronisé : {indexed} documents pour {} messages — `mail index`",
                    stats.messages
                ));
            }
        }
        Err(source) => findings.push(format!("index plein texte illisible : {source}")),
    }

    if stats.unthreaded > 0 {
        findings.push(format!(
            "{} messages sans fil — la passe de threading n'est pas passée",
            stats.unthreaded
        ));
    }

    // 5. La file d'envoi. **Le seul point de ce diagnostic qui demande une décision humaine**,
    //    et non une commande à relancer : un message douteux a peut-être été remis, et rien ne
    //    lèvera le doute. Voir `docs/PHASE-3.md`, critère 2.
    let outbox = store.outbox().context("file d'envoi")?;
    if !outbox.is_empty() {
        let count =
            |wanted: mailcore::SendState| outbox.iter().filter(|it| it.state == wanted).count();
        println!(
            "File d'envoi       {} en attente, {} envoyés, {} échoués",
            count(mailcore::SendState::Queued) + count(mailcore::SendState::Sending),
            count(mailcore::SendState::Sent),
            count(mailcore::SendState::Failed),
        );
    }
    let doubtful = store.doubtful().context("envois douteux")?;
    if !doubtful.is_empty() {
        findings.push(format!(
            "{} message(s) dont personne ne sait s'ils sont partis — coupés entre le point \
             final et la réponse du serveur. **Aucune reprise automatique** : les renvoyer \
             risque un doublon chez le destinataire, les abandonner risque une perte. \
             Premier : #{}",
            doubtful.len(),
            doubtful[0].id.0
        ));
    }

    println!();
    if findings.is_empty() {
        println!("Aucune incohérence.");
        return Ok(());
    }
    println!("{} points à regarder :", findings.len());
    for finding in &findings {
        println!("  - {finding}");
    }
    Ok(())
}
