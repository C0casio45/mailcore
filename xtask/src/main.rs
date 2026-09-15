//! # xtask
//!
//! L'outillage de mesure, en Rust — règle 2 du `CLAUDE.md` : pas de script shell, y compris
//! pour les bancs de mesure.
//!
//! Les critères de `docs/PHASE-1.md` sont à **mesurer, pas à estimer**. Ce binaire est ce
//! qui les mesure. Invocation : `cargo xtask <commande>` (alias dans `.cargo/config.toml`).
//!
//! Le chemin du profil Thunderbird n'est jamais écrit en dur : il se passe en argument, ou
//! par la variable d'environnement `MAILCORE_TB_PROFILE`. C'est une donnée de la machine de
//! l'utilisateur, pas du projet.

#![forbid(unsafe_code)]

mod api;
mod browser;
mod calendar;
mod client;
mod corpus;
mod followup;
mod inspect;
mod measure;
mod profile;
mod recall;
mod replies;
mod schema;
mod secrets;
mod ui;

use anyhow::Result;
use camino::Utf8PathBuf;
use clap::{Parser, Subcommand};

/// Outillage de mesure des critères d'acceptation.
#[derive(Debug, Parser)]
#[command(name = "xtask", about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Inventaire d'un profil : comptes, dossiers, tailles. Métadonnées seulement.
    ProfileScan {
        /// Racine du profil Thunderbird.
        #[arg(long, env = "MAILCORE_TB_PROFILE")]
        profile: Utf8PathBuf,
    },

    /// Relève taille et `mtime` de chaque fichier du profil.
    ///
    /// À lancer avant et après un import : les deux relevés doivent être identiques sur les
    /// mbox (critère 7 — exactement zéro écriture dans le profil).
    ProfileSnapshot {
        /// Racine du profil Thunderbird.
        #[arg(long, env = "MAILCORE_TB_PROFILE")]
        profile: Utf8PathBuf,
        /// Où écrire le relevé. Doit être hors du profil.
        #[arg(long, default_value = "measurements/profile.tsv")]
        out: Utf8PathBuf,
    },

    /// Compare deux relevés et signale toute différence sur un mbox.
    ProfileDiff {
        /// Le relevé d'avant.
        before: Utf8PathBuf,
        /// Le relevé d'après.
        after: Utf8PathBuf,
    },

    /// Lâche le lecteur mbox sur le corpus réel, en lecture seule.
    ///
    /// Rend le nombre de messages, le taux de dédup mesuré, l'espace mort des messages
    /// supprimés non compactés, et la convention de *From-mangling* de l'écrivain. N'écrit
    /// rien, ni dans le profil ni dans un store.
    ProfileProbe {
        /// Racine du profil Thunderbird.
        #[arg(long, env = "MAILCORE_TB_PROFILE")]
        profile: Utf8PathBuf,
        /// Plafond d'octets lus par fichier. Sans plafond, lit tout le corpus.
        #[arg(long)]
        budget: Option<u64>,
    },

    /// Mesure un import : durée, RSS crête, taux de dédup (critères 3 et 6).
    MeasureImport {
        /// Racine du profil Thunderbird.
        #[arg(long, env = "MAILCORE_TB_PROFILE")]
        profile: Utf8PathBuf,
        /// Racine du store à remplir. Un répertoire jetable convient.
        #[arg(long, default_value = "measurements/store")]
        store: Utf8PathBuf,
        /// Tout lire et tout compter sans rien écrire.
        #[arg(long)]
        dry_run: bool,
    },

    /// Mesure ce que coûtent les passes qui suivent une moisson : index et carnet.
    ///
    /// **Aucun `--store`** : le banc écrit, donc il crée son propre répertoire jetable. Un
    /// paramètre qui peut désigner le store réel le désignera — voir `measure-attachment`, qui
    /// a laissé 533 Mo dans les données de production le 2026-09-09.
    MeasureFollowup {
        /// Taille du corpus synthétique.
        #[arg(long, default_value_t = 20_000)]
        messages: usize,
    },

    /// Mesure la latence de recherche et rend le p95 (critère 4).
    MeasureSearch {
        /// Racine du store indexé.
        #[arg(long, default_value = "measurements/store")]
        store: Utf8PathBuf,
        /// Répétitions du jeu de requêtes.
        #[arg(long, default_value_t = 5)]
        repeats: usize,
    },

    /// Mesure le critère 3 : une synchronisation incrémentale où rien n'a changé.
    ///
    /// Lance `mail sync --account N` plusieurs fois par compte et rend les relevés bruts, la
    /// première passe à part — elle porte la connexion et le jeton — puis la médiane des
    /// suivantes.
    ///
    /// **Ne compile pas.** Construire d'abord, laisser la machine retomber au repos, puis
    /// mesurer : c'est la règle du `CLAUDE.md`, et elle vient d'un relevé faux d'un facteur
    /// trois.
    MeasureSync {
        /// Racine du store réel à synchroniser.
        #[arg(long, default_value = "measurements/store-imap")]
        store: Utf8PathBuf,
        /// Passes par compte. Cinq, parce que Gmail est bimodal.
        #[arg(long, default_value_t = 5)]
        passes: usize,
        /// Mesurer avec le binaire `release` plutôt que celui de débogage.
        #[arg(long)]
        release: bool,
    },

    /// Mesure la crête mémoire d'une **moisson complète** sur un vrai compte — critère 2.
    ///
    /// Moissonne dans un store jetable, donc sans toucher au store réel : c'est ce qui lève
    /// la réserve du critère 2, qu'une moisson complète était devenue impossible à provoquer.
    ///
    /// **Ne compile pas.** Construire d'abord, laisser la machine retomber, puis mesurer.
    MeasureHarvestRss {
        /// Racine du store réel, lue pour y trouver le compte.
        #[arg(long, default_value = "measurements/store-imap")]
        store: Utf8PathBuf,
        /// Le compte à moissonner. Une moisson complète consomme de la bande passante.
        #[arg(long)]
        account: i64,
        /// Contenus à écrire avant d'annuler. Borne la dépense, pas la validité.
        #[arg(long, default_value_t = 1_000)]
        messages: u64,
    },

    /// Mesure un message à grosses pièces jointes — critère 3 de `docs/PHASE-3.md`.
    ///
    /// Assemble, met en file et remet un message dont la pièce jointe fait la taille demandée,
    /// en relevant la mémoire résidente à chaque étape. Le seuil est **relatif autant
    /// qu'absolu** : la mesure échoue si la croissance dépasse le tiers de la taille du
    /// message, quel que soit le seuil absolu.
    ///
    /// **Elle crée son propre store jetable** et ne prend pas de `--store` : la première
    /// version en prenait un, elle a été visée sur le store réel, et elle y a laissé deux
    /// lignes de file et 533 Mo de blobs orphelins. Une mesure qui écrit n'a rien à faire dans
    /// un store qu'on garde.
    MeasureAttachment {
        /// La taille de la pièce jointe, en mégaoctets.
        #[arg(long, default_value_t = 25)]
        megabytes: u64,
    },
    /// Mesure la complétion d'un destinataire — critère 4 de `docs/PHASE-3.md`.
    ///
    /// Simule une **frappe** et non une requête : `a`, puis `an`, puis `ann`… Les préfixes
    /// viennent du carnet réel, parce que leur distribution de premières lettres est celle du
    /// corpus de l'utilisateur.
    MeasureComplete {
        /// Racine du store. Le carnet doit avoir été construit — `mail contacts rebuild`.
        #[arg(long)]
        store: Utf8PathBuf,
        /// Combien de fois répéter la séquence complète.
        #[arg(long, default_value_t = 20)]
        repeats: usize,
    },
    /// Mesure le critère 6 sur les **vrais** comptes : aucun identifiant en clair.
    ///
    /// Lit les secrets au trousseau — mot de passe, jeton de rafraîchissement, secret client,
    /// jeton d'accès — synchronise pour de bon en capturant les journaux au niveau `TRACE`,
    /// puis cherche ces octets-là dans tout le store, dans les journaux et dans `%TEMP%`.
    ///
    /// **Les secrets ne sont ni affichés ni écrits nulle part** : le contrôle positif qui
    /// prouve que le chercheur les trouverait est fait en mémoire.
    MeasureSecrets {
        /// Racine du store réel à synchroniser et à fouiller.
        #[arg(long, default_value = "measurements/store-imap")]
        store: Utf8PathBuf,
        /// N'auditer que ce compte. Par défaut, tous les comptes actifs.
        #[arg(long)]
        account: Option<i64>,
    },

    /// Mesure les critères 4 et 5 **à travers l'API du démon**, pas en processus.
    ///
    /// Démarre `maild` en release sur le bouclage, l'interroge en HTTP avec le même jeu de
    /// requêtes que `measure-search`, et rend les percentiles. La différence entre les deux
    /// relevés est ce que coûte tout ce qui n'est pas l'index : JSON, verrou, pile HTTP,
    /// aller-retour TCP.
    MeasureApi {
        /// Racine du store indexé.
        #[arg(long, default_value = "measurements/store")]
        store: Utf8PathBuf,
        /// Répétitions du jeu de requêtes.
        #[arg(long, default_value_t = 5)]
        repeats: usize,
    },

    /// Compte ce que les sujets du corpus exigent d'un moteur de texte.
    ///
    /// Décide du choix d'un toolkit natif : un toolkit sans façonnage rend faux — et pas
    /// seulement moins joli — tout sujet en arabe, en hébreu ou en devanagari.
    CorpusScripts {
        /// Racine du store à lire.
        #[arg(long, default_value = "measurements/store")]
        store: Utf8PathBuf,
    },

    /// Mesure le critère 1 de la phase 4 : le rappel sur un jeu de requêtes à réponse connue.
    ///
    /// Le premier relevé porte sur **tantivy seul**, avant qu'un moteur sémantique existe :
    /// c'est le chiffre auquel tout le reste de la phase se comparera.
    ///
    /// Le jeu de requêtes nomme des messages réels, donc il vit dans `measurements/`, qui est
    /// gitignoré. Il est lié au store sur lequel il a été écrit : une cible absente fait
    /// échouer la commande au lieu de rendre un chiffre présentable.
    MeasureRecall {
        /// Racine du store indexé.
        #[arg(long, default_value = "measurements/store")]
        store: Utf8PathBuf,
        /// Le jeu de requêtes et leurs réponses attendues.
        #[arg(long, default_value = "measurements/phase4-queries.toml")]
        queries: Utf8PathBuf,
        /// Combien de résultats demander au moteur. Au-delà du top mesuré, pour voir de
        /// combien une cible manquée est manquée.
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },

    /// Sort un échantillon de messages réels, étalé sur toute la période du corpus.
    ///
    /// Sert l'étape 1 de `docs/PHASE-4.md` : écrire des requêtes en français avec la réponse
    /// qu'elles doivent trouver. C'est la seule étape de la phase qu'un programme ne peut pas
    /// faire, et elle demande d'avoir de vrais messages sous les yeux.
    ///
    /// **Ne fait que lire**, donc elle peut viser le store de production — c'est même son
    /// intérêt : un jeu de requêtes ne vaut que sur du vrai courrier.
    CorpusSample {
        /// Racine du store à lire.
        #[arg(long, default_value = "measurements/store")]
        store: Utf8PathBuf,
        /// Combien de messages tirer.
        #[arg(long, default_value_t = 30)]
        count: usize,
        /// Combien de caractères de corps montrer par message.
        #[arg(long)]
        excerpt: Option<usize>,
        /// Au-dessus de ce nombre de messages, un expéditeur est une source récurrente et ses
        /// messages ne sont pas tirés — sauf si l'utilisateur lui a écrit.
        ///
        /// Une facture arrive une fois ; Facebook écrit quatre cents fois. C'est la rareté qui
        /// distingue un message qu'on cherchera d'un fil de vie qu'on ne relit jamais.
        #[arg(long, default_value_t = 5)]
        max_from: usize,
        /// Tirer sans filtre, notifications comprises.
        ///
        /// Par défaut, seuls les messages d'une adresse à qui l'utilisateur a **écrit** sont
        /// tirés : un tirage uniforme sur un corpus réel donne neuf notifications sur dix.
        #[arg(long)]
        everything: bool,
    },

    /// Mesure quel lanceur ouvre vraiment le navigateur, et à quelle longueur d'URL.
    ///
    /// Ouvre un port éphémère sur le bouclage, donne son URL à chaque lanceur candidat, et
    /// attend une connexion. **La preuve est la connexion reçue, jamais le code de sortie** :
    /// `rundll32` comme `explorer.exe` rendent 0 dans des cas où aucun onglet ne s'ouvre.
    /// La cible reçue est comparée à l'octet, pour attraper le développement des `%XX` par
    /// `cmd`.
    ///
    /// Chaque lanceur qui fonctionne ouvre un onglet : c'est le résultat attendu. Rien ne sort
    /// de la machine, aucun compte n'est utilisé.
    BrowserProbe {
        /// Longueur totale de l'URL sondée. Le défaut reproduit une URL de consentement réelle.
        #[arg(long, default_value_t = 420)]
        length: usize,
        /// Secondes d'attente d'une connexion, par candidat.
        #[arg(long, default_value_t = 8)]
        wait: u64,
    },

    /// Les dossiers d'un store, avec leurs compteurs, et celui que la coquille ouvrirait.
    ///
    /// Diagnostic de lecture seule, utilisable pendant une synchronisation.
    StoreFolders {
        /// Racine du store à lire.
        #[arg(long)]
        store: Utf8PathBuf,
        /// Masquer les dossiers en dessous de ce nombre de messages.
        #[arg(long, default_value_t = 1)]
        min: u64,
    },

    /// Vérifie la **conformité d'une réponse** sur les vrais fils du corpus — critère 1.
    ///
    /// Construit une réponse à chaque message qui porte un `Message-ID`, l'assemble en octets
    /// RFC 5322, relit ces octets, et compare. **Rien n'est envoyé, rien n'est écrit.**
    MeasureReplies {
        /// Le store à parcourir.
        #[arg(long)]
        store: Utf8PathBuf,
        /// S'arrêter après ce nombre de réponses. `0` pour tout le corpus.
        #[arg(long, default_value_t = 0)]
        limit: usize,
    },

    /// Mesure le **critère 6** : le lecteur d'invitations passé sur toutes les pièces
    /// `text/calendar` du corpus.
    ///
    /// Lues juste, ou refusées en nommant ce qui manque. Une pièce illisible **sans raison
    /// nommée** est le seul échec possible. Rien n'est écrit, et aucune requête ne peut partir.
    MeasureInvitations {
        /// Le store à parcourir.
        #[arg(long)]
        store: Utf8PathBuf,
        /// Afficher les manques relevés, avec l'identifiant du message qui les porte.
        ///
        /// Une phrase par manque et un numéro de message : de quoi aller voir la pièce fautive.
        /// Aucun contenu d'invitation n'est affiché.
        #[arg(long)]
        gaps: bool,
    },

    /// Relève la forme des pièces `text/calendar` du corpus : combien, quelles propriétés,
    /// quels fuseaux, quels producteurs.
    ///
    /// Des formes et des comptes, **jamais un contenu** : une invitation dit avec qui
    /// l'utilisateur déjeune et où. Rien n'est écrit.
    CorpusCalendar {
        /// Le store à parcourir.
        #[arg(long)]
        store: Utf8PathBuf,
    },

    /// Migre une **copie** d'un index réel et vérifie que rien n'a été perdu.
    ///
    /// L'original n'est jamais ouvert en écriture. Rend la durée de la migration, les comptes
    /// de lignes avant et après, les références orphelines et l'intégrité SQLite.
    SchemaCheck {
        /// L'index à copier. Typiquement `measurements/store3/index.sqlite`.
        #[arg(long)]
        db: Utf8PathBuf,
        /// Où écrire la copie. Doit être un chemin jetable, hors du store d'origine.
        #[arg(long)]
        work: Utf8PathBuf,
    },

    /// Mesure les critères 1 et 2 dans une coquille, avec son vrai moteur de rendu.
    ///
    /// Lance l'application en release, la chronomètre depuis le `spawn` et lit le relevé
    /// qu'elle consigne sur sa sortie d'erreur. Le front est reconstruit d'abord : il est
    /// embarqué dans l'exécutable à la compilation.
    MeasureUi {
        /// Racine du store à ouvrir.
        #[arg(long)]
        store: Utf8PathBuf,
        /// `shell` — la coquille native egui. `tauri` — la coquille Tauri et son front Solid.
        /// `native` et `software` — les sondes jetables, pour comparer sur le même harnais.
        #[arg(long, default_value = "shell")]
        shell: String,
        /// `startup` — critère 1 seul, l'application se referme aussitôt.
        /// `scroll` — charge le plus gros dossier et relève le critère 2.
        /// `open` — ouvre trente messages depuis la liste et relève le critère 5.
        /// `signature` — tape dans l'éditeur de signature et relève le critère 5 de la phase 3.
        /// Le store est jetable, `--store` est alors sans objet.
        /// `sync` — défile pendant qu'une moisson écrit dans le store : critère 4 de la
        /// phase 2, contre `mailfake`. Le store est jetable, `--store` est alors sans objet.
        /// `sync-real` — le même, mais contre un **vrai** serveur : `--store` sert à trouver
        /// le compte, et la moisson écrit dans un store jetable. Retélécharge le compte.
        /// Voir `--account`.
        /// `startup-remote` — lance un vrai démon et mesure ce que le cache de lecture achète.
        /// `offline` — lance un vrai démon, le coupe, et relève le critère 9.
        #[arg(long, default_value = "startup")]
        bench: String,
        /// Exécutions. La première est le démarrage à froid, les suivantes donnent la médiane.
        #[arg(long, default_value_t = 5)]
        runs: usize,
        /// Le compte à moissonner, pour le banc `sync-real`. Sans objet ailleurs.
        ///
        /// Par défaut **tous** les comptes actifs, en parallèle : c'est ce que dit le critère 4,
        /// et un seul compte ne fait pas assez écrire le store pendant la fenêtre de
        /// défilement. Nommer un compte sert à en isoler un.
        ///
        /// Les moissons sont annulées dès la coquille refermée, donc le coût en bande passante
        /// est celui de la durée du banc, pas celui des comptes.
        #[arg(long)]
        account: Option<i64>,
        /// Ne pas compiler : mesurer l'exécutable déjà présent.
        ///
        /// **À utiliser pour tout relevé qu'on publie.** Compiler puis mesurer aussitôt gonfle
        /// les chiffres — d'un facteur trois sur les démarrages du 2026-09-02, de 45 % sur le
        /// critère 5. Compiler d'abord, laisser la machine retomber, puis mesurer avec ce
        /// drapeau.
        #[arg(long)]
        no_build: bool,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::ProfileScan { profile } => profile::scan(profile.as_std_path()),
        Command::ProfileSnapshot { profile, out } => profile::snapshot(profile.as_std_path(), &out),
        Command::ProfileDiff { before, after } => profile::diff(&before, &after),
        Command::ProfileProbe { profile, budget } => profile::probe(profile.as_std_path(), budget),
        Command::MeasureImport {
            profile,
            store,
            dry_run,
        } => measure::import(&profile, &store, dry_run),
        Command::MeasureSearch { store, repeats } => measure::search(&store, repeats),
        Command::MeasureSync {
            store,
            passes,
            release,
        } => measure::sync(&store, passes, release),
        Command::MeasureHarvestRss {
            store,
            account,
            messages,
        } => measure::harvest_rss(&store, account, messages),
        Command::MeasureAttachment { megabytes } => measure::attachment(megabytes),
        Command::MeasureComplete { store, repeats } => measure::complete(&store, repeats),
        Command::MeasureSecrets { store, account } => secrets::audit(&store, account),
        Command::MeasureApi { store, repeats } => api::measure(&store, repeats),
        Command::CorpusScripts { store } => corpus::scripts(&store),
        Command::MeasureRecall {
            store,
            queries,
            limit,
        } => recall::measure(&store, &queries, limit),
        Command::CorpusSample {
            store,
            count,
            excerpt,
            everything,
            max_from,
        } => corpus::sample(&store, count, excerpt, everything, max_from),
        Command::CorpusCalendar { store } => calendar::survey(&store),
        Command::MeasureInvitations { store, gaps } => calendar::measure(&store, gaps),
        Command::MeasureReplies { store, limit } => replies::measure(&store, limit),
        Command::MeasureFollowup { messages } => followup::measure(messages),
        Command::SchemaCheck { db, work } => schema::check(&db, &work),
        Command::StoreFolders { store, min } => inspect::folders(&store, min),
        Command::BrowserProbe { length, wait } => {
            browser::probe(length, std::time::Duration::from_secs(wait))
        }
        Command::MeasureUi {
            store,
            shell,
            bench,
            runs,
            account,
            no_build,
        } => {
            let shell = match shell.as_str() {
                "tauri" => ui::Shell::Tauri,
                "shell" => ui::Shell::Egui,
                "native" => ui::Shell::SpikeGl,
                "software" => ui::Shell::SpikeCpu,
                other => anyhow::bail!(
                    "coquille inconnue : {other} — attendu `tauri`, `shell`, `native` ou `software`"
                ),
            };
            ui::set_skip_build(no_build);
            ui::measure(&store, shell, &bench, runs, account)
        }
    }
}
