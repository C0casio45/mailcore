//! `mail account` : déclarer un compte IMAP, et ranger son secret.
//!
//! ## Le secret ne passe jamais par un argument
//!
//! `docs/PRIVACY.md` §7, et la même règle que `mail daemon login` : un argument de ligne de
//! commande est visible dans la table des processus par les autres utilisateurs de la machine,
//! et reste dans l'historique du shell. Il n'y a donc **pas d'option** pour passer un mot de
//! passe — pas même une option cachée « pour scripter ».
//!
//! Il se lit sur l'entrée standard, **sans écho** quand c'est un terminal. Un mot de passe tapé
//! en clair reste dans le tampon de défilement du terminal, et parfois dans un journal de
//! session ; c'est la même classe de fuite qu'un argument, en plus discret.
//!
//! Quand l'entrée standard n'est pas un terminal — `mail account add … < secret.txt` — la
//! lecture est ordinaire. C'est ce qui rend la commande scriptable sans ouvrir de porte : le
//! secret vient alors d'un fichier dont l'utilisateur contrôle les droits.
//!
//! ## Ce que la commande écrit, et où
//!
//! | Donnée | Où |
//! |---|---|
//! | hôte, port, identifiant, mode de chiffrement | le store, table `accounts` |
//! | le secret | **le trousseau du système, et rien d'autre** |
//!
//! Le schéma de `mailcore` n'a aucune colonne où ranger un secret : ce n'est pas une
//! discipline, c'est une impossibilité. Voir la migration v2.

use std::io::{BufRead, Write};

use anyhow::{Context, Result, bail};
use camino::Utf8PathBuf;
use mailcore::{AccountKind, AuthKind, Security, Server, Store};

/// Ouvre le store local.
fn open(root: Option<&Utf8PathBuf>) -> Result<Store> {
    let root = crate::store_root(root)?;
    Store::open(&root).with_context(|| format!("ouverture du store {root}"))
}

/// Ce que la ligne de commande dit d'un compte à déclarer.
///
/// Une structure plutôt que huit paramètres, et pas seulement pour faire taire un lint : sept
/// des huit décrivent **le même compte**, et les passer un par un rend une erreur d'ordre
/// possible entre deux `&str` — l'identifiant à la place de l'hôte se déclare sans broncher.
#[derive(Debug, Clone, Copy)]
pub struct Declaration<'a> {
    /// Nom d'hôte du serveur IMAP.
    pub host: &'a str,
    /// Port, ou le défaut du mode de chiffrement.
    pub port: Option<u16>,
    /// L'identifiant présenté au serveur.
    pub username: &'a str,
    /// `tls` ou `starttls`, tel quel : la validation est ici, pas chez l'appelant.
    pub security: &'a str,
    /// `password` ou `oauth2`, tel quel.
    pub auth: &'a str,
    /// L'identifiant client OAuth2. Sans objet en `password`.
    pub client_id: Option<&'a str>,
    /// Port de bouclage épinglé pour le consentement. Sans objet en `password`.
    pub redirect_port: Option<u16>,
    /// Remplacer le secret déjà rangé au lieu de le réutiliser.
    ///
    /// Le défaut est de réutiliser : un compte redéclaré après un store perdu ne doit pas
    /// coûter un consentement complet pour aboutir au jeton qu'on avait déjà. Ce drapeau est
    /// pour le cas inverse — le mot de passe a changé chez le fournisseur, ou le jeton a été
    /// révoqué.
    pub renew: bool,
}

/// Déclare un compte, ou met le sien à jour.
///
/// # Errors
///
/// Si le store est illisible, si le mode de chiffrement ou le mécanisme est inconnu, si le
/// secret est vide, ou si le trousseau ne répond pas.
pub fn add(root: Option<&Utf8PathBuf>, it: Declaration<'_>) -> Result<()> {
    let Declaration {
        host,
        port,
        username,
        security,
        auth,
        client_id,
        redirect_port,
        renew,
    } = it;
    let security = Security::parse(security).with_context(|| {
        format!("mode de chiffrement `{security}` : attendu `tls` ou `starttls`")
    })?;
    let auth = AuthKind::parse(auth)
        .with_context(|| format!("mécanisme `{auth}` : attendu `password` ou `oauth2`"))?;
    let server = Server {
        host: host.to_owned(),
        // Le port par défaut suit le mode : 993 en TLS, 143 en STARTTLS. Se tromper de port
        // donne une erreur de connexion illisible, alors que le mode le détermine.
        port: port.unwrap_or_else(|| security.default_port()),
        username: username.to_owned(),
        auth,
        security,
    };

    // **Le trousseau d'abord.** Si le secret n'arrive pas à s'enregistrer — session Linux sans
    // Secret Service, consentement refusé — on ne veut pas d'un compte inscrit dans le store
    // dont la synchronisation échouera sans expliquer pourquoi.
    //
    // **Et on ne redemande pas ce qu'on a déjà.** Le trousseau survit au store : le 2026-09-11,
    // le store de production était vide et les cinq entrées du Credential Manager intactes.
    // Redéclarer un compte OAuth2 refaisait alors un consentement complet — navigateur,
    // identifiant client, écran du fournisseur — pour aboutir au jeton déjà rangé. Un mot de
    // passe déjà là se retape sans dommage ; un consentement, non.
    //
    // `--renew` reste le chemin de celui qui veut justement remplacer ce qui est rangé.
    let held = mailauth::session::has_secret(host, username, auth.as_str());
    match (auth, held && !renew) {
        (_, true) => {
            println!("Secret déjà dans le trousseau pour {username} : réutilisé tel quel.");
            println!("  `--renew` pour le remplacer.");
            println!();
        }
        (AuthKind::Password, false) => {
            let secret = read_secret(host, username)?;
            if secret.is_empty() {
                bail!("secret vide : rien n'a été enregistré");
            }
            mailauth::store(host, username, &secret)?;
        }
        (AuthKind::OAuth2, false) => consent(host, username, client_id, redirect_port)?,
    }

    let store = open(root)?;
    let account = {
        let writer = store.writer()?;
        let account = writer.upsert_imap_account(username, &server)?;
        // **Redéclarer un compte le réactive.** `account forget` le met en pause ; y reposer
        // un secret veut dire qu'on veut qu'il remarche. Sans ça, le message de `sync` sur un
        // compte en pause — « `mail account add` pour le réactiver » — serait un mensonge, et
        // l'utilisateur retaperait son mot de passe pour rien.
        writer.set_account_enabled(account, true)?;
        writer.commit()?;
        account
    };

    println!("Compte {} enregistré (#{}).", username, account.0);
    println!("  Serveur   {host}:{}", server.port);
    println!("  Chiffrage {}", server.security.as_str());
    println!("  Auth      {}", server.auth.as_str());
    println!("  Secret    dans le trousseau du système");
    println!();
    println!("Prochaine étape : `mail sync` pour découvrir les dossiers et moissonner.");
    Ok(())
}

/// Déroule un consentement OAuth2 et range les jetons.
///
/// ## Le secret client se lit sur l'entrée standard, comme un mot de passe
///
/// Chez Google il n'est **pas** un secret au sens cryptographique : une application installée
/// le porte dans son binaire, et c'est précisément pour ça que PKCE existe. Mais il a la forme
/// d'un identifiant, et le traiter autrement demanderait à l'utilisateur de distinguer deux
/// régimes de confidentialité pour deux valeurs collées l'une à l'autre dans la même console.
/// Un seul régime, le plus strict, est plus sûr à utiliser.
///
/// Une ligne vide veut dire « pas de secret client » : Microsoft n'en impose pas pour une
/// application publique, et en envoyer un vide fait refuser l'échange.
fn consent(
    host: &str,
    username: &str,
    client_id: Option<&str>,
    redirect_port: Option<u16>,
) -> Result<()> {
    // Les lignes sont assemblées par `concat!` plutôt qu'avec des continuations `\` : une
    // continuation mange l'indentation de la ligne suivante, donc les étapes numérotées
    // arriveraient collées à la marge.
    let client_id = client_id
        .map(str::trim)
        .filter(|it| !it.is_empty())
        .context(concat!(
            "`--auth oauth2` demande `--client-id`.\n",
            "Le fournisseur ne délivre de jeton qu'à une application déclarée, et celle-ci est ",
            "la vôtre :\n",
            "\n",
            "  Google\n",
            "    1. console.cloud.google.com → créer un projet\n",
            "    2. API et services → Bibliothèque → activer l'API Gmail\n",
            "    3. Écran de consentement OAuth → externe → ajouter votre adresse dans\n",
            "       « utilisateurs de test »\n",
            "    4. Identifiants → Créer → ID client OAuth → type « Application de bureau »\n",
            "    5. Reprendre l'ID client ici, et le secret client à l'invite suivante\n",
            "\n",
            "  Microsoft\n",
            "    1. entra.microsoft.com → Inscriptions d'applications → Nouvelle inscription\n",
            "    2. Type « client public », redirection « http://127.0.0.1 »\n",
            "    3. Autorisations d'API → IMAP.AccessAsUser.All et offline_access\n",
            "    4. Reprendre l'ID d'application ici ; laisser le secret client vide\n",
            "\n",
            "L'ID client n'est pas un secret : il apparaît dans l'URL de consentement.",
        ))?;

    let provider = mailauth::oauth::Provider::for_host(host).with_context(|| {
        format!(
            "aucun fournisseur OAuth2 connu pour {host}. Le point de terminaison de jeton \
             n'est pas configurable : le rendre configurable serait le moyen le plus simple de \
             faire envoyer un jeton de rafraîchissement ailleurs. Utiliser `--auth password` \
             avec un mot de passe applicatif."
        )
    })?;

    let client_secret = read_client_secret()?;
    let credentials = mailauth::oauth::ClientCredentials {
        client_id: client_id.to_owned(),
        client_secret: client_secret.filter(|it| !it.is_empty()),
    };

    // L'URL est **affichée**, pas seulement journalisée : `tracing` peut être configuré pour
    // n'écrire nulle part, et si le navigateur ne s'ouvre pas, cette ligne est le seul moyen
    // pour l'utilisateur de finir son consentement.
    let announce = |url: &str| {
        println!();
        println!("Ouverture du navigateur pour autoriser {username}.");
        println!("Si rien ne s'ouvre, coller cette adresse :");
        println!();
        println!("{url}");
        println!();
        println!("En attente du retour du navigateur…");
    };

    let tokens =
        mailauth::session::authorize(&provider, &credentials, username, &announce, redirect_port)?;
    mailauth::session::store_tokens(host, username, &credentials, &tokens, now())?;
    println!("Consentement obtenu.");
    Ok(())
}

/// Lit le secret client, en acceptant une ligne vide.
///
/// La même mécanique que [`read_secret`] — `IsTerminal` avant `rpassword`, BOM retiré — mais un
/// vide n'est pas une erreur ici : tous les fournisseurs n'en imposent pas.
fn read_client_secret() -> Result<Option<String>> {
    use std::io::IsTerminal;

    if std::io::stdin().is_terminal() {
        eprint!("Secret client OAuth2 (vide si le fournisseur n'en impose pas) : ");
        std::io::stderr().flush().ok();
        let secret = rpassword::read_password().context("lecture du secret client sans écho")?;
        eprintln!();
        return Ok(Some(secret.trim().to_owned()).filter(|it| !it.is_empty()));
    }

    let mut line = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut line)
        .context("lecture du secret client sur l'entrée standard")?;
    let secret = line.strip_prefix('\u{FEFF}').unwrap_or(&line);
    Ok(Some(secret.trim().to_owned()).filter(|it| !it.is_empty()))
}

/// L'instant présent en secondes Unix, pour datter la péremption d'un jeton d'accès.
fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| {
            i64::try_from(since.as_secs()).unwrap_or(i64::MAX)
        })
}

/// Liste les comptes. **Aucun secret n'est affiché**, seulement sa présence.
///
/// # Errors
///
/// Si le store est illisible, ou si un compte y est incohérent.
pub fn list(root: Option<&Utf8PathBuf>) -> Result<()> {
    let store = open(root)?;
    let accounts = store.full_accounts()?;
    if accounts.is_empty() {
        println!("Aucun compte. `mail account add --host … --username …` pour en déclarer un.");
        return Ok(());
    }

    for account in &accounts {
        let state = if account.enabled { "actif" } else { "en pause" };
        match (&account.kind, &account.server) {
            (AccountKind::Imap, Some(server)) => {
                // Ni `is_stored` ni `has_oauth2` ne lisent le secret pour l'afficher : ils
                // disent s'il y en a un. Un inventaire des secrets n'a qu'un usage, et ce
                // n'est pas celui-ci.
                //
                // **La présence est cherchée là où le mécanisme du compte la range**, et pas
                // dans les deux : un compte passé de `password` à `oauth2` garde parfois son
                // ancienne entrée, et l'annoncer « secret présent » sur la foi de celle-là
                // ferait croire à un compte prêt qui ne s'authentifiera pas.
                let stored = match server.auth {
                    AuthKind::Password => mailauth::is_stored(&server.host, &server.username),
                    AuthKind::OAuth2 => {
                        mailauth::session::has_oauth2(&server.host, &server.username)
                    }
                };
                let secret = if stored {
                    "présent"
                } else {
                    "ABSENT — `mail account add` pour le poser"
                };
                println!(
                    "#{} {} — imap {}:{} {} {} [{state}], secret {secret}",
                    account.id.0,
                    account.display_name,
                    server.host,
                    server.port,
                    server.security.as_str(),
                    server.auth.as_str(),
                );
                // L'envoi est dit sur sa propre ligne, et **son absence aussi**. Un compte qui
                // ne peut pas envoyer doit le dire ici plutôt qu'au moment où l'utilisateur a
                // fini d'écrire son message — critère 8.
                match &account.submission {
                    Some(smtp) => println!(
                        "    envoi  smtp {}:{} {} {}{}",
                        smtp.host,
                        smtp.port,
                        smtp.security.as_str(),
                        smtp.auth.as_str(),
                        if smtp.username == server.username {
                            String::new()
                        } else {
                            format!(" (identifiant {})", smtp.username)
                        },
                    ),
                    None => println!(
                        "    envoi  non configuré — `mail account submission --account {} \
                         --host {}` (hôte à vérifier chez le fournisseur)",
                        account.id.0,
                        suggest_submission(&server.host),
                    ),
                }
            }
            _ => println!(
                "#{} {} — importé d'un mbox, pas de serveur",
                account.id.0, account.display_name
            ),
        }
    }
    Ok(())
}

/// Ce que `mail account submission` a reçu.
#[derive(Debug, Clone, Copy)]
pub struct Submission<'a> {
    /// L'hôte du serveur de soumission. `None` seulement avec `clear`.
    pub host: Option<&'a str>,
    /// Le port, ou le défaut du mode de chiffrement.
    pub port: Option<u16>,
    /// `tls` ou `starttls`.
    pub security: &'a str,
    /// L'identifiant, s'il diffère de celui de la lecture.
    pub username: Option<&'a str>,
    /// Le mécanisme, s'il diffère de celui de la lecture.
    pub auth: Option<&'a str>,
    /// Effacer la configuration d'envoi.
    pub clear: bool,
}

/// Un hôte de soumission **plausible**, à vérifier.
///
/// ## C'est une suggestion, jamais une valeur écrite
///
/// La règle « remplacer `imap.` par `smtp.` » couvre Gmail, Free, SFR, Microsoft et la plupart
/// des hébergeurs mutualisés. Elle échoue chez ceux qui séparent autrement, et elle n'a aucun
/// moyen de le savoir. Écrire le résultat serait envoyer le message à un hôte que personne n'a
/// vérifié ; l'afficher laisse la vérification à l'utilisateur, qui a la documentation de son
/// fournisseur sous les yeux.
fn suggest_submission(imap_host: &str) -> String {
    match imap_host.strip_prefix("imap.") {
        Some(rest) => format!("smtp.{rest}"),
        // Un hôte qui ne commence pas par `imap.` sert souvent les deux protocoles — c'est le
        // cas de `mail.exemple.fr` chez la plupart des mutualisés. Le rendre tel quel est la
        // suggestion la moins fausse.
        None => imap_host.to_owned(),
    }
}

/// Déclare, ou efface, le serveur de soumission d'un compte.
///
/// ## Aucun secret n'est demandé
///
/// C'est celui de la lecture, déjà dans le trousseau du système sous la clé
/// `(host, username)` du serveur IMAP. Les fournisseurs du corpus acceptent le même secret pour
/// la lecture et pour l'envoi ; en redemander un serait exiger une saisie qui ne sert à rien, et
/// **créer une seconde entrée de trousseau à tenir à jour**.
///
/// La conséquence à connaître : un fournisseur qui exigerait un secret d'envoi distinct n'est
/// pas géré. Le dire vaut mieux que de le découvrir à l'envoi.
///
/// # Errors
///
/// Si le compte est inconnu ou sans serveur, si le mode de chiffrement ou le mécanisme est
/// inconnu, ou si le store est illisible.
pub fn submission(root: Option<&Utf8PathBuf>, id: i64, wanted: Submission<'_>) -> Result<()> {
    let store = open(root)?;
    let accounts = store.full_accounts()?;
    let account = accounts
        .iter()
        .find(|it| it.id.0 == id)
        .with_context(|| format!("aucun compte #{id} — voir `mail account list`"))?;
    let server = account
        .server
        .as_ref()
        .with_context(|| format!("le compte #{id} n'a pas de serveur : il ne peut pas envoyer"))?;

    if wanted.clear {
        let writer = store.writer()?;
        writer.set_submission(account.id, None)?;
        writer.commit()?;
        println!("Le compte #{id} n'a plus de serveur d'envoi.");
        return Ok(());
    }

    let host = wanted
        .host
        .context("`--host` est obligatoire sans `--clear`")?;
    let security = Security::parse(wanted.security)
        .with_context(|| format!("mode de chiffrement inconnu : {}", wanted.security))?;
    let auth = match wanted.auth {
        Some(label) => {
            AuthKind::parse(label).with_context(|| format!("mécanisme inconnu : {label}"))?
        }
        None => server.auth,
    };
    let submission = Server {
        host: host.to_owned(),
        port: wanted.port.unwrap_or_else(|| security.submission_port()),
        username: wanted.username.unwrap_or(&server.username).to_owned(),
        auth,
        security,
    };

    let writer = store.writer()?;
    writer.set_submission(account.id, Some(&submission))?;
    writer.commit()?;

    println!(
        "Compte #{id} : envoi par smtp {}:{} {} {}.",
        submission.host,
        submission.port,
        submission.security.as_str(),
        submission.auth.as_str(),
    );
    if submission.username != server.username {
        println!("Identifiant d'envoi : {}", submission.username);
    }
    println!();
    // Ce que l'utilisateur doit savoir avant d'essayer, et qui ne se devine pas.
    println!(
        "Le secret est celui de la lecture, déjà dans le trousseau. Rien n'a été envoyé : \
         `mail send --help` pour le premier essai."
    );
    Ok(())
}

/// Oublie le secret d'un compte, et met sa synchronisation en pause.
///
/// ## Le compte et son courrier restent
///
/// Retirer le compte du store retirerait ses dossiers, donc leurs références, donc ferait
/// disparaître du courrier déjà téléchargé. Cette commande coupe l'accès au serveur ; elle ne
/// supprime rien.
///
/// # Errors
///
/// Si le compte est inconnu, si le store est illisible, ou si le trousseau ne répond pas.
pub fn forget(root: Option<&Utf8PathBuf>, id: i64) -> Result<()> {
    let store = open(root)?;
    let accounts = store.full_accounts()?;
    let account = accounts
        .iter()
        .find(|it| it.id.0 == id)
        .with_context(|| format!("aucun compte #{id} — voir `mail account list`"))?;
    let server = account
        .server
        .as_ref()
        .with_context(|| format!("le compte #{id} n'a pas de serveur"))?;

    // **Les deux entrées, quel que soit le mécanisme déclaré.** Un compte a pu passer du mot
    // de passe applicatif à OAuth2 ; n'effacer que celle qui correspond au mécanisme du jour
    // laisserait l'autre au trousseau, où l'utilisateur ne pensera pas à la chercher après
    // avoir lu « secret oublié ».
    mailauth::forget(&server.host, &server.username)?;
    mailauth::session::forget_oauth2(&server.host, &server.username)?;

    let writer = store.writer()?;
    writer.set_account_enabled(account.id, false)?;
    writer.commit()?;

    println!("Secret oublié, synchronisation du compte #{id} en pause.");
    println!("Le courrier déjà téléchargé reste dans le store.");
    if server.auth == AuthKind::OAuth2 {
        println!();
        // Le dire, parce que l'inverse se croit facilement : « j'ai retiré le compte, donc
        // l'application n'a plus accès ». Le jeton de rafraîchissement reste valide côté
        // serveur ; on l'a seulement effacé de cette machine.
        println!(
            "Le jeton de rafraîchissement a été effacé de cette machine, mais il reste \
             valide chez le fournisseur."
        );
        println!("Pour couper l'accès partout, révoquer l'application dans le compte :");
        println!("  Google    myaccount.google.com/permissions");
        println!("  Microsoft myapplications.microsoft.com");
    }
    Ok(())
}

/// Lit le secret : sans écho sur un terminal, ligne à ligne sinon.
///
/// ## Le terminal est détecté **avant** d'appeler `rpassword`, pas après
///
/// La première version essayait `rpassword::read_password()` et retombait sur une lecture
/// ordinaire en cas d'échec. Ça ne marche pas sur Windows : `rpassword` y ouvre la console
/// (`CONIN$`) au lieu de lire l'entrée standard, donc il **ignore un tube** et attend une
/// frappe qui ne viendra jamais. Le cas scripté — `mail account add … < secret.txt` — se
/// bloquait pour toujours.
///
/// `IsTerminal` de la bibliothèque standard répond à la vraie question : est-ce qu'il y a
/// quelqu'un pour taper ?
fn read_secret(host: &str, username: &str) -> Result<String> {
    use std::io::IsTerminal;

    if std::io::stdin().is_terminal() {
        eprint!("Mot de passe applicatif pour {username} sur {host} : ");
        std::io::stderr().flush().ok();
        let secret = rpassword::read_password().context("lecture du secret sans écho")?;
        eprintln!();
        return Ok(secret.trim().to_owned());
    }

    // Pas de terminal : le secret vient d'un tube ou d'un fichier, et il n'y a pas d'écho à
    // masquer. Pas de message d'invite non plus — il polluerait la sortie d'un script.
    let mut line = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut line)
        .context("lecture du secret sur l'entrée standard")?;

    // **Le BOM est retiré explicitement.** `trim` ne le fait pas : U+FEFF n'est pas un
    // caractère d'espacement au sens de Rust. Un éditeur Windows — VS Code, Notepad — peut
    // enregistrer en « UTF-8 avec BOM », et le secret partirait alors avec trois octets
    // invisibles en tête. Le serveur refuse, et rien dans le message ne dit pourquoi.
    let secret = line.strip_prefix('\u{FEFF}').unwrap_or(&line);
    Ok(secret.trim().to_owned())
}
