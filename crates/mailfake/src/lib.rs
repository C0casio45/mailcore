//! Un serveur IMAP de test **qui sait répondre faux**.
//!
//! ## Pourquoi ça existe, alors qu'un Dovecot en conteneur tient debout
//!
//! Un Dovecot répond correctement. C'est utile pour l'interopérabilité, et ça ne suffit pas :
//! la règle du `CLAUDE.md` sur le parsing d'entrée hostile demande des tests **sur des cas
//! malformés**, et un serveur correct ne sait pas répondre de travers.
//!
//! Les pannes qu'on trouve chez un fournisseur — un littéral tronqué, un `UIDVALIDITY` qui
//! change entre deux `SELECT`, une capacité annoncée puis refusée — arrivent une fois, sans
//! reproduction possible, et se débuguent sur des captures. Ici, elles deviennent des
//! [`Fault`], donc des tests.
//!
//! ## Pourquoi rien n'est réutilisé
//!
//! Le crate n'a aucune dépendance IMAP, et c'est le point : **une bibliothèque IMAP refuserait
//! d'émettre ce qu'on veut émettre.** Un littéral dont la longueur annoncée ne correspond pas
//! aux octets qui suivent est exactement ce qu'une bibliothèque correcte empêche d'écrire.
//! Tout le protocole sortant est donc écrit à la main, sur `std::net`.
//!
//! Pas d'async non plus : un fil par connexion, et le harnais de test de Rust n'a alors rien à
//! monter. Un serveur de test qui demande un exécuteur pour démarrer est un serveur de test
//! qu'on n'utilise pas.
//!
//! ## Ce qu'il parle
//!
//! Le sous-ensemble d'IMAP4rev1 qu'une synchronisation **en lecture** demande, plus
//! `CONDSTORE`, `QRESYNC`, `LIST-STATUS` (RFC 5819) et `IDLE` (RFC 2177) : `CAPABILITY`,
//! `LOGIN`, `AUTHENTICATE`, `LIST`, `LSUB`, `SELECT`, `EXAMINE`, `UID FETCH`, `ENABLE`,
//! `IDLE`, `NOOP`, `LOGOUT`. Ni `APPEND`, ni `STORE`, ni `EXPUNGE` — la phase 2 n'écrit pas
//! côté serveur, et un serveur de test qui accepte des commandes que le client n'émet jamais
//! est du code non couvert qui prétend l'être.
//!
//! **Pas de TLS.** Il écoute sur le bouclage, dans un test, et monter une autorité de
//! certification jetable n'éprouverait que `rustls`. Le mode de chiffrement se vérifie sur un
//! vrai serveur, à l'étape 3.
//!
//! ## Comment on s'en sert
//!
//! ```no_run
//! let server = mailfake::Server::start(mailfake::Config::with_inbox()).unwrap();
//! let address = server.address();
//! // …monter un client sur `address`, puis laisser tomber `server` pour l'arrêter.
//! ```

use std::io;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

mod session;
mod wire;

pub use session::CONTINUATION;

/// Un message servi par le serveur.
#[derive(Debug, Clone)]
pub struct Message {
    /// L'UID, tel que le serveur l'annonce. Non nul, croissant dans un dossier.
    pub uid: u32,
    /// Les drapeaux, en toutes lettres : `\Seen`, `\Flagged`…
    pub flags: Vec<String>,
    /// Le `MODSEQ` de cette copie.
    pub modseq: u64,
    /// Les octets RFC 5322 bruts.
    ///
    /// **Pas de normalisation à l'écriture.** Un test doit pouvoir servir un message dont les
    /// fins de ligne sont fausses ou dont un en-tête est illisible ; c'est le client qui doit
    /// s'en sortir, pas le serveur qui doit l'en protéger.
    pub body: Vec<u8>,
}

impl Message {
    /// Un message minimal mais valide, pour les cas où le contenu n'est pas le sujet du test.
    #[must_use]
    pub fn simple(uid: u32, subject: &str) -> Self {
        let body = format!(
            "From: Marie <marie@exemple.fr>\r\n\
             To: Jean <jean@exemple.fr>\r\n\
             Subject: {subject}\r\n\
             Date: Tue, 1 Sep 2026 10:00:00 +0200\r\n\
             Message-ID: <{uid}@exemple.fr>\r\n\
             \r\n\
             Bonjour.\r\n"
        );
        Self {
            uid,
            flags: Vec::new(),
            modseq: u64::from(uid),
            body: body.into_bytes(),
        }
    }
}

/// Une boîte, telle que le serveur la présente.
#[derive(Debug, Clone)]
pub struct Mailbox {
    /// Le nom **en octets**, tel qu'il partira sur le fil.
    ///
    /// Des octets et non une `String` : un test doit pouvoir servir un nom qui n'est pas de
    /// l'UTF-7 modifié valide, ce qu'une chaîne Rust ne permettrait pas d'exprimer.
    pub name: Vec<u8>,
    /// Le séparateur de hiérarchie annoncé dans `LIST`.
    pub delimiter: u8,
    /// `UIDVALIDITY`.
    pub uidvalidity: u32,
    /// Les messages, dans l'ordre des numéros de séquence.
    pub messages: Vec<Message>,
    /// Faux pour une boîte que `LSUB` ne rendra pas.
    pub subscribed: bool,
    /// Ce qui a été purgé, et à quel `MODSEQ`.
    ///
    /// ## Pourquoi une purge se mémorise au lieu de simplement retirer le message
    ///
    /// C'est toute la difficulté que `QRESYNC` résout. Un message retiré de `messages` a
    /// disparu **sans laisser de trace** : un client qui revient ne peut le découvrir qu'en
    /// redemandant la liste complète des UID. Un serveur qui sert `QRESYNC` garde au contraire
    /// l'historique de ses purges, avec le `MODSEQ` auquel chacune a eu lieu, pour pouvoir
    /// répondre « voici ce qui est parti depuis ce point ».
    ///
    /// Un test de reprise doit donc pouvoir exprimer les deux : le message n'est plus dans
    /// `messages`, **et** son départ est daté ici.
    pub expunged: Vec<(u32, u64)>,
    /// Les attributs annoncés dans `LIST`, en toutes lettres.
    ///
    /// ## Pourquoi ils sont configurables
    ///
    /// Les attributs `SPECIAL-USE` de la RFC 6154 — `\Sent`, `\Trash`, `\All` — sont ce qu'un
    /// client doit croire **avant** le nom : un utilisateur qui a renommé sa corbeille
    /// `Poubelle` a un dossier qu'aucune heuristique de nom ne reconnaît.
    ///
    /// Un serveur de test qui n'annoncerait jamais d'attribut ne permettrait donc de tester
    /// que le repli, jamais le chemin principal.
    pub attributes: Vec<String>,
}

impl Mailbox {
    /// Une boîte nommée, avec les messages donnés.
    #[must_use]
    pub fn new(name: &str, uidvalidity: u32, messages: Vec<Message>) -> Self {
        Self {
            name: name.as_bytes().to_vec(),
            delimiter: b'/',
            uidvalidity,
            messages,
            subscribed: true,
            expunged: Vec::new(),
            attributes: vec!["\\HasNoChildren".to_owned()],
        }
    }

    /// Ajoute un attribut `LIST`, typiquement un `SPECIAL-USE`.
    #[must_use]
    pub fn with_attribute(mut self, attribute: &str) -> Self {
        self.attributes.push(attribute.to_owned());
        self
    }

    /// `UIDNEXT` : un de plus que le plus grand UID, au minimum 1.
    #[must_use]
    pub fn uidnext(&self) -> u32 {
        self.messages
            .iter()
            .map(|it| it.uid)
            .max()
            .map_or(1, |it| it.saturating_add(1))
    }

    /// `HIGHESTMODSEQ` : le plus grand `MODSEQ`, ou 1 pour une boîte vide.
    ///
    /// **Les purges comptent.** Une suppression fait avancer le `MODSEQ` d'une boîte : c'est
    /// même la seule chose qui permet à un client de savoir qu'il doit redemander. Un serveur
    /// de test qui n'en tiendrait pas compte laisserait le client conclure « rien n'a changé »
    /// et ne jamais découvrir la purge — et le test de purge passerait en ne testant rien.
    #[must_use]
    pub fn highest_modseq(&self) -> u64 {
        let live = self.messages.iter().map(|it| it.modseq).max();
        let gone = self.expunged.iter().map(|&(_, modseq)| modseq).max();
        live.into_iter().chain(gone).max().unwrap_or(1)
    }

    /// Purge un message : il quitte la boîte, et son départ est daté.
    ///
    /// Le `MODSEQ` de la purge est pris au-dessus de tout ce que la boîte a déjà, pour que le
    /// `HIGHESTMODSEQ` avance — sans quoi le client n'aurait aucune raison de revenir.
    #[must_use]
    pub fn expunge(mut self, uid: u32) -> Self {
        let modseq = self.highest_modseq().saturating_add(1);
        self.messages.retain(|it| it.uid != uid);
        self.expunged.push((uid, modseq));
        self
    }
}

/// Une façon de mal répondre.
///
/// Chaque variante est une panne observée chez un vrai fournisseur, ou une malveillance qu'un
/// serveur compromis pourrait tenter. Aucune n'est théorique.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fault {
    /// Annonce `{n}` puis n'envoie que `sent` octets, et raccroche.
    ///
    /// La panne la plus vicieuse du lot : un client qui lit « jusqu'à la parenthèse » au lieu
    /// de compter les octets se retrouve à attendre pour toujours, ou à prendre la réponse
    /// suivante pour la fin du message.
    TruncatedLiteral {
        /// L'UID dont le corps sera tronqué.
        uid: u32,
        /// Combien d'octets partiront réellement.
        sent: usize,
    },

    /// Annonce une longueur de littéral fausse, puis envoie le corps entier.
    ///
    /// `delta` est ajouté à la longueur réelle : positif, le client attend des octets qui ne
    /// viendront pas ; négatif, il prend la fin du corps pour de la syntaxe IMAP.
    LiteralLengthMismatch {
        /// L'UID visé.
        uid: u32,
        /// L'écart annoncé.
        delta: i64,
    },

    /// Change `UIDVALIDITY` à chaque `SELECT`.
    ///
    /// Ce qu'un serveur fait après une restauration de sauvegarde. Tous les UID connus
    /// deviennent faux d'un coup, et un client qui ne le remarque pas retélécharge — ou
    /// pire, croit avoir déjà tout.
    UidvalidityChangesOnSelect,

    /// Annonce `CONDSTORE` dans `CAPABILITY`, puis refuse `ENABLE` et `CHANGEDSINCE`.
    ///
    /// **C'est le test du chemin de repli.** Un repli qu'on n'exécute jamais n'existe pas, et
    /// celui-là ne s'exécutera que le jour d'une panne — donc jamais en développement, si on
    /// ne le provoque pas.
    AdvertisesCondstoreThenRefuses,

    /// Ferme la connexion après avoir écrit `after` octets, sans rien terminer.
    ///
    /// Le câble débranché au milieu d'une moisson. Le store doit rester cohérent, et la
    /// reprise doit repartir de ce qui est validé.
    ClosesAfter {
        /// Nombre d'octets écrits avant la coupure.
        after: usize,
    },

    /// Refuse l'authentification, avec un `NO` sec.
    RefusesLogin,

    /// Rend un `FETCH` dont le numéro de séquence ne correspond à rien.
    ///
    /// Un client qui indexe par numéro de séquence plutôt que par UID écrira le message au
    /// mauvais endroit. C'est la raison pour laquelle la synchronisation ne doit jamais
    /// s'appuyer sur un numéro de séquence.
    ImpossibleSequenceNumber,

    /// Rend deux fois le même UID dans une même réponse `FETCH`.
    ///
    /// Vu chez des serveurs sous charge. Le client doit être idempotent, pas surpris.
    DuplicateUid {
        /// L'UID qui sortira deux fois.
        uid: u32,
    },

    /// Annonce `LIST-STATUS` dans `CAPABILITY`, puis refuse le `RETURN (STATUS …)`.
    ///
    /// La même panne que [`Fault::AdvertisesCondstoreThenRefuses`], sur l'extension dont
    /// dépend le raccourci du critère 3. Elle vérifie ce qui compte : un refus ne doit pas
    /// faire échouer la synchronisation, il doit la faire retomber sur l'`EXAMINE` par
    /// dossier, avec le même résultat.
    AdvertisesListStatusThenRefuses,

    /// Annonce `IDLE` dans `CAPABILITY`, puis refuse la commande.
    ///
    /// La même famille que [`Fault::AdvertisesCondstoreThenRefuses`]. Un `IDLE` refusé doit
    /// faire retomber le veilleur sur la synchronisation périodique, pas éteindre le compte :
    /// `IDLE` est du confort, la sync périodique est le mécanisme.
    AdvertisesIdleThenRefuses,

    /// Accepte l'`IDLE`, envoie la demande de continuation, puis **ne dit plus rien**.
    ///
    /// Le cas normal d'une boîte tranquille, et celui qui doit rester arrêtable : sans
    /// tranches d'attente, un démon posé là attendrait le prochain message pour s'éteindre.
    IdleStaysSilent,

    /// Répond au `RETURN (STATUS …)`, mais pour une boîte sur deux.
    ///
    /// **La RFC 5819 §2 l'autorise explicitement** : le serveur peut omettre le `STATUS`
    /// d'une boîte qu'il n'a pas pu ouvrir. Un client qui prendrait « pas de `STATUS` » pour
    /// « rien à faire » cesserait de moissonner ces boîtes-là, en silence, et personne ne le
    /// verrait avant d'avoir perdu du courrier.
    PartialListStatus,
}

/// Ce que le serveur annonce et ce qu'il fait.
#[derive(Debug, Clone)]
pub struct Config {
    /// Les boîtes servies.
    pub mailboxes: Vec<Mailbox>,
    /// L'identifiant attendu par `LOGIN`.
    pub username: String,
    /// Le mot de passe attendu par `LOGIN`.
    pub password: String,
    /// Vrai pour annoncer et honorer `CONDSTORE`.
    pub condstore: bool,
    /// Vrai pour annoncer et honorer `QRESYNC`.
    ///
    /// ## Pourquoi c'est un drapeau distinct de `condstore`
    ///
    /// La RFC 7162 dit que `QRESYNC` implique `CONDSTORE`, pas l'inverse. Un serveur qui
    /// suit les `MODSEQ` sans savoir rejouer les purges existe — c'est le cas majoritaire —
    /// et le chemin de repli par balayage doit rester testable pour lui.
    pub qresync: bool,
    /// Le jeton d'accès attendu par `AUTHENTICATE XOAUTH2`.
    ///
    /// `None` : le serveur n'annonce pas `AUTH=XOAUTH2` et refuse la commande. C'est le cas
    /// d'un serveur qui ne connaît que le mot de passe, et un client doit le voir avant
    /// d'essayer.
    pub access_token: Option<String>,
    /// Vrai pour annoncer `SASL-IR`.
    ///
    /// ## Pourquoi c'est configurable
    ///
    /// Avec `SASL-IR`, la réponse initiale tient dans la commande. Sans, il faut un
    /// aller-retour de continuation — et **c'est le chemin qu'un client oublie d'écrire**,
    /// parce que Google annonce `SASL-IR` et que tout marche jusqu'au serveur qui ne
    /// l'annonce pas.
    pub sasl_ir: bool,
    /// Vrai pour annoncer `LIST-EXTENDED` et `LIST-STATUS`, et honorer le `RETURN (STATUS …)`.
    ///
    /// ## Pourquoi c'est configurable
    ///
    /// Les cinq serveurs du corpus réel l'annoncent tous — mesuré le 2026-09-08. C'est
    /// précisément pourquoi le serveur qui ne l'annonce pas doit exister ici : sans lui, le
    /// chemin dossier par dossier ne serait plus jamais exercé, et il resterait celui de tout
    /// serveur plus ancien.
    pub list_status: bool,
    /// Vrai pour annoncer et honorer `IDLE` (RFC 2177).
    pub idle: bool,
    /// Ce que le serveur annonce dès qu'un client se met en `IDLE`.
    ///
    /// ## Pourquoi l'événement est immédiat et non différé
    ///
    /// Un serveur de test qui attendrait N millisecondes avant de parler demanderait un fil de
    /// plus, ou un délai de lecture sur sa propre session — donc du temps réel dans une suite
    /// de tests, ce qui les rend lents et intermittents. Ce que le client doit savoir faire ne
    /// dépend pas du délai : lire l'événement, sortir de l'`IDLE` par un `DONE`, et moissonner.
    ///
    /// Le cas « le serveur se tait » a sa panne à lui, [`Fault::IdleStaysSilent`], et c'est
    /// celle qui exerce l'attente par tranches.
    pub idle_announces: Option<String>,
    /// Les pannes à provoquer.
    pub faults: Vec<Fault>,
}

impl Config {
    /// Une configuration correcte : une `INBOX` de trois messages, `CONDSTORE` actif.
    #[must_use]
    pub fn with_inbox() -> Self {
        Self {
            mailboxes: vec![Mailbox::new(
                "INBOX",
                1_000,
                vec![
                    Message::simple(1, "facture"),
                    Message::simple(2, "devis"),
                    Message::simple(3, "relance"),
                ],
            )],
            username: "marie@exemple.fr".to_owned(),
            password: "secret".to_owned(),
            condstore: true,
            qresync: false,
            access_token: None,
            sasl_ir: true,
            list_status: true,
            idle: true,
            idle_announces: None,
            faults: Vec::new(),
        }
    }

    /// La même, sans `CONDSTORE` : c'est le serveur qui force le chemin de repli.
    #[must_use]
    pub fn without_condstore() -> Self {
        Self {
            condstore: false,
            ..Self::with_inbox()
        }
    }

    /// La même, avec `QRESYNC` : les purges arrivent en `VANISHED`, sans balayage.
    #[must_use]
    pub fn with_qresync() -> Self {
        Self {
            condstore: true,
            qresync: true,
            ..Self::with_inbox()
        }
    }

    /// Un serveur qui n'accepte que `XOAUTH2`, avec ce jeton d'accès.
    ///
    /// Le mot de passe reste configuré mais `LOGIN` sera refusé : c'est ce que fait Google
    /// depuis qu'il a retiré l'authentification par mot de passe. Un client qui essaierait
    /// `LOGIN` doit le découvrir ici, pas en production.
    #[must_use]
    pub fn with_oauth(token: &str) -> Self {
        Self {
            access_token: Some(token.to_owned()),
            ..Self::with_inbox()
        }
    }

    /// Sans `SASL-IR` : la réponse initiale doit passer par une continuation.
    #[must_use]
    pub fn without_sasl_ir(mut self) -> Self {
        self.sasl_ir = false;
        self
    }

    /// Sans `LIST-STATUS` : chaque dossier coûte son `EXAMINE`, comme avant la RFC 5819.
    #[must_use]
    pub fn without_list_status(mut self) -> Self {
        self.list_status = false;
        self
    }

    /// Sans `IDLE` : le courrier n'arrive que si on le demande.
    #[must_use]
    pub fn without_idle(mut self) -> Self {
        self.idle = false;
        self
    }

    /// Un serveur qui annonce cette ligne dès qu'un client se met en `IDLE`.
    #[must_use]
    pub fn announcing_on_idle(mut self, line: &str) -> Self {
        self.idle_announces = Some(line.to_owned());
        self
    }

    /// Ajoute une panne.
    #[must_use]
    pub fn with_fault(mut self, fault: Fault) -> Self {
        self.faults.push(fault);
        self
    }

    /// Remplace les boîtes.
    #[must_use]
    pub fn with_mailboxes(mut self, mailboxes: Vec<Mailbox>) -> Self {
        self.mailboxes = mailboxes;
        self
    }

    /// Vrai si cette panne est demandée.
    pub(crate) fn has(&self, fault: &Fault) -> bool {
        self.faults.contains(fault)
    }

    /// La première panne qui satisfait le prédicat.
    pub(crate) fn find(&self, mut pick: impl FnMut(&Fault) -> bool) -> Option<&Fault> {
        self.faults.iter().find(|it| pick(it))
    }
}

/// L'état mutable partagé par les connexions.
///
/// Une seule chose y mute pour l'instant, et elle a une raison d'être :
/// [`Fault::UidvalidityChangesOnSelect`] doit rendre une valeur **différente à chaque
/// `SELECT`**, ce qu'un état immuable ne peut pas exprimer.
#[derive(Debug)]
pub(crate) struct State {
    pub(crate) config: Config,
    /// Le nombre de `SELECT` servis, pour faire dériver `UIDVALIDITY`.
    pub(crate) selects: u32,
}

/// Un serveur en écoute sur le bouclage.
///
/// L'arrêter est un `drop`. Ce qui rend ça sûr est décrit sur [`Server::drop`].
#[derive(Debug)]
pub struct Server {
    address: SocketAddr,
    stopping: Arc<AtomicBool>,
}

impl Server {
    /// Démarre le serveur sur un port éphémère du bouclage.
    ///
    /// # Errors
    ///
    /// L'erreur d'`io` si le socket ne peut pas être ouvert. **Jamais de panique** : un test
    /// qui n'arrive pas à monter son serveur doit échouer sur un message, pas sur un
    /// `unwrap`.
    pub fn start(config: Config) -> io::Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", 0))?;
        let address = listener.local_addr()?;
        let stopping = Arc::new(AtomicBool::new(false));
        let state = Arc::new(Mutex::new(State { config, selects: 0 }));

        let flag = Arc::clone(&stopping);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                if flag.load(Ordering::SeqCst) {
                    return;
                }
                let Ok(stream) = stream else { continue };
                let state = Arc::clone(&state);
                // **Un fil par connexion.** Servir en série est le défaut qui a bloqué le
                // harnais du critère 8 pendant une exécution entière : un client qui ouvre et
                // attend retient la boucle pour tout le monde.
                std::thread::spawn(move || {
                    if let Err(error) = session::serve(stream, &state) {
                        // Un client qui raccroche au milieu est le comportement **attendu**
                        // de plusieurs pannes. Ça se trace, ça ne se signale pas.
                        tracing::debug!(%error, "session mailfake terminée");
                    }
                });
            }
        });

        tracing::debug!(%address, "mailfake en écoute");
        Ok(Self { address, stopping })
    }

    /// L'adresse à donner au client.
    #[must_use]
    pub const fn address(&self) -> SocketAddr {
        self.address
    }
}

impl Drop for Server {
    /// Arrête la boucle d'acceptation.
    ///
    /// `TcpListener::accept` bloque, donc lever un drapeau ne suffit pas : il faut réveiller
    /// le fil. Une connexion vers soi-même le fait, et la première chose que le fil regarde
    /// en sortant d'`accept` est le drapeau — il rend la main sans servir cette connexion.
    ///
    /// L'échec est ignoré, et il le faut : si le socket est déjà fermé, il n'y a rien à
    /// réveiller, et paniquer dans un `drop` masquerait l'échec du test qui l'a déclenché.
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(self.address);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn uidnext_is_one_past_the_highest_uid() {
        let mailbox = Mailbox::new(
            "INBOX",
            1,
            vec![Message::simple(3, "a"), Message::simple(9, "b")],
        );
        assert_eq!(mailbox.uidnext(), 10);
    }

    #[test]
    fn an_empty_mailbox_still_has_a_uidnext() {
        // Un `UIDNEXT` de zéro n'existe pas : les UID commencent à 1.
        assert_eq!(Mailbox::new("Vide", 1, Vec::new()).uidnext(), 1);
    }

    #[test]
    fn uidnext_does_not_overflow_on_the_last_possible_uid() {
        // Un serveur peut légitimement servir l'UID maximal. Additionner sans saturation
        // paniquerait en debug, dans le serveur de test — soit exactement là où un test
        // deviendrait illisible.
        let mailbox = Mailbox::new("INBOX", 1, vec![Message::simple(u32::MAX, "a")]);
        assert_eq!(mailbox.uidnext(), u32::MAX);
    }

    #[test]
    fn a_mailbox_name_may_be_invalid_utf8() {
        // Ce que `Vec<u8>` autorise et qu'une `String` interdirait.
        let mailbox = Mailbox {
            name: vec![0x49, 0x26, 0xFF, 0xFE],
            ..Mailbox::new("x", 1, Vec::new())
        };
        assert!(String::from_utf8(mailbox.name.clone()).is_err());
    }
}
