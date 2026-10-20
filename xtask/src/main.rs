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
mod attach;
mod bertmodel;
mod browser;
mod calendar;
mod client;
mod corpus;
mod findq;
mod followup;
mod guard;
mod inspect;
mod measure;
mod merge;
mod model;
mod pdf;
mod pdfcompare;
mod pdfeffect;
mod probe;
mod profile;
mod rebind;
mod recall;
mod replies;
mod scan;
mod schema;
mod secrets;
mod semantic;
mod ui;
mod vectors;
mod words;

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

    /// Ce que coûte et rapporte la fusion des segments de l'index plein texte, sur deux répliques
    /// d'un store réel : la première arrivée, qui fusionne l'arriéré, les suivantes, et la recherche
    /// avant et après. **Ne compile pas** : compiler d'abord, laisser la machine retomber.
    ///
    /// Le store est lu, jamais écrit : le banc travaille sur des copies, et une arrivée y est un
    /// message existant remis à indexer — jamais un blob neuf, qui passerait par le lien des blobs.
    MeasureMerge {
        /// Le store d'origine. Lu, jamais écrit.
        #[arg(long, default_value = "measurements/store")]
        store: Utf8PathBuf,
        /// Combien d'arrivées, une par passe ; la première fusionne l'arriéré.
        #[arg(long, default_value_t = 9)]
        arrivals: usize,
        /// Tours du jeu de requêtes ; le premier est jeté.
        #[arg(long, default_value_t = 4)]
        repeats: usize,
        /// Un jeu de requêtes du critère 1 de la phase 4, dont seuls les textes sont lus : des
        /// phrases, à comparer aux mots seuls. Absent, le banc ne compare que les mots.
        #[arg(long)]
        phrases: Option<Utf8PathBuf>,
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

    /// Taille le vocabulaire d'un modèle d'embeddings sur ce que le corpus emploie.
    ///
    /// Le modèle multilingue porte 500 353 tokens pour cent langues ; un corpus de courrier en
    /// emploie une fraction. Ne garder que les lignes utiles est ce qui rend le critère 5
    /// atteignable — voir `docs/PHASE-4.md`.
    ///
    /// Ne touche ni au store ni au modèle d'origine : elle lit les deux et écrit ailleurs.
    ModelTrim {
        /// Racine du store dont le vocabulaire sert de référence.
        #[arg(long, default_value = "measurements/store")]
        store: Utf8PathBuf,
        /// Le modèle d'origine : `config.json`, `tokenizer.json`, `model.safetensors`.
        #[arg(long)]
        model: Utf8PathBuf,
        /// Où écrire le modèle taillé.
        #[arg(long)]
        out: Utf8PathBuf,
    },

    /// Vérifie qu'un modèle taillé rend les mêmes vecteurs que celui dont il vient.
    ///
    /// Une carte d'index fausse d'un cran donne un modèle qui charge, encode, et rend des
    /// vecteurs entièrement faux sans que rien ne le signale.
    ModelCheck {
        /// Racine du store d'où viennent les messages comparés.
        #[arg(long, default_value = "measurements/store")]
        store: Utf8PathBuf,
        /// Le modèle d'origine.
        #[arg(long)]
        model: Utf8PathBuf,
        /// Le modèle taillé.
        #[arg(long)]
        trimmed: Utf8PathBuf,
        /// Combien de messages comparer.
        #[arg(long, default_value_t = 200)]
        count: usize,
    },

    /// Mesure ce que coûte la vectorisation : débit par message, et mémoire résidente.
    ///
    /// **Ne compile pas.** Construire d'abord, laisser la machine retomber au repos, puis
    /// mesurer — et jeter la première des trois exécutions.
    ModelBench {
        /// Racine du store d'où viennent les messages.
        #[arg(long, default_value = "measurements/store")]
        store: Utf8PathBuf,
        /// Le modèle à mesurer.
        #[arg(long)]
        model: Utf8PathBuf,
        /// Combien de messages encoder.
        #[arg(long, default_value_t = 1000)]
        count: usize,
        /// Combien de messages par lot remis au modèle.
        #[arg(long, default_value_t = 256)]
        batch: usize,
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
        /// Combien de résultats demander au moteur. **Par défaut, tout le corpus** : une cible
        /// classée au-delà de la limite s'affiche comme absente, et c'est l'absence fabriquée
        /// que le 2026-09-23 avait retirée — la valeur par défaut de 50 la refabriquait le
        /// 2026-10-08. Une limite donnée est rappelée en tête du relevé.
        #[arg(long)]
        limit: Option<usize>,
        /// Le répertoire du modèle. **Sans lui, le banc mesure tantivy seul** — c'est le relevé
        /// de référence du 2026-09-15, rejouable avec le code d'aujourd'hui.
        #[arg(long)]
        model: Option<Utf8PathBuf>,
        /// L'amortissement de la fusion par rangs réciproques.
        ///
        /// Réglable pour que le choix de `mailcore::fusion::K` soit un **résultat** et pas une
        /// valeur reprise d'un article et jamais éprouvée sur ce corpus.
        #[arg(long, default_value_t = mailcore::fusion::K)]
        k: f32,
        /// Le modèle est un transformeur (`candle`) et non un embedding statique.
        ///
        /// La famille doit correspondre à celle qui a écrit le magasin de vecteurs : les
        /// dimensions diffèrent, et le magasin refuse une dimension qui n'est pas la sienne.
        #[arg(long)]
        bert: bool,
        /// Garder les rangs de chaque requête dans ce fichier JSON — un fichier neuf, jamais un
        /// existant —, pour comparer ensuite deux modèles avec `recall-models`.
        #[arg(long)]
        ranks: Option<Utf8PathBuf>,
    },

    /// Compare deux modèles sur le même jeu de requêtes, à partir des rangs que
    /// `measure-recall --ranks` a gardés : le même bras, deux magasins de vecteurs.
    ///
    /// Le relevé de `measure-recall` compare des moteurs entre eux sur un seul magasin ; choisir un
    /// modèle demande l'autre comparaison. Mêmes règles de comptage et même test de signe.
    RecallModels {
        /// Les rangs du modèle en place.
        #[arg(long)]
        base: Utf8PathBuf,
        /// Les rangs du modèle candidat.
        #[arg(long)]
        candidate: Utf8PathBuf,
        /// Le bras de la comparaison primaire, déclaré avant le relevé : `hybride` ou `sens`.
        #[arg(long, default_value = "hybride")]
        primary: String,
    },

    /// Mesure le critère 2 de la phase 4 : la latence d'un balayage vectoriel complet.
    ///
    /// Sur des vecteurs **fabriqués**, et c'est tout l'intérêt : un produit scalaire coûte le
    /// même prix sur un vecteur qui décrit un message et sur un vecteur tiré au sort, donc le
    /// banc tourne **avant** la première passe de vectorisation. On saura si un balayage
    /// complet tient le budget avant d'avoir écrit le moteur qui le remplirait.
    ///
    /// **Aucun `--store`** : elle écrit, donc elle crée son propre répertoire jetable — la
    /// règle laissée par `measure-attachment`, qui a été visée sur les données de production.
    MeasureScan {
        /// Combien de messages fabriquer. Par défaut, la taille du corpus réel.
        #[arg(long, default_value_t = 73_658)]
        messages: usize,
        /// La dimension des vecteurs. Par défaut, celle de `potion-multilingual-128M`.
        #[arg(long, default_value_t = 256)]
        dim: usize,
        /// Combien de résultats garder par requête, donc la taille du tas borné.
        #[arg(long, default_value_t = 50)]
        limit: usize,
        /// Combien de requêtes chronométrer. Le p95 du critère demande d'en avoir assez pour
        /// que le rang 95 % veuille dire quelque chose.
        #[arg(long, default_value_t = 200)]
        queries: usize,
    },

    /// Taille le vocabulaire du transformeur **par écriture** : garde les pièces en alphabet
    /// latin, avec la ponctuation, les chiffres, les symboles et les émoji.
    ///
    /// Le résultat ne dépend que du modèle source — pas d'un corpus —, donc son empreinte est une
    /// constante. Voir `mailvec::trim`.
    ModelTrimBert {
        /// Le modèle d'origine : `config.json`, `tokenizer.json`, `model.safetensors`.
        #[arg(
            long,
            default_value = "measurements/models/granite-embedding-107m-multilingual"
        )]
        model: Utf8PathBuf,
        /// Où écrire le modèle taillé. Refusé s'il y a déjà un modèle.
        #[arg(long)]
        out: Utf8PathBuf,
    },

    /// Vérifie un transformeur taillé sur le corpus réel : les pièces de **chaque** message, puis
    /// les vecteurs d'un échantillon, au bit près.
    ModelCheckBert {
        /// Racine du store d'où viennent les messages.
        #[arg(long, default_value = "measurements/store")]
        store: Utf8PathBuf,
        /// Le modèle d'origine.
        #[arg(
            long,
            default_value = "measurements/models/granite-embedding-107m-multilingual"
        )]
        model: Utf8PathBuf,
        /// Le modèle taillé.
        #[arg(long)]
        trimmed: Utf8PathBuf,
        /// Combien de messages encoder, de chaque sorte, pour l'étage des vecteurs.
        #[arg(long, default_value_t = 200)]
        sample: usize,
    },

    /// Demande à un modèle d'embeddings si deux mots qu'un humain juge proches le sont pour lui.
    ///
    /// La question la moins chère qu'on puisse poser à un modèle, et celle qui départage « le
    /// rappel est nul parce que le modèle n'a pas de sens » de « le rappel est nul parce qu'un
    /// réglage en aval est mauvais ». Une seconde, contre six minutes pour une passe complète.
    ModelProbe {
        /// Le répertoire du modèle à sonder.
        #[arg(long, default_value = "measurements/models/potion-taille")]
        model: Utf8PathBuf,
        /// Sonder un transformeur (candle) plutôt qu'un embedding statique.
        #[arg(long)]
        bert: bool,
    },

    /// Relie un jeu de requêtes à un store par le **contenu** des messages, pas par leur
    /// identifiant.
    ///
    /// À lancer quand le store a été réimporté : les `messages.id` changent, les empreintes non.
    /// L'outil propose une empreinte quand il peut la déduire sans ambiguïté du sujet cité dans
    /// la note, et **refuse** partout ailleurs — une vérité terrain devinée rend un chiffre, et
    /// ce chiffre est faux sans le dire.
    QueriesRebind {
        /// Racine du store. Lu seulement.
        #[arg(long, default_value = "measurements/store")]
        store: Utf8PathBuf,
        /// Le jeu de requêtes à relier.
        #[arg(long, default_value = "measurements/phase4-queries.toml")]
        queries: Utf8PathBuf,
        /// Où écrire le jeu relié. Sans ce paramètre, l'outil ne fait que dire ce qu'il ferait.
        ///
        /// Un fichier neuf, jamais celui d'entrée : une vérité terrain écrite à la main ne se
        /// remplace pas par un programme sans que quelqu'un ait relu ce qui la remplace.
        #[arg(long)]
        out: Option<Utf8PathBuf>,
    },

    /// Aide à écrire le jeu de requêtes du critère 1 : un tirage au hasard, ou les messages dont
    /// le sujet ou l'expéditeur contient un texte, avec leurs empreintes.
    ///
    /// Aucun moteur n'est lancé — un jeu écrit en regardant ce que le moteur rend ressemble au
    /// moteur. La sortie nomme des messages réels : elle ne va jamais dans `docs/`.
    QueriesFind {
        /// Racine du store. Lu seulement.
        #[arg(long, default_value = "measurements/store")]
        store: Utf8PathBuf,
        /// Tirer ce nombre de messages au hasard.
        #[arg(long)]
        sample: Option<usize>,
        /// La graine du tirage : le même nombre rend le même échantillon.
        #[arg(long, default_value_t = 2026)]
        seed: u64,
        /// Un texte que le sujet doit contenir, sans distinction de casse.
        #[arg(long)]
        subject: Option<String>,
        /// Un texte que l'adresse ou le nom de l'expéditeur doit contenir.
        #[arg(long)]
        from: Option<String>,
        /// Montrer le début du corps.
        #[arg(long)]
        body: bool,
    },

    /// Mesure les critères 3 et 5 de la phase 4 : le coût d'une passe de vectorisation.
    ///
    /// Débit, durée, mémoire **par palier** — le critère 5 demande de dire où elle va —, et le
    /// contrôle de reprise, qui est ce que le critère 3 exige en propre.
    ///
    /// Elle prend un `--store` parce qu'une passe ne veut rien dire sur un corpus fabriqué, mais
    /// elle **copie** le `index.sqlite` avant d'y toucher et relie les blobs en lecture. Le store
    /// de production n'est jamais modifié — la règle laissée par `measure-attachment`.
    MeasureVectors {
        /// Racine du store à vectoriser. Lu, jamais écrit.
        #[arg(long, default_value = "measurements/store")]
        store: Utf8PathBuf,
        /// Le répertoire du modèle taillé, tel que `model-trim` l'a produit.
        #[arg(long, default_value = "measurements/models/potion-taille")]
        model: Utf8PathBuf,
        /// Charger le transformeur plutôt que l'embedding statique. La famille se choisit, elle ne
        /// se devine pas depuis le répertoire.
        #[arg(long)]
        bert: bool,
        /// Arrêter la passe après ce nombre de messages. Pour comparer deux réglages sans les
        /// quatre heures d'une passe complète — **pas pour trancher le critère 5** : la
        /// composition d'un message coûte selon sa taille, et les premiers messages ne sont pas
        /// les plus gros.
        #[arg(long)]
        limit: Option<u64>,
        /// Enchaîner le contrôle de reprise, qui refait une passe complète puis une seconde.
        ///
        /// Séparé parce qu'il double la durée du banc : on veut le débit à chaque exécution, la
        /// reprise une fois.
        #[arg(long)]
        resume: bool,
        /// Où garder le magasin de vecteurs produit — un fichier neuf, jamais un magasin existant.
        /// Pour mesurer ensuite le rappel du modèle mesuré sans refaire la passe.
        #[arg(long)]
        keep: Option<Utf8PathBuf>,
    },

    /// Critère 2 de `docs/PHASE-4.md` : la recherche hybride de bout en bout, par l'API que la
    /// coquille emprunte, le sens servi par l'exécutable `maild` relancé en enfant.
    ///
    /// Travaille sur une réplique jetable sous `target/` ; le store n'est que lu.
    MeasureSemanticSearch {
        /// Le store d'origine. Lu, jamais écrit.
        #[arg(long, default_value = "measurements/store")]
        store: Utf8PathBuf,
        /// Le magasin de vecteurs à servir, tel que `measure-vectors --keep` l'a gardé.
        #[arg(long, default_value = "measurements/vectors-granite-latin.bin")]
        vectors: Utf8PathBuf,
        /// Le modèle qui l'a écrit.
        #[arg(long, default_value = "measurements/models/granite-latin")]
        model: Utf8PathBuf,
        /// Le jeu de requêtes du critère 1 : seuls les textes sont lus.
        #[arg(long, default_value = "measurements/phase4-queries.toml")]
        queries: Utf8PathBuf,
        /// L'exécutable `maild` à relancer. Par défaut, celui de `target/release`.
        #[arg(long)]
        maild: Option<Utf8PathBuf>,
        /// Tours du jeu de requêtes ; le premier est jeté.
        #[arg(long, default_value_t = 3)]
        rounds: usize,
    },

    /// Critère 7 de `docs/PHASE-4.md` : le démarrage de la coquille livrée sur une réplique, avec
    /// le modèle et les vecteurs, puis sans. **Ne compile pas** : compiler d'abord, laisser la
    /// machine retomber, puis lancer.
    MeasureStartupModel {
        /// Le store d'origine. Lu, jamais écrit — et refusé s'il porte un compte IMAP.
        #[arg(long, default_value = "measurements/store")]
        store: Utf8PathBuf,
        /// Le magasin de vecteurs, tel que `measure-vectors --keep` l'a gardé.
        #[arg(long, default_value = "measurements/vectors-granite-latin.bin")]
        vectors: Utf8PathBuf,
        /// Le modèle qui l'a écrit.
        #[arg(long, default_value = "measurements/models/granite-latin")]
        model: Utf8PathBuf,
        /// Exécutions par configuration ; la première est le démarrage à froid.
        #[arg(long, default_value_t = 5)]
        runs: usize,
    },

    /// Critère 4 de `docs/PHASE-4.md` : ce que coûte un message qui arrive — l'enfant de la passe
    /// de fond lancé comme le démon le lance, sur une réplique où un seul message attend.
    MeasureArrival {
        /// Le store d'origine. Lu, jamais écrit.
        #[arg(long, default_value = "measurements/store")]
        store: Utf8PathBuf,
        /// Le magasin de vecteurs du corpus, tel que `measure-vectors --keep` l'a gardé.
        #[arg(long, default_value = "measurements/vectors-granite-latin.bin")]
        vectors: Utf8PathBuf,
        /// Le modèle qui l'a écrit.
        #[arg(long, default_value = "measurements/models/granite-latin")]
        model: Utf8PathBuf,
        /// L'exécutable `maild` à relancer. Par défaut, celui de `target/release`.
        #[arg(long)]
        maild: Option<Utf8PathBuf>,
        /// Combien d'arrivées ; la première est jetée.
        #[arg(long, default_value_t = 4)]
        arrivals: usize,
        /// Le bras PDF : un message qui arrive avec un PDF — l'extracteur et la seconde mise à
        /// jour de l'index en plus de l'enfant des vecteurs.
        #[arg(long)]
        pdf: bool,
    },

    /// Recense les pièces jointes du corpus : types, nombres, volumes.
    ///
    /// L'étape 6 quater de `docs/PHASE-4.md`, **avant** d'écrire un extracteur. Combien de
    /// messages sont concernés et quels formats il faudrait savoir lire est ce qui décide de ce
    /// qu'on écrit ; l'ignorer reviendrait à choisir la réponse avant la question.
    ///
    /// **Ne fait que lire.**
    AttachmentCensus {
        /// Racine du store à lire.
        #[arg(long, default_value = "measurements/store")]
        store: Utf8PathBuf,
        /// Combien de types montrer.
        #[arg(long, default_value_t = 20)]
        top: usize,
    },

    /// Recense les PDF du corpus avec la règle de détection livrée : voies, signature, doublons,
    /// tailles, coût du balayage MIME. Avant d'en lire un seul — `docs/PHASE-4.md`, 2026-10-20.
    ///
    /// **Ne fait que lire**, sur une réplique : `Store::open` migrerait le store.
    PdfCensus {
        /// Racine du store à recenser.
        #[arg(long, default_value = "measurements/store")]
        store: Utf8PathBuf,
    },

    /// L'effet du texte des PDF sur la recherche, sur une réplique du corpus : le critère 1 — un
    /// témoin contre l'après, selon une règle déclarée avant le premier relevé —, la taille et la
    /// latence de l'index à fragmentation égale, la mise à jour de l'index et la revectorisation.
    /// **Ne compile pas** : compiler d'abord, laisser la machine retomber.
    PdfEffect {
        /// Le corpus. Lu, jamais écrit.
        #[arg(long, default_value = "measurements/store")]
        store: Utf8PathBuf,
        /// Ses vecteurs.
        #[arg(long, default_value = "measurements/vectors-granite-latin.bin")]
        vectors: Utf8PathBuf,
        /// Le modèle qui les a écrits.
        #[arg(long, default_value = "measurements/models/granite-latin")]
        model: Utf8PathBuf,
        /// L'exécutable `maild` qui lit les PDF. Par défaut, celui de `target/release`.
        #[arg(long)]
        maild: Option<Utf8PathBuf>,
        /// La latence seule, trois fois de suite — sans les rangs ni les vecteurs.
        #[arg(long)]
        latency_only: bool,
    },

    /// La première lecture des PDF d'un store, sur une réplique, par l'enfant livré : durée,
    /// crête de mémoire, coût en base, et ce que chaque pièce est devenue.
    PdfCatchUp {
        /// Racine du store à lire.
        #[arg(long, default_value = "measurements/store-imap")]
        store: Utf8PathBuf,
        /// Combien d'exécutions ; la première est jetée pour les temps.
        #[arg(long, default_value_t = 3)]
        runs: usize,
    },

    /// Tire des PDF distincts à graine fixe, les dépose avec un squelette de vérité terrain, pour
    /// qu'un humain y relève à l'œil les termes qu'il chercherait.
    ///
    /// **Écrit des documents réels** : la sortie est refusée dans le dépôt.
    PdfSample {
        /// Racine du store à tirer.
        #[arg(long, default_value = "measurements/store")]
        store: Utf8PathBuf,
        /// Où déposer les PDF et le squelette — hors du dépôt, vide ou absent.
        #[arg(long)]
        out: Utf8PathBuf,
        /// Combien de PDF décident.
        #[arg(long, default_value_t = 40)]
        count: usize,
        /// Combien de plus, pour mettre le banc au point sans toucher aux premiers.
        #[arg(long, default_value_t = 10)]
        reserve: usize,
        /// La graine du tirage.
        #[arg(long, default_value = "mailcore-pdf-2026-10-20")]
        seed: String,
    },

    /// Le banc A2 du PDF : pdf-inspector, pdf-extract et lopdf, un enfant par document, par le
    /// lecteur hors espace de travail de `tools/pdf-runner`, jugés par la règle gelée en tête du
    /// fichier de termes.
    ///
    /// Refuse de lire le corpus si un contrôle bloquant échoue. N'imprime que des comptes et des
    /// centiles ; n'écrit rien, ni dans le dépôt ni sous `measurements/`.
    PdfCompare {
        /// Racine du store où lire les PDF — sur une réplique.
        #[arg(long, default_value = "measurements/store")]
        store: Utf8PathBuf,
        /// Le lecteur de candidats : `cd tools/pdf-runner && cargo build --release`.
        #[arg(long, default_value = pdfcompare::DEFAULT_RUNNER)]
        runner: Utf8PathBuf,
        /// La vérité terrain gelée par `pdf-terms`.
        #[arg(long, default_value = "measurements/pdf-terms.toml")]
        terms: Utf8PathBuf,
        /// Combien d'exécutions complètes ; la première est jetée pour les temps.
        #[arg(long, default_value_t = 3)]
        runs: usize,
        /// `reserve` pour mettre le banc au point, `decision` pour les termes seuls, `corpus` pour
        /// tous les PDF du store — les conditions dures de la règle ne se jugent que là.
        #[arg(long, default_value = "corpus")]
        set: String,
    },

    /// Gèle la vérité terrain du banc du PDF : verse les relevés dans le squelette du tirage, la
    /// règle de décision en tête, et imprime l'empreinte à écrire dans le journal.
    ///
    /// **Refuse d'écraser** un fichier gelé, et toute sortie dans le dépôt.
    PdfTerms {
        /// Le squelette rendu par `pdf-sample`.
        #[arg(long)]
        skeleton: Utf8PathBuf,
        /// Les relevés, en JSON : une liste de `{ id, scan, terms, note }`. Répétable.
        #[arg(long = "readings", required = true)]
        readings: Vec<Utf8PathBuf>,
        /// La règle de décision, recopiée telle quelle en tête du fichier.
        #[arg(long)]
        rule: Utf8PathBuf,
        /// Le fichier gelé, hors du dépôt ; il ne doit pas exister.
        #[arg(long)]
        out: Utf8PathBuf,
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
        Command::ModelTrim { store, model, out } => model::trim(&store, &model, &out),
        Command::ModelCheck {
            store,
            model,
            trimmed,
            count,
        } => model::check(&store, &model, &trimmed, count),
        Command::ModelBench {
            store,
            model,
            count,
            batch,
        } => model::bench(&store, &model, count, batch),
        Command::MeasureRecall {
            store,
            queries,
            limit,
            model,
            k,
            bert,
            ranks,
        } => recall::measure(
            &store,
            &queries,
            limit,
            model.as_ref(),
            k,
            bert,
            ranks.as_ref(),
        ),
        Command::RecallModels {
            base,
            candidate,
            primary,
        } => recall::compare_models(&base, &candidate, &primary),
        Command::MeasureScan {
            messages,
            dim,
            limit,
            queries,
        } => scan::measure(messages, dim, limit, queries),
        Command::ModelProbe { model, bert } => probe::model(&model, bert),
        Command::ModelTrimBert { model, out } => bertmodel::trim(&model, &out),
        Command::ModelCheckBert {
            store,
            model,
            trimmed,
            sample,
        } => bertmodel::check(&store, &model, &trimmed, sample),
        Command::QueriesRebind {
            store,
            queries,
            out,
        } => rebind::run(&store, &queries, out.as_ref()),
        Command::QueriesFind {
            store,
            sample,
            seed,
            subject,
            from,
            body,
        } => findq::run(&findq::Options {
            store,
            sample,
            seed,
            subject,
            from,
            body,
        }),
        Command::MeasureVectors {
            store,
            model,
            bert,
            limit,
            resume,
            keep,
        } => vectors::measure(&vectors::Options {
            store,
            model,
            bert,
            limit,
            resume,
            keep,
        }),
        Command::AttachmentCensus { store, top } => attach::census(&store, top),
        Command::PdfCensus { store } => pdf::census(&store),
        Command::PdfCatchUp { store, runs } => pdf::catch_up(&store, runs),
        Command::PdfEffect {
            store,
            vectors,
            model,
            maild,
            latency_only,
        } => (if latency_only {
            pdfeffect::latency_only
        } else {
            pdfeffect::run
        })(&pdfeffect::Inputs {
            store,
            vectors,
            model,
            primary: Utf8PathBuf::from("measurements/phase4-queries-v5.toml"),
            others: vec![
                Utf8PathBuf::from("measurements/phase4-queries-v4.toml"),
                Utf8PathBuf::from("measurements/phase4-queries-v3.toml"),
            ],
            phrases: Utf8PathBuf::from("measurements/phase4-queries.toml"),
            maild,
        }),
        Command::PdfCompare {
            store,
            runner,
            terms,
            runs,
            set,
        } => pdfcompare::compare(&pdfcompare::Options {
            store,
            runner,
            terms,
            runs,
            set,
        }),
        Command::PdfTerms {
            skeleton,
            readings,
            rule,
            out,
        } => pdf::freeze(&pdf::FreezeOptions {
            skeleton,
            readings,
            rule,
            out,
        }),
        Command::PdfSample {
            store,
            out,
            count,
            reserve,
            seed,
        } => pdf::sample(&pdf::SampleOptions {
            store,
            out,
            count,
            reserve,
            seed,
        }),
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
        Command::MeasureMerge {
            store,
            arrivals,
            repeats,
            phrases,
        } => merge::measure(&store, arrivals, repeats, phrases.as_ref()),
        Command::MeasureSemanticSearch {
            store,
            vectors,
            model,
            queries,
            maild,
            rounds,
        } => semantic::measure_search(
            &semantic::Sources {
                store,
                vectors,
                model,
                maild,
            },
            &queries,
            rounds,
        ),
        Command::MeasureStartupModel {
            store,
            vectors,
            model,
            runs,
        } => semantic::measure_startup(
            &semantic::Sources {
                store,
                vectors,
                model,
                maild: None,
            },
            runs,
        ),
        Command::MeasureArrival {
            store,
            vectors,
            model,
            maild,
            arrivals,
            pdf,
        } => {
            let sources = semantic::Sources {
                store,
                vectors,
                model,
                maild,
            };
            if pdf {
                semantic::measure_arrival_pdf(&sources, arrivals)
            } else {
                semantic::measure_arrival(&sources, arrivals)
            }
        }
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
