//! # mail
//!
//! Le client CLI : import, indexation, recherche, statistiques, diagnostic. Le premier
//! consommateur du cœur, et l'outil avec lequel on mesure les critères de `docs/PHASE-1.md`.
//!
//! Le chemin du profil Thunderbird n'est jamais écrit en dur : argument, ou variable
//! d'environnement `MAILCORE_TB_PROFILE`. C'est une donnée de la machine, pas du projet.

#![forbid(unsafe_code)]

mod commands;
use mailapi::human;

use anyhow::{Context, Result};
use camino::Utf8PathBuf;
use clap::{Parser, Subcommand};

/// Client en ligne de commande de mailcore.
#[derive(Debug, Parser)]
#[command(name = "mail", version, about)]
struct Cli {
    /// Racine du store. Par défaut, le répertoire de données de l'utilisateur.
    #[arg(long, global = true, env = "MAILCORE_STORE")]
    store: Option<Utf8PathBuf>,

    /// Adresse d'un démon — `hôte` ou `hôte:port`. Sans ce drapeau, le store est ouvert
    /// directement.
    ///
    /// Le jeton n'est **jamais** passé en argument : il vit dans le trousseau du système,
    /// déposé par `mail daemon login` (`docs/PRIVACY.md`, §7).
    ///
    /// Le client parle en clair et refuse d'envoyer un jeton hors de la machine. Pour un
    /// démon distant, monter un tunnel chiffré et viser `127.0.0.1` — c'est le déploiement
    /// que `docs/ARCHITECTURE.md` recommande de toute façon.
    #[arg(long, global = true, env = "MAILCORE_DAEMON")]
    daemon: Option<String>,

    #[command(subcommand)]
    command: Command,
}

/// Les sous-commandes de gestion d'un démon.
#[derive(Debug, Subcommand)]
enum DaemonCommand {
    /// Enregistre le jeton d'un démon dans le trousseau du système.
    ///
    /// Le jeton se lit sur l'entrée standard, jamais en argument : un argument est visible
    /// dans la table des processus et reste dans l'historique du shell.
    Login,
    /// Oublie le jeton d'un démon.
    Logout,
    /// Ce que le démon dit de lui-même : version, messages, tâches, profils importables.
    Status,
}

/// Les sous-commandes du carnet d'adresses.
#[derive(Debug, Subcommand)]
enum ContactsCommand {
    /// Reconstruit le carnet depuis tous les messages du store.
    ///
    /// Parcourt les en-têtes de tout le corpus : sur 48 000 messages, ça se compte en minutes.
    /// La reconstruction est complète et non incrémentale, ce qui la rend idempotente — voir
    /// `mailcore::contacts::rebuild`.
    Rebuild,
    /// Interroge le carnet, comme un champ de destinataire le ferait.
    ///
    /// Affiche le score de chaque proposition : c'est ce qui rend le classement critiquable au
    /// lieu d'être subi.
    Complete {
        /// Le début de saisie. Vide pour les mieux classées.
        #[arg(default_value = "")]
        prefix: String,
        /// Nombre maximum de propositions.
        #[arg(long, default_value_t = 10)]
        limit: usize,
    },
    /// Le carnet en quelques nombres, et les adresses les mieux classées.
    Stats,
}

/// Les sous-commandes de gestion des comptes IMAP.
#[derive(Debug, Subcommand)]
enum AccountCommand {
    /// Déclare un compte IMAP et range son secret dans le trousseau du système.
    ///
    /// **Le secret ne se passe pas en argument.** Il se lit sur l'entrée standard, sans écho
    /// quand c'est un terminal : un argument est visible dans la table des processus et reste
    /// dans l'historique du shell.
    Add {
        /// Nom d'hôte du serveur IMAP.
        #[arg(long)]
        host: String,
        /// Port. Par défaut 993 en `tls`, 143 en `starttls`.
        #[arg(long)]
        port: Option<u16>,
        /// L'identifiant présenté au serveur, en général l'adresse complète.
        #[arg(long)]
        username: String,
        /// `tls` — chiffré dès la connexion. `starttls` — chiffré après négociation.
        ///
        /// Il n'y a pas de mode en clair : un IMAP non chiffré transporte le mot de passe sur
        /// le réseau, et il n'existe pas de configuration où on l'accepterait.
        #[arg(long, default_value = "tls")]
        security: String,
        /// `password` — mot de passe, ou mot de passe applicatif. `oauth2` — consentement
        /// dans le navigateur, puis jeton rafraîchi tout seul.
        ///
        /// `oauth2` demande `--client-id` : le fournisseur ne délivre de jeton qu'à une
        /// application déclarée, et cette application est la vôtre. Voir la sortie de la
        /// commande, qui donne les étapes de la console du fournisseur.
        #[arg(long, default_value = "password")]
        auth: String,
        /// L'identifiant client OAuth2, obtenu dans la console du fournisseur.
        ///
        /// **Ce n'est pas un secret** : il apparaît dans l'URL de consentement, que le
        /// navigateur affiche. Il peut donc être un argument, contrairement au secret client
        /// qui, lui, se lit sur l'entrée standard.
        #[arg(long)]
        client_id: Option<String>,
        /// Épingle le port de bouclage du consentement OAuth2, au lieu d'en prendre un libre.
        ///
        /// Sans objet dans le cas normal : Google autorise le port à varier pour une
        /// redirection de bouclage. **Sortie de secours pour un fournisseur qui compare
        /// l'URI de redirection avec son port** — enregistrer alors `http://127.0.0.1:PORT`
        /// chez lui et passer le même ici. Le chemin Microsoft n'a jamais été éprouvé contre
        /// un vrai serveur, et c'est le premier endroit où il pourrait achopper.
        #[arg(long)]
        redirect_port: Option<u16>,
        /// Remplace le secret déjà rangé au lieu de le réutiliser.
        ///
        /// Sans ce drapeau, un compte dont le trousseau porte déjà un secret se redéclare sans
        /// rien redemander. C'est ce qu'on veut après un store perdu : le trousseau lui survit,
        /// et refaire un consentement OAuth2 complet pour aboutir au jeton déjà rangé est du
        /// travail pour rien. Avec, on retape — mot de passe changé chez le fournisseur, jeton
        /// révoqué.
        #[arg(long)]
        renew: bool,
    },
    /// Liste les comptes. N'affiche aucun secret, seulement sa présence.
    List,
    /// Interroge les serveurs : capacités annoncées, extensions réellement activées, et le
    /// coût d'un `EXAMINE` par dossier.
    ///
    /// Aucune écriture, ni dans le store ni côté serveur. C'est la commande qui répond à
    /// « ce fournisseur sait-il faire `LIST-STATUS` ? » par une mesure plutôt que par un
    /// souvenir — voir le critère 3 de `docs/PHASE-2.md`.
    Probe {
        /// N'interroger que ce compte. Par défaut, tous les comptes actifs.
        #[arg(long)]
        account: Option<i64>,
        /// Afficher l'état de chaque boîte : messages, `UIDNEXT`, `UIDVALIDITY`, `MODSEQ`.
        ///
        /// Ce sont les nombres que la moisson compare à son état local pour décider si un
        /// dossier a bougé. Les lire est la façon de comprendre pourquoi un dossier a été
        /// moissonné plutôt qu'évité.
        #[arg(long)]
        detail: bool,
    },
    /// Déclare le serveur de **soumission** d'un compte : celui par lequel il envoie.
    ///
    /// ## Pourquoi ce n'est pas déduit du serveur IMAP
    ///
    /// `imap.gmail.com` → `smtp.gmail.com` est vrai, et la déduction marche jusqu'au jour où
    /// elle échoue. Ce jour-là, elle envoie le message au mauvais endroit, ou échoue sans dire
    /// pourquoi. La commande **suggère** un hôte plausible et n'en écrit aucun tout seul.
    ///
    /// Le secret n'est pas redemandé : c'est celui de la lecture, déjà dans le trousseau du
    /// système. `--username` et `--auth` ne servent qu'aux fournisseurs — il en existe — qui
    /// demandent un identifiant ou un mécanisme différents pour l'envoi.
    Submission {
        /// Le numéro du compte, tel que `mail account list` l'affiche.
        #[arg(long)]
        account: i64,
        /// Nom d'hôte du serveur de soumission. Obligatoire sauf avec `--clear`.
        #[arg(long, required_unless_present = "clear")]
        host: Option<String>,
        /// Port. Par défaut 465 en `tls`, 587 en `starttls`.
        ///
        /// **Pas le 25** : c'est le port de relais entre serveurs, les fournisseurs le
        /// bloquent en sortie, et il n'exige pas de chiffrement.
        #[arg(long)]
        port: Option<u16>,
        /// `tls` — chiffré dès la connexion (465). `starttls` — chiffré après négociation (587).
        ///
        /// Il n'y a pas de mode en clair : un `SUBMIT` non chiffré transporte le mot de passe
        /// **et** le message.
        #[arg(long, default_value = "tls")]
        security: String,
        /// L'identifiant de soumission, s'il diffère de celui de la lecture.
        #[arg(long)]
        username: Option<String>,
        /// Le mécanisme de soumission, s'il diffère de celui de la lecture.
        #[arg(long)]
        auth: Option<String>,
        /// Efface la configuration d'envoi. Le compte redevient un compte qui ne peut pas
        /// envoyer, ce qui est un état valide.
        #[arg(long, conflicts_with_all = ["host", "port", "username", "auth"])]
        clear: bool,
    },
    /// Oublie le secret d'un compte et met sa synchronisation en pause.
    ///
    /// Le courrier déjà téléchargé reste : retirer le compte du store retirerait ses
    /// dossiers, donc leurs références, donc ferait disparaître du courrier.
    Forget {
        /// Le numéro du compte, tel que `mail account list` l'affiche.
        id: i64,
    },
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Gestion d'un démon : jeton, état.
    Daemon {
        #[command(subcommand)]
        command: DaemonCommand,
    },
    /// Gestion des comptes IMAP : déclaration, secret, mise en pause.
    Account {
        #[command(subcommand)]
        command: AccountCommand,
    },
    /// Synchronise les comptes IMAP : découvre les dossiers, moissonne les messages.
    ///
    /// Un passage, puis la commande rend la main — une synchronisation périodique est une
    /// tâche de fond du démon, pas une commande qui ne s'arrête jamais.
    ///
    /// Un compte qui échoue n'empêche pas les autres de se synchroniser ; le code de sortie
    /// est non nul dès qu'un compte a échoué.
    Sync {
        /// Ne synchroniser que ce compte. Par défaut, tous les comptes actifs.
        #[arg(long)]
        account: Option<i64>,
    },
    /// Envoie un message : compose, met en file, remet.
    ///
    /// ## L'ordre est délibéré
    ///
    /// Le message est écrit dans la file **et validé** avant qu'un octet ne sorte. Un `Ctrl-C`
    /// entre les deux ne perd rien : `mail send --flush` reprendra. C'est l'inverse d'un envoi
    /// qui enregistrerait après, où le même `Ctrl-C` perdrait le message ou le doublerait.
    ///
    /// `--dry-run` assemble et affiche sans ouvrir de connexion. C'est le premier essai à
    /// faire : il montre quels en-têtes partent, et donc qu'une copie cachée n'y est pas.
    Send {
        /// Le compte qui envoie, tel que `mail account list` l'affiche.
        #[arg(long, required_unless_present = "flush")]
        account: Option<i64>,
        /// Un destinataire visible. Répétable. Au moins un, sauf avec `--flush`.
        #[arg(long, required_unless_present = "flush")]
        to: Vec<String>,
        /// Un destinataire en copie visible. Répétable.
        #[arg(long)]
        cc: Vec<String>,
        /// Un destinataire en copie **cachée** : dans l'enveloppe SMTP, dans aucun en-tête.
        /// Répétable.
        #[arg(long)]
        bcc: Vec<String>,
        /// Le sujet.
        #[arg(long, default_value = "")]
        subject: String,
        /// Le corps, en texte brut. Lu sur l'entrée standard s'il est absent.
        #[arg(long)]
        body: Option<String>,
        /// Un fichier à joindre. Répétable.
        ///
        /// Le fichier est rangé dans le magasin de blobs **avant** l'assemblage, en flux : une
        /// pièce de 25 Mo ne passe jamais en entier par la mémoire.
        #[arg(long)]
        attach: Vec<Utf8PathBuf>,
        /// Joindre la signature du compte au corps.
        ///
        /// **Explicite, et pas d'office.** Une signature ajoutée en silence par la CLI serait
        /// un ajout que la commande n'a pas montré ; avec ce drapeau, elle est demandée. La
        /// coquille, elle, la **montre** avant l'envoi, ce qui répond autrement à la même
        /// exigence de `docs/PHASE-3.md`.
        ///
        /// `--dry-run` affiche le message signé sans rien envoyer : c'est le moyen de la relire.
        #[arg(long)]
        signature: bool,
        /// Assembler et afficher, sans rien envoyer ni ouvrir de connexion.
        #[arg(long)]
        dry_run: bool,
        /// Ne rien composer : réessayer ce qui est déjà en file.
        ///
        /// Les messages **douteux** ne sont pas repris : voir `mail outbox`.
        #[arg(long, conflicts_with_all = ["account", "to", "cc", "bcc", "body", "attach", "signature", "dry_run"])]
        flush: bool,
    },
    /// Le carnet d'adresses, dérivé du corpus. Aucune requête réseau.
    Contacts {
        #[command(subcommand)]
        command: ContactsCommand,
    },
    /// Affiche la file d'envoi : ce qui attend, ce qui est parti, ce qui est incertain.
    ///
    /// `--resend` et `--accept` tranchent le doute sur un message incertain. C'est la **seule**
    /// sortie de cet état, et elle demande d'avoir vérifié chez le fournisseur : renvoyer un
    /// message arrivé le fait recevoir deux fois.
    ///
    /// `--retry` est l'autre geste, et il ne porte pas le même risque : un envoi **échoué** a
    /// été refusé par le serveur, qui n'a donc rien pris. Le renvoyer ne peut pas faire de
    /// doublon.
    Outbox {
        /// Le destinataire ne l'a pas reçu : remettre ce message **incertain** en file.
        #[arg(long, conflicts_with_all = ["accept", "forget", "retry"])]
        resend: Option<i64>,
        /// Le destinataire l'a reçu : marquer ce message envoyé, sans rien envoyer.
        #[arg(long, conflicts_with_all = ["forget", "retry"])]
        accept: Option<i64>,
        /// Renvoyer un envoi **échoué**, tel quel.
        ///
        /// Le compteur de tentatives repart de zéro : un renvoi demandé est un envoi neuf, pas
        /// la septième tentative d'un ancien. Refusé sur tout autre état.
        #[arg(long, conflicts_with = "forget")]
        retry: Option<i64>,
        /// Retirer de la file un envoi **fini** — envoyé ou échoué.
        ///
        /// Refusé sur un envoi en cours ou incertain : ce serait perdre un message, ou
        /// effacer la trace d'un message peut-être parti.
        #[arg(long)]
        forget: Option<i64>,
    },
    /// Importe un profil Thunderbird dans le store. Lecture seule sur le profil.
    ///
    /// Avec `--daemon`, l'import devient une tâche de fond du démon : `--profile` est alors
    /// sans objet — c'est l'opérateur du démon qui déclare les profils importables, et on en
    /// choisit un avec `--source`.
    Import {
        /// Racine du profil Thunderbird. Sans objet avec `--daemon`.
        #[arg(long, env = "MAILCORE_TB_PROFILE")]
        profile: Option<Utf8PathBuf>,
        /// Le rang du profil déclaré côté démon. Voir `mail daemon status`.
        #[arg(long)]
        source: Option<usize>,
        /// Tout lire et tout compter sans rien écrire.
        #[arg(long)]
        dry_run: bool,
        /// Importer aussi les comptes de flux RSS.
        ///
        /// Écartés par défaut : mailcore est un client mail, et les articles d'un
        /// agrégateur noieraient la liste et l'index. Le bilan dit toujours combien de
        /// dossiers ont été écartés.
        #[arg(long)]
        include_feeds: bool,
        /// Importer aussi les répertoires de comptes absents de `prefs.js`.
        ///
        /// Écartés par défaut : Thunderbird ne supprime pas les mbox d'un compte retiré de
        /// sa configuration, et ces restes ne sont pas du courrier vivant. Le bilan dit
        /// toujours combien de dossiers ont été écartés.
        #[arg(long)]
        include_orphans: bool,
        /// Rendre la main dès que la tâche est acceptée, sans suivre sa progression.
        #[arg(long)]
        detach: bool,
    },
    /// (Re)construit l'index plein texte depuis le store.
    Index {
        /// Rendre la main dès que la tâche est acceptée. Sans objet en local.
        #[arg(long)]
        detach: bool,
    },
    /// (Re)construit les fils de discussion depuis le store.
    Thread {
        /// Rendre la main dès que la tâche est acceptée. Sans objet en local.
        #[arg(long)]
        detach: bool,
    },
    /// La source d'un message : les octets tels qu'ils sont.
    ///
    /// Ce que le message porte, et rien d'autre : ni sujet décodé, ni en-tête déplié, ni HTML
    /// rendu. C'est ce qui permet de vérifier la promesse de `docs/PHASE-3.md` plutôt que de
    /// la croire — et `--outbox` est la moitié qui compte, puisque c'est le seul message que
    /// mailcore compose lui-même.
    Source {
        /// L'identifiant du message, tel que `mail search` le rend.
        #[arg(long, conflicts_with = "outbox")]
        id: Option<i64>,
        /// L'identifiant d'une ligne de la file d'envoi, tel que `mail outbox` le rend.
        #[arg(long)]
        outbox: Option<i64>,
        /// N'afficher que le bloc d'en-têtes.
        #[arg(long)]
        headers: bool,
    },
    /// Recherche dans le corpus indexé.
    Search {
        /// La requête. Syntaxe tantivy : `facture`, `"phrase exacte"`, `from:plombier`.
        query: Vec<String>,
        /// Nombre maximum de résultats.
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Statistiques du store : blobs, références, taux de dédup.
    Stats,
    /// Diagnostic : cohérence du store, blobs orphelins, dérive de l'index.
    Doctor {
        /// Supprime les blobs que plus aucun message et plus aucune ligne de file ne désignent.
        ///
        /// ## À n'utiliser que quand rien n'écrit
        ///
        /// Un import ou une moisson écrit ses blobs **avant** les lignes qui les désignent —
        /// c'est l'ordre qui évite qu'une ligne pointe vers un blob absent. Un blob
        /// fraîchement écrit ressemble donc à un orphelin, et le purger supprimerait du
        /// courrier en cours d'arrivée.
        ///
        /// C'est pour ça que rien ne l'appelle automatiquement, et que c'est un drapeau
        /// explicite plutôt qu'un comportement par défaut du diagnostic.
        #[arg(long)]
        purge_orphans: bool,
    },
}

/// La racine du store : celle demandée, ou le répertoire de données de l'utilisateur.
///
/// # Errors
///
/// Si la plateforme ne sait pas dire où vivent les données utilisateur.
/// Ce que `mail source` doit lire, à partir des deux drapeaux exclusifs.
///
/// Refuser l'absence des deux plutôt que d'en choisir un par défaut : un défaut ferait lire le
/// message #1 à qui a tapé `mail source` en pensant à sa dernière ligne de file.
fn source_target(id: Option<i64>, outbox: Option<i64>) -> Result<commands::source::Target> {
    match (id, outbox) {
        (Some(id), None) => Ok(commands::source::Target::Message(id)),
        (None, Some(id)) => Ok(commands::source::Target::Outgoing(id)),
        _ => anyhow::bail!("indiquer ce qu'il faut lire : --id <message> ou --outbox <ligne>"),
    }
}

fn store_root(requested: Option<&Utf8PathBuf>) -> Result<Utf8PathBuf> {
    match requested {
        Some(root) => Ok(root.clone()),
        None => mailcore::store::default_root().context("répertoire de données par défaut"),
    }
}

fn main() -> Result<()> {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn"));
    tracing_subscriber::fmt().with_env_filter(filter).init();

    let cli = Cli::parse();
    let store = cli.store.as_ref();
    let daemon = cli.daemon.as_deref();

    match (&cli.command, daemon) {
        // --- Contre un démon ---
        (Command::Daemon { command }, Some(daemon)) => match command {
            DaemonCommand::Login => commands::remote::login(daemon),
            DaemonCommand::Logout => commands::remote::logout(daemon),
            DaemonCommand::Status => commands::remote::status(daemon),
        },
        (
            Command::Import {
                source,
                dry_run,
                detach,
                profile,
                include_feeds,
                include_orphans,
            },
            Some(daemon),
        ) => {
            if profile.is_some() {
                // Le dire plutôt que de l'ignorer en silence : quelqu'un qui passe les deux
                // croit que son chemin sera utilisé, et il ne le sera pas.
                eprintln!(
                    "Note : --profile est ignoré avec --daemon. Les profils importables sont \
                     déclarés par l'opérateur du démon ; voir `mail daemon status`."
                );
            }
            commands::remote::job(
                daemon,
                &commands::remote::JobRequest {
                    kind: "import",
                    account: None,
                    source: *source,
                    dry_run: *dry_run,
                    include_feeds: *include_feeds,
                    include_orphans: *include_orphans,
                    follow: !*detach,
                },
            )
        }
        (Command::Index { detach }, Some(daemon)) => commands::remote::job(
            daemon,
            &commands::remote::JobRequest {
                kind: "index",
                account: None,
                source: None,
                dry_run: false,
                include_feeds: false,
                include_orphans: false,
                follow: !*detach,
            },
        ),
        (Command::Thread { detach }, Some(daemon)) => commands::remote::job(
            daemon,
            &commands::remote::JobRequest {
                kind: "thread",
                account: None,
                source: None,
                dry_run: false,
                include_feeds: false,
                include_orphans: false,
                follow: !*detach,
            },
        ),
        (Command::Search { query, limit }, Some(daemon)) => {
            commands::remote::search(daemon, &query.join(" "), *limit)
        }
        (
            Command::Source {
                id,
                outbox,
                headers,
            },
            Some(daemon),
        ) => commands::remote::source(daemon, source_target(*id, *outbox)?, *headers),
        (Command::Stats, Some(daemon)) => commands::remote::stats(daemon),
        // **Tout ce qui touche à un compte reste local au démon**, et pour une seule raison :
        // le trousseau. Celui de *ce* poste n'est pas celui qui se connectera au serveur IMAP.
        //
        // Ça vaut pour la déclaration — le compte se déclarerait ici et la synchronisation
        // échouerait là-bas, sur un secret introuvable — mais aussi pour `probe`, qui doit
        // ouvrir une connexion IMAP **avec le secret du compte** : la lancer d'ici sonderait le
        // serveur depuis le mauvais réseau, avec le mauvais trousseau, et ne dirait rien de ce
        // que le démon voit.
        // L'envoi est la même situation que les comptes, en plus tranchée : le secret du
        // compte est dans le trousseau du démon, et **la file d'envoi est dans son store**.
        // Composer ici puis remettre là-bas demanderait de transporter le message par le
        // réseau, ce qui est un protocole qui n'existe pas encore.
        (Command::Contacts { .. }, Some(_)) => anyhow::bail!(
            "le carnet vit dans le store du démon : c'est lui qui le reconstruit. Lancer \n             `mail contacts` sur la machine du démon, sans --daemon."
        ),
        (Command::Send { .. } | Command::Outbox { .. }, Some(_)) => anyhow::bail!(
            "l'envoi vit du côté du démon : c'est son trousseau qui porte le secret et son \n             store qui porte la file d'envoi. Lancer `mail send` sur la machine du démon, \n             sans --daemon."
        ),
        (Command::Account { .. }, Some(_)) => anyhow::bail!(
            "ce qui touche à un compte vit du côté du démon : c'est lui qui se connecte au \
             serveur IMAP, donc c'est son trousseau qui porte le secret. Lancer \
             `mail account …` sur la machine du démon, sans --daemon.\n\
             \n\
             `mail sync --daemon` marche, en revanche : la synchronisation est une tâche de \
             fond du démon, qui lit son propre trousseau."
        ),
        // La synchronisation, elle, se déclenche à distance : c'est une tâche de fond comme
        // l'import, et c'est le démon qui fait le travail avec ses propres secrets.
        (Command::Sync { account }, Some(daemon)) => commands::remote::job(
            daemon,
            &commands::remote::JobRequest {
                kind: "sync",
                account: *account,
                source: None,
                dry_run: false,
                include_feeds: false,
                include_orphans: false,
                follow: true,
            },
        ),
        (Command::Doctor { .. }, Some(_)) => anyhow::bail!(
            "`doctor` inspecte les blobs, les orphelins et la dérive de l'index : ça demande \
             un accès au store, pas une API. L'exposer voudrait dire ouvrir un chemin de \
             lecture arbitraire dans le stockage. Lancer la commande sur la machine du démon, \
             sans --daemon."
        ),

        // --- Contre le store local ---
        (Command::Daemon { .. }, None) => anyhow::bail!(
            "`daemon` a besoin de savoir de quel démon on parle : ajouter --daemon <hôte>."
        ),
        (
            Command::Import {
                profile,
                dry_run,
                include_feeds,
                include_orphans,
                ..
            },
            None,
        ) => {
            let profile = profile.as_ref().context(
                "chemin du profil requis en local : --profile <chemin>, ou MAILCORE_TB_PROFILE",
            )?;
            commands::import::run(profile, store, *dry_run, *include_feeds, *include_orphans)
        }
        (Command::Index { .. }, None) => commands::index::run(store),
        (Command::Thread { .. }, None) => commands::thread::run(store),
        (Command::Search { query, limit }, None) => {
            commands::search::run(store, &query.join(" "), *limit)
        }
        (
            Command::Source {
                id,
                outbox,
                headers,
            },
            None,
        ) => commands::source::run(store, source_target(*id, *outbox)?, *headers),
        (Command::Stats, None) => commands::stats::run(store),
        (Command::Doctor { purge_orphans }, None) => commands::doctor::run(store, *purge_orphans),
        (
            Command::Send {
                account,
                to,
                cc,
                bcc,
                subject,
                body,
                attach,
                signature,
                dry_run,
                flush,
            },
            None,
        ) => {
            if *flush {
                commands::send::flush(store)
            } else {
                // `required_unless_present` garantit la présence hors `--flush` ; le
                // `context` est là pour que le jour où la contrainte bouge, l'erreur soit
                // lisible plutôt qu'une panique.
                let account = account.context("`--account` est obligatoire sans `--flush`")?;
                commands::send::run(
                    store,
                    &commands::send::Message {
                        account,
                        to: to.clone(),
                        cc: cc.clone(),
                        bcc: bcc.clone(),
                        subject: subject.clone(),
                        body: body.clone(),
                        attach: attach.clone(),
                        signature: *signature,
                        dry_run: *dry_run,
                    },
                )
            }
        }
        (
            Command::Outbox {
                resend,
                accept,
                retry,
                forget,
            },
            None,
        ) => match (resend, accept, retry, forget) {
            (Some(id), _, _, _) => commands::send::decide(store, *id, "resend"),
            (_, Some(id), _, _) => commands::send::decide(store, *id, "accept"),
            (_, _, Some(id), _) => commands::send::retry(store, *id),
            (_, _, _, Some(id)) => commands::send::forget(store, *id),
            (None, None, None, None) => commands::send::list(store),
        },
        (Command::Contacts { command }, None) => match command {
            ContactsCommand::Rebuild => commands::contacts::rebuild(store),
            ContactsCommand::Complete { prefix, limit } => {
                commands::contacts::complete(store, prefix, *limit)
            }
            ContactsCommand::Stats => commands::contacts::stats(store),
        },
        (Command::Account { command }, None) => match command {
            AccountCommand::Add {
                host,
                port,
                username,
                security,
                auth,
                client_id,
                redirect_port,
                renew,
            } => commands::account::add(
                store,
                commands::account::Declaration {
                    host,
                    port: *port,
                    username,
                    security,
                    auth,
                    client_id: client_id.as_deref(),
                    redirect_port: *redirect_port,
                    renew: *renew,
                },
            ),
            AccountCommand::List => commands::account::list(store),
            AccountCommand::Probe { account, detail } => {
                // Même convention que `mail sync` : un compte injoignable est affiché, puis
                // le code de sortie le dit.
                if commands::probe::run(store, *account, *detail)? {
                    Ok(())
                } else {
                    anyhow::bail!("au moins un compte n'a pas pu être interrogé")
                }
            }
            AccountCommand::Submission {
                account,
                host,
                port,
                security,
                username,
                auth,
                clear,
            } => commands::account::submission(
                store,
                *account,
                commands::account::Submission {
                    host: host.as_deref(),
                    port: *port,
                    security,
                    username: username.as_deref(),
                    auth: auth.as_deref(),
                    clear: *clear,
                },
            ),
            AccountCommand::Forget { id } => commands::account::forget(store, *id),
        },
        (Command::Sync { account }, None) => {
            // **Le code de sortie porte le verdict.** Un compte en échec ne doit pas rendre
            // zéro : un planificateur qui relance la commande toutes les dix minutes n'a que
            // ça pour savoir qu'il s'est passé quelque chose.
            if commands::sync::run(store, *account)? {
                Ok(())
            } else {
                anyhow::bail!("au moins un compte n'a pas pu être synchronisé")
            }
        }
    }
}
