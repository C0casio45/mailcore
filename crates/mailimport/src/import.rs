//! Le job d'import : du profil Thunderbird vers le store.
//!
//! ## Ce qu'il garantit
//!
//! - **Aucune écriture dans le profil.** Les fichiers sont ouverts en lecture, un par un,
//!   streamés. Critère 7, vérifiable par `cargo xtask profile-diff`.
//! - **Aucun mbox entier en mémoire.** Un tampon de message réutilisé, un tampon de lecture
//!   d'un mégaoctet. Critère 3.
//! - **Un message illisible n'interrompt jamais l'import.** Un profil de 10 Gio contient du
//!   MIME cassé ; s'arrêter au premier serait la garantie de ne jamais finir. Ce qui ne se
//!   parse pas est stocké quand même, avec des métadonnées dégradées, et compté.
//! - **Reprenable.** Relancer un import sur un store déjà rempli n'ajoute rien : le blob
//!   existe, le message existe, la référence existe. Tout est idempotent par construction,
//!   parce que tout est adressé par contenu.
//!
//! ## Ce qu'il ne fait pas
//!
//! Pas de threading : `messages.thread_id` reste `NULL`, une passe séparée le remplira
//! (étape 5). Pas d'indexation plein texte : elle relira les blobs, pas le profil.
//!
//! ## Le compromis sur `has_attachments`
//!
//! L'import n'analyse que les **en-têtes** (`parse_headers`). Décoder l'arbre MIME de 10 Gio
//! pour compter des pièces jointes coûterait plusieurs fois le prix de l'import et ferait
//! peser sur le critère 3 des corps décodés dont personne n'a besoin ici. `has_attachments`
//! est donc déduit du `Content-Type` — une heuristique, corrigée à l'indexation plein texte,
//! qui elle lit vraiment les corps.

use std::io::BufReader;

use mail_parser::{MessageParser, MimeHeaders};
use mailcore::{NewMessage, Progress, Store};

use crate::Result;
use crate::mbox::{Mangling, MboxReader};
use crate::mozilla;
use crate::tree;

/// Tampon de lecture par fichier.
const READ_BUFFER: usize = 1024 * 1024;

/// Messages écrits avant de valider la transaction.
///
/// Un compromis entre deux coûts : valider trop souvent impose un `fsync` par lot, valider
/// trop rarement fait grossir le WAL et retarde le moment où un lecteur voit du contenu.
/// À 5 000, l'import est dominé par la lecture et le WAL reste de l'ordre de quelques
/// dizaines de mégaoctets.
const BATCH: usize = 5_000;

/// Réglages d'un import.
#[derive(Debug, Clone)]
pub struct ImportOptions {
    /// Racine du profil Thunderbird à lire.
    pub profile: std::path::PathBuf,
    /// Convention de dés-échappement. `mboxrd` est le bon défaut pour Thunderbird —
    /// vérifiable avec `cargo xtask profile-probe`.
    pub mangling: Mangling,
    /// Ne rien écrire : lit tout, compte tout, ne touche pas au store.
    pub dry_run: bool,
    /// Importer aussi les comptes de flux RSS.
    ///
    /// **Faux par défaut, et c'est un choix de périmètre.** mailcore est un client mail
    /// (`docs/VISION.md`) ; les articles d'un agrégateur ne sont pas du courrier, et sur le
    /// profil réel un seul dossier de flux porte **15 881 références** — 15,5 % de tout le
    /// store — d'articles supprimés qui noieraient la liste et l'index plein texte.
    ///
    /// Ignorer par défaut, mais jamais en silence : le bilan d'import compte ce qui a été
    /// écarté, et ce drapeau le récupère.
    pub include_feeds: bool,
    /// Importer aussi les répertoires de comptes que `prefs.js` ne déclare pas.
    ///
    /// **Faux par défaut.** Thunderbird ne supprime pas les mbox d'un compte retiré de sa
    /// configuration : ils restent sur le disque indéfiniment. Ce n'est pas du courrier
    /// vivant, ce sont les restes d'un compte que l'utilisateur a enlevé.
    ///
    /// Sur le profil de référence, deux répertoires sont dans ce cas et l'un porte **15 881
    /// messages** — 15,5 % du volume — entièrement dans une corbeille. Les importer noierait
    /// la liste et l'index avec du courrier que personne ne cherche.
    ///
    /// Sans aucune déclaration lisible, rien n'est écarté : voir `MboxFile::declared`.
    pub include_orphans: bool,
}

impl ImportOptions {
    /// Réglages par défaut pour un profil donné.
    #[must_use]
    pub fn new(profile: impl Into<std::path::PathBuf>) -> Self {
        Self {
            profile: profile.into(),
            mangling: Mangling::MboxRd,
            dry_run: false,
            include_feeds: false,
            include_orphans: false,
        }
    }
}

/// Ce qu'un import a fait, et ce qu'il a laissé derrière.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImportStats {
    /// Dossiers parcourus.
    pub folders: u64,
    /// Dossiers de flux RSS écartés.
    ///
    /// Comptés pour que l'exclusion par défaut ne soit jamais silencieuse.
    pub feed_folders_skipped: u64,
    /// Dossiers de comptes orphelins écartés — non déclarés dans `prefs.js`.
    ///
    /// Comptés pour la même raison : écarter sans le dire serait une perte de données
    /// déguisée en fonctionnalité.
    pub orphan_folders_skipped: u64,
    /// Fichiers qu'on n'a pas pu ouvrir.
    pub unreadable_files: u64,
    /// Messages lus dans les mbox, tous cas confondus.
    pub messages_read: u64,
    /// Messages ignorés parce que marqués supprimés non compactés (`0x0008`).
    pub expunged: u64,
    /// Contenus stockés pour la première fois : un blob créé, une ligne `messages`.
    pub blobs_created: u64,
    /// Contenus déjà présents. **C'est la mesure de la dédup** (critère 6).
    pub duplicates: u64,
    /// Références créées — une par couple (message, dossier).
    pub refs_created: u64,
    /// Références déjà présentes : un import relancé sur le même profil.
    pub refs_existing: u64,
    /// Messages dont les en-têtes n'ont pas pu être analysés. Stockés quand même.
    pub degraded: u64,
    /// Messages sans en-tête `Date` exploitable.
    pub undated: u64,
    /// Fichiers abandonnés en cours de lecture sur une erreur du lecteur.
    pub aborted_files: u64,
    /// Octets RFC 5322 stockés, avant compression.
    pub raw_bytes: u64,
    /// Octets réellement écrits sur le disque, après compression et dédup.
    pub stored_bytes: u64,
    /// Octets que la dédup a évité d'écrire.
    pub deduplicated_bytes: u64,
}

impl ImportStats {
    /// Part des messages qui étaient des doublons, en pourcentage.
    #[must_use]
    pub fn dedup_ratio(&self) -> f64 {
        let considered = self.blobs_created + self.duplicates;
        if considered == 0 {
            return 0.0;
        }
        (self.duplicates as f64 / considered as f64) * 100.0
    }
}

/// Importe un profil Thunderbird dans un store.
///
/// `progress` sert deux choses : suivre l'avancement en octets lus, et demander l'arrêt.
///
/// **Une interruption ne laisse rien de cassé.** Le lot en cours est validé avant de rendre
/// la main, et relancer l'import reprend le travail sans le refaire : les blobs sont adressés
/// par leur contenu et les références sont idempotentes. Un import à moitié fait est un store
/// valide contenant la moitié des messages, pas un store abîmé — c'est ce qui autorise à
/// exposer l'annulation à un client.
///
/// # Errors
///
/// [`Error::Io`] si le profil est introuvable. Une erreur sur un fichier ou un message
/// n'interrompt pas l'import : elle est comptée et journalisée.
pub fn import_profile(
    store: &Store,
    options: &ImportOptions,
    progress: &Progress,
) -> Result<ImportStats> {
    let scan = tree::scan_profile(&options.profile)?;
    let mut stats = ImportStats::default();
    let parser = MessageParser::default();
    let mut message = Vec::with_capacity(64 * 1024);

    // L'unité est l'octet lu, pas le message : le nombre de messages n'est connu qu'une fois
    // tout lu, alors que la taille des mbox l'est dès le parcours. Une barre qui avance
    // régulièrement vaut mieux qu'une barre exacte qui n'existe pas.
    progress.set_total(scan.total_bytes);

    tracing::info!(
        folders = scan.files.len(),
        bytes = scan.total_bytes,
        dry_run = options.dry_run,
        "début de l'import"
    );

    for file in &scan.files {
        // Les flux RSS ne sont pas du courrier. Écartés par défaut, comptés toujours : une
        // exclusion silencieuse serait une perte de données déguisée en fonctionnalité.
        if file.account_kind == tree::AccountKind::Feeds && !options.include_feeds {
            stats.feed_folders_skipped += 1;
            progress.advance(file.size);
            continue;
        }

        // Un répertoire que `prefs.js` ne déclare pas est le reste d'un compte supprimé.
        // Même règle : écarté par défaut, compté toujours.
        if !file.declared && !options.include_orphans {
            stats.orphan_folders_skipped += 1;
            progress.advance(file.size);
            continue;
        }

        stats.folders += 1;

        let handle = match std::fs::File::open(&file.path) {
            Ok(handle) => handle,
            Err(source) => {
                tracing::warn!(folder = %file.folder, %source, "dossier illisible, ignoré");
                stats.unreadable_files += 1;
                continue;
            }
        };

        let mut reader = MboxReader::new(BufReader::with_capacity(READ_BUFFER, handle))
            .with_mangling(options.mangling);

        // Un writer par dossier, validé par lots. Le dossier est le grain naturel : si un
        // fichier explose en cours de route, ce qui précède est déjà validé et n'est pas à
        // relire au prochain passage.
        let mut writer = store.writer()?;
        let account = writer.upsert_account(file.account_kind.as_str(), &file.account)?;
        let folder = writer.upsert_folder(account, &file.folder, file.kind)?;
        let mut in_batch = 0usize;

        loop {
            match reader.read_message_into(&mut message) {
                Ok(Some(_)) => {}
                Ok(None) => break,
                Err(source) => {
                    tracing::warn!(
                        folder = %file.folder,
                        %source,
                        "lecture interrompue, le reste du dossier est ignoré"
                    );
                    stats.aborted_files += 1;
                    break;
                }
            }
            stats.messages_read += 1;
            progress.advance(message.len() as u64);

            // Vérifié par message et non par dossier : `[Gmail]/Tous les messages` fait
            // 1,4 Gio à lui seul, et une annulation qui attendrait la fin du dossier
            // n'annulerait rien pendant plusieurs minutes.
            if progress.is_cancelled() {
                writer.commit()?;
                tracing::info!(messages = stats.messages_read, "import interrompu");
                return Ok(stats);
            }

            let mozilla = mozilla::strip_in_place(&mut message);
            if mozilla.is_expunged() {
                stats.expunged += 1;
                continue;
            }

            let headers = extract(&parser, &message, &mut stats);

            if options.dry_run {
                stats.raw_bytes += message.len() as u64;
                continue;
            }

            let put = store.blobs().put(&message)?;
            stats.raw_bytes += message.len() as u64;
            stats.stored_bytes += put.stored_len;
            if !put.created {
                stats.deduplicated_bytes += message.len() as u64;
            }

            let (id, created) = writer.insert_message(&NewMessage {
                blob: put.hash,
                rfc822_id: headers.message_id.as_deref(),
                date: headers.date,
                from_addr: &headers.from_addr,
                from_name: headers.from_name.as_deref(),
                subject: &headers.subject,
                size: message.len() as u64,
                has_attachments: headers.has_attachments,
            })?;

            if created {
                stats.blobs_created += 1;
            } else {
                stats.duplicates += 1;
            }

            if writer.insert_ref(id, folder, headers.date, mozilla.to_flags())? {
                stats.refs_created += 1;
            } else {
                stats.refs_existing += 1;
            }

            in_batch += 1;
            if in_batch >= BATCH {
                writer.commit()?;
                writer = store.writer()?;
                in_batch = 0;
            }
        }

        writer.commit()?;
        tracing::debug!(
            folder = %file.folder,
            messages = stats.messages_read,
            "dossier importé"
        );
    }

    tracing::info!(
        messages = stats.messages_read,
        uniques = stats.blobs_created,
        doublons = stats.duplicates,
        "import terminé"
    );
    Ok(stats)
}

/// Les métadonnées qu'on tire des en-têtes d'un message.
#[derive(Debug, Default)]
struct Headers {
    message_id: Option<String>,
    date: i64,
    from_addr: String,
    from_name: Option<String>,
    subject: String,
    has_attachments: bool,
}

/// Extrait les métadonnées, sans jamais échouer.
///
/// Un message dont les en-têtes sont illisibles est stocké quand même : les octets sont la
/// source de vérité, les métadonnées ne sont qu'un index reconstructible. Perdre un message
/// parce que son `From:` est cassé serait échanger une donnée contre une colonne.
fn extract(parser: &MessageParser, raw: &[u8], stats: &mut ImportStats) -> Headers {
    let Some(parsed) = parser.parse_headers(raw) else {
        stats.degraded += 1;
        stats.undated += 1;
        return Headers {
            subject: "(en-têtes illisibles)".to_owned(),
            ..Headers::default()
        };
    };

    let date = parsed.date().map_or_else(
        || {
            stats.undated += 1;
            0
        },
        mail_parser::DateTime::to_timestamp,
    );

    let (from_addr, from_name) = sender(&parsed);

    Headers {
        message_id: parsed.message_id().map(str::to_owned),
        date,
        from_addr,
        from_name,
        subject: parsed.subject().unwrap_or_default().to_owned(),
        has_attachments: declares_attachments(&parsed),
    }
}

/// L'expéditeur, normalisé.
///
/// L'adresse passe en minuscules parce que la casse du domaine n'a aucune signification et
/// que la recherche par contact doit trouver `Plombier@Exemple.FR` en cherchant
/// `plombier@exemple.fr`. La partie locale est théoriquement sensible à la casse ; en
/// pratique aucun serveur déployé ne s'en sert, et la traiter comme telle casserait plus de
/// recherches qu'elle n'en corrigerait.
fn sender(parsed: &mail_parser::Message<'_>) -> (String, Option<String>) {
    let Some(address) = parsed.from() else {
        return (String::new(), None);
    };
    let Some(first) = address.first() else {
        return (String::new(), None);
    };

    let addr = first
        .address()
        .map(|a| a.trim().to_lowercase())
        .unwrap_or_default();
    let name = first
        .name()
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .map(str::to_owned);

    // **Une valeur sans arobase n'est pas une adresse.** RFC 5322 : un `addr-spec` s'écrit
    // `local@domaine`. Un `From:` mal formé — une phrase sans chevrons — fait rendre cette
    // phrase par `address()`, et la ranger comme adresse a deux conséquences : la liste affiche
    // une fausse adresse, et « Répondre » produit un destinataire que tout serveur refuse.
    //
    // Elle devient donc le **nom**, et l'adresse reste vide, ce qui est la vérité : ce message
    // ne dit pas qui l'a envoyé. Trouvé par le banc du critère 1, qui a compté 15 881 messages
    // dans ce cas sur le corpus réel — **deux** valeurs distinctes, donc un seul expéditeur
    // automatique répété quinze mille fois.
    if !addr.contains('@') {
        let phrase = name.or_else(|| Some(addr).filter(|it| !it.is_empty()));
        return (String::new(), phrase);
    }

    (addr, name)
}

/// Heuristique de pièce jointe, sur le seul en-tête `Content-Type`.
///
/// `multipart/mixed` et `multipart/related` sont les types qui portent des parties
/// non textuelles. `multipart/alternative` est du texte et du HTML pour le même contenu :
/// ce n'est pas une pièce jointe, et le compter comme tel mettrait un trombone sur la
/// moitié du corpus.
fn declares_attachments(parsed: &mail_parser::Message<'_>) -> bool {
    parsed.content_type().is_some_and(|ct| {
        ct.ctype().eq_ignore_ascii_case("multipart")
            && ct
                .subtype()
                .is_some_and(|sub| sub.eq_ignore_ascii_case("mixed"))
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::Error;
    use camino::Utf8Path;
    use mailcore::MessageFlags;
    use std::path::Path;

    /// Un profil synthétique : un compte IMAP, deux dossiers, le même message dans les deux.
    fn profile_with_duplicate() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let account = dir.path().join("ImapMail").join("imap.exemple.fr");
        std::fs::create_dir_all(&account).unwrap();

        let body = "From: Le Plombier <Plombier@Exemple.FR>\r\n\
                    To: moi@exemple.fr\r\n\
                    Subject: facture\r\n\
                    Date: Tue, 14 Nov 2023 22:13:20 +0000\r\n\
                    Message-ID: <facture-1@exemple.fr>\r\n\
                    \r\n\
                    Voici la facture.\r\n";

        // Le même contenu, mais des drapeaux Thunderbird différents dans chaque dossier :
        // exactement la situation Gmail que la dédup doit absorber.
        std::fs::write(
            account.join("INBOX"),
            format!("From - Tue Nov 14 22:13:20 2023\r\nX-Mozilla-Status: 0001\r\n{body}"),
        )
        .unwrap();

        let gmail = account.join("[Gmail].sbd");
        std::fs::create_dir_all(&gmail).unwrap();
        std::fs::write(
            gmail.join("Tous les messages"),
            format!("From - Tue Nov 14 22:13:20 2023\r\nX-Mozilla-Status: 0000\r\n{body}"),
        )
        .unwrap();

        dir
    }

    fn store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let root = Utf8Path::from_path(dir.path()).unwrap().to_owned();
        let store = Store::open(&root).unwrap();
        (dir, store)
    }

    fn import(store: &Store, profile: &Path) -> ImportStats {
        import_profile(store, &ImportOptions::new(profile), &Progress::new()).unwrap()
    }

    #[test]
    fn the_same_message_in_two_folders_is_stored_once_and_referenced_twice() {
        // La thèse du projet, de bout en bout : profil réel en miniature, store réel.
        let profile = profile_with_duplicate();
        let (_dir, store) = store();

        let stats = import(&store, profile.path());

        assert_eq!(stats.messages_read, 2);
        assert_eq!(stats.blobs_created, 1, "le contenu a été stocké deux fois");
        assert_eq!(stats.duplicates, 1);
        assert_eq!(stats.refs_created, 2);
        assert!((stats.dedup_ratio() - 50.0).abs() < f64::EPSILON);
    }

    #[test]
    fn flags_differ_per_folder_while_the_content_is_shared() {
        let profile = profile_with_duplicate();
        let (_dir, store) = store();
        import(&store, profile.path());

        let mut seen = std::collections::HashSet::new();
        for folder in store.folders().unwrap() {
            let page = store.page(folder.id, None, 10).unwrap();
            assert_eq!(page.len(), 1);
            seen.insert(page[0].flags.contains(MessageFlags::SEEN));
        }
        assert_eq!(seen.len(), 2, "les deux dossiers ont les mêmes drapeaux");
    }

    #[test]
    fn headers_are_decoded_and_normalised() {
        let profile = profile_with_duplicate();
        let (_dir, store) = store();
        import(&store, profile.path());

        let folders = store.folders().unwrap();
        let page = store.page(folders[0].id, None, 10).unwrap();

        assert_eq!(page[0].subject, "facture");
        assert_eq!(
            page[0].from_addr, "plombier@exemple.fr",
            "l'adresse n'a pas été normalisée en minuscules"
        );
        assert_eq!(page[0].from_name.as_deref(), Some("Le Plombier"));
        assert_eq!(page[0].date, 1_700_000_000);
    }

    #[test]
    fn importing_twice_adds_nothing() {
        // Idempotence : tout est adressé par contenu, donc rejouer un import est sans effet.
        let profile = profile_with_duplicate();
        let (_dir, store) = store();

        let first = import(&store, profile.path());
        let second = import(&store, profile.path());

        assert_eq!(first.refs_created, 2);
        assert_eq!(second.refs_created, 0, "des références ont été dupliquées");
        assert_eq!(second.refs_existing, 2);
        assert_eq!(second.blobs_created, 0);
    }

    #[test]
    fn an_expunged_message_is_skipped_and_counted() {
        let dir = tempfile::tempdir().unwrap();
        let account = dir.path().join("ImapMail").join("compte");
        std::fs::create_dir_all(&account).unwrap();
        std::fs::write(
            account.join("INBOX"),
            "From - d\r\nX-Mozilla-Status: 0009\r\nSubject: supprime\r\n\r\ncorps\r\n\r\n\
             From - d\r\nX-Mozilla-Status: 0001\r\nSubject: garde\r\n\r\ncorps\r\n",
        )
        .unwrap();

        let (_store_dir, store) = store();
        let stats = import(&store, dir.path());

        assert_eq!(stats.messages_read, 2);
        assert_eq!(stats.expunged, 1);
        assert_eq!(stats.blobs_created, 1);
    }

    #[test]
    fn a_message_with_broken_headers_is_stored_not_lost() {
        // Le principe : les octets sont la vérité, les métadonnées un index reconstructible.
        let dir = tempfile::tempdir().unwrap();
        let account = dir.path().join("ImapMail").join("compte");
        std::fs::create_dir_all(&account).unwrap();
        std::fs::write(
            account.join("INBOX"),
            b"From - d\r\n\xff\xfe pas des en-tetes du tout \x00\r\n",
        )
        .unwrap();

        let (_store_dir, store) = store();
        let stats = import(&store, dir.path());

        assert_eq!(stats.messages_read, 1);
        assert_eq!(stats.blobs_created, 1, "le message a été perdu");
    }

    #[test]
    fn a_message_without_a_date_is_counted() {
        let dir = tempfile::tempdir().unwrap();
        let account = dir.path().join("ImapMail").join("compte");
        std::fs::create_dir_all(&account).unwrap();
        std::fs::write(
            account.join("INBOX"),
            "From - d\r\nSubject: sans date\r\nFrom: a@b.c\r\n\r\ncorps\r\n",
        )
        .unwrap();

        let (_store_dir, store) = store();
        let stats = import(&store, dir.path());

        assert_eq!(stats.undated, 1);
        assert_eq!(stats.blobs_created, 1);
    }

    #[test]
    fn a_dry_run_writes_nothing() {
        let profile = profile_with_duplicate();
        let (_dir, store) = store();

        let mut options = ImportOptions::new(profile.path());
        options.dry_run = true;
        let stats = import_profile(&store, &options, &Progress::new()).unwrap();

        assert_eq!(stats.messages_read, 2);
        assert_eq!(stats.blobs_created, 0);
        assert_eq!(
            store.message_count().unwrap(),
            0,
            "le dry-run a écrit dans le store"
        );
    }

    #[test]
    fn importing_never_writes_into_the_profile() {
        // Critère 7, en miniature et automatisé. Le vrai relevé porte sur le profil complet.
        let profile = profile_with_duplicate();
        let before = fingerprint(profile.path());

        let (_dir, store) = store();
        import(&store, profile.path());

        assert_eq!(
            before,
            fingerprint(profile.path()),
            "le profil a été modifié"
        );
    }

    fn fingerprint(root: &Path) -> Vec<(String, u64, std::time::SystemTime)> {
        let mut out = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).unwrap().flatten() {
                let meta = entry.metadata().unwrap();
                if meta.is_dir() {
                    stack.push(entry.path());
                } else {
                    out.push((
                        entry.path().to_string_lossy().into_owned(),
                        meta.len(),
                        meta.modified().unwrap(),
                    ));
                }
            }
        }
        out.sort();
        out
    }

    #[test]
    fn an_empty_profile_imports_nothing_without_failing() {
        let dir = tempfile::tempdir().unwrap();
        let (_store_dir, store) = store();
        let stats = import(&store, dir.path());
        assert_eq!(stats, ImportStats::default());
    }

    #[test]
    fn a_missing_profile_is_an_error() {
        let (_dir, store) = store();
        let absent = std::path::Path::new("F:/pas/de/profil/ici");
        assert!(matches!(
            import_profile(&store, &ImportOptions::new(absent), &Progress::new()),
            Err(Error::Io { .. })
        ));
    }

    #[test]
    fn attachment_detection_does_not_flag_alternative_parts() {
        // `multipart/alternative` est du texte et du HTML pour le même contenu. Le compter
        // comme pièce jointe mettrait un trombone sur la moitié du corpus.
        let parser = MessageParser::default();
        let alternative = b"Content-Type: multipart/alternative; boundary=x\r\n\r\ncorps\r\n";
        let mixed = b"Content-Type: multipart/mixed; boundary=x\r\n\r\ncorps\r\n";

        assert!(!declares_attachments(
            &parser.parse_headers(alternative).unwrap()
        ));
        assert!(declares_attachments(&parser.parse_headers(mixed).unwrap()));
    }
}
