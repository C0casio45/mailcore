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
mod followup;
mod inspect;
mod measure;
mod model;
mod probe;
mod profile;
mod rebind;
mod recall;
mod replies;
mod scan;
mod schema;
mod secrets;
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
        /// Combien de résultats demander au moteur. Au-delà du top mesuré, pour voir de
        /// combien une cible manquée est manquée.
        #[arg(long, default_value_t = 50)]
        limit: usize,
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
        #[arg(long, default_value = "measurements/models/multilingual-e5-small")]
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
        #[arg(long, default_value = "measurements/models/multilingual-e5-small")]
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
        } => recall::measure(&store, &queries, limit, model.as_ref(), k, bert),
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
