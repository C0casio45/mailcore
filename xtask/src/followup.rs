//! Ce que coûtent les passes qui suivent une moisson.
//!
//! ## Pourquoi cette mesure existe
//!
//! Le 2026-09-11, l'index plein texte et le carnet d'adresses sont devenus **incrémentaux**, et
//! le démon les lance après chaque moisson. L'argument était : « une passe ne coûte rien quand
//! rien n'est arrivé, donc on peut la lancer à chaque `IDLE` ». C'est un argument, pas un
//! chiffre — et `IDLE` met un job en file à **chaque** arrivée de courrier, donc l'argument doit
//! devenir un chiffre avant qu'on s'y fie.
//!
//! La question, précisément : **combien coûte une passe qui n'a rien à faire, et une passe qui a
//! un message à traiter, comparées à la reconstruction complète qu'elles remplacent ?**
//!
//! ## Elle crée son propre store, et n'accepte aucun chemin
//!
//! Pas de `--store`. C'est la leçon du 2026-09-09 : `measure-attachment` prenait un `--store`,
//! elle a été visée sur les données de production, et elle y a laissé 533 Mo. **Un paramètre qui
//! peut désigner le store réel le désignera.** Ce banc écrit, donc il fabrique son propre
//! répertoire jetable et l'efface en partant.
//!
//! ## Le corpus est synthétique, et c'est dit
//!
//! Le corpus réel n'est plus sur cette machine depuis le 2026-09-10. Les messages sont donc
//! fabriqués : en-têtes plausibles, corps court, adresses tirées d'un petit ensemble pour que le
//! carnet ait quelque chose à classer. Ce que ça mesure fidèlement, c'est le **rapport** entre
//! les trois régimes — et c'est la question posée. Ce que ça ne mesure pas : le coût absolu sur
//! des messages réels, qui portent du HTML, des pièces jointes et des en-têtes à rallonge.

use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use camino::Utf8PathBuf;
use mailcore::{Progress, Store};

/// Combien d'expéditeurs différents. Assez pour que le carnet ait un classement à faire.
const SENDERS: usize = 200;

/// Point d'entrée de la sous-commande.
///
/// # Errors
///
/// Si le store jetable ne peut pas être créé, ou si une passe échoue.
pub fn measure(messages: usize) -> Result<()> {
    let dir = tempfile::tempdir().context("répertoire jetable")?;
    let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf())
        .map_err(|path| anyhow::anyhow!("chemin non UTF-8 : {}", path.display()))?;

    println!("Banc des passes de suivi");
    println!("  corpus synthétique : {messages} messages, {SENDERS} expéditeurs");
    println!("  store jetable      : créé et effacé par ce banc\n");

    let store = Store::open(&root).context("ouverture du store jetable")?;
    let folder = seed(&store, messages)?;

    // --- Régime 1 : la reconstruction complète, celle qu'on remplace. ---
    let index_full = timed(|| {
        mailcore::index::rebuild(&store, &Progress::new())
            .map(|it| it.indexed)
            .context("reconstruction de l'index")
    })?;
    let book_full = timed(|| {
        mailcore::contacts::rebuild(&store, &Progress::new())
            .map(|it| it.scanned)
            .context("reconstruction du carnet")
    })?;
    let thread_full = timed(|| {
        mailcore::thread::rebuild(&store, &Progress::new())
            .map(|it| it.messages)
            .context("reconstruction des fils")
    })?;

    // --- Régime 2 : la passe qui n'a rien à faire. Le cas de très loin le plus fréquent. ---
    let index_idle = timed(|| {
        mailcore::index::update(&store, &Progress::new())
            .map(|it| it.indexed)
            .context("passe d'index à vide")
    })?;
    let book_idle = timed(|| {
        mailcore::contacts::update(&store, &Progress::new())
            .map(|it| it.scanned)
            .context("passe de carnet à vide")
    })?;
    let thread_idle = timed(|| {
        mailcore::thread::update(&store, &Progress::new())
            .map(|it| it.messages)
            .context("passe de fils à vide")
    })?;

    // --- Régime 3 : un message arrive, comme une moisson l'apporterait. ---
    add_one(&store, folder, messages)?;
    let index_one = timed(|| {
        mailcore::index::update(&store, &Progress::new())
            .map(|it| it.indexed)
            .context("passe d'index sur un message")
    })?;
    let book_one = timed(|| {
        mailcore::contacts::update(&store, &Progress::new())
            .map(|it| it.scanned)
            .context("passe de carnet sur un message")
    })?;
    // Les fils en dernier : cette passe est la seule des trois qui **écrit** sur des messages
    // déjà rangés, et la mesurer avant fausserait ce que les deux autres ont à faire.
    let thread_one = timed(|| {
        mailcore::thread::update(&store, &Progress::new())
            .map(|it| it.messages)
            .context("passe de fils sur un message")
    })?;

    report(
        messages,
        &[
            ("index", index_full, index_idle, index_one),
            ("carnet", book_full, book_idle, book_one),
            ("fils", thread_full, thread_idle, thread_one),
        ],
    );
    Ok(())
}

/// Une mesure : ce que la passe a traité, et ce qu'elle a mis.
#[derive(Debug, Clone, Copy)]
struct Run {
    treated: u64,
    elapsed: Duration,
}

/// Chronomètre une passe.
fn timed(work: impl FnOnce() -> Result<u64>) -> Result<Run> {
    let at = Instant::now();
    let treated = work()?;
    Ok(Run {
        treated,
        elapsed: at.elapsed(),
    })
}

/// Écrit le relevé.
fn report(messages: usize, rows: &[(&str, Run, Run, Run)]) {
    println!(
        "{:<8} {:>12} {:>12} {:>14} {:>12}",
        "passe", "complète", "à vide", "un message", "rapport"
    );
    for (name, full, idle, one) in rows {
        // Le rapport qui décide : combien de fois moins cher est la passe qui suit une moisson
        // ordinaire, comparée à la reconstruction qu'elle remplace. C'est ce nombre qui dit si
        // lancer la passe après chaque `IDLE` est tenable.
        let ratio = full.elapsed.as_secs_f64() / one.elapsed.as_secs_f64().max(f64::EPSILON);
        println!(
            "{name:<8} {:>12} {:>12} {:>14} {:>11.0}×",
            format!("{:.1?}", full.elapsed),
            format!("{:.1?}", idle.elapsed),
            format!("{:.1?}", one.elapsed),
            ratio
        );
    }

    println!();
    for (name, full, idle, one) in rows {
        println!(
            "{name} : {} traités en complet, {} à vide, {} sur l'arrivée d'un message",
            full.treated, idle.treated, one.treated
        );
    }

    println!();
    println!(
        "Lecture : la colonne « à vide » est ce que coûte un `IDLE` qui n'apporte rien, et la\n\
         colonne « un message » ce que coûte une arrivée ordinaire. Les deux doivent rester\n\
         sans commune mesure avec la colonne « complète », qui est ce qui se serait produit à\n\
         chaque message si la passe n'était pas devenue incrémentale — sur {messages} messages\n\
         ici, et sur 73 825 dans la vraie vie."
    );
}

/// Remplit le store de messages fabriqués, et rend le dossier qui les porte.
fn seed(store: &Store, messages: usize) -> Result<mailcore::FolderId> {
    use mailcore::store::write::NewMessage;
    use mailcore::{AuthKind, FolderKind, MessageFlags, Security, Server};

    let writer = store.writer()?;
    let account = writer.upsert_imap_account(
        "moi@exemple.invalid",
        &Server {
            host: "imap.exemple.invalid".to_owned(),
            port: 993,
            username: "moi@exemple.invalid".to_owned(),
            auth: AuthKind::Password,
            security: Security::Tls,
        },
    )?;
    let folder = writer.upsert_folder(account, "INBOX", FolderKind::Inbox)?;
    writer.commit()?;

    let at = Instant::now();
    // Une transaction par millier : le banc mesure les passes, pas son propre remplissage, mais
    // un `fsync` par message ferait passer la préparation avant la mesure en durée.
    for index in 0..messages {
        let writer = store.writer()?;
        let raw = synthetic(index);
        let blob = store.blobs().put(raw.as_bytes())?.hash;
        let date = 1_700_000_000 + index as i64;
        let (id, _) = writer.insert_message(&NewMessage {
            blob,
            rfc822_id: Some(&rfc822_id(index)),
            date,
            from_addr: &sender(index),
            from_name: None,
            subject: &subject(index),
            size: raw.len() as u64,
            has_attachments: false,
        })?;
        writer.insert_ref(id, folder, date, MessageFlags::empty())?;
        writer.commit()?;
    }
    println!("Corpus préparé en {:.1?}\n", at.elapsed());
    Ok(folder)
}

/// Ajoute un message de plus, comme une moisson l'apporterait.
///
/// C'est **une réponse** à la dernière conversation, et pas un message isolé : c'est le cas qui
/// coûte quelque chose aux fils, puisqu'il faut retrouver le voisinage et réécrire un fil
/// existant. Un message sans parent ne mesurerait que le chemin facile.
fn add_one(store: &Store, folder: mailcore::FolderId, index: usize) -> Result<()> {
    use mailcore::MessageFlags;
    use mailcore::store::write::NewMessage;

    let writer = store.writer()?;
    let raw = synthetic(index);
    let blob = store.blobs().put(raw.as_bytes())?.hash;
    let date = 1_700_000_000 + index as i64;
    let (id, _) = writer.insert_message(&NewMessage {
        blob,
        rfc822_id: Some(&rfc822_id(index)),
        date,
        from_addr: &sender(index),
        from_name: None,
        subject: &subject(index),
        size: raw.len() as u64,
        has_attachments: false,
    })?;
    writer.insert_ref(id, folder, date, MessageFlags::empty())?;
    writer.commit()?;
    Ok(())
}

/// L'expéditeur d'un message, pris dans un petit ensemble.
fn sender(index: usize) -> String {
    format!("personne{}@exemple.invalid", index % SENDERS)
}

/// Combien de messages par conversation.
///
/// Trois, parce que le corpus réel de la phase 1 donne 87,5 % de fils d'un seul message et des
/// conversations courtes pour le reste. Un fil de trois est ce qu'une réponse rejoint.
const PER_THREAD: usize = 3;

/// L'identifiant RFC 5322 d'un message.
fn rfc822_id(index: usize) -> String {
    format!("{index}@exemple.invalid")
}

/// Le sujet d'un message : celui de sa conversation.
fn subject(index: usize) -> String {
    format!("Conversation numero {}", index / PER_THREAD)
}

/// Le premier message de la conversation à laquelle celui-ci appartient.
fn thread_root(index: usize) -> usize {
    (index / PER_THREAD) * PER_THREAD
}

/// Un message RFC 5322 plausible : expéditeur, sujet de conversation, et sa place dans le fil.
///
/// Les messages qui ne sont pas en tête de conversation citent leur parent. Sans ça, la passe de
/// fils ne mesurerait que son chemin vide : rien à résoudre, aucun fil à réécrire.
fn synthetic(index: usize) -> String {
    let from = sender(index);
    let to = sender(index + 1);
    let root = thread_root(index);
    let references = if root == index {
        String::new()
    } else {
        format!(
            "In-Reply-To: <{parent}>\r\nReferences: <{parent}>\r\n",
            parent = rfc822_id(root)
        )
    };
    format!(
        "From: Personne {n} <{from}>\r\n\
         To: moi@exemple.invalid\r\n\
         Cc: {to}\r\n\
         Subject: {subject}\r\n\
         Message-ID: <{id}>\r\n\
         {references}\
         Date: Thu, 11 Sep 2026 10:00:00 +0200\r\n\
         MIME-Version: 1.0\r\n\
         Content-Type: text/plain; charset=utf-8\r\n\
         \r\n\
         Bonjour,\r\n\
         Ceci est un message de mesure, numéro {n}. Il parle de facture, de devis et de\r\n\
         rendez-vous pour que l'index ait des mots à ranger.\r\n\
         Cordialement.\r\n",
        n = index,
        id = rfc822_id(index),
        subject = subject(index)
    )
}
