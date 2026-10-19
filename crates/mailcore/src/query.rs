//! L'API de requête : ce que le démon expose, et la seule porte d'entrée en lecture.
//!
//! [`Mailbox`] réunit les trois moteurs — blobs, SQLite, tantivy — derrière une surface qui
//! n'en laisse fuir aucun. Un appelant reçoit des types de [`crate::model`] et de ce module,
//! jamais un `rusqlite::Row` ni un document tantivy. C'est ce qui rend le choix des moteurs
//! révisable, et c'est aussi ce qui permet à `mailmcp` d'exister sans savoir ce qu'il y a
//! dessous.
//!
//! ## Contraintes de conception, tirées des critères de `docs/PHASE-1.md`
//!
//! - La pagination ne charge jamais plus d'une page (critère 2 : 60 fps sur 100 000
//!   messages).
//! - Aucune requête n'attend la complétion d'un index (critère 1 : l'UI affiche ce qui
//!   existe déjà). Un store sans index plein texte répond à tout sauf à la recherche.
//! - Ouvrir un message est une lecture de blob plus un parse MIME (critère 5).
//!
//! ## Ce qui sort d'ici est du texte assaini
//!
//! [`MessageDetail::body`] est du texte aplati, jamais du HTML brut. Ce module sert des
//! clients — dont un modèle de langage — et un corps de message est du contenu écrit par un
//! inconnu. Le HTML fidèle est l'affaire du front, qui le rend dans une `<iframe>` sous CSP
//! (`docs/PRIVACY.md`).

use camino::Utf8Path;
use mail_parser::{MessageParser, MimeHeaders};

use crate::error::Result;
use crate::index::{self, Searcher};
use crate::model::{Folder, FolderId, MessageFlags, MessageId, ThreadId};
use crate::store::Store;
use crate::store::read::{Cursor, ListItem, StoreStats};

/// Nombre maximum de résultats qu'une recherche peut demander.
///
/// Un client qui demande 100 000 résultats ne veut pas les lire, il veut vider le store.
/// Le plafond protège aussi le budget du critère 4 : chaque résultat coûte une lecture
/// SQLite.
pub const MAX_RESULTS: usize = 500;

/// Une liste de `returned` résultats, pour `requested` demandés, a-t-elle pu être coupée ?
///
/// ## Pourquoi c'est ici et pas chez chaque client
///
/// `mailapi` et `mailmcp` le calculaient chacun, à l'identique, comme `returned >= requested` — et
/// tous deux se trompaient pareil : [`Mailbox::search`] plafonne **en silence** à [`MAX_RESULTS`].
/// Un client qui demandait mille résultats sur une requête qui en a trois mille en recevait cinq
/// cents, avec `truncated: false`. Le modèle de langage à l'autre bout croyait avoir tout vu. Une
/// relecture contradictoire l'a trouvé le 2026-09-23.
///
/// La règle vit donc à côté du plafond qu'elle doit connaître, pour qu'aucun client ne puisse
/// l'oublier.
///
/// Deux cas limites, à connaître plutôt qu'à découvrir :
///
/// - **une limite nulle** rend `true` : zéro résultat pour zéro demandé, la liste est « pleine ».
///   Un client qui demande zéro ne regarde de toute façon pas la suite ;
/// - **un résultat obsolète** — un document que l'index porte encore pour un message supprimé de
///   SQLite — est écarté par [`Mailbox::search`] **après** la coupe. Une liste pleine peut donc
///   revenir avec un résultat de moins et se dire complète. C'est rare — il faut un index en
///   avance sur le store, ce que [`crate::index::Coverage`] détecte — et c'est dit ici.
#[must_use]
pub const fn truncated(returned: usize, requested: usize) -> bool {
    let effective = if requested < MAX_RESULTS {
        requested
    } else {
        MAX_RESULTS
    };
    returned >= effective
}

/// Taille maximale du texte rendu pour un corps de message.
///
/// 256 Kio : au-delà, c'est une pièce jointe déguisée en texte, et personne — humain ou
/// modèle — n'en lit autant. Le texte est tronqué proprement, pas refusé.
pub const MAX_BODY_TEXT: usize = 256 * 1024;

/// Taille maximale du HTML assaini rendu pour un corps de message.
///
/// 2 Mio, soit huit fois le plafond du texte : un mail très mis en page a légitimement
/// beaucoup plus de balisage que de mots, et couper à la même valeur amputerait des messages
/// ordinaires. Au-delà, le webview ramerait de toute façon, et le texte reste disponible.
pub const MAX_BODY_HTML: usize = 2 * 1024 * 1024;

/// Une boîte mail ouverte : blobs, métadonnées et recherche.
///
/// À construire une fois et à garder vivante. Ouvrir un `Searcher` monte les segments de
/// l'index ; le refaire à chaque requête ferait payer ce coût à chaque frappe.
#[derive(Debug)]
pub struct Mailbox {
    store: Store,
    /// `None` quand l'index plein texte est absent ou illisible.
    ///
    /// Volontairement optionnel : un store fraîchement importé mais pas encore indexé doit
    /// répondre à tout le reste. Faire échouer l'ouverture entière parce que la recherche
    /// n'est pas prête violerait le critère 1.
    searcher: Option<Searcher>,
}
/// Un dossier et ses compteurs.
#[derive(Debug, Clone)]
pub struct FolderSummary {
    /// Le dossier.
    pub folder: Folder,
    /// Le nom du compte, tel que l'utilisateur le connaît.
    ///
    /// Un identifiant numérique ne dit rien à un lecteur — humain ou modèle. Il faut le nom.
    pub account_name: String,
    /// Nombre de messages référencés.
    pub total: u64,
    /// Nombre de messages non lus.
    pub unread: u64,
}

/// Un résultat de recherche : les métadonnées, plus la pertinence.
#[derive(Debug, Clone)]
pub struct SearchResult {
    /// Le message.
    pub item: ListItem,
    /// Le score BM25. Comparable au sein d'une même requête, pas entre requêtes.
    pub score: f32,
}

/// Une pièce jointe, décrite mais jamais rendue.
///
/// La phase 1 les liste, ne les ouvre pas (`docs/PHASE-1.md`). Rendre le contenu d'une pièce
/// jointe à un client — un modèle, par exemple — sortirait des octets écrits par un inconnu
/// hors de leur bac à sable.
#[derive(Debug, Clone)]
pub struct Attachment {
    /// Le nom déclaré, s'il y en a un.
    pub name: Option<String>,
    /// Le type MIME déclaré.
    pub mime: String,
    /// La taille en octets, après décodage.
    pub size: usize,
}

/// Le corps HTML d'un message, assaini et accompagné de ce qui en a été retiré.
///
/// Ce qui manque ici volontairement : le HTML brut. Il n'existe qu'entre la lecture du blob
/// et l'assainissement, à l'intérieur de [`Mailbox::render`], et ne traverse aucune frontière
/// de module. C'est ce qui fait qu'aucun appelant ne peut oublier de l'assainir.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Rendered {
    /// Le HTML assaini.
    ///
    /// Rempli aussi pour un message en texte seul : `mail-parser` convertit alors la partie
    /// `text/plain` en HTML échappé, ce qui donne au front **un seul chemin de rendu** au
    /// lieu de deux. Vide seulement quand le message n'a aucun corps.
    pub html: String,
    /// Vrai si le HTML a été tronqué parce qu'il dépassait [`MAX_BODY_HTML`].
    pub truncated: bool,
    /// Nombre de sources d'images distantes retirées par la politique en vigueur.
    ///
    /// C'est le chiffre du bandeau « Contenu distant bloqué » de `docs/PRIVACY.md` §2.
    pub blocked_images: usize,
    /// Nombre de ressources distantes que le message référençait, traceurs compris.
    pub remote_resources: usize,
    /// Les traceurs relevés, sans doublon. Voir `mailhtml::trackers`.
    pub trackers: Vec<mailhtml::Tracker>,
}

/// Un message ouvert : métadonnées, corps en texte, pièces jointes, dossiers.
#[derive(Debug, Clone)]
pub struct MessageDetail {
    /// Les métadonnées de liste.
    pub item: ListItem,
    /// L'en-tête `Message-ID`, s'il était présent.
    pub rfc822_id: Option<String>,
    /// La chaîne `References`, du plus ancien au plus récent.
    ///
    /// ## Pourquoi elle est là, et pourquoi `In-Reply-To` n'y est pas mêlé
    ///
    /// Elle sert à **répondre** : la RFC 5322 §3.6.4 demande que le `References` d'une réponse
    /// soit celui du message auquel on répond, prolongé de son `Message-ID`. Reconstruire la
    /// chaîne depuis le fil local donnerait une chaîne différente de celle des autres clients,
    /// et un lecteur qui regroupe sur `References` verrait deux fils au lieu d'un.
    ///
    /// `crate::thread::referenced` mêle les deux en-têtes, parce que pour **rattacher** un
    /// message l'ordre est sans importance et tout lien est bon à prendre. Ici l'ordre compte,
    /// donc seul `References` est lu.
    pub references: Vec<String>,
    /// Les destinataires, décodés.
    pub to: Vec<String>,
    /// Le corps, aplati en texte et tronqué à [`MAX_BODY_TEXT`].
    pub body: String,
    /// Vrai si le corps a été tronqué.
    pub body_truncated: bool,
    /// Les pièces jointes, listées.
    pub attachments: Vec<Attachment>,
    /// Les dossiers qui référencent ce message, avec leurs drapeaux.
    pub folders: Vec<(String, MessageFlags)>,
    /// Le fil, si la passe de threading est passée.
    pub thread: Option<ThreadId>,
    /// L'invitation que le message porte, lue ou refusée en nommant ce qui manque.
    ///
    /// ## Elle est ici et pas dans les pièces jointes
    ///
    /// Une invitation n'est pas un fichier à ouvrir : c'est un rendez-vous à lire. Elle arrive
    /// pourtant sous forme de pièce — jointe ou en ligne — et la laisser dans la liste des
    /// pièces obligerait chaque client à savoir la reconnaître, la décoder et l'analyser. Un
    /// seul endroit le fait : voir `crate::invitation_of`.
    pub invitation: Option<mailcal::Invitation>,
}

impl Mailbox {
    /// Ouvre une boîte mail.
    ///
    /// L'absence d'index plein texte n'est pas une erreur : tout répond sauf la recherche.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Io`] ou [`crate::Error::Sqlite`] si le store est inutilisable.
    pub fn open(root: impl AsRef<Utf8Path>) -> Result<Self> {
        let store = Store::open(root)?;
        let searcher = match index::open_or_create(&store).and_then(|(idx, _)| Searcher::open(&idx))
        {
            Ok(searcher) => Some(searcher),
            Err(source) => {
                tracing::warn!(%source, "index plein texte indisponible, recherche désactivée");
                None
            }
        };
        Ok(Self { store, searcher })
    }

    /// Le store sous-jacent, pour ce que la boîte ne sait pas faire.
    ///
    /// [`Mailbox`] est la surface de **lecture** : elle assemble des vues — un dossier avec ses
    /// compteurs, un message avec son HTML assaini. Deux choses n'en sont pas : les jobs
    /// d'écriture, et la **file d'envoi**, qui est un journal que le démon écrit. Recopier la
    /// file en méthodes de [`Mailbox`] doublerait une API pour ne rien ajouter.
    ///
    /// L'écriture reste protégée là où elle l'était : les méthodes de la file prennent `&self`
    /// mais chacune ouvre sa propre transaction et monte `synchronous` à `FULL` — voir
    /// [`crate::store::outbox`] — et le `Mutex` qui entoure la boîte chez les appelants
    /// sérialise le reste.
    ///
    /// **Ce que cet accès ne doit pas devenir** : un contournement pour écrire des messages ou
    /// des dossiers depuis un frontend. L'import et la moisson passent par un job de fond, et
    /// c'est la règle 3 du `CLAUDE.md` qui le demande.
    #[must_use]
    pub const fn store(&self) -> &Store {
        &self.store
    }

    /// Remonte l'index plein texte depuis le disque.
    ///
    /// Le `Searcher` est monté à l'ouverture et garde les segments qu'il a trouvés alors :
    /// une réindexation faite à côté reste invisible jusqu'à ce qu'on le remonte. C'était une
    /// limite documentée tant que l'indexation ne pouvait être lancée que par un autre
    /// processus ; dès lors que le démon la lance lui-même, servir l'ancien index après avoir
    /// annoncé « indexation terminée » serait un mensonge.
    ///
    /// Sert aussi de rattrapage : appeler cette méthode sur une boîte ouverte avant que
    /// l'index existe active la recherche sans redémarrer.
    ///
    /// # Errors
    ///
    /// Aucune. Un index illisible laisse la boîte sans recherche plutôt que de la casser —
    /// même arbitrage qu'à l'ouverture, critère 1. La valeur rendue dit si la recherche est
    /// disponible après l'opération.
    pub fn reload_search(&mut self) -> bool {
        self.searcher =
            match index::open_or_create(&self.store).and_then(|(idx, _)| Searcher::open(&idx)) {
                Ok(searcher) => Some(searcher),
                Err(source) => {
                    tracing::warn!(%source, "index plein texte indisponible après rechargement");
                    None
                }
            };
        self.searcher.is_some()
    }

    /// Vrai si la recherche plein texte est disponible.
    #[must_use]
    pub const fn search_available(&self) -> bool {
        self.searcher.is_some()
    }

    /// L'état du store.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Sqlite`].
    pub fn stats(&self) -> Result<StoreStats> {
        self.store.stats()
    }

    /// Un jeton de révision du store, à comparer par égalité.
    ///
    /// **Le mécanisme d'abonnement de la phase 1.** Ni un canal interne, ni un `notify` du
    /// système de fichiers : l'import et l'indexation tournent dans un autre processus
    /// (`mail import`, `mail index`), donc le démon ne peut pas les observer de l'intérieur.
    /// Deux sources externes suffisent à couvrir tout ce qui change :
    ///
    /// - `PRAGMA data_version` pour l'index de métadonnées, qui bouge dès qu'une autre
    ///   connexion valide une écriture ;
    /// - la date de `meta.json` de l'index tantivy, qu'une reconstruction réécrit.
    ///
    /// ## Et une troisième, qui manquait : les écritures de **cette** connexion
    ///
    /// `PRAGMA data_version` ne bouge **pas** pour les écritures validées sur la connexion qui
    /// l'interroge — c'est écrit dans la documentation de SQLite, et c'est ce qui a fait qu'un
    /// message marqué lu ne se rafraîchissait jamais dans la coquille livrée : elle sert son
    /// propre service, donc elle écrit et lit par la même connexion, donc sa propre écriture
    /// lui était invisible. Le drapeau partait bien au serveur ; c'est la liste qui continuait
    /// d'afficher « non lu ».
    ///
    /// `Connection::total_changes` compte les lignes modifiées **par cette connexion** depuis
    /// son ouverture. Les deux compteurs sont donc complémentaires par construction : l'un voit
    /// les autres, l'autre voit soi. Aucun registre à tenir à la main, et rien à oublier de
    /// bumper au prochain chemin d'écriture — ce qui était l'autre solution, et celle qui se
    /// serait dégradée en silence.
    ///
    /// **Opaque.** La forme rendue n'est pas un contrat : les clients la stockent et la
    /// comparent, ils ne l'analysent pas. Elle ne s'ordonne pas non plus — « différent »
    /// veut dire « relire », pas « plus récent ».
    ///
    /// Limite connue et assumée : le `Searcher` est monté à l'ouverture. Après un
    /// `mail index`, la révision change donc — le client saura qu'il doit relire — mais le
    /// démon continue de chercher dans l'ancien index jusqu'à son redémarrage. Recharger un
    /// index à chaud est de la phase 2.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Sqlite`] si la base est illisible.
    pub fn revision(&self) -> Result<String> {
        let data = self.store.data_version()?;
        let mine = self.store.local_changes();

        // L'absence de l'index n'est pas une erreur : un store importé mais pas encore
        // indexé doit répondre à tout le reste (critère 1). Elle se code par un zéro, qui se
        // distingue de toute date réelle.
        let search = std::fs::metadata(self.store.search_dir().join("meta.json"))
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |since| since.as_secs());

        // **La ligne en cours de remise**, qui ne vit pas dans SQLite : le nom dans
        // `outbox.sending` et le verrou de remise. Un remettant qui meurt n'écrit rien, et sa
        // ligne doit pourtant passer d'« envoi en cours » à « incertain » à l'écran ; sans ce
        // terme, rien ne faisait relire la file, et le doute restait caché jusqu'à la prochaine
        // écriture. Trouvé par la relecture du 2026-10-19 — voir `Store::in_flight`. Une erreur
        // vaut « aucune », sans journal : cette fonction tourne quatre fois par seconde.
        let moving = self.store.in_flight().ok().flatten().map_or(0, |id| id.0);

        Ok(format!("{data}.{mine}.{search}.{moving}"))
    }

    /// Tous les dossiers, avec leurs compteurs.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Sqlite`].
    pub fn folders(&self) -> Result<Vec<FolderSummary>> {
        let counts = self.store.folder_counts()?;
        // Les noms de comptes en une requête, pas une par dossier : un profil réel a 96
        // dossiers pour 11 comptes, et `list_folders` est appelé à chaque ouverture.
        let names: std::collections::HashMap<_, _> = self
            .store
            .accounts()?
            .into_iter()
            .map(|(id, _, name)| (id, name))
            .collect();

        Ok(self
            .store
            .folders()?
            .into_iter()
            .map(|folder| {
                let (total, unread) = counts.get(&folder.id).copied().unwrap_or((0, 0));
                // Un compte référencé mais absent de la table ne devrait pas exister ; s'il
                // arrive, un libellé lisible vaut mieux qu'un dossier escamoté.
                let account_name = names
                    .get(&folder.account)
                    .cloned()
                    .unwrap_or_else(|| format!("compte {}", folder.account.0));
                FolderSummary {
                    folder,
                    account_name,
                    total,
                    unread,
                }
            })
            .collect())
    }

    /// Une page de messages d'un dossier, du plus récent au plus ancien.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Sqlite`].
    pub fn page(
        &self,
        folder: FolderId,
        after: Option<Cursor>,
        limit: u32,
    ) -> Result<Vec<ListItem>> {
        self.store
            .page(folder, after, limit.min(MAX_RESULTS as u32))
    }

    /// Recherche plein texte, avec les métadonnées de chaque résultat.
    ///
    /// Rend une liste vide si l'index est indisponible — la recherche est une fonction en
    /// plus, pas un prérequis pour lire son courrier.
    ///
    /// **Plafonnée à [`MAX_RESULTS`]**, et c'est voulu : c'est le chemin qu'emprunte un client,
    /// y compris distant. Une mesure, elle, n'est pas un client — voir [`Self::search_deep`].
    ///
    /// # Errors
    ///
    /// [`crate::Error::Query`] si la requête est mal formée.
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchResult>> {
        self.search_within(query, limit.min(MAX_RESULTS))
    }

    /// Combien de messages indexés contiennent ce mot, replié en minuscules par l'appelant.
    ///
    /// Voir [`crate::index::Searcher::doc_freq`] pour ce que ça mesure.
    ///
    /// ## `None` ne veut **pas** dire « pas encore indexé »
    ///
    /// La première version l'affirmait, et un test l'a démenti : [`Self::open`] crée un index
    /// **vide** quand il n'y en a pas, donc un store fraîchement importé rend `Some(0)` pour tous
    /// les mots, pas `None`. `None` ne sort que d'un index illisible.
    ///
    /// La conséquence est à connaître de tout appelant qui s'en sert pour juger une requête : sur
    /// un index vide ou incomplet, **chaque mot a l'air introuvable**. Un banc qui classe ses
    /// requêtes par la rareté de leurs mots doit donc vérifier d'abord que l'index couvre le
    /// store — voir [`Self::index_coverage`] — sans quoi il rangerait tout dans « aucun mot
    /// commun » sans rien dire.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Tantivy`] si l'index est illisible.
    pub fn doc_freq(&self, word: &str) -> Result<Option<u64>> {
        match &self.searcher {
            Some(searcher) => searcher.doc_freq(word).map(Some),
            None => Ok(None),
        }
    }

    /// Ce message porte-t-il ce mot, tel que l'index le voit ? `None` si l'index n'est pas lisible.
    ///
    /// Voir [`crate::index::Searcher::contains`] pour pourquoi on le demande à l'index plutôt que
    /// de relire le message.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Tantivy`] si l'index est illisible.
    pub fn contains(&self, id: MessageId, word: &str) -> Result<Option<bool>> {
        match &self.searcher {
            Some(searcher) => searcher.contains(id, word).map(Some),
            None => Ok(None),
        }
    }

    /// Combien de documents l'index plein texte porte, ou `None` s'il n'est pas lisible.
    ///
    /// **Ne pas le comparer à [`Store::message_count`]** : un message dont le blob manque est
    /// marqué indexé sans avoir de document, donc l'écart ne dit pas qu'un index est incomplet.
    /// La comparaison juste est celle de [`Self::index_coverage`].
    #[must_use]
    pub fn indexed_documents(&self) -> Option<u64> {
        self.searcher.as_ref().map(Searcher::document_count)
    }

    /// L'index couvre-t-il le store ? `None` si l'index n'est pas lisible.
    ///
    /// Voir [`crate::index::Coverage`] : la règle y est écrite une fois, et `mail doctor` comme
    /// le banc du critère 1 l'appliquent. Le coût est un parcours des blobs du store — une
    /// vérification de présence par message —, donc c'est un diagnostic, pas un appel de routine.
    ///
    /// # Errors
    ///
    /// [`crate::Error`] si le store ou le magasin de blobs est illisible.
    pub fn index_coverage(&self) -> Result<Option<crate::index::Coverage>> {
        let Some(documents) = self.indexed_documents() else {
            return Ok(None);
        };
        Ok(Some(crate::index::Coverage {
            documents,
            messages: self.store.message_count()?,
            missing_blobs: self.store.missing_blobs()?.len() as u64,
        }))
    }

    /// La même recherche, **sans plafond**.
    ///
    /// ## Pourquoi ce chemin existe depuis le 2026-09-23
    ///
    /// Le banc du critère 1 demandait 73 658 résultats et en recevait 500, sans que rien ne le
    /// dise. Tous ses relevés de rang plein texte étaient donc **censurés à droite** : ce qu'il
    /// imprimait « absente » voulait dire « au-delà de 500 », pas « introuvable ». Deux entrées
    /// du journal ont été écrites sur cette lecture, et une conclusion — « le plein texte ne
    /// rend rien sur 73 658 messages » — était fausse.
    ///
    /// La leçon est celle du 2026-09-21, encore : **un instrument qui tronque en silence
    /// fabrique des absences.** Et le plafond n'était pas en cause — il protège l'API contre un
    /// client qui demanderait cent mille lignes pour vider le store, ce qui reste une bonne
    /// raison. Ce qui manquait, c'est que la mesure n'est pas un client : elle tourne dans le
    /// même processus, elle lit son propre store, et elle a besoin du rang **vrai**.
    ///
    /// Un rang censuré n'est pas seulement imprécis : il n'a pas de sens arithmétique. On ne
    /// peut ni le moyenner, ni en prendre la médiane, ni le comparer à un rang non censuré —
    /// exactement ce que le banc faisait.
    ///
    /// # Errors
    ///
    /// Les mêmes que [`Self::search`].
    pub fn search_deep(&self, query: &str, limit: usize) -> Result<Vec<SearchResult>> {
        self.search_within(query, limit)
    }

    /// Le corps commun des deux, qui ne décide d'aucun plafond.
    fn search_within(&self, query: &str, limit: usize) -> Result<Vec<SearchResult>> {
        let Some(searcher) = &self.searcher else {
            return Ok(Vec::new());
        };

        let hits = searcher.search(query, limit)?;
        let mut results = Vec::with_capacity(hits.len());
        for hit in hits {
            // Un résultat dont le message a disparu de SQLite est ignoré plutôt que rendu
            // vide : l'index est dérivé, il peut être en avance sur une suppression.
            if let Some(item) = self.store.message(hit.id)? {
                results.push(SearchResult {
                    item,
                    score: hit.score,
                });
            }
        }
        Ok(results)
    }

    /// Le fil d'un message, en entier, dans l'ordre chronologique.
    ///
    /// Un message non threadé rend un fil d'un seul élément : lui-même. C'est honnête —
    /// « pas encore threadé » et « seul dans son fil » sont indiscernables pour un lecteur,
    /// et rendre une liste vide serait faux.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Sqlite`].
    pub fn thread(&self, message: MessageId) -> Result<Vec<ListItem>> {
        match self.store.thread_of(message)? {
            Some(thread) => self.store.thread_messages(thread),
            None => Ok(self.store.message(message)?.into_iter().collect()),
        }
    }

    /// La ligne de liste d'un message, sans relire son blob — ce qu'une recherche rend.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Sqlite`].
    pub fn item(&self, id: MessageId) -> Result<Option<ListItem>> {
        self.store.message(id)
    }

    /// Ouvre un message : corps en texte, pièces jointes listées, dossiers.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Sqlite`] ou [`crate::Error::Io`]. Un blob absent rend `Ok(None)`
    /// plutôt qu'une erreur : c'est un état diagnostiquable par `mail doctor`, pas une
    /// panne.
    pub fn message(&self, id: MessageId) -> Result<Option<MessageDetail>> {
        let Some(item) = self.store.message(id)? else {
            return Ok(None);
        };
        let raw = match self.store.blobs().read(item.blob) {
            Ok(bytes) => bytes,
            Err(crate::Error::BlobNotFound(hash)) => {
                tracing::warn!(%hash, id = id.0, "blob absent");
                return Ok(None);
            }
            Err(other) => return Err(other),
        };

        let parser = MessageParser::default();
        let parsed = parser.parse(&raw);

        let mut body = String::new();
        let mut attachments = Vec::new();
        let mut to = Vec::new();
        let mut rfc822_id = None;
        let mut references = Vec::new();
        let mut invitation = None;

        if let Some(parsed) = &parsed {
            rfc822_id = parsed.message_id().map(str::to_owned);
            references = reference_chain(parsed);
            collect_recipients(parsed, &mut to);
            flatten_body(parsed, &mut body);
            invitation = invitation_in(parsed);
            for part in parsed.attachments() {
                attachments.push(Attachment {
                    name: part.attachment_name().map(str::to_owned),
                    mime: part.content_type().map_or_else(
                        || "application/octet-stream".to_owned(),
                        |ct| match ct.subtype() {
                            Some(sub) => format!("{}/{sub}", ct.ctype()),
                            None => ct.ctype().to_owned(),
                        },
                    ),
                    size: part.contents().len(),
                });
            }
        }

        let body_truncated = body.len() > MAX_BODY_TEXT;
        if body_truncated {
            // Tronquer sur une frontière de caractère, sinon `truncate` panique.
            let mut cut = MAX_BODY_TEXT;
            while cut > 0 && !body.is_char_boundary(cut) {
                cut -= 1;
            }
            body.truncate(cut);
        }

        Ok(Some(MessageDetail {
            // **Lue depuis l'arbre MIME déjà analysé**, et pas depuis les octets bruts : la
            // relire coûterait un second `MessageParser::parse` complet — 4,3 ms sur un message
            // HTML — pour tous les messages, dont les 99 % qui n'ont pas d'invitation. Le
            // critère 5 de `docs/PHASE-1.md` borne cette ouverture.
            //
            // La lecture du fichier lui-même coûte 0,3 ms en moyenne, mesurée sur les 930
            // pièces du corpus réel.
            invitation,
            rfc822_id,
            references,
            to,
            body,
            body_truncated,
            attachments,
            folders: self.store.folders_of(id)?,
            thread: self.store.thread_of(id)?,
            item,
        }))
    }

    /// Le corps d'un message, **assaini et prêt à rendre dans une `<iframe>`**.
    ///
    /// Séparé de [`Mailbox::message`] à dessein : les deux ne servent pas le même public et
    /// n'ont pas le même coût. Un modèle de langage lit du texte aplati et n'a rien à faire
    /// d'un balisage ; une interface veut le HTML fidèle, et paie pour ça un assainissement
    /// complet. Mettre les deux dans un seul appel ferait payer chacun pour l'autre.
    ///
    /// **Le HTML brut ne sort jamais d'ici.** Il est passé par [`mailhtml::sanitize`] avant
    /// de quitter cette fonction, donc aucun appelant ne peut se tromper en oubliant de le
    /// faire — c'est la barrière qui est par défaut, pas la vigilance de l'appelant. Le
    /// rendu final l'entoure quand même de la CSP de [`mailhtml::MESSAGE_CSP`] : deux
    /// barrières indépendantes, `docs/PRIVACY.md`.
    ///
    /// Rend `None` si le message ou son contenu est absent, comme [`Mailbox::message`].
    ///
    /// # Errors
    ///
    /// [`crate::Error::Sqlite`] ou [`crate::Error::Io`] si le store est illisible.
    pub fn render(&self, id: MessageId, policy: mailhtml::Policy) -> Result<Option<Rendered>> {
        let Some(item) = self.store.message(id)? else {
            return Ok(None);
        };
        let raw = match self.store.blobs().read(item.blob) {
            Ok(bytes) => bytes,
            Err(crate::Error::BlobNotFound(hash)) => {
                tracing::warn!(%hash, id = id.0, "blob absent");
                return Ok(None);
            }
            Err(other) => return Err(other),
        };

        let parser = MessageParser::default();
        let Some(parsed) = parser.parse(&raw) else {
            // Un message que le parseur refuse n'a pas de corps HTML : ce n'est pas une
            // erreur, c'est un message vide de ce point de vue.
            return Ok(Some(Rendered::default()));
        };

        // Les parties HTML dans l'ordre du message. Plusieurs parties arrivent sur les
        // messages composés — une signature dans une partie séparée, par exemple — et les
        // concaténer est ce que fait un client mail.
        //
        // **`mail-parser` compte aussi les parties `text/plain` ici**, et c'est pourquoi la
        // conversion est faite par [`mailhtml::plain`] au lieu d'être laissée à `body_html`.
        // Celle de `mail-parser` remplace `\n` par `<br/>` et s'arrête là : une citation en
        // texte brut — des lignes préfixées de `>` — en ressort en texte ordinaire, donc
        // invisible à `mailhtml::blocks`, donc affichée comme le reste. Trouvé le 2026-09-21
        // en écrivant la passe de vectorisation, qui avale ces citations pour la même raison.
        // Un seul détecteur de citation dans le projet, et il est ici.
        //
        // Elle n'échappe pas non plus `&` : un `&amp;` écrit en toutes lettres ressortait en
        // `&`. Le symétrique du piège que la version précédente de ce commentaire relevait
        // pour `<script>` — là l'échappement sauvait le texte, ici son absence le mangeait.
        let mut source = String::new();
        for id in &parsed.html_body {
            match parsed.part(*id).map(|part| &part.body) {
                Some(mail_parser::PartType::Text(text)) => {
                    source.push_str(&mailhtml::plain::to_html(text));
                }
                Some(mail_parser::PartType::Html(html)) => source.push_str(html),
                _ => {}
            }
        }

        if source.is_empty() {
            // Un message sans corps du tout. Rare, mais il existe : un message qui n'a que
            // des pièces jointes, ou dont le corps n'a pas survécu au transport.
            return Ok(Some(Rendered::default()));
        }

        // Le relevé se fait sur la source **brute** : après assainissement, les URL
        // distantes ont disparu et il ne resterait rien à compter.
        let trackers = mailhtml::trackers::scan(&source);
        let cleaned = mailhtml::sanitize::clean(&source, policy);

        let truncated = cleaned.html.len() > MAX_BODY_HTML;
        let mut html = cleaned.html;
        if truncated {
            // Tronquer du HTML produit du balisage non fermé — sans conséquence, un moteur
            // ferme les éléments ouverts en fin de document. Couper sur une frontière de
            // caractère reste obligatoire, sinon `truncate` panique.
            let mut cut = MAX_BODY_HTML;
            while cut > 0 && !html.is_char_boundary(cut) {
                cut -= 1;
            }
            html.truncate(cut);
        }

        Ok(Some(Rendered {
            html,
            truncated,
            blocked_images: cleaned.blocked_images,
            remote_resources: trackers.remote_resources,
            trackers: trackers.trackers,
        }))
    }

    /// Les octets RFC 5322 d'un message reçu, rendus lisibles.
    ///
    /// C'est la moitié « ce qu'on reçoit » de la vue source. La moitié qui tient la promesse de
    /// `docs/PHASE-3.md` est l'autre : [`Self::outgoing_source`], parce que c'est le message
    /// sortant que nous composons, donc le seul où « rien n'est ajouté en secret » demande à
    /// être vérifié plutôt que cru.
    ///
    /// Rend `None` si le message ou son contenu est absent, comme [`Self::message`].
    ///
    /// # Errors
    ///
    /// [`crate::Error::Sqlite`] ou [`crate::Error::Io`] si le store est illisible.
    pub fn source(&self, id: MessageId) -> Result<Option<crate::source::Source>> {
        let Some(item) = self.store.message(id)? else {
            return Ok(None);
        };
        // La taille vient de l'index, pas du comptage des octets lus : c'est précisément quand
        // la lecture s'arrête au plafond qu'il faut pouvoir dire de combien.
        let total = self.store.message_size(id)?.unwrap_or(0);
        self.source_of(item.blob, total)
    }

    /// Les octets RFC 5322 d'une ligne de la file d'envoi, rendus lisibles.
    ///
    /// **Les mêmes octets que ceux remis au `DATA`** : la ligne désigne le blob que
    /// `mailsmtp::queue::stage` a écrit, et l'envoi ne le recompose pas. Ce que l'utilisateur
    /// lit ici est donc ce que le serveur a reçu, et non une reconstruction qui pourrait
    /// diverger du vrai message — une vue qui recomposerait ne prouverait rien.
    ///
    /// Rend `None` si la ligne ou son contenu est absent.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Sqlite`] ou [`crate::Error::Io`] si le store est illisible.
    pub fn outgoing_source(&self, id: crate::OutboxId) -> Result<Option<crate::source::Source>> {
        let Some(row) = self.store.outgoing(id)? else {
            return Ok(None);
        };
        self.source_of(row.blob, row.size)
    }

    /// L'objet d'un message de la file d'envoi, lu dans son propre bloc d'en-têtes.
    ///
    /// ## Lu dans le blob, pas rangé dans la ligne
    ///
    /// La ligne `outbox` ne porte que ce que l'envoi décide — enveloppe, état, tentatives — et la
    /// règle du critère 2 vit dans ses colonnes. Y ajouter une étiquette demanderait une
    /// migration sur la table la plus sensible du store, pour un affichage. Le blob, lui, est
    /// **ce qui part** : l'objet qu'on y lit est celui que le destinataire verra, pas une copie
    /// qui pourrait en diverger — la règle de [`Self::outgoing_source`].
    ///
    /// Seul le bloc d'en-têtes est lu, ligne à ligne et borné à
    /// [`crate::source::MAX_HEADERS`] : une pièce jointe de 25 Mio derrière ne coûte rien.
    ///
    /// Rend `None` si le blob manque ou si le message n'a pas d'objet. Les caractères de contrôle
    /// sont remplacés par des espaces : la phrase s'affiche dans une liste, et un en-tête hostile
    /// n'a pas à y dessiner — la leçon de `crate::source`.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Io`] si le blob est illisible.
    pub fn outgoing_subject(&self, blob: crate::BlobHash) -> Result<Option<String>> {
        use std::io::{BufRead as _, Read as _};

        let reader = match self.store.blobs().open(blob) {
            Ok(reader) => reader,
            Err(crate::Error::BlobNotFound(hash)) => {
                tracing::warn!(%hash, "blob absent");
                return Ok(None);
            }
            Err(other) => return Err(other),
        };
        let mut reader = std::io::BufReader::new(reader.take(crate::source::MAX_HEADERS as u64));
        let mut head = Vec::new();
        loop {
            let start = head.len();
            let read = reader
                .read_until(b'\n', &mut head)
                .map_err(|source| crate::Error::Io {
                    path: "blob".into(),
                    source,
                })?;
            let line = &head[start..];
            if read == 0 || line == b"\r\n" || line == b"\n" {
                break;
            }
        }
        // Une dernière ligne coupée par la fin du contenu — ou par la borne — reste un en-tête,
        // mais `mail-parser` ne la lit qu'une fois terminée.
        if !head.is_empty() && !head.ends_with(b"\n") {
            head.extend_from_slice(b"\r\n");
        }
        Ok(MessageParser::default()
            .parse_headers(&head)
            .and_then(|parsed| parsed.subject().map(visible_subject))
            .filter(|it| !it.is_empty()))
    }

    /// Lit au plus [`crate::source::MAX_READ`] octets d'un blob et les rend lisibles.
    ///
    /// **En flux, et borné dès la lecture.** Le plus gros message du corpus réel fait 48 Mio ;
    /// le lire en entier pour en montrer le premier mégaoctet chargerait quarante-sept Mio dont
    /// personne ne veut, et ce serait la règle 4 du `CLAUDE.md` contournée au dernier maillon.
    /// `Read::take` donne la borne au maillon qui lit, pas à celui qui jette.
    fn source_of(
        &self,
        blob: crate::BlobHash,
        total: u64,
    ) -> Result<Option<crate::source::Source>> {
        use std::io::Read as _;

        let mut reader = match self.store.blobs().open(blob) {
            Ok(reader) => reader,
            Err(crate::Error::BlobNotFound(hash)) => {
                tracing::warn!(%hash, "blob absent");
                return Ok(None);
            }
            Err(other) => return Err(other),
        };
        let mut raw = Vec::new();
        reader
            .by_ref()
            .take(crate::source::MAX_READ as u64)
            .read_to_end(&mut raw)
            .map_err(|source| crate::Error::Io {
                path: "blob".into(),
                source,
            })?;
        Ok(Some(crate::source::render(&raw, total)))
    }

    /// Les échanges avec une adresse, du plus récent au plus ancien.
    ///
    /// Interroge l'index plein texte plutôt que SQLite : les destinataires (`To`, `Cc`) sont
    /// indexés mais pas stockés en colonnes, parce qu'ils sont multivalués et qu'une table
    /// de jointure pour eux coûterait plus que ce qu'elle rapporte. La contrepartie est que
    /// cette fonction a besoin de l'index.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Query`] si l'adresse produit une requête invalide.
    pub fn contact_history(&self, address: &str, limit: usize) -> Result<Vec<SearchResult>> {
        let cleaned: String = address
            .chars()
            .filter(|c| c.is_alphanumeric() || matches!(c, '@' | '.' | '-' | '_' | '+'))
            .collect();
        if cleaned.is_empty() {
            return Ok(Vec::new());
        }
        // Guillemets : une adresse contient des points et des arobases que l'analyseur de
        // requêtes découperait en plusieurs termes.
        self.search(&format!("from:\"{cleaned}\" OR to:\"{cleaned}\""), limit)
    }
}

/// Un objet prêt à tenir sur une ligne : les caractères de contrôle — retours à la ligne d'un
/// en-tête replié mal décodé compris — deviennent des espaces, et les bords sont rognés.
fn visible_subject(subject: &str) -> String {
    subject
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .trim()
        .to_owned()
}

/// La chaîne `References` d'un message, dans l'ordre où elle est écrite.
///
/// Seul `References`, et pas `In-Reply-To` : voir [`MessageDetail::references`]. Un en-tête
/// absent donne une chaîne vide, ce qui est le cas normal d'un message qui ouvre un fil.
fn reference_chain(parsed: &mail_parser::Message<'_>) -> Vec<String> {
    match parsed.header("References") {
        Some(mail_parser::HeaderValue::Text(value)) => vec![value.to_string()],
        Some(mail_parser::HeaderValue::TextList(values)) => {
            values.iter().map(ToString::to_string).collect()
        }
        _ => Vec::new(),
    }
}

/// Ajoute les adresses de `To` et `Cc`.
fn collect_recipients(parsed: &mail_parser::Message<'_>, out: &mut Vec<String>) {
    for header in [parsed.to(), parsed.cc()].into_iter().flatten() {
        for addr in header.iter() {
            match (addr.name(), addr.address()) {
                (Some(name), Some(email)) => out.push(format!("{name} <{email}>")),
                (None, Some(email)) => out.push(email.to_owned()),
                _ => {}
            }
        }
    }
}

/// Aplatit le corps en texte : parties texte telles quelles, parties HTML débalisées.
fn flatten_body(parsed: &mail_parser::Message<'_>, out: &mut String) {
    for index in 0..parsed.text_body_count() {
        if let Some(part) = parsed.body_text(index) {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&part);
        }
    }
    // Le HTML ne s'ajoute que si aucune partie texte n'a été trouvée. À l'indexation on
    // prend les deux, pour ne rater aucun mot ; ici c'est un humain ou un modèle qui lit, et
    // le même contenu deux fois est du bruit.
    if out.is_empty() {
        for index in 0..parsed.html_body_count() {
            if let Some(part) = parsed.body_html(index) {
                mailhtml::text::html_to_text(&part, out);
            }
        }
    }
}

/// Les pièces `text/calendar` d'un message, décodées en texte.
///
/// ## Pourquoi le parcours ne se limite pas aux pièces jointes
///
/// Une invitation n'est pas toujours jointe. Outlook l'envoie dans un `multipart/alternative`
/// à côté du corps — donc dans le corps, pas dans les pièces — et Google l'envoie **deux
/// fois** : une part en ligne et une pièce `invite.ics`. Ne regarder que `attachments()` en
/// manquerait la moitié ; ne regarder que le corps, l'autre. Le parcours passe donc sur toutes
/// les parties.
///
/// Le décodage du transfert — base64, quoted-printable — et du jeu de caractères déclaré est
/// fait par `mail-parser` : un `.ics` en base64 est le cas ordinaire chez Exchange.
///
/// Une partie qui se déclare `text/calendar` mais dont le contenu ne se décode pas rend une
/// chaîne **vide** plutôt que rien : le nombre de pièces rendues est celui du message, et un
/// appelant qui mesure une couverture doit pouvoir compter ce qu'il n'a pas pu lire.
#[must_use]
pub fn calendar_parts(raw: &[u8]) -> Vec<String> {
    let Some(parsed) = MessageParser::default().parse(raw) else {
        return Vec::new();
    };
    parts_of(&parsed)
}

/// Les pièces `text/calendar` d'un message **déjà analysé**.
///
/// ## Pourquoi cette variante existe
///
/// Parce que [`Mailbox::message`] a déjà l'arbre MIME en main. Le premier jet appelait
/// [`invitation_of`] avec les octets bruts, ce qui refaisait un `MessageParser::parse` complet
/// — donc **doublait** le coût d'ouverture de n'importe quel message, invitation ou pas. Le
/// critère 5 de `docs/PHASE-1.md` borne cette ouverture, et 4,3 ms de parse HTML payés deux
/// fois n'y avaient rien à faire.
fn parts_of(parsed: &mail_parser::Message<'_>) -> Vec<String> {
    parsed
        .parts
        .iter()
        // `is_content_type` compare sans tenir compte de la casse, ce que la RFC 2045 demande :
        // `TEXT/CALENDAR` et `text/calendar` sont le même type.
        .filter(|part| part.is_content_type("text", "calendar"))
        .map(|part| part.text_contents().map(str::to_owned).unwrap_or_default())
        .collect()
}

/// Le contenu décodé d'une pièce jointe d'un message, par son rang.
///
/// ## Le rang est celui que `MessageDetail::attachments` a listé
///
/// Même itérateur, même ordre : `parsed.attachments()`. C'est l'invariant qui rend cette
/// fonction utilisable — un client choisit une pièce dans la liste qu'il a reçue, et doit
/// obtenir **celle-là**. Deux parcours différents feraient joindre un fichier à la place d'un
/// autre, ce qui est la façon la plus discrète d'envoyer à quelqu'un un document qui ne lui
/// était pas destiné. Un test le vérifie sur un message à plusieurs pièces.
///
/// Rend le nom déclaré et les octets **décodés** — base64, quoted-printable — ou `None` si le
/// rang n'existe pas.
///
/// ## Ce que ça coûte
///
/// Le message entier est analysé, et la pièce sort en mémoire. Pour une pièce de 25 Mo, c'est
/// 25 Mo : le chemin d'envoi, lui, est streamé de bout en bout (critère 3), mais décoder une
/// partie d'un message reçu demande d'avoir le message. Le blob est déjà lu en entier par
/// l'appelant de toute façon.
#[must_use]
pub fn attachment_bytes(raw: &[u8], rank: usize) -> Option<(String, Vec<u8>)> {
    let parsed = MessageParser::default().parse(raw)?;
    let part = parsed.attachments().nth(rank)?;
    let name = part
        .attachment_name()
        .map(str::to_owned)
        .unwrap_or_else(|| format!("piece-{}", rank + 1));
    Some((name, part.contents().to_vec()))
}

/// L'invitation d'un message **déjà analysé**, s'il en porte une.
///
/// La même règle que [`invitation_of`], sans le second parse : voir [`parts_of`].
fn invitation_in(parsed: &mail_parser::Message<'_>) -> Option<mailcal::Invitation> {
    parts_of(parsed)
        .into_iter()
        .filter(|text| !text.trim().is_empty())
        .map(|text| mailcal::read(&text))
        .find(|invitation| invitation.is_readable() || !invitation.gaps.is_empty())
}

/// L'invitation d'un message, s'il en porte une.
///
/// La **première** pièce lisible : Google envoie la même invitation deux fois — en ligne et en
/// pièce jointe — et afficher deux fois le même rendez-vous serait une erreur d'affichage plus
/// visible que l'absence.
///
/// La lecture elle-même est `mailcal::read`, qui ne peut pas échouer : une invitation de
/// travers rend ses manques nommés, et ne doit surtout pas empêcher d'afficher le message.
#[must_use]
pub fn invitation_of(raw: &[u8]) -> Option<mailcal::Invitation> {
    let parsed = MessageParser::default().parse(raw)?;
    invitation_in(&parsed)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::model::FolderKind;
    use crate::store::write::NewMessage;

    /// Une boîte avec deux messages, dont un dans deux dossiers.
    fn mailbox() -> (tempfile::TempDir, Mailbox) {
        let dir = tempfile::tempdir().unwrap();
        let root = Utf8Path::from_path(dir.path()).unwrap().to_owned();
        let store = Store::open(&root).unwrap();

        let facture = b"From: Le Plombier <plombier@exemple.fr>\r\n\
                        To: moi@exemple.fr\r\n\
                        Subject: facture\r\n\
                        Message-ID: <f1@exemple.fr>\r\n\
                        \r\n\
                        Voici la facture du mois.\r\n";
        let devis = b"From: moi@exemple.fr\r\n\
                      To: Le Plombier <plombier@exemple.fr>\r\n\
                      Subject: Re: facture\r\n\
                      Message-ID: <f2@exemple.fr>\r\n\
                      References: <f1@exemple.fr>\r\n\
                      \r\n\
                      Merci, je regarde.\r\n";

        let writer = store.writer().unwrap();
        let account = writer.upsert_account("imap", "compte").unwrap();
        let inbox = writer
            .upsert_folder(account, "INBOX", FolderKind::Inbox)
            .unwrap();
        let archive = writer
            .upsert_folder(account, "Archive", FolderKind::Archive)
            .unwrap();

        for (bytes, subject, from, in_two) in [
            (&facture[..], "facture", "plombier@exemple.fr", true),
            (&devis[..], "Re: facture", "moi@exemple.fr", false),
        ] {
            let put = store.blobs().put(bytes).unwrap();
            let (id, _) = writer
                .insert_message(&NewMessage {
                    blob: put.hash,
                    rfc822_id: None,
                    date: 1_700_000_000,
                    from_addr: from,
                    from_name: None,
                    subject,
                    size: bytes.len() as u64,
                    has_attachments: false,
                })
                .unwrap();
            writer
                .insert_ref(id, inbox, 1_700_000_000, MessageFlags::SEEN)
                .unwrap();
            if in_two {
                writer
                    .insert_ref(id, archive, 1_700_000_000, MessageFlags::empty())
                    .unwrap();
            }
        }
        writer.commit().unwrap();
        drop(store);

        let mailbox = Mailbox::open(&root).unwrap();
        (dir, mailbox)
    }

    fn first_message(mailbox: &Mailbox) -> MessageId {
        let folders = mailbox.folders().unwrap();
        mailbox.page(folders[0].folder.id, None, 10).unwrap()[0].id
    }

    #[test]
    fn folders_come_back_with_counts() {
        let (_dir, mailbox) = mailbox();
        let folders = mailbox.folders().unwrap();

        let archive = folders.iter().find(|f| f.folder.path == "Archive").unwrap();
        let inbox = folders.iter().find(|f| f.folder.path == "INBOX").unwrap();

        assert_eq!(inbox.total, 2);
        assert_eq!(inbox.unread, 0, "les deux sont marqués lus dans INBOX");
        assert_eq!(archive.total, 1);
        assert_eq!(archive.unread, 1, "non lu dans l'archive");
    }

    #[test]
    fn a_message_lists_every_folder_that_references_it() {
        // Ce que la dédup rend visible : un contenu, deux emplacements, deux jeux de
        // drapeaux.
        let (_dir, mailbox) = mailbox();
        let detail = mailbox
            .message(first_message(&mailbox))
            .unwrap()
            .unwrap_or_else(|| panic!("message introuvable"));

        let paths: Vec<&str> = detail.folders.iter().map(|(p, _)| p.as_str()).collect();
        assert!(paths.contains(&"INBOX") || paths.contains(&"Archive"));
    }

    #[test]
    fn opening_a_message_yields_flat_text_and_recipients() {
        let (_dir, mailbox) = mailbox();
        let detail = mailbox
            .message(first_message(&mailbox))
            .unwrap()
            .unwrap_or_else(|| panic!("message introuvable"));

        assert!(detail.body.contains("facture") || detail.body.contains("regarde"));
        assert!(!detail.body_truncated);
        assert!(detail.attachments.is_empty());
        assert!(!detail.to.is_empty());
        assert!(detail.rfc822_id.is_some());
    }

    #[test]
    fn a_missing_message_is_none_not_an_error() {
        let (_dir, mailbox) = mailbox();
        assert!(mailbox.message(MessageId(99_999)).unwrap().is_none());
    }

    #[test]
    fn an_ordinary_message_carries_no_invitation_and_an_invitation_is_read_from_the_same_parse() {
        // Un message sans pièce de calendrier — la très grande majorité : 930 pièces sur
        // 73 825 messages du corpus réel.
        let (_dir, mailbox) = mailbox();
        let detail = mailbox
            .message(first_message(&mailbox))
            .unwrap()
            .unwrap_or_else(|| panic!("message introuvable"));
        assert!(detail.invitation.is_none());

        // Et une invitation en `multipart/alternative`, comme Exchange l'envoie : elle n'est pas
        // dans les pièces jointes, donc un parcours qui ne regarderait que `attachments()` la
        // manquerait.
        let raw = b"From: eloise@exemple.fr\r\n\
                    To: moi@exemple.fr\r\n\
                    Subject: invitation\r\n\
                    MIME-Version: 1.0\r\n\
                    Content-Type: multipart/alternative; boundary=\"f\"\r\n\
                    \r\n\
                    --f\r\n\
                    Content-Type: text/plain\r\n\
                    \r\n\
                    Quand : jeudi 14h\r\n\
                    \r\n\
                    --f\r\n\
                    Content-Type: text/calendar; method=REQUEST\r\n\
                    \r\n\
                    BEGIN:VCALENDAR\r\n\
                    METHOD:REQUEST\r\n\
                    BEGIN:VEVENT\r\n\
                    SUMMARY:Point hebdo\r\n\
                    DTSTART:20260910T120000Z\r\n\
                    DTEND:20260910T130000Z\r\n\
                    END:VEVENT\r\n\
                    END:VCALENDAR\r\n\
                    --f--\r\n";
        let invitation = crate::invitation_of(raw).unwrap_or_else(|| panic!("invitation absente"));
        assert_eq!(invitation.summary.as_deref(), Some("Point hebdo"));
        assert!(invitation.has_instant());
        assert!(invitation.gaps.is_empty(), "{:?}", invitation.gaps);
        // La pièce est bien vue **hors** des pièces jointes : c'est le cas qui décide du
        // parcours.
        assert_eq!(crate::calendar_parts(raw).len(), 1);
    }

    #[test]
    fn an_unthreaded_message_yields_a_thread_of_one() {
        // Le threading n'a pas tourné dans cette fixture : un fil d'un seul élément est la
        // réponse honnête, pas une liste vide.
        let (_dir, mailbox) = mailbox();
        let thread = mailbox.thread(first_message(&mailbox)).unwrap();
        assert_eq!(thread.len(), 1);
    }

    #[test]
    fn search_without_an_index_yields_nothing_instead_of_failing() {
        // Critère 1 : un store importé mais pas encore indexé doit rester utilisable.
        let (_dir, mailbox) = mailbox();
        assert!(mailbox.search("facture", 10).unwrap().is_empty());
        assert!(
            mailbox
                .contact_history("plombier@exemple.fr", 10)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn search_finds_what_was_indexed() {
        let (_dir, mailbox) = mailbox();
        let cancelled = crate::Progress::new();
        crate::index::rebuild(mailbox.store(), &cancelled).unwrap();

        // Rouvrir : le `Searcher` a été construit avant que l'index existe.
        let reopened = Mailbox::open(mailbox.store().root()).unwrap();
        assert!(reopened.search_available());

        let hits = reopened.search("facture", 10).unwrap();
        assert_eq!(hits.len(), 2, "les deux messages parlent de facture");
        assert!(hits[0].score > 0.0);
    }

    /// La fréquence documentaire est une **union** de champs, et elle se trompe dans deux sens si
    /// on la raccourcit.
    ///
    /// « plombier » est dans l'expéditeur du premier message et le destinataire du second : un
    /// **maximum** par champ dirait 1, ce que faisait la première version. « facture » est dans
    /// les deux sujets et dans un corps : une **somme** dirait 3. Les deux doivent rendre 2.
    #[test]
    fn doc_freq_counts_documents_across_fields_without_double_counting() {
        let (_dir, mailbox) = mailbox();
        crate::index::rebuild(mailbox.store(), &crate::Progress::new()).unwrap();
        let reopened = Mailbox::open(mailbox.store().root()).unwrap();

        assert_eq!(
            reopened.doc_freq("plombier").unwrap(),
            Some(2),
            "un maximum par champ oublie le message qui ne le porte qu'en destinataire"
        );
        assert_eq!(
            reopened.doc_freq("facture").unwrap(),
            Some(2),
            "une somme par champ compte deux fois le message qui le porte en sujet et en corps"
        );
        assert_eq!(reopened.doc_freq("merci").unwrap(), Some(1));
        assert_eq!(reopened.doc_freq("inexistant").unwrap(), Some(0));
        // « archive » n'est que dans le champ des dossiers, indexé mais **pas** interrogé par
        // défaut : la fréquence doit l'ignorer comme la recherche l'ignore. Le contrôle que
        // `Fields::searched` est bien la liste commune, et pas une copie qui pourrait dériver.
        assert_eq!(reopened.doc_freq("archive").unwrap(), Some(0));
    }

    /// Le cas exact que le banc du critère 1 ratait : un mot présent **seulement** chez les
    /// destinataires. « plombier » n'est dans la réponse que comme destinataire ; relire le sujet,
    /// l'expéditeur et le corps ne le trouve pas, l'index si.
    #[test]
    fn contains_sees_every_field_the_index_searches_including_recipients() {
        let (_dir, mailbox) = mailbox();
        crate::index::rebuild(mailbox.store(), &crate::Progress::new()).unwrap();
        let reopened = Mailbox::open(mailbox.store().root()).unwrap();

        let hits = reopened.search_deep("facture", 10).unwrap();
        let from_plumber = hits
            .iter()
            .find(|hit| hit.item.from_addr == "plombier@exemple.fr")
            .unwrap()
            .item
            .id;
        let reply = hits
            .iter()
            .find(|hit| hit.item.from_addr == "moi@exemple.fr")
            .unwrap()
            .item
            .id;

        assert_eq!(
            reopened.contains(reply, "plombier").unwrap(),
            Some(true),
            "le destinataire est indexé, donc le mot est partagé"
        );
        assert_eq!(
            reopened.contains(from_plumber, "voici").unwrap(),
            Some(true)
        );
        // Le contrôle inverse : un mot d'un autre message n'appartient pas à celui-ci.
        assert_eq!(reopened.contains(reply, "voici").unwrap(), Some(false));
        assert_eq!(reopened.contains(reply, "inexistant").unwrap(), Some(false));
    }

    /// Le défaut que `truncated` corrige : une liste coupée au plafond se disait complète.
    #[test]
    fn a_list_cut_at_the_ceiling_says_it_was_cut() {
        assert!(
            crate::query::truncated(MAX_RESULTS, 1_000),
            "cinq cents rendus pour mille demandés : la liste a été coupée par le plafond"
        );
        assert!(crate::query::truncated(20, 20));
        // Le contrôle inverse : moins que demandé et moins que le plafond, rien n'a été coupé.
        assert!(!crate::query::truncated(19, 20));
        assert!(!crate::query::truncated(499, 1_000));
    }

    /// Une limite nulle rend une liste vide, et ne fait pas paniquer tantivy : la limite vient
    /// d'un client.
    #[test]
    fn a_zero_limit_yields_nothing_instead_of_panicking() {
        let (_dir, mailbox) = mailbox();
        crate::index::rebuild(mailbox.store(), &crate::Progress::new()).unwrap();
        let reopened = Mailbox::open(mailbox.store().root()).unwrap();
        assert!(reopened.search("facture", 0).unwrap().is_empty());
        assert!(reopened.search_deep("facture", 0).unwrap().is_empty());
        // Le contrôle inverse : une requête mal formée reste refusée même à limite nulle. La
        // première version du retour anticipé, placée avant l'analyse, la faisait passer en silence.
        assert!(matches!(
            reopened.search("subject:(", 0),
            Err(crate::Error::Query(_))
        ));
        assert!(matches!(
            reopened.search_deep("subject:(", 0),
            Err(crate::Error::Query(_))
        ));
    }

    /// Le piège que ce test a trouvé en échouant : un store non indexé porte un index **vide**,
    /// donc chaque mot y a une fréquence de zéro — il a l'air introuvable, pas inconnu.
    ///
    /// Ce test s'écrivait d'abord « sans index, on ne sait pas » et attendait `None`. Il a échoué
    /// avec `Some(0)`, et c'est la justification qui était fausse, pas le code. D'où
    /// `indexed_documents` et `index_coverage` : la seconde est la façon juste de savoir qu'une
    /// fréquence veut dire quelque chose.
    #[test]
    fn an_unindexed_store_makes_every_word_look_absent_and_says_so_elsewhere() {
        let (_dir, mailbox) = mailbox();
        assert_eq!(mailbox.doc_freq("facture").unwrap(), Some(0));
        assert_eq!(
            mailbox.indexed_documents(),
            Some(0),
            "l'index vide doit se voir au compte, puisqu'il ne se voit pas à la fréquence"
        );
        assert_eq!(mailbox.store().message_count().unwrap(), 2);

        crate::index::rebuild(mailbox.store(), &crate::Progress::new()).unwrap();
        let reopened = Mailbox::open(mailbox.store().root()).unwrap();
        assert_eq!(reopened.indexed_documents(), Some(2));
    }

    #[test]
    fn an_incremental_index_pass_finds_what_a_rebuild_finds() {
        // **La propriété qui justifie la colonne.** Deux chemins mènent maintenant à l'index,
        // et un index tenu à jour qui ne rendrait pas ce qu'un index reconstruit rend serait
        // une recherche subtilement fausse — le pire défaut possible pour un client mail.
        let (_dir, mailbox) = mailbox();
        crate::index::update(mailbox.store(), &crate::Progress::new()).unwrap();

        let reopened = Mailbox::open(mailbox.store().root()).unwrap();
        assert_eq!(reopened.search("facture", 10).unwrap().len(), 2);
    }

    #[test]
    fn indexing_the_same_message_twice_does_not_duplicate_it() {
        // Le défaut que `delete_term` empêche, et qui serait très difficile à rattacher à sa
        // cause : la recherche rendrait deux fois la même ligne.
        let (_dir, mailbox) = mailbox();
        crate::index::update(mailbox.store(), &crate::Progress::new()).unwrap();
        // Les drapeaux remis à la main : c'est exactement l'état que laisserait une coupure
        // entre le `commit` de l'index et le marquage.
        mailbox.store().reset_indexed().unwrap();
        crate::index::update(mailbox.store(), &crate::Progress::new()).unwrap();

        let reopened = Mailbox::open(mailbox.store().root()).unwrap();
        assert_eq!(
            reopened.search("facture", 10).unwrap().len(),
            2,
            "un message a été indexé deux fois"
        );
    }

    #[test]
    fn an_index_kept_up_to_date_merges_its_segments() {
        // Chaque passe qui apporte du courrier valide un segment, et une recherche les parcourt
        // tous. Treize passes, au moins treize segments — quatorze au contrôle négatif : c'est ce
        // que faisait un écrivain lâché sitôt validé, qui abandonne ses fusions — 149 segments
        // dans le store du quotidien, le 2026-10-18. La politique de tantivy
        // fusionne huit segments de même taille ; il en reste donc moins de huit, sinon les
        // fusions n'aboutissent pas.
        let (_dir, mailbox) = mailbox();
        crate::index::update(mailbox.store(), &crate::Progress::new()).unwrap();
        for n in 0..12 {
            add_numbered_message(mailbox.store(), n);
            let stats = crate::index::update(mailbox.store(), &crate::Progress::new()).unwrap();
            assert_eq!(stats.indexed, 1);
        }

        let (index, _) = crate::index::open_or_create(mailbox.store()).unwrap();
        let segments = index.searchable_segment_ids().unwrap().len();
        assert!(
            segments < 8,
            "{segments} segments pour treize passes : les fusions n'aboutissent pas"
        );
        // Rien n'est perdu dans la fusion : les quatorze messages y sont, et chacun une fois.
        let reopened = Mailbox::open(mailbox.store().root()).unwrap();
        assert_eq!(reopened.indexed_documents(), Some(14));
        assert_eq!(reopened.search("chantier", 50).unwrap().len(), 12);
    }

    #[test]
    fn a_cancelled_pass_starts_no_merge() {
        // Le dernier `commit` d'une passe annulée valide ce qui a été fait — et un `commit`, même
        // vide, déclenche les fusions que la politique juge possibles. Sur un store de 149
        // segments, c'était la fusion de tout l'arriéré : des secondes, qu'on attend désormais
        // jusqu'au bout plutôt que de les laisser tourner hors du job. L'annulation doit donc
        // rendre la main sans en lancer aucune.
        let (_dir, mailbox) = mailbox();
        let (index, fields) = crate::index::open_or_create(mailbox.store()).unwrap();
        {
            // Huit segments que rien n'a fusionnés : de quoi faire une fusion de la politique.
            let mut writer: tantivy::IndexWriter = index.writer(15_000_000).unwrap();
            writer.set_merge_policy(Box::new(tantivy::merge_policy::NoMergePolicy));
            for n in 0..8_i64 {
                writer
                    .add_document(tantivy::doc!(fields.id => -1_000 - n))
                    .unwrap();
                writer.commit().unwrap();
            }
            writer.wait_merging_threads().unwrap();
        }
        assert_eq!(index.searchable_segment_ids().unwrap().len(), 8);

        let cancelled = crate::Progress::new();
        cancelled.cancel();
        let stats = crate::index::update(mailbox.store(), &cancelled).unwrap();
        assert_eq!(stats.indexed, 0);
        let (index, _) = crate::index::open_or_create(mailbox.store()).unwrap();
        assert_eq!(
            index.searchable_segment_ids().unwrap().len(),
            8,
            "la passe annulée a fusionné"
        );

        // Le contrôle inverse : la passe suivante, elle, fusionne.
        crate::index::update(mailbox.store(), &crate::Progress::new()).unwrap();
        let (index, _) = crate::index::open_or_create(mailbox.store()).unwrap();
        assert!(index.searchable_segment_ids().unwrap().len() < 8);
    }

    #[test]
    fn the_lock_files_tantivy_leaves_behind_do_not_lock_the_next_pass() {
        // Le verrou de tantivy laisse ses fichiers sur le disque, et le verrou par défaut de son
        // trait prend un fichier présent pour un verrou tenu : sans la délégation de
        // `directory::Patient`, tout store dont l'index a déjà servi refuserait chaque écrivain.
        // Les autres tests partent d'un répertoire neuf, où les deux verrous se confondent —
        // celui-ci part d'un index qui a servi.
        let (_dir, mailbox) = mailbox();
        crate::index::update(mailbox.store(), &crate::Progress::new()).unwrap();
        let search = mailbox.store().search_dir();
        for lock in [".tantivy-writer.lock", ".tantivy-meta.lock"] {
            assert!(search.join(lock).is_file(), "{lock} : la précondition");
        }

        add_numbered_message(mailbox.store(), 0);
        let stats = crate::index::update(mailbox.store(), &crate::Progress::new()).unwrap();
        assert_eq!(stats.indexed, 1);
        let reopened = Mailbox::open(mailbox.store().root()).unwrap();
        assert!(reopened.search_available());
        assert_eq!(reopened.search("chantier", 50).unwrap().len(), 1);
    }

    #[test]
    fn an_unreadable_index_is_not_erased_by_opening_and_a_rebuild_repairs_it() {
        let (_dir, mailbox) = mailbox();
        crate::index::update(mailbox.store(), &crate::Progress::new()).unwrap();
        let root = mailbox.store().root().to_owned();
        // La boîte relâche ses fichiers : sous Windows, un fichier mappé ne s'efface pas.
        drop(mailbox);
        let store = Store::open(&root).unwrap();
        let meta = store.search_dir().join("meta.json");
        std::fs::write(&meta, b"{ abime").unwrap();
        let files = |store: &Store| {
            let mut names: Vec<String> = std::fs::read_dir(store.search_dir())
                .unwrap()
                .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
                .collect();
            names.sort();
            names
        };
        let before = files(&store);

        // L'ouverture refuse, et n'efface rien : elle sert des chemins de lecture, et l'index
        // n'était peut-être illisible que cette fois-ci.
        assert!(crate::index::open_or_create(&store).is_err());
        assert_eq!(std::fs::read(&meta).unwrap(), b"{ abime");
        assert_eq!(files(&store), before);

        // La reconstruction, elle, repart d'un répertoire vide : c'est ce qu'on lui demande, et
        // c'est ce que la coquille propose quand la recherche manque.
        let stats = crate::index::rebuild(&store, &crate::Progress::new()).unwrap();
        assert_eq!(stats.indexed, 2);
        drop(store);
        let reopened = Mailbox::open(&root).unwrap();
        assert!(reopened.search_available());
        assert_eq!(reopened.search("facture", 50).unwrap().len(), 2);
    }

    /// Le câblage du produit : l'index d'une passe passe bien par `directory::Patient`. Le
    /// `meta.json` est tenu comme le tient le veilleur de tantivy — une ouverture ordinaire — et
    /// relâché au bout de 300 ms par un autre fil ; la passe doit attendre au lieu d'échouer.
    #[cfg(windows)]
    #[test]
    fn an_index_pass_waits_for_a_meta_json_held_elsewhere() {
        use tantivy::directory::Directory;

        let (_dir, mailbox) = mailbox();
        crate::index::update(mailbox.store(), &crate::Progress::new()).unwrap();
        add_numbered_message(mailbox.store(), 0);
        let search = mailbox.store().search_dir();
        let held = std::fs::File::open(search.join("meta.json")).unwrap();

        // Le contrôle négatif : le répertoire de tantivy, nu, refuse le remplacement.
        let current = std::fs::read(search.join("meta.json")).unwrap();
        let refused = tantivy::directory::MmapDirectory::open(search.as_std_path())
            .unwrap()
            .atomic_write(std::path::Path::new("meta.json"), &current)
            .unwrap_err();
        assert_eq!(refused.raw_os_error(), Some(5), "{refused}");

        let releaser = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(300));
            drop(held);
        });
        let stats = crate::index::update(mailbox.store(), &crate::Progress::new()).unwrap();
        releaser.join().unwrap();
        assert_eq!(stats.indexed, 1);
    }

    /// Ajoute un message distinct par `n` — même dossier, contenu différent, donc un blob neuf.
    fn add_numbered_message(store: &Store, n: u32) {
        let raw = format!(
            "From: Le Couvreur <couvreur@exemple.fr>\r\n\
             To: moi@exemple.fr\r\n\
             Subject: chantier {n}\r\n\
             Message-ID: <chantier-{n}@exemple.fr>\r\n\
             \r\n\
             Le point sur le chantier, semaine {n}.\r\n"
        );
        let folder = store.folders().unwrap()[0].id;
        let put = store.blobs().put(raw.as_bytes()).unwrap();
        let writer = store.writer().unwrap();
        let (id, _) = writer
            .insert_message(&NewMessage {
                blob: put.hash,
                rfc822_id: None,
                date: 1_700_000_200 + i64::from(n),
                from_addr: "couvreur@exemple.fr",
                from_name: None,
                subject: "chantier",
                size: raw.len() as u64,
                has_attachments: false,
            })
            .unwrap();
        writer
            .insert_ref(
                id,
                folder,
                1_700_000_200 + i64::from(n),
                MessageFlags::empty(),
            )
            .unwrap();
        writer.commit().unwrap();
    }

    #[test]
    fn a_second_pass_indexes_nothing_and_costs_nothing() {
        let (_dir, mailbox) = mailbox();
        crate::index::update(mailbox.store(), &crate::Progress::new()).unwrap();
        let again = crate::index::update(mailbox.store(), &crate::Progress::new()).unwrap();
        assert_eq!(again.indexed, 0);
    }

    #[test]
    fn an_index_wiped_behind_the_store_is_rebuilt_at_the_next_arrival() {
        // **Le cas que le carnet n'a pas**, parce que ses deux moitiés vivent dans le même
        // fichier. Ici le drapeau est dans SQLite et les documents dans tantivy : si l'index
        // disparaît, des drapeaux qui disent « indexé » feraient sauter tous les messages et la
        // recherche resterait vide, sans que rien ne le signale.
        //
        // ## Pourquoi ce test s'appelle « à la prochaine arrivée » depuis le 2026-09-11
        //
        // Il vérifiait que la passe **suivante** rattrapait la divergence, quelle qu'elle soit.
        // Le banc `measure-followup` a montré ce que ça coûtait : ouvrir l'index pour poser la
        // question vaut 11 à 15 ms, payées à chaque moisson — donc en très grande majorité pour
        // des moissons qui n'apportent rien. La passe sort maintenant avant d'ouvrir l'index
        // quand il n'y a rien à indexer.
        //
        // La justification du test tient toujours — un index effacé doit se rattraper — mais son
        // échéance a changé : **à la prochaine arrivée de courrier**, et non à la prochaine
        // passe. C'est le test qui bouge, et voici pourquoi.
        let (_dir, mailbox) = mailbox();
        crate::index::update(mailbox.store(), &crate::Progress::new()).unwrap();
        assert!(mailbox.store().indexed_count().unwrap() > 0);
        // **La boîte relâche l'index avant qu'on l'efface.** Son lecteur suit les validations et
        // garde les segments mappés en mémoire ; sous Windows, un fichier mappé qu'on supprime
        // reste « en suppression », et le répertoire refuse de partir — erreur 145. C'est ce qui
        // faisait tomber ce test sur la CI Windows le 2026-10-09, et pas le comportement qu'il
        // vérifie : la divergence entre les drapeaux et l'index, pas un effacement sous une
        // application ouverte.
        let root = mailbox.store().root().to_owned();
        drop(mailbox);
        let store = Store::open(&root).unwrap();

        // Le répertoire de recherche disparaît — disque remplacé, nettoyage trop zélé.
        //
        // **Et l'effacement attend le veilleur de tantivy**, depuis le 2026-10-19. Relâcher la
        // boîte ne suffisait pas : le `Drop` du veilleur de `meta.json` lui demande de s'arrêter
        // sans l'attendre (`FileWatcher::drop`, tantivy 0.26.1), et pendant jusqu'à 500 ms son fil
        // peut encore tenir le fichier ouvert pour le relire. Sous Windows le répertoire refuse
        // alors de partir — erreur 145. Mesuré : 0 échec sur 60 exécutions de ce test seul, 2 sur
        // 6 de la suite complète, où la charge allonge la lecture. La reconstruction du produit
        // n'a pas ce cas : elle n'efface qu'un index illisible, que personne n'a pu ouvrir.
        wipe_once_released(store.search_dir().as_std_path());

        // Le compromis, vérifié plutôt que décrit : sans rien de nouveau, la passe ne fait rien
        // et ne le découvre pas. C'est ce qu'on a acheté avec les 15 ms.
        let quiet = crate::index::update(&store, &crate::Progress::new()).unwrap();
        assert_eq!(quiet.indexed, 0);

        // Un message arrive, comme une moisson l'apporterait.
        add_one_message(&store);

        let stats = crate::index::update(&store, &crate::Progress::new()).unwrap();
        assert_eq!(stats.indexed, 3, "la divergence n'a pas été rattrapée");

        let reopened = Mailbox::open(&root).unwrap();
        assert_eq!(reopened.search("facture", 10).unwrap().len(), 2);
    }

    /// Efface un répertoire qu'un fil en cours d'arrêt tient peut-être encore, deux secondes au
    /// plus — quatre fois l'intervalle du veilleur de tantivy.
    fn wipe_once_released(dir: &std::path::Path) {
        for _ in 0..40 {
            match std::fs::remove_dir_all(dir) {
                Ok(()) => return,
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::DirectoryNotEmpty
                            | std::io::ErrorKind::PermissionDenied
                    ) =>
                {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                Err(error) => panic!("effacement de {}: {error}", dir.display()),
            }
        }
        panic!("{} toujours tenu après deux secondes", dir.display());
    }

    /// Ajoute un message au premier dossier, comme une moisson le ferait.
    fn add_one_message(store: &Store) {
        let raw = b"From: Le Couvreur <couvreur@exemple.fr>\r\n\
                    To: moi@exemple.fr\r\n\
                    Subject: toiture\r\n\
                    Message-ID: <f3@exemple.fr>\r\n\
                    \r\n\
                    Un devis pour la toiture.\r\n";
        let folder = store.folders().unwrap()[0].id;
        let put = store.blobs().put(raw).unwrap();
        let writer = store.writer().unwrap();
        let (id, _) = writer
            .insert_message(&NewMessage {
                blob: put.hash,
                rfc822_id: None,
                date: 1_700_000_100,
                from_addr: "couvreur@exemple.fr",
                from_name: None,
                subject: "toiture",
                size: raw.len() as u64,
                has_attachments: false,
            })
            .unwrap();
        writer
            .insert_ref(id, folder, 1_700_000_100, MessageFlags::empty())
            .unwrap();
        writer.commit().unwrap();
    }

    #[test]
    fn contact_history_finds_both_directions() {
        let (_dir, mailbox) = mailbox();
        let cancelled = crate::Progress::new();
        crate::index::rebuild(mailbox.store(), &cancelled).unwrap();
        let reopened = Mailbox::open(mailbox.store().root()).unwrap();

        // Le plombier est expéditeur d'un message et destinataire de l'autre.
        let history = reopened.contact_history("plombier@exemple.fr", 10).unwrap();
        assert_eq!(history.len(), 2);
    }

    #[test]
    fn an_address_with_query_syntax_does_not_break_the_search() {
        // Une adresse est une entrée utilisateur : elle ne doit pas pouvoir injecter de
        // syntaxe de requête.
        let (_dir, mailbox) = mailbox();
        let cancelled = crate::Progress::new();
        crate::index::rebuild(mailbox.store(), &cancelled).unwrap();
        let reopened = Mailbox::open(mailbox.store().root()).unwrap();

        for hostile in ["a@b.c\" OR *:*", "((((", "a@b.c AND subject:x", ""] {
            let result = reopened.contact_history(hostile, 10);
            assert!(result.is_ok(), "sur {hostile:?} : {result:?}");
        }
    }

    #[test]
    fn the_result_limit_is_capped() {
        let (_dir, mailbox) = mailbox();
        let cancelled = crate::Progress::new();
        crate::index::rebuild(mailbox.store(), &cancelled).unwrap();
        let reopened = Mailbox::open(mailbox.store().root()).unwrap();

        // Demander l'infini ne doit pas vider le store d'un coup.
        assert!(reopened.search("facture", usize::MAX).unwrap().len() <= MAX_RESULTS);
    }

    #[test]
    fn a_malformed_query_is_an_error_not_a_silent_empty_result() {
        let (_dir, mailbox) = mailbox();
        let cancelled = crate::Progress::new();
        crate::index::rebuild(mailbox.store(), &cancelled).unwrap();
        let reopened = Mailbox::open(mailbox.store().root()).unwrap();

        assert!(matches!(
            reopened.search("subject:(", 10),
            Err(crate::Error::Query(_))
        ));
    }

    #[test]
    fn a_write_made_by_this_very_process_changes_the_revision() {
        // **Le défaut que l'utilisateur a vu : « le statut vu des mails ne se met pas à jour ».**
        //
        // La coquille en mode embarqué sert son propre store : elle écrit et lit par la même
        // connexion SQLite. Or `PRAGMA data_version` ne bouge pas pour les écritures de la
        // connexion qui l'interroge — c'est écrit dans la documentation de SQLite. La révision
        // ne changeait donc jamais, l'abonnement ne signalait rien, et la liste continuait
        // d'afficher « non lu » un message qu'on venait de lire. Le drapeau, lui, partait bien
        // au serveur : c'était l'affichage qui mentait.
        let (_dir, mailbox) = mailbox();
        let before = mailbox.revision().unwrap();

        // Le dossier « Archive » du fixture, dont la référence n'est pas encore lue — celle
        // d'`INBOX` l'est déjà, et `mark_seen` est idempotent.
        let folders = mailbox.folders().unwrap();
        let (folder, unread) = folders
            .iter()
            .find_map(|summary| {
                let page = mailbox.page(summary.folder.id, None, 10).ok()?;
                let item = page
                    .iter()
                    .find(|item| !item.flags.contains(MessageFlags::SEEN))?;
                Some((summary.folder.id, item.id))
            })
            .unwrap();
        let marked = mailbox
            .store()
            .mark_seen(unread, folder, 1_700_000_000)
            .unwrap();
        assert!(marked > 0, "le message devait être marqué");

        assert_ne!(
            mailbox.revision().unwrap(),
            before,
            "une écriture locale doit changer la révision"
        );
    }

    #[test]
    fn a_deliverer_that_dies_mid_send_changes_the_revision() {
        // **Trouvé par la relecture du 2026-10-19.** Une ligne en cours de remise est montrée
        // « envoi en cours » ; si son remettant meurt, elle doit passer à « incertain ». Mais une
        // mort n'écrit rien dans SQLite : sans ce terme de la révision, rien ne faisait relire la
        // file, et le doute restait caché à l'écran jusqu'à la prochaine écriture.
        let (_dir, mailbox) = mailbox();
        let root = mailbox.store().root().to_owned();
        let elsewhere = Store::open(&root).unwrap();
        let before = mailbox.revision().unwrap();

        let lock = elsewhere.lock_delivery().unwrap();
        lock.announce(crate::model::OutboxId(7));
        let sending = mailbox.revision().unwrap();
        assert_ne!(
            sending, before,
            "une remise qui commence ne change pas la révision"
        );

        // La mort : le verrou rendu par le système, le nom laissé derrière.
        drop(lock);
        std::fs::write(root.join("outbox.sending"), "7").unwrap();
        assert_ne!(
            mailbox.revision().unwrap(),
            sending,
            "un remettant mort ne change pas la révision : le doute reste caché"
        );
    }

    #[test]
    fn a_read_alone_does_not_change_the_revision() {
        // Le contrôle inverse, sans lequel le précédent ne prouverait rien : si la révision
        // changeait à chaque lecture, l'abonnement signalerait un changement en boucle et
        // l'interface rechargerait sa liste soixante fois par seconde.
        let (_dir, mailbox) = mailbox();
        let before = mailbox.revision().unwrap();
        let _ = mailbox.page(FolderId(1), None, 10).unwrap();
        let _ = mailbox.folders().unwrap();
        let _ = mailbox.stats().unwrap();
        assert_eq!(mailbox.revision().unwrap(), before);
    }

    /// Range des octets au magasin et rend l'objet que la file en lirait.
    fn subject_of(raw: &[u8]) -> Option<String> {
        let (_dir, mailbox) = mailbox();
        let put = mailbox.store().blobs().put(raw).unwrap();
        mailbox.outgoing_subject(put.hash).unwrap()
    }

    #[test]
    fn an_outgoing_subject_is_read_from_the_header_block() {
        assert_eq!(
            subject_of(b"From: a@b.c\r\nSubject: Devis\r\n\r\nSubject: pas moi\r\n").as_deref(),
            Some("Devis")
        );
    }

    #[test]
    fn an_outgoing_subject_is_decoded() {
        // Un mot encodé, comme `mailsmtp::compose` en écrit pour tout objet accentué.
        assert_eq!(
            subject_of(b"Subject: =?utf-8?Q?R=C3=A9union?=\r\n\r\ncorps\r\n").as_deref(),
            Some("Réunion")
        );
    }

    #[test]
    fn a_subject_in_the_body_is_not_the_subject() {
        // La propriété, pas le mécanisme : `mail-parser` s'arrête lui aussi à la ligne blanche,
        // donc ce test ne verrait pas la boucle de lecture perdre son arrêt — il verrait qu'un
        // « Subject: » du corps est pris pour l'objet, quelle qu'en soit la cause.
        assert_eq!(
            subject_of(b"From: a@b.c\r\n\r\nSubject: dans le corps\r\n"),
            None
        );
        assert_eq!(subject_of(b"From: a@b.c\n\nSubject: dans le corps\n"), None);
    }

    #[test]
    fn hostile_outgoing_subjects_are_tamed() {
        assert_eq!(
            subject_of(b"Subject: =?utf-8?Q?a=1B[2Jb=0Dc?=\r\n\r\n").as_deref(),
            Some("a [2Jb c")
        );
        // Vide, ou blanc : pas d'objet plutôt qu'une ligne vide.
        assert_eq!(subject_of(b"Subject:   \r\n\r\n"), None);
        // Sans corps ni ligne blanche, et sans rien du tout.
        assert_eq!(subject_of(b"Subject: seul").as_deref(), Some("seul"));
        assert_eq!(subject_of(b""), None);
        // Un bloc d'en-têtes sans fin, au-delà de la borne : lu jusqu'à elle, sans paniquer.
        let mut endless = b"Subject: long\r\n".to_vec();
        endless.extend(std::iter::repeat_n(b'x', crate::source::MAX_HEADERS * 2));
        assert_eq!(subject_of(&endless).as_deref(), Some("long"));
    }

    #[test]
    fn a_missing_outgoing_blob_has_no_subject() {
        let (_dir, mailbox) = mailbox();
        let absent = crate::BlobHash::of(b"jamais range");
        assert_eq!(mailbox.outgoing_subject(absent).unwrap(), None);
    }
}
