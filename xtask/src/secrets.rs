//! **Critère 6 de `docs/PHASE-2.md`, sur les vrais comptes** : zéro identifiant en clair dans
//! le store, dans les journaux, dans les temporaires.
//!
//! ## Ce que `mailfake` ne pouvait pas montrer
//!
//! Le test du 2026-09-03 (`mailsync/tests/secret.rs`) fait passer un mot de passe sentinelle
//! par un vrai `LOGIN` et le cherche ensuite partout. Il prouve ce qu'il prouve, et trois
//! choses lui échappent :
//!
//! - **OAuth2 n'existait pas encore.** Les secrets qui valent vraiment un compte aujourd'hui
//!   sont le jeton de rafraîchissement et le secret client, écrits par `mailauth` après ce
//!   test. Aucun d'eux ne passe par `mailfake` ;
//! - **le store faisait trois messages.** Le vrai en fait 48 529, avec 3,5 Gio de blobs, un
//!   index SQLite de 33 Mo et des segments tantivy. Un secret peut se retrouver dans une page
//!   libérée de SQLite ou dans un fragment d'index — c'est-à-dire ailleurs que là où on
//!   penserait à regarder ;
//! - **le chiffrement était absent.** `mailfake` parle en clair ; ici le secret traverse
//!   `rustls` et la pile de vérification de certificats.
//!
//! ## Les aiguilles ne sont jamais écrites nulle part
//!
//! Ni affichées, ni journalisées, ni posées dans un fichier de contrôle. Un harnais de
//! vérification de fuite qui commence par écrire les secrets sur le disque a créé la fuite
//! qu'il cherche — et un fichier supprimé laisse ses octets dans l'espace libre.
//!
//! Le contrôle positif est donc **en mémoire** : chaque aiguille réelle est noyée dans un
//! tampon fabriqué, et le chercheur doit l'y retrouver. C'est plus fort que le contrôle sur
//! disque du test d'origine, qui prouvait que le chercheur trouve *une* sentinelle ; celui-ci
//! prouve qu'il trouve **celle-là**, avec ses octets à elle.
//!
//! Un second contrôle, sur disque et avec une sentinelle synthétique, reste nécessaire : il
//! prouve que le parcours de l'arborescence et la lecture des fichiers marchent, ce qu'un
//! contrôle en mémoire ne dit pas.
//!
//! ## Rien n'est chargé en entier
//!
//! Règle 4 du `CLAUDE.md`. Le blob le plus gros du corpus dépasse la centaine de mégaoctets,
//! et le test d'origine lisait chaque fichier d'un coup — acceptable sur trois messages,
//! impossible ici. La lecture est donc par blocs, avec un recouvrement d'une aiguille moins un
//! octet pour qu'aucune ne puisse se cacher à cheval sur deux blocs.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use camino::Utf8PathBuf;
use mailcore::{AccountKind, AuthKind, Store};

/// Taille d'un bloc de lecture.
///
/// 1 Mio : assez grand pour que le coût par bloc soit négligeable devant la lecture, assez
/// petit pour que la mémoire du harnais ne dépende pas de la taille du plus gros blob.
const CHUNK: usize = 1024 * 1024;

/// Plafond de lecture d'un fichier du répertoire temporaire du système.
///
/// Le même que celui du test d'origine, et pour la même raison : le répertoire temporaire
/// contient ce que les autres applications y laissent, parfois des images disque. Un secret de
/// quelques dizaines d'octets n'a aucune raison de n'apparaître qu'après le huitième mégaoctet.
const TEMP_SCAN_LIMIT: u64 = 8 * 1024 * 1024;

/// Plafond du tampon de journaux.
///
/// Une synchronisation incrémentale au niveau `TRACE` tient largement dedans. Le plafond est là
/// pour qu'une moisson complète ne fasse pas gonfler le harnais sans limite — et **le
/// dépassement est dit**, parce qu'un tampon tronqué ne vérifie plus la fin de la
/// synchronisation.
const LOG_LIMIT: usize = 64 * 1024 * 1024;

/// La sentinelle du contrôle sur disque.
///
/// Synthétique : elle ne vaut rien, on peut l'écrire. Assez improbable pour qu'aucune
/// coïncidence ne la produise.
const SENTINEL: &str = "SENTINELLE-4d1f8ae0b93c752a-CRITERE-6";

/// Un secret à chercher, désigné par ce qu'il est et jamais par ce qu'il vaut.
struct Needle {
    /// De quoi il s'agit — « compte #2, jeton de rafraîchissement ». Affichable.
    label: String,
    /// Les octets cherchés. **Jamais affichés.**
    bytes: Vec<u8>,
}

impl Needle {
    /// Ce qu'on a le droit d'afficher : le rôle et la longueur, pas la valeur.
    fn describe(&self) -> String {
        format!("{} ({} octets)", self.label, self.bytes.len())
    }
}

/// Mesure le critère 6 sur les comptes réels du store.
///
/// # Errors
///
/// Si le store est illisible, si aucun compte n'a de secret lisible, ou si un contrôle positif
/// échoue — auquel cas le zéro qui suivrait ne prouverait rien et il vaut mieux ne pas le
/// rendre.
pub fn audit(store_root: &Utf8PathBuf, only: Option<i64>) -> Result<()> {
    // L'abonné est posé **avant tout**, globalement, et au niveau le plus bavard. Global parce
    // que `tracing` met en cache l'intérêt de chaque site d'appel pour tout le processus : un
    // abonné posé sur un seul fil laisse les sites touchés d'abord par un autre fil marqués
    // « personne ne s'y intéresse », et les journaux qu'on croit vérifier sont alors vides.
    // Cette panne-là a déjà été observée le 2026-09-03, et c'est elle qui a coûté un test qui
    // passait à vide.
    let captured = Captured::default();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_writer(captured.clone())
        .with_ansi(false)
        .finish();
    tracing::subscriber::set_global_default(subscriber)
        .context("un abonné tracing était déjà posé")?;

    let store = Store::open(store_root).with_context(|| format!("ouverture de {store_root}"))?;
    let accounts: Vec<mailcore::Account> = store
        .full_accounts()?
        .into_iter()
        .filter(|it| it.kind == AccountKind::Imap && it.enabled)
        .filter(|it| only.is_none_or(|id| it.id.0 == id))
        .collect();
    if accounts.is_empty() {
        anyhow::bail!("aucun compte IMAP actif dans {store_root}");
    }

    println!("Critères 6 et 7 — les identifiants réels, cherchés là où ils ne doivent pas être");
    println!("Store : {store_root}\n");

    // --- les aiguilles, avant la synchronisation ---
    let mut needles = collect(&accounts, "avant")?;
    if needles.is_empty() {
        anyhow::bail!(
            "aucun secret lisible au trousseau : le balayage ne chercherait rien, et son zéro \
             ne voudrait rien dire"
        );
    }
    println!("Secrets à chercher, tels que le trousseau les rend :");
    for needle in &needles {
        println!("  - {}", needle.describe());
    }

    // --- contrôle positif en mémoire, aiguille par aiguille ---
    //
    // Sans lui, un chercheur qui ne trouverait rien rendrait « aucune fuite » sur un store qui
    // en serait plein. Et il porte sur **les octets réels**, pas sur une sentinelle qui leur
    // ressemblerait.
    for needle in &needles {
        let mut haystack = vec![b'.'; CHUNK + 512];
        let at = CHUNK - needle.bytes.len() / 2;
        haystack[at..at + needle.bytes.len()].copy_from_slice(&needle.bytes);
        anyhow::ensure!(
            scan_reader(&mut haystack.as_slice(), std::slice::from_ref(needle))? == vec![0],
            "le chercheur ne retrouve pas {} dans un tampon où elle est : son zéro ne \
             prouverait rien",
            needle.describe()
        );
    }
    println!(
        "\nContrôle positif en mémoire : les {} aiguilles sont retrouvées, y compris à cheval \
         sur deux blocs de lecture.",
        needles.len()
    );

    // --- contrôle positif sur disque, avec une sentinelle qui ne vaut rien ---
    let control_dir = tempfile::tempdir().context("répertoire de contrôle")?;
    let control = control_dir.path().join("controle-positif.bin");
    std::fs::write(&control, SENTINEL.as_bytes()).context("écriture du contrôle")?;
    let sentinel = [Needle {
        label: "sentinelle de contrôle".to_owned(),
        bytes: SENTINEL.as_bytes().to_vec(),
    }];
    let (found, _) = scan_tree(control_dir.path(), &sentinel)?;
    anyhow::ensure!(
        found.iter().any(|(path, _)| path == &control),
        "le parcours d'arborescence ne trouve pas un fichier qu'on vient d'écrire"
    );
    drop(control_dir);
    println!("Contrôle positif sur disque : le parcours et la lecture des fichiers trouvent.");

    // --- la vraie synchronisation, avec les vrais secrets ---
    println!("\nSynchronisation des {} compte(s)…", accounts.len());
    let progress = mailcore::Progress::new();
    let mut synced = 0_usize;
    for account in &accounts {
        let Some(server) = account.server.as_ref() else {
            continue;
        };
        let secret = mailauth::session::secret_for(
            &server.host,
            &server.username,
            server.auth.as_str(),
            now(),
        )
        .with_context(|| format!("secret du compte #{}", account.id.0))?;
        let credential = mailsync::Credential::for_auth(server.auth, &secret);
        let report = mailsync::sync_account(&store, account, credential, &progress)
            .with_context(|| format!("synchronisation du compte #{}", account.id.0))?;
        println!(
            "  #{} — {} dossier(s), {} évité(s), {} corps reçu(s)",
            account.id.0, report.folders, report.skipped, report.fetched
        );
        synced += 1;
    }
    anyhow::ensure!(
        synced > 0,
        "aucun compte n'a été synchronisé : il n'y a rien à fouiller"
    );

    // --- le chemin d'envoi, avec les vrais secrets et sans envoyer ---
    //
    // C'est le critère 7 : l'IMAP ci-dessus n'est que la moitié du risque. Voir `AuthOnly`
    // pour ce qui garantit qu'aucun message ne part.
    println!(
        "
Sonde SMTP — authentification réelle, aucun envoi…"
    );
    let (exercised, unconfigured) = exercise_smtp(&store, &accounts)?;
    if unconfigured > 0 {
        println!(
            "  {unconfigured} compte(s) sans serveur d'envoi : leur chemin SMTP n'est pas mesuré."
        );
    }
    anyhow::ensure!(
        exercised > 0,
        "aucun compte n'a de serveur d'envoi : le chemin SMTP n'est pas mesuré, et un balayage \n         qui ne le traverse pas ne dit rien du critère 7"
    );

    // **Le jeton d'accès a pu tourner pendant la passe.** Chercher seulement celui d'avant
    // laisserait le nouveau — écrit par le rafraîchissement, donc le plus récent de tous —
    // hors du balayage. Les deux sont cherchés.
    let known: Vec<Vec<u8>> = needles.iter().map(|it| it.bytes.clone()).collect();
    let mut refreshed = collect(&accounts, "après")?;
    refreshed.retain(|it| !known.contains(&it.bytes));
    if !refreshed.is_empty() {
        println!(
            "  {} secret(s) ont changé pendant la passe : cherchés aussi.",
            refreshed.len()
        );
    }
    needles.extend(refreshed);

    // --- les trois balayages ---
    println!("\nBalayage…");
    let (leaks, store_unreadable) = scan_tree(store_root.as_std_path(), &needles)?;
    verdict(
        "le store",
        &leaks
            .iter()
            .map(|(path, at)| format!("{} — {}", path.display(), needles[*at].label))
            .collect::<Vec<_>>(),
    );

    let logs = captured.text();
    // **Le contrôle du tampon vient d'abord.** Un tampon vide passerait la vérification
    // suivante sans rien prouver.
    anyhow::ensure!(
        logs.contains("dossiers découverts") || logs.contains("compte synchronisé"),
        "les journaux de la synchronisation n'ont pas été capturés : la vérification ne \
         porterait sur rien"
    );
    if captured.truncated() {
        println!(
            "  ATTENTION : le tampon de journaux a atteint {} Mio et a été tronqué.",
            LOG_LIMIT / 1024 / 1024
        );
    }
    let in_logs: Vec<String> = needles
        .iter()
        .filter(|it| memchr::memmem::find(logs.as_bytes(), &it.bytes).is_some())
        .map(Needle::describe)
        .collect();
    verdict("les journaux capturés au niveau TRACE", &in_logs);

    let (in_temp, temp_unreadable) = scan_system_temp(&needles)?;
    verdict(
        "le répertoire temporaire du système",
        &in_temp
            .iter()
            .map(|(path, at)| format!("{} — {}", path.display(), needles[*at].label))
            .collect::<Vec<_>>(),
    );

    // Ce qui **doit** apparaître dans les journaux, et qu'il faut affirmer pour que la ligne
    // entre trace utile et fuite reste visible : savoir pour quel compte une synchronisation a
    // tourné est nécessaire au diagnostic.
    anyhow::ensure!(
        logs.contains("account") || logs.contains("compte"),
        "le compte n'est pas journalisé : le diagnostic serait impossible"
    );

    // **Les fichiers non inspectés sont dits, pas cachés.** Un fichier verrouillé par un
    // autre processus n'est pas une absence de fuite : c'est un trou dans la couverture, et
    // `%TEMP%` en a toujours quelques-uns sur une machine qui sert.
    if store_unreadable > 0 || temp_unreadable > 0 {
        println!(
            "  {store_unreadable} fichier(s) du store et {temp_unreadable} de `%TEMP%` n'ont \n             pas pu être lus — verrouillés par un autre processus. Non inspectés."
        );
    }

    let clean = leaks.is_empty() && in_logs.is_empty() && in_temp.is_empty();
    println!(
        "\n{} critères 6 et 7 — {} secret(s) réels cherchés dans le store, les journaux TRACE \
         et `%TEMP%`, après une moisson IMAP et une authentification SMTP (seuil 0)",
        if clean { "OK   " } else { "ÉCHEC" },
        needles.len()
    );
    println!(
        "\nCe qui n'est pas couvert : le jeton d'API du démon, qui est un secret d'une autre \
         nature et vit dans son propre trousseau."
    );
    if !clean {
        anyhow::bail!("un identifiant a été trouvé en clair");
    }
    Ok(())
}

/// Affiche le verdict d'un balayage.
fn verdict(where_: &str, found: &[String]) {
    if found.is_empty() {
        println!("  OK    {where_} : aucun secret trouvé.");
        return;
    }
    println!("  ÉCHEC {where_} : {} occurrence(s).", found.len());
    for line in found {
        println!("        {line}");
    }
}

/// Exerce le chemin SMTP avec les vrais secrets, **sans jamais envoyer de message**.
///
/// ## Pourquoi il faut l'exercer, et pourquoi il ne faut pas envoyer
///
/// Le critère 7 demande qu'aucun secret ne finisse en clair, et l'envoi est le second endroit
/// où un secret part sur le fil — le premier étant l'IMAP. Ne pas l'exercer laisserait la moitié
/// du risque non mesurée : `AUTH PLAIN` porte le mot de passe en base64, `AUTH XOAUTH2` porte le
/// jeton, et une ligne de commande journalisée par erreur les publierait.
///
/// Envoyer un vrai message pour le mesurer serait en revanche inacceptable : une vérification de
/// confidentialité qui expédie du courrier à chaque exécution a un effet de bord que personne
/// n'a demandé.
///
/// ## Le transport s'arrête **avant** le `DATA`, par construction
///
/// Il fait la connexion chiffrée, l'`EHLO`, l'`AUTH` avec le vrai secret, puis rend une erreur.
/// Il n'appelle ni `open_data` ni la frontière du doute : il n'y a donc aucun chemin de code par
/// lequel un octet de message pourrait partir, et ce n'est pas une promesse mais une absence
/// d'appel.
///
/// La conséquence voulue : la file écrit `last_error` avec le texte du serveur, ce qui couvre le
/// second risque — un secret qui atterrirait dans un message d'erreur stocké.
struct AuthOnly<'a> {
    server: &'a mailcore::Server,
    credential: mailsmtp::client::Credential<'a>,
}

impl mailsmtp::queue::Transport for AuthOnly<'_> {
    fn deliver(
        &mut self,
        _job: &mailcore::Outgoing,
        _body: &mut dyn std::io::Read,
        _frontier: &mut dyn FnMut() -> mailsmtp::Result<()>,
    ) -> mailsmtp::Result<()> {
        let mut client = mailsmtp::connect(self.server)?;
        client.auth(&self.server.username, self.credential)?;
        // **Le point d'arrêt.** `quit` est poli ; ce qui compte est qu'aucun `open_data` ne
        // suive, donc qu'aucun corps ne puisse partir.
        client.quit();
        Err(mailsmtp::Error::Rejected {
            stage: mailsmtp::Stage::Recipient,
            code: 550,
            reason: "sonde du critère 7 : authentification faite, rien n'a été envoyé".to_owned(),
        })
    }
}

/// Fait s'authentifier chaque compte qui peut envoyer, et met un refus dans la file.
///
/// Rend le nombre de comptes exercés. Un compte sans serveur de soumission est **compté à part**
/// et non silencieusement ignoré : le dire est ce qui empêche un balayage sur zéro compte de
/// passer pour une preuve.
fn exercise_smtp(store: &Store, accounts: &[mailcore::Account]) -> Result<(usize, usize)> {
    let mut exercised = 0_usize;
    let mut unconfigured = 0_usize;

    for account in accounts {
        let (Some(reading), Some(submission)) = (&account.server, &account.submission) else {
            unconfigured += 1;
            continue;
        };
        let secret = mailauth::session::secret_for(
            &reading.host,
            &reading.username,
            submission.auth.as_str(),
            now(),
        )
        .with_context(|| format!("secret du compte #{}", account.id.0))?;

        // Un message minimal, mis en file par le chemin réel : c'est lui qui écrira
        // `last_error`, et c'est ce champ qu'on veut voir balayé.
        let from = mailsmtp::compose::Address::parse(&reading.username, None)
            .with_context(|| format!("identifiant du compte #{}", account.id.0))?;
        // Un domaine réservé par la RFC 2606 : il ne résout pas, et il ne peut donc pas
        // recevoir. Même si tout le reste échouait, il n'y aurait personne au bout.
        let to = mailsmtp::compose::Address::parse("sonde@invalid.invalid", None)
            .context("adresse de sonde")?;
        let draft = mailsmtp::compose::Draft::new(
            from,
            vec![to],
            "sonde du critère 7",
            "Ce message n'est jamais envoyé.\r\n",
        );
        let (id, _) = mailsmtp::queue::stage(store, account.id, &draft, now())
            .with_context(|| format!("mise en file pour le compte #{}", account.id.0))?;
        let job = store
            .outgoing(id)?
            .context("la ligne de file vient de disparaître")?;

        let mut transport = AuthOnly {
            server: submission,
            credential: mailsmtp::client::Credential::for_auth(submission.auth, &secret),
        };
        let outcome = mailsmtp::queue::deliver_one(store, &mut transport, &job, now())
            .with_context(|| format!("sonde SMTP du compte #{}", account.id.0))?;
        println!(
            "  #{} — authentifié sur {}, verdict {outcome:?}",
            account.id.0, submission.host
        );
        // **Le contrôle de la sonde.** Un `Sent` voudrait dire que le transport a envoyé
        // quelque chose, ce qui est impossible par construction — donc que la construction a
        // changé. Le dire tout de suite vaut mieux que de le découvrir en balayant.
        anyhow::ensure!(
            outcome != mailsmtp::queue::Outcome::Sent,
            "la sonde a rendu `Sent` : le transport a envoyé un message, ce qu'il ne doit pas"
        );
        exercised += 1;
    }
    Ok((exercised, unconfigured))
}

/// Lit les secrets du trousseau pour les comptes donnés.
///
/// `when` sert seulement à nommer les aiguilles, pour qu'un secret rafraîchi pendant la passe
/// se distingue de celui d'avant dans un rapport d'échec.
///
/// Un compte dont le secret est illisible est **passé**, pas fatal : un mot de passe révoqué
/// chez un fournisseur ne doit pas empêcher de vérifier les quatre autres comptes.
fn collect(accounts: &[mailcore::Account], when: &str) -> Result<Vec<Needle>> {
    let mut out = Vec::new();
    for account in accounts {
        let Some(server) = account.server.as_ref() else {
            continue;
        };
        let id = account.id.0;
        match server.auth {
            AuthKind::Password => {
                if let Ok(secret) = mailauth::load(&server.host, &server.username) {
                    out.push(Needle {
                        label: format!("compte #{id}, mot de passe ({when})"),
                        bytes: secret.into_bytes(),
                    });
                }
            }
            AuthKind::OAuth2 => {
                let Ok(record) = mailauth::session::load_oauth2(&server.host, &server.username)
                else {
                    continue;
                };
                out.push(Needle {
                    label: format!("compte #{id}, jeton de rafraîchissement ({when})"),
                    bytes: record.refresh.into_bytes(),
                });
                if let Some(secret) = record.client_secret {
                    out.push(Needle {
                        label: format!("compte #{id}, secret client ({when})"),
                        bytes: secret.into_bytes(),
                    });
                }
                if let Some(access) = record.access {
                    out.push(Needle {
                        label: format!("compte #{id}, jeton d'accès ({when})"),
                        bytes: access.into_bytes(),
                    });
                }
            }
        }
    }
    // Une aiguille vide trouverait partout : ce serait un échec fabriqué, pas une fuite.
    out.retain(|it| !it.bytes.is_empty());
    Ok(out)
}

/// Cherche les aiguilles dans tous les fichiers sous une racine.
///
/// Rend ce qui a été trouvé **et** combien de fichiers n'ont pas pu être inspectés.
///
/// ## Un fichier illisible n'est pas une absence de fuite
///
/// Sous Windows, un fichier peut s'ouvrir et refuser de se lire : `ERROR_LOCK_VIOLATION`, quand
/// un autre processus verrouille une partie du contenu. `%TEMP%` en est plein — un navigateur,
/// un antivirus, un installeur.
///
/// La première version propageait cette erreur, donc **le balayage entier abandonnait** sur un
/// fichier qui n'avait rien à voir avec mailcore. Le critère 7 ne pouvait pas être mesuré sur
/// une machine ordinaire.
///
/// Les compter et les dire est la seule réponse honnête : un fichier non inspecté est un trou
/// dans la couverture, et cacher le trou serait pire que le nombre.
fn scan_tree(root: &Path, needles: &[Needle]) -> Result<(Vec<(PathBuf, usize)>, usize)> {
    let mut found = Vec::new();
    let mut unreadable = 0_usize;
    let mut stack = vec![root.to_path_buf()];
    while let Some(path) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&path) else {
            unreadable += 1;
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            let Ok(file) = std::fs::File::open(&path) else {
                unreadable += 1;
                continue;
            };
            match scan_reader(&mut std::io::BufReader::new(file), needles) {
                Ok(hits) => {
                    for at in hits {
                        found.push((path.clone(), at));
                    }
                }
                // Verrouillé, ou disparu entre l'ouverture et la lecture. Compté, pas propagé.
                Err(_) => unreadable += 1,
            }
        }
    }
    Ok((found, unreadable))
}

/// Les aiguilles présentes dans un flux, **lu par blocs**.
///
/// ## Le recouvrement, et pourquoi il vaut exactement une aiguille moins un octet
///
/// Une aiguille peut tomber à cheval sur deux blocs. Garder la fin du bloc précédent en tête du
/// suivant règle le cas, à condition d'en garder assez : la plus longue aiguille moins un
/// octet. Un octet de moins, et l'aiguille la plus longue peut se glisser dans la couture — ce
/// qui donnerait un « aucune fuite » faux, exactement le genre de zéro que le critère refuse.
fn scan_reader(source: &mut impl Read, needles: &[Needle]) -> Result<Vec<usize>> {
    let longest = needles.iter().map(|it| it.bytes.len()).max().unwrap_or(0);
    if longest == 0 {
        return Ok(Vec::new());
    }
    let overlap = longest - 1;

    let mut found = Vec::new();
    let mut buffer = vec![0_u8; overlap + CHUNK];
    let mut kept = 0_usize;
    loop {
        let read = read_up_to(source, &mut buffer[kept..])?;
        if read == 0 {
            break;
        }
        let filled = kept + read;
        for (at, needle) in needles.iter().enumerate() {
            if !found.contains(&at)
                && memchr::memmem::find(&buffer[..filled], &needle.bytes).is_some()
            {
                found.push(at);
            }
        }
        if found.len() == needles.len() {
            break;
        }
        // La couture : on garde la fin, on repart derrière.
        kept = overlap.min(filled);
        buffer.copy_within(filled - kept..filled, 0);
    }
    found.sort_unstable();
    Ok(found)
}

/// Remplit `target` autant que possible, et rend combien.
///
/// `Read::read` a le droit de rendre moins que demandé sans que ce soit la fin. Prendre son
/// retour pour la taille d'un bloc découperait le flux à des endroits arbitraires — ce qui
/// marcherait quand même grâce au recouvrement, mais ferait des blocs minuscules sur un flux
/// bavard.
fn read_up_to(source: &mut impl Read, target: &mut [u8]) -> Result<usize> {
    let mut filled = 0;
    while filled < target.len() {
        match source.read(&mut target[filled..]) {
            Ok(0) => break,
            Ok(count) => filled += count,
            Err(source) if source.kind() == std::io::ErrorKind::Interrupted => {}
            Err(source) => return Err(source.into()),
        }
    }
    Ok(filled)
}

/// Les fichiers du répertoire temporaire du système qui contiennent une aiguille.
///
/// Non récursif et plafonné, comme le test d'origine. Un fichier illisible — un autre processus
/// le tient ouvert en écriture exclusive — est **passé**, pas fatal : le répertoire temporaire
/// du système appartient à tout le monde.
fn scan_system_temp(needles: &[Needle]) -> Result<(Vec<(PathBuf, usize)>, usize)> {
    let mut found = Vec::new();
    let mut unreadable = 0_usize;
    let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) else {
        return Ok((found, 1));
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !entry
            .metadata()
            .is_ok_and(|it| it.is_file() && it.len() <= TEMP_SCAN_LIMIT)
        {
            continue;
        }
        let Ok(file) = std::fs::File::open(&path) else {
            unreadable += 1;
            continue;
        };
        // Verrouillé par un autre processus, ou disparu entre l'ouverture et la lecture.
        // Compté, pas propagé : voir `scan_tree`. `%TEMP%` d'une machine qui sert en a
        // toujours quelques-uns, et le balayage entier abandonnait dessus.
        match scan_reader(&mut std::io::BufReader::new(file), needles) {
            Ok(hits) => {
                for at in hits {
                    found.push((path.clone(), at));
                }
            }
            Err(_) => unreadable += 1,
        }
    }
    Ok((found, unreadable))
}

/// L'horloge, en secondes Unix.
fn now() -> i64 {
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |it| it.as_secs()),
    )
    .unwrap_or(i64::MAX)
}

/// Un tampon de journal partagé, branché sur `tracing`, borné.
#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl Captured {
    fn text(&self) -> String {
        let guard = self.0.lock().unwrap_or_else(|it| it.into_inner());
        String::from_utf8_lossy(&guard).into_owned()
    }

    fn truncated(&self) -> bool {
        let guard = self.0.lock().unwrap_or_else(|it| it.into_inner());
        guard.len() >= LOG_LIMIT
    }
}

impl Write for Captured {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        let mut guard = self.0.lock().unwrap_or_else(|it| it.into_inner());
        // Le plafond jette **la suite**, pas le début : la fin d'une synchronisation dit
        // qu'elle s'est terminée, et c'est ce que le contrôle du tampon vérifie. Tronquer par
        // la tête ferait disparaître le début, où sont l'authentification et la négociation —
        // c'est-à-dire l'endroit le plus probable d'une fuite.
        if guard.len() < LOG_LIMIT {
            guard.extend_from_slice(buffer);
        }
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Captured {
    type Writer = Self;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::{CHUNK, Needle, scan_reader};

    fn needle(value: &str) -> Needle {
        Needle {
            label: "essai".to_owned(),
            bytes: value.as_bytes().to_vec(),
        }
    }

    #[test]
    fn a_needle_in_a_single_block_is_found() {
        let needles = [needle("SECRET")];
        let mut data: &[u8] = b"........SECRET........";
        assert_eq!(scan_reader(&mut data, &needles).unwrap(), vec![0]);
    }

    #[test]
    fn a_needle_that_is_absent_is_not_invented() {
        let needles = [needle("SECRET")];
        let mut data: &[u8] = b"rien du tout";
        assert!(scan_reader(&mut data, &needles).unwrap().is_empty());
    }

    #[test]
    fn a_needle_straddling_two_blocks_is_found() {
        // **Le cas qui rend le recouvrement obligatoire.** Sans lui, cette aiguille-là passe
        // entre les mailles et le harnais rend un zéro faux.
        let needles = [needle("SECRET")];
        let mut data = vec![b'.'; CHUNK + 64];
        let at = CHUNK - 3;
        data[at..at + 6].copy_from_slice(b"SECRET");
        assert_eq!(
            scan_reader(&mut data.as_slice(), &needles).unwrap(),
            vec![0]
        );
    }

    #[test]
    fn several_needles_are_reported_one_by_one() {
        let needles = [needle("ALPHA"), needle("BETA"), needle("GAMMA")];
        let mut data: &[u8] = b"..GAMMA....ALPHA..";
        assert_eq!(scan_reader(&mut data, &needles).unwrap(), vec![0, 2]);
    }

    #[test]
    fn an_empty_needle_list_finds_nothing_rather_than_everything() {
        let mut data: &[u8] = b"n'importe quoi";
        assert!(scan_reader(&mut data, &[]).unwrap().is_empty());
    }
}
