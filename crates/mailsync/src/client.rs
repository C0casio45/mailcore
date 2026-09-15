//! Le client IMAP : **le seul endroit du crate qui lise des octets venus du réseau**.
//!
//! ## Ce module est un analyseur d'entrée hostile
//!
//! Le `CLAUDE.md` demande des tests sur des cas malformés pour tout ce qui parse de l'entrée
//! hostile. Un serveur IMAP est exactement ça, et pas seulement quand il est malveillant : un
//! fournisseur sous charge répète un UID, un autre annonce une longueur qui ne correspond pas.
//! Les deux plafonds ci-dessous ne sont donc pas des limites fonctionnelles, ce sont des
//! défenses.
//!
//! ## Deux règles qui portent tout le reste
//!
//! **Un littéral se lit en comptant les octets, jamais jusqu'à un délimiteur.** C'est la seule
//! lecture correcte, et c'est aussi la seule qui distingue un corps de message d'un corps
//! tronqué : un client qui lit « jusqu'à la parenthèse » attend pour toujours sur un littéral
//! coupé, ou prend la réponse suivante pour la fin du message.
//!
//! **On indexe par UID, jamais par numéro de séquence.** Un numéro de séquence change dès
//! qu'un message est purgé, et [`mailfake::Fault::ImpossibleSequenceNumber`] existe pour
//! qu'un client qui s'y appuierait échoue en test plutôt qu'en production. Une réponse `FETCH`
//! sans UID est donc une erreur, pas une ligne à ranger au petit bonheur.
//!
//! ## Pourquoi le flux est un paramètre de type
//!
//! [`Client`] travaille sur n'importe quel `Read + Write`. Ça sert deux choses : les tests
//! parlent à `mailfake` sur le bouclage en clair, et le code de production parle en TLS.
//!
//! **Ce n'est pas une porte vers l'IMAP en clair.** `mailcore::Security` n'a pas de variante
//! en clair : aucune configuration d'un compte ne peut demander une connexion non chiffrée.
//! Le constructeur générique existe pour injecter un flux, pas pour en choisir un.

use std::io::{BufRead, BufReader, Read, Write};
use std::time::Duration;

use crate::error::{Error, Result};

/// Un flux dont on peut borner l'attente d'une lecture.
///
/// ## Pourquoi un trait plutôt qu'un `TcpStream` en dur
///
/// `IDLE` est la seule commande d'IMAP où **le client n'attend rien de précis** : il reste
/// silencieux, et le serveur parle quand il a quelque chose à dire — ou jamais. Une lecture
/// sans borne s'y bloquerait pour toujours, et un démon qu'on ne peut pas arrêter n'est pas
/// arrêtable.
///
/// La borne appartient au flux, pas au protocole : un `TcpStream` la connaît, un tampon en
/// mémoire n'en a pas besoin. Le trait est ce qui laisse [`Client`] rester générique — la
/// raison écrite en tête de ce module — tout en permettant d'attendre par tranches.
pub trait Timed {
    /// La borne en place, telle que le flux la connaît.
    ///
    /// **Elle est lue et non mémorisée par [`Client`].** Le client ne l'a pas posée — c'est
    /// [`crate::connect`] qui le fait — et la deviner pour la remettre après un `IDLE`
    /// laisserait le flux sans borne du tout. Un flux sans borne, c'est une moisson qui se
    /// bloque pour toujours sur un serveur muet.
    ///
    /// # Errors
    ///
    /// Ce que le système rend, tel quel.
    fn read_timeout(&self) -> std::io::Result<Option<Duration>>;

    /// Borne l'attente d'une lecture. `None` retire la borne.
    ///
    /// # Errors
    ///
    /// Ce que le système rend, tel quel.
    fn set_read_timeout(&self, after: Option<Duration>) -> std::io::Result<()>;
}

impl Timed for std::net::TcpStream {
    fn read_timeout(&self) -> std::io::Result<Option<Duration>> {
        Self::read_timeout(self)
    }

    fn set_read_timeout(&self, after: Option<Duration>) -> std::io::Result<()> {
        Self::set_read_timeout(self, after)
    }
}

/// Vrai si cette erreur d'`io` est une attente qui a expiré, et non une panne.
///
/// **Les deux variantes comptent, et c'est spécifique à la plateforme.** Un `SO_RCVTIMEO`
/// dépassé rend `WouldBlock` sur Unix et `TimedOut` sur Windows. N'en tester qu'une ferait
/// marcher `IDLE` sur une plateforme et le ferait tomber en panne franche sur l'autre.
#[must_use]
pub(crate) fn is_timeout(source: &std::io::Error) -> bool {
    matches!(
        source.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
    )
}

/// Plafond de la taille d'un message moissonné.
///
/// 128 Mio, la même valeur que `mailimport::DEFAULT_MESSAGE_LIMIT` — très au-delà du plus gros
/// mail plausible (les serveurs plafonnent la pièce jointe autour de 25 Mo, soit ~35 Mo encodés
/// en base64). La franchir ne veut pas dire « gros message », ça veut dire « le serveur annonce
/// n'importe quoi ».
///
/// La vérification a lieu **avant** l'allocation. Un `{4294967295}` d'un serveur compromis ne
/// doit pas faire réserver quatre gigaoctets pour découvrir ensuite que rien ne suit.
pub const MESSAGE_LIMIT: usize = 128 * 1024 * 1024;

/// Plafond d'une ligne de protocole.
///
/// Une ligne IMAP est courte : une commande, une réponse d'état, un en-tête de `FETCH`. 64 Kio
/// laisse une marge confortable pour une liste de drapeaux ou un nom de boîte long.
///
/// Le plafond existe parce que `read_until` sans limite grossit **jusqu'à la mémoire
/// disponible** face à un serveur qui n'envoie jamais de fin de ligne. C'est un déni de
/// service à une connexion, et il coûte une constante à fermer.
pub const LINE_LIMIT: usize = 64 * 1024;

/// Une réponse du serveur, littéraux séparés du texte.
///
/// ## Pourquoi les littéraux sont à côté et non dans le texte
///
/// Un littéral contient des octets arbitraires — un message entier, avec ses `\r\n`, ses
/// parenthèses et éventuellement des octets non UTF-8. Les recoller dans la ligne rendrait
/// l'analyse de la ligne impossible : on ne saurait plus quelle parenthèse ferme la réponse.
///
/// Ils sont donc rendus dans l'ordre d'apparition, et l'appelant sait lequel il attendait.
/// Dans une réponse `FETCH` de cette moisson, il y en a exactement un : `BODY[]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Raw {
    /// Le texte de la réponse, littéraux retirés.
    pub text: String,
    /// Les littéraux, dans l'ordre où ils sont arrivés.
    pub literals: Vec<Vec<u8>>,
}

impl Raw {
    /// Vrai si c'est une réponse non étiquetée (`* …`).
    #[must_use]
    pub fn is_untagged(&self) -> bool {
        self.text.starts_with("* ")
    }
}

/// L'état d'une réponse étiquetée.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Ok,
    No,
    Bad,
}

/// Un message moissonné.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fetched {
    /// L'UID, la seule identité stable côté serveur.
    pub uid: u32,
    /// Les drapeaux, tels que le serveur les nomme.
    pub flags: Vec<String>,
    /// Le `MODSEQ`, si le serveur en donne un.
    pub modseq: Option<u64>,
    /// Les octets RFC 5322 bruts, ou `None` si la réponse ne portait pas de corps.
    ///
    /// `None` est le cas normal d'un `UID FETCH (FLAGS)` : on moissonne les drapeaux sans
    /// retélécharger les corps, ce qui est l'essentiel d'une synchronisation incrémentale.
    pub body: Option<Vec<u8>>,
    /// `RFC822.SIZE` : la taille que le serveur **annonce**, avant de rien envoyer.
    ///
    /// Elle sert à borner un lot de corps **en octets** et pas seulement en nombre. Cent
    /// messages de 25 Mo font 2,5 Go en mémoire ; cent messages de 3 Ko en font 300. Un lot
    /// compté en messages ne borne donc rien d'utile, et c'est ce que le relevé du 2026-09-09
    /// a montré : 266 Mio de crête sur un compte dont les messages font 580 Ko en moyenne.
    ///
    /// **C'est une annonce, pas une mesure.** Un serveur qui mentirait dessus ramènerait la
    /// borne à celle d'avant — le nombre, multiplié par [`MESSAGE_LIMIT`]. La défense contre
    /// un littéral surdimensionné reste celle du plafond par message.
    pub size: Option<u64>,
}

/// Une boîte annoncée par `LIST`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listed {
    /// Le nom **en octets**, tel que le serveur l'écrit. C'est ce qu'il faut lui renvoyer.
    pub name: Vec<u8>,
    /// Le séparateur de hiérarchie, `None` pour une hiérarchie plate.
    pub delimiter: Option<u8>,
    /// Les attributs, `\HasChildren` et les `SPECIAL-USE` de la RFC 6154 compris.
    pub attributes: Vec<String>,
}

impl Listed {
    /// Vrai si la boîte ne peut pas être sélectionnée.
    ///
    /// `\Noselect` désigne un **nœud de hiérarchie sans contenu** — le `[Gmail]` de Gmail en
    /// est un. Tenter de le moissonner rendrait un `NO`, ce qui ferait échouer une
    /// synchronisation par ailleurs correcte.
    #[must_use]
    pub fn selectable(&self) -> bool {
        !self
            .attributes
            .iter()
            .any(|it| it.eq_ignore_ascii_case("\\Noselect"))
    }

    /// Le rôle du dossier : l'attribut `SPECIAL-USE` s'il y en a un, le nom sinon.
    ///
    /// L'attribut gagne parce que c'est une **déclaration du serveur** là où le nom est une
    /// devinette. Voir `mailcore::FolderKind::from_special_use`.
    #[must_use]
    pub fn kind(&self, path: &str) -> mailcore::FolderKind {
        self.attributes
            .iter()
            .find_map(|it| mailcore::FolderKind::from_special_use(it))
            .unwrap_or_else(|| mailcore::FolderKind::guess(path))
    }
}

/// Ce qu'un `SELECT` a appris.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Selected {
    /// Le nombre de messages annoncé.
    pub exists: u32,
    /// `UIDVALIDITY`. Absent d'un serveur qui n'en donne pas — auquel cas aucune moisson
    /// incrémentale n'est possible, et [`crate::sync`] le traite comme tel.
    pub uidvalidity: Option<u32>,
    /// `UIDNEXT`.
    pub uidnext: Option<u32>,
    /// `HIGHESTMODSEQ`, présent seulement avec `CONDSTORE`.
    pub highest_modseq: Option<u64>,
}

/// Ce qu'un `STATUS` a appris d'une boîte **sans la sélectionner**.
///
/// ## Pourquoi ça mérite un type à côté de [`Selected`]
///
/// Les deux portent presque les mêmes nombres, et la différence est celle qui compte : un
/// `EXAMINE` coûte un aller-retour **par boîte**, un `LIST … RETURN (STATUS …)` en coûte un
/// pour toutes. Mesuré le 2026-09-08 sur `compte-c@gmail.invalid` : 18 `EXAMINE` en 4 775 ms,
/// contre 280 ms pour la même information en une commande.
///
/// Le champ qui manque par rapport à [`Selected`] est celui qu'un `STATUS` ne donne pas : la
/// liste des drapeaux acceptés par la boîte. On ne s'en sert pas.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MailboxStatus {
    /// Le nom **en octets**, tel que le serveur l'écrit — la clé pour retrouver la boîte.
    pub name: Vec<u8>,
    /// `MESSAGES` : ce que l'`EXAMINE` appellerait `EXISTS`.
    pub messages: Option<u32>,
    /// `UIDNEXT`.
    pub uidnext: Option<u32>,
    /// `UIDVALIDITY`.
    pub uidvalidity: Option<u32>,
    /// `HIGHESTMODSEQ`, seulement si l'élément a été demandé **et** rendu.
    pub highest_modseq: Option<u64>,
}

impl MailboxStatus {
    /// La même information sous la forme qu'attend [`crate::sync::plan`].
    ///
    /// **Rend `None` si le serveur n'a pas donné `MESSAGES`.** Sans lui, `exists` vaudrait zéro
    /// par défaut, et zéro est un nombre qui veut dire quelque chose : « le serveur n'a plus
    /// aucun message », donc « tout a disparu ». Un champ absent ne doit pas se déguiser en
    /// boîte vide.
    #[must_use]
    pub fn as_selected(&self) -> Option<Selected> {
        Some(Selected {
            exists: self.messages?,
            uidvalidity: self.uidvalidity,
            uidnext: self.uidnext,
            highest_modseq: self.highest_modseq,
        })
    }
}

/// Le résultat d'un `EXAMINE` avec `QRESYNC` : la boîte, et ce qui a changé depuis.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Resynced {
    /// Ce qu'un `EXAMINE` ordinaire aurait rendu.
    pub selected: Selected,
    /// Les UID purgés depuis le `MODSEQ` demandé.
    ///
    /// **Vide ne veut pas dire « rien n'a disparu »** si l'`UIDVALIDITY` rendu diffère de celui
    /// envoyé : le serveur a alors ignoré le paramètre. Voir [`Client::examine_qresync`].
    pub vanished: Vec<u32>,
    /// Les messages dont les drapeaux ont changé depuis le `MODSEQ` demandé.
    pub changed: Vec<Fetched>,
}

/// Une connexion IMAP authentifiée, ou en cours de l'être.
#[derive(Debug)]
pub struct Client<S> {
    io: BufReader<S>,
    tag: u32,
    capabilities: Vec<String>,
    /// Vrai dès qu'une liste de capacités a été absorbée. Remis à faux au début d'une
    /// authentification, pour que [`Client::refresh_capabilities_if_silent`] sache si le
    /// serveur a parlé **après** elle.
    announced: bool,
    limit: usize,
}

impl<S: Read + Write> Client<S> {
    /// Prend un flux déjà établi et lit le salut du serveur.
    ///
    /// # Errors
    ///
    /// [`Error::Network`] si le salut n'arrive pas, [`Error::Malformed`] s'il n'est pas un
    /// `* OK`. Un serveur qui commence par `* BYE` ou `* NO` refuse la connexion, et le dire
    /// tout de suite vaut mieux que d'envoyer un `LOGIN` dans le vide.
    pub fn greet(stream: S) -> Result<Self> {
        let mut client = Self {
            io: BufReader::new(stream),
            tag: 0,
            capabilities: Vec::new(),
            announced: false,
            limit: MESSAGE_LIMIT,
        };
        let greeting = client.read_response()?;
        if !greeting.text.starts_with("* OK") {
            return Err(Error::Malformed {
                reason: format!("salut inattendu : {}", head(&greeting.text)),
            });
        }
        client.absorb_capabilities(&greeting.text);
        Ok(client)
    }

    /// Prend un flux **sans attendre de salut**.
    ///
    /// ## Le seul cas où c'est correct, et le bug que ça corrige
    ///
    /// Après un `STARTTLS`, le serveur **ne renvoie pas de salut**. La RFC 2595 décrit la
    /// suite : commande `STARTTLS`, réponse `OK`, poignée de main TLS, puis le client
    /// redemande `CAPABILITY`. Il n'y a pas de deuxième `* OK`.
    ///
    /// La première version de [`crate::connect`] appelait [`Client::greet`] après la poignée
    /// de main, donc elle attendait une ligne qui ne viendrait jamais — jusqu'au délai de
    /// lecture de 120 s, rapporté en erreur réseau. **Aucun test ne pouvait l'attraper** :
    /// `mailfake` ne parle pas TLS, donc le chemin `STARTTLS` n'existait que contre un vrai
    /// serveur. Il a été trouvé à la première connexion réelle, le 2026-09-03.
    ///
    /// Les capacités repartent **vides**, et c'est exigé par la même RFC : celles annoncées
    /// avant le chiffrement ont pu être modifiées en vol par quelqu'un qui voulait faire
    /// disparaître `STARTTLS` de la liste.
    #[must_use]
    pub fn over(stream: S) -> Self {
        Self {
            io: BufReader::new(stream),
            tag: 0,
            capabilities: Vec::new(),
            announced: false,
            limit: MESSAGE_LIMIT,
        }
    }

    /// Change le plafond de taille d'un message.
    ///
    /// Sert aux tests, qui veulent provoquer [`Error::LiteralTooLarge`] sans fabriquer 128 Mio.
    #[must_use]
    pub const fn with_message_limit(mut self, limit: usize) -> Self {
        self.limit = limit;
        self
    }

    /// Les capacités connues, en majuscules.
    #[must_use]
    pub fn capabilities(&self) -> &[String] {
        &self.capabilities
    }

    /// Vrai si le serveur a annoncé cette capacité.
    #[must_use]
    pub fn has(&self, capability: &str) -> bool {
        let wanted = capability.to_ascii_uppercase();
        self.capabilities.contains(&wanted)
    }

    /// `CAPABILITY`, pour rafraîchir la liste après authentification.
    ///
    /// Un serveur peut annoncer plus de capacités une fois authentifié : c'est le cas normal,
    /// pas une curiosité. S'en tenir au salut ferait rater `CONDSTORE` chez plusieurs
    /// fournisseurs.
    ///
    /// # Errors
    ///
    /// Voir [`Client::command`].
    pub fn refresh_capabilities(&mut self) -> Result<()> {
        let replies = self.command("CAPABILITY")?;
        for reply in &replies {
            self.absorb_capabilities(&reply.text);
        }
        Ok(())
    }

    /// Redemande les capacités **seulement si le serveur ne les a pas données lui-même**.
    ///
    /// ## Ce que ça répare
    ///
    /// La liste d'avant authentification n'est pas la bonne. Un serveur a le droit d'en
    /// annoncer plus une fois connecté, et Dovecot le fait : `mail.perso.invalid` n'annonce ni
    /// `CONDSTORE`, ni `QRESYNC`, ni `LIST-STATUS` avant le `LOGIN`, et les trois après —
    /// mesuré le 2026-09-08. S'en tenir au salut, c'était synchroniser ce compte par le chemin
    /// de repli en croyant que le serveur n'avait rien de mieux à offrir.
    ///
    /// ## Pourquoi « seulement si »
    ///
    /// Parce que la plupart des serveurs répondent déjà `OK [CAPABILITY …]` au `LOGIN`, et que
    /// cette liste-là est absorbée gratuitement. Redemander systématiquement coûterait un
    /// aller-retour par compte et par passage — 260 ms sur la plus lente des connexions du
    /// corpus, à comparer aux 5 s du critère 3.
    ///
    /// # Errors
    ///
    /// Voir [`Client::command`].
    pub fn refresh_capabilities_if_silent(&mut self) -> Result<()> {
        if self.announced {
            return Ok(());
        }
        self.refresh_capabilities()
    }

    /// Oublie qu'une liste de capacités a été vue, pour ne compter que ce qui vient après.
    ///
    /// Les capacités **déjà connues restent** : elles ne deviennent pas fausses, elles
    /// deviennent incomplètes. C'est le drapeau seul qui est remis à zéro.
    fn forget_announcement(&mut self) {
        self.announced = false;
    }

    /// `LOGIN`.
    ///
    /// ## Le mot de passe est cité, pas concaténé
    ///
    /// Un mot de passe applicatif peut contenir une espace — ceux de Google en contiennent
    /// quatre. Sans guillemets, le serveur lirait la commande comme trois arguments et
    /// refuserait. Et un mot de passe contenant un guillemet ou un antislash doit être
    /// déguisé, sinon il coupe la commande en deux : c'est le même trou qu'une injection, sur
    /// un protocole plus vieux.
    ///
    /// # Errors
    ///
    /// [`Error::AuthRefused`] sur un `NO` ou un `BAD` — et non [`Error::Refused`], parce que
    /// l'appelant doit distinguer « à réessayer » de « demander à l'utilisateur ».
    pub fn login(&mut self, username: &str, password: &str) -> Result<()> {
        self.forget_announcement();
        let command = format!("LOGIN {} {}", quote(username), quote(password));
        match self.command(&command) {
            Ok(_) => Ok(()),
            Err(Error::Refused { reason, .. }) => Err(Error::AuthRefused { reason }),
            Err(other) => Err(other),
        }
    }

    /// `AUTHENTICATE XOAUTH2` : s'authentifier avec un jeton d'accès.
    ///
    /// ## Deux chemins, et le second n'est pas du luxe
    ///
    /// Avec `SASL-IR` annoncé, la réponse initiale tient dans la commande :
    /// `AUTHENTICATE XOAUTH2 <base64>`. C'est un aller-retour de moins, et Google l'annonce.
    ///
    /// Sans `SASL-IR`, il faut envoyer `AUTHENTICATE XOAUTH2` seul, attendre une continuation
    /// `+`, puis envoyer le base64 sur sa propre ligne. Des serveurs le font encore, et un
    /// client qui ne sait que le premier chemin échoue chez eux sans rien dire d'utile.
    ///
    /// ## Le piège de la continuation d'erreur
    ///
    /// Quand le jeton est refusé, Google **ne répond pas `NO` tout de suite** : il envoie une
    /// continuation `+ <base64>` contenant un objet JSON qui dit pourquoi, et il attend une
    /// **ligne vide** avant de conclure par le `NO`.
    ///
    /// Un client qui ne renvoie pas cette ligne attend une réponse qui ne viendra jamais. Ce
    /// n'est pas une erreur qui se voit en relecture : elle se manifeste en blocage, et
    /// seulement quand un jeton est expiré — donc jamais pendant le développement, et toujours
    /// un mois après la mise en service.
    ///
    /// # Errors
    ///
    /// [`Error::AuthRefused`] sur un refus, avec **le JSON du serveur décodé** dans la raison —
    /// c'est la seule information qui distingue un jeton expiré d'un périmètre insuffisant, et
    /// les deux se corrigent autrement. [`Error::MissingCapability`] si le serveur n'annonce
    /// pas `AUTH=XOAUTH2`.
    pub fn authenticate_xoauth2(&mut self, username: &str, access_token: &str) -> Result<()> {
        if !self.has("AUTH=XOAUTH2") {
            return Err(Error::MissingCapability {
                capability: "AUTH=XOAUTH2".to_owned(),
            });
        }
        self.forget_announcement();
        let response = crate::sasl::xoauth2(username, access_token);

        // **La commande n'est pas journalisée**, contrairement aux autres : sa ligne porte le
        // jeton d'accès en base64, et `docs/PRIVACY.md` §8 interdit qu'un secret finisse dans
        // un journal — même trivialement réversible.
        self.tag += 1;
        let tag = format!("m{}", self.tag);
        let mut answered = self.has("SASL-IR");
        if answered {
            self.raw_line(&format!("{tag} AUTHENTICATE XOAUTH2 {response}"))?;
        } else {
            self.raw_line(&format!("{tag} AUTHENTICATE XOAUTH2"))?;
        }

        loop {
            let reply = self.read_response()?;

            if let Some(rest) = reply.text.strip_prefix('+') {
                if !answered {
                    // Le serveur demande la réponse. **Une ligne nue, sans étiquette** : une
                    // réponse SASL n'est pas une commande.
                    //
                    // La première version l'envoyait étiquetée, et le serveur comparait donc
                    // `m2 <base64>` au SASL attendu. Le refus était indiscernable d'un mauvais
                    // jeton, et le chemin sans `SASL-IR` était le seul touché — donc invisible
                    // chez Google, qui l'annonce.
                    self.raw_line(&response)?;
                    answered = true;
                    continue;
                }

                // On a déjà répondu : cette continuation est le message d'erreur. Il faut une
                // **ligne vide** pour que le serveur conclue par son `NO`.
                let reason = crate::sasl::decode_challenge(rest);
                self.raw_line("")?;
                let closing = self.read_until_tagged(&tag);
                return Err(Error::AuthRefused {
                    reason: match closing {
                        Some(text) if !text.is_empty() => format!("{reason} — {text}"),
                        _ => reason,
                    },
                });
            }

            if let Some((status, text)) = tagged(&reply.text, &tag) {
                // Comme pour le `LOGIN` : le `OK` d'un `AUTHENTICATE` porte souvent la liste
                // d'après authentification, et c'est la seule qui vaille.
                self.absorb_capabilities(&reply.text);
                return match status {
                    Status::Ok => Ok(()),
                    Status::No | Status::Bad => Err(Error::AuthRefused { reason: text }),
                };
            }

            // Une réponse non étiquetée pendant l'authentification — un `* CAPABILITY`, que
            // des serveurs glissent là. Absorbée, pas rejetée.
            self.absorb_capabilities(&reply.text);
        }
    }

    /// Écrit une ligne brute, terminée par `CRLF`, sans rien journaliser.
    fn raw_line(&mut self, line: &str) -> Result<()> {
        let mut bytes = line.as_bytes().to_vec();
        bytes.extend_from_slice(b"\r\n");
        self.io.get_mut().write_all(&bytes)?;
        self.io.get_mut().flush()?;
        Ok(())
    }

    /// Lit jusqu'à la réponse étiquetée et rend son texte.
    ///
    /// Les échecs sont avalés : on est déjà sur un chemin d'erreur, et la vraie cause est le
    /// message du serveur qu'on vient de lire.
    fn read_until_tagged(&mut self, tag: &str) -> Option<String> {
        for _ in 0..8 {
            let reply = self.read_response().ok()?;
            if let Some((_, text)) = tagged(&reply.text, tag) {
                return Some(text);
            }
        }
        None
    }

    /// `ENABLE CONDSTORE`, et rend vrai si le serveur l'a vraiment activé.
    ///
    /// ## Pourquoi le retour n'est pas un `Result<()>`
    ///
    /// Un serveur peut annoncer `CONDSTORE` puis refuser de l'activer — c'est
    /// [`mailfake::Fault::AdvertisesCondstoreThenRefuses`], et c'est une vraie panne de
    /// fournisseur. Ce n'est pas une erreur de synchronisation : il y a un chemin de repli, et
    /// il faut l'emprunter. Rendre `Err` obligerait l'appelant à distinguer ce refus-là de
    /// tous les autres en relisant un message.
    ///
    /// # Errors
    ///
    /// [`Error::Network`] seulement. Un refus du serveur rend `Ok(false)`.
    pub fn enable_condstore(&mut self) -> Result<bool> {
        if !self.has("CONDSTORE") {
            return Ok(false);
        }
        match self.command("ENABLE CONDSTORE") {
            Ok(replies) => Ok(was_enabled(&replies, "CONDSTORE")),
            Err(Error::Refused { .. }) => {
                tracing::info!("le serveur annonce CONDSTORE puis le refuse : repli par UID");
                Ok(false)
            }
            Err(other) => Err(other),
        }
    }

    /// `ENABLE QRESYNC`, et rend vrai si le serveur l'a vraiment activé.
    ///
    /// ## Ce que `QRESYNC` change, et pourquoi il valait le détour
    ///
    /// Sans lui, détecter les purges demande un balayage `UID FETCH 1:* (UID)` par dossier et
    /// par passage : le serveur renvoie une ligne par message, qu'il se soit passé quelque
    /// chose ou non. **Mesuré le 2026-09-08** sur le corpus réel : 67 s pour une
    /// synchronisation où rien n'avait changé, sur un compte de 51 496 messages.
    ///
    /// Avec `QRESYNC`, l'`EXAMINE` rend directement ce qui a disparu et ce dont les drapeaux
    /// ont bougé depuis un `MODSEQ` donné. Le coût d'un passage devient proportionnel à ce qui
    /// a changé, c'est-à-dire à rien quand rien n'a changé.
    ///
    /// ## `QRESYNC` implique `CONDSTORE`
    ///
    /// La RFC 7162 §3.2.3 l'exige : activer `QRESYNC` active `CONDSTORE`. Il est donc inutile
    /// — et incorrect selon certains serveurs — d'envoyer les deux `ENABLE`.
    ///
    /// # Errors
    ///
    /// [`Error::Network`] seulement. Un refus du serveur rend `Ok(false)`, comme pour
    /// [`Client::enable_condstore`] et pour la même raison : il y a un chemin de repli.
    pub fn enable_qresync(&mut self) -> Result<bool> {
        if !self.has("QRESYNC") {
            return Ok(false);
        }
        match self.command("ENABLE QRESYNC") {
            Ok(replies) => Ok(was_enabled(&replies, "QRESYNC")),
            Err(Error::Refused { .. }) => {
                tracing::info!("le serveur annonce QRESYNC puis le refuse : repli par balayage");
                Ok(false)
            }
            Err(other) => Err(other),
        }
    }

    /// `EXAMINE … (QRESYNC (uidvalidity modseq))` : la boîte **et** ce qui a changé.
    ///
    /// ## Ce que le serveur rend en plus d'un `EXAMINE` ordinaire
    ///
    /// - `* VANISHED (EARLIER) <ensemble>` — les UID purgés depuis `modseq` ;
    /// - des `* … FETCH (UID … FLAGS … MODSEQ …)` — les drapeaux modifiés depuis `modseq`.
    ///
    /// Les deux ensemble remplacent le balayage **et** la passe de drapeaux.
    ///
    /// ## `uidvalidity` est envoyé, et il n'est pas décoratif
    ///
    /// C'est lui qui autorise le serveur à répondre. S'il ne correspond plus, un serveur
    /// conforme ignore le paramètre et répond comme à un `EXAMINE` nu — donc **sans**
    /// `VANISHED`. L'appelant doit comparer l'`UIDVALIDITY` rendu au sien avant de croire que
    /// « rien n'a disparu » ; [`crate::sync`] le fait, et une moisson complète est alors la
    /// seule réponse correcte.
    ///
    /// # Errors
    ///
    /// Voir [`Client::command`]. [`Error::Malformed`] si un `FETCH` arrive sans UID.
    pub fn examine_qresync(
        &mut self,
        mailbox: &[u8],
        uidvalidity: u32,
        modseq: u64,
    ) -> Result<Resynced> {
        let mut command = b"EXAMINE ".to_vec();
        command.extend_from_slice(&quote_bytes(mailbox));
        command.extend_from_slice(format!(" (QRESYNC ({uidvalidity} {modseq}))").as_bytes());
        let replies = self.command_bytes(&command)?;

        let mut out = Resynced::default();
        for reply in &replies {
            let text = &reply.text;
            if let Some(count) = untagged_number(text, "EXISTS") {
                out.selected.exists = count;
            }
            if let Some(value) = bracketed(text, "UIDVALIDITY") {
                out.selected.uidvalidity = value.parse().ok();
            }
            if let Some(value) = bracketed(text, "UIDNEXT") {
                out.selected.uidnext = value.parse().ok();
            }
            if let Some(value) = bracketed(text, "HIGHESTMODSEQ") {
                out.selected.highest_modseq = value.parse().ok();
            }
            if let Some(set) = vanished_set(text) {
                out.vanished.extend(parse_uid_set(set));
            }
            if is_fetch(text) {
                out.changed.push(parse_fetch(reply)?);
            }
        }
        Ok(out)
    }

    /// `LIST "" "*"`, et rend les boîtes avec leurs attributs.
    ///
    /// Le nom est rendu **en octets** parce qu'un nom de boîte est de l'UTF-7 modifié et que
    /// rien ne garantit qu'il soit valide. C'est cette suite d'octets qu'il faudra renvoyer
    /// dans un `EXAMINE` ; la décoder pour l'affichage est un autre problème
    /// ([`crate::mutf7`]), et il ne doit pas abîmer celle-ci.
    ///
    /// # Errors
    ///
    /// Voir [`Client::command`].
    pub fn list(&mut self) -> Result<Vec<Listed>> {
        let replies = self.command("LIST \"\" \"*\"")?;
        let mut out = Vec::new();
        for reply in &replies {
            if let Some(listed) = parse_list(&reply.text) {
                out.push(listed);
            }
        }
        Ok(out)
    }

    /// `LIST` avec l'état de chaque boîte, en **une** commande (RFC 5819).
    ///
    /// `items` est la liste des éléments `STATUS` demandés, sans les parenthèses :
    /// `"MESSAGES UIDNEXT UIDVALIDITY"`.
    ///
    /// ## Ce que ça remplace
    ///
    /// Un `EXAMINE` par dossier, c'est-à-dire un aller-retour par dossier. Sur un compte à
    /// dix-huit dossiers et 260 ms d'aller-retour, ça fait 4,8 s dépensées à demander
    /// dix-huit fois « quoi de neuf ? » — l'essentiel de ce qui faisait échouer le critère 3
    /// de `docs/PHASE-2.md`.
    ///
    /// ## Une boîte sans `STATUS` n'est pas une erreur
    ///
    /// Le serveur a le droit de ne pas répondre pour une boîte qu'il n'a pas pu ouvrir, et
    /// la RFC 5819 §2 le dit explicitement. L'appelant retombe alors sur l'`EXAMINE` pour
    /// celle-là, ce qui est le comportement d'avant : la commande accélère, elle ne décide
    /// pas.
    ///
    /// ## Un nom en littéral est écarté
    ///
    /// [`Raw`] range les littéraux à part, et le texte n'en garde pas la place : on ne peut
    /// donc pas recoller le nom à son `STATUS`. Plutôt que de deviner, l'entrée est ignorée
    /// et la boîte repasse par l'`EXAMINE`. C'est la même limite que [`Client::list`], qui ne
    /// lit pas non plus les noms en littéral, si bien qu'une telle boîte n'est de toute façon
    /// pas dans l'ensemble découvert.
    ///
    /// # Errors
    ///
    /// Voir [`Client::command`]. Un serveur qui annonce `LIST-STATUS` puis refuse la commande
    /// rend [`Error::Refused`] — l'appelant doit **continuer sans**, pas échouer.
    pub fn list_status(&mut self, items: &str) -> Result<Vec<MailboxStatus>> {
        let replies = self.command(&format!("LIST \"\" \"*\" RETURN (STATUS ({items}))"))?;
        let mut out = Vec::new();
        for reply in &replies {
            if !reply.literals.is_empty() {
                continue;
            }
            if let Some(status) = parse_status(&reply.text) {
                out.push(status);
            }
        }
        Ok(out)
    }

    /// `EXAMINE`, et non `SELECT`.
    ///
    /// ## Pourquoi `EXAMINE`
    ///
    /// `EXAMINE` est un `SELECT` en lecture seule : il n'efface pas `\Recent` et ne permet
    /// aucune écriture. La phase 2 n'écrit pas côté serveur (`docs/PHASE-2.md`), et se donner
    /// les droits qu'on n'utilise pas est ce qui permet à un bug de faire des dégâts.
    ///
    /// # Errors
    ///
    /// Voir [`Client::command`]. [`Error::Refused`] si la boîte est inconnue.
    pub fn examine(&mut self, mailbox: &[u8]) -> Result<Selected> {
        // Le nom part en octets, cité. Un nom qui n'est pas de l'UTF-8 ne peut pas passer par
        // `format!`, donc la commande est assemblée à la main.
        let mut command = b"EXAMINE ".to_vec();
        command.extend_from_slice(&quote_bytes(mailbox));
        let replies = self.command_bytes(&command)?;
        Ok(Self::selected_from(&replies))
    }

    /// `SELECT`, la sélection **en lecture-écriture**.
    ///
    /// ## Pourquoi elle existe alors que [`Client::examine`] est le défaut
    ///
    /// `EXAMINE` interdit toute écriture, et c'est ce qui fait que la moisson ne peut pas
    /// abîmer une boîte même en cas de bogue. La règle de la phase 2 tient : **rien n'écrit
    /// côté serveur, sauf `\Seen`**.
    ///
    /// Or `UID STORE` sur une boîte ouverte en `EXAMINE` est refusé par le serveur — à juste
    /// titre. Cette fonction est donc appelée **seulement** pour les dossiers qui ont une
    /// poussée en attente ; partout ailleurs, `examine` reste le chemin. Se donner les droits
    /// qu'on n'utilise pas est ce qui permet à un bug de faire des dégâts, et la conséquence
    /// est que le droit d'écrire est demandé au dernier moment et pour un dossier à la fois.
    ///
    /// ## Elle efface `\Recent`
    ///
    /// C'est la différence observable entre les deux, et elle est sans conséquence ici : rien
    /// dans `mailcore` ne lit `\Recent`, dont la sémantique — « arrivé depuis la dernière
    /// session » — n'est de toute façon pas exploitable par un client qui se connecte souvent.
    ///
    /// # Errors
    ///
    /// Voir [`Client::command`]. [`Error::Refused`] si la boîte est inconnue, ou si le serveur
    /// refuse de l'ouvrir en écriture — un dossier `\Noselect`, ou une boîte partagée en
    /// lecture seule. Le refus est rendu tel quel : réessayer en `EXAMINE` derrière le dos de
    /// l'appelant lui ferait croire que la poussée aura lieu.
    pub fn select(&mut self, mailbox: &[u8]) -> Result<Selected> {
        let mut command = b"SELECT ".to_vec();
        command.extend_from_slice(&quote_bytes(mailbox));
        let replies = self.command_bytes(&command)?;
        Ok(Self::selected_from(&replies))
    }

    /// Ajoute `\Seen` à un ensemble d'UID de la boîte courante.
    ///
    /// ## `+FLAGS.SILENT` et non `+FLAGS`
    ///
    /// Le `.SILENT` demande au serveur de **ne pas** renvoyer les `FETCH` de confirmation. Sans
    /// lui, un ensemble de deux cents UID rend deux cents réponses non étiquetées qu'il faudrait
    /// analyser pour rien : l'état local est déjà écrit, et c'est la réponse étiquetée qui dit
    /// si le serveur a accepté.
    ///
    /// ## `+FLAGS` et non `FLAGS`
    ///
    /// `FLAGS` **remplace** la liste entière, donc effacerait `\Flagged` et `\Answered` sur les
    /// messages qui les portent. `+FLAGS` ajoute. La différence est d'un caractère et elle
    /// détruit des données de l'utilisateur.
    ///
    /// ## Elle est idempotente côté serveur
    ///
    /// Ajouter `\Seen` à un message déjà lu ne fait rien. C'est ce qui permet à
    /// `Store::forget_pending_seen` d'être appelé **après** la réponse : une poussée refaite
    /// est sans effet, une poussée perdue serait une marque perdue.
    ///
    /// # Errors
    ///
    /// Voir [`Client::command`]. [`Error::Refused`] si la boîte est ouverte en lecture seule.
    pub fn store_seen(&mut self, set: &str) -> Result<()> {
        self.command(&format!(r"UID STORE {set} +FLAGS.SILENT (\Seen)"))?;
        Ok(())
    }

    /// Lit ce qu'une sélection annonce. Commun à `SELECT` et `EXAMINE`.
    ///
    /// Extrait plutôt que dupliqué : les deux commandes rendent exactement les mêmes réponses
    /// non étiquetées, et deux copies de l'analyse finiraient par ne plus lire les mêmes.
    fn selected_from(replies: &[Raw]) -> Selected {
        let mut out = Selected::default();
        for reply in replies {
            let text = &reply.text;
            if let Some(count) = untagged_number(text, "EXISTS") {
                out.exists = count;
            }
            if let Some(value) = bracketed(text, "UIDVALIDITY") {
                out.uidvalidity = value.parse().ok();
            }
            if let Some(value) = bracketed(text, "UIDNEXT") {
                out.uidnext = value.parse().ok();
            }
            if let Some(value) = bracketed(text, "HIGHESTMODSEQ") {
                out.highest_modseq = value.parse().ok();
            }
        }
        out
    }

    /// `UID FETCH`, avec les éléments demandés.
    ///
    /// `set` est un ensemble d'UID au sens de la RFC 3501 (`1:*`, `4,7:9`). `items` est la
    /// liste entre parenthèses, sans les parenthèses.
    ///
    /// ## Ce que la fonction refuse
    ///
    /// Une réponse `FETCH` **sans UID** est une erreur ([`Error::Malformed`]) et non une ligne
    /// ignorée. Sans UID, il n'y a aucun endroit sûr où ranger le message : le numéro de
    /// séquence ne suffit pas, et [`mailfake::Fault::ImpossibleSequenceNumber`] existe pour
    /// qu'un client qui s'y appuierait le découvre ici.
    ///
    /// # Errors
    ///
    /// Voir [`Client::command`], plus [`Error::Malformed`] et [`Error::LiteralTooLarge`].
    pub fn uid_fetch(&mut self, set: &str, items: &str) -> Result<Vec<Fetched>> {
        let mut out = Vec::new();
        self.uid_fetch_each(set, items, &mut |fetched| {
            out.push(fetched);
            Ok(())
        })?;
        Ok(out)
    }

    /// `UID FETCH`, **un message à la fois**.
    ///
    /// ## Pourquoi cette forme existe à côté de [`Client::uid_fetch`]
    ///
    /// Règle 4 du `CLAUDE.md` : rien ne charge un corpus entier en mémoire. Un `UID FETCH
    /// 1:* (BODY[])` sur un dossier de 100 000 messages rendrait 4 Go de corps dans un
    /// `Vec` — et le plafond par message ne protège pas de ça, il protège d'un seul message
    /// démesuré.
    ///
    /// Ici, un corps est en mémoire à la fois : il est passé à `on`, qui l'écrit dans le
    /// store, puis libéré. C'est la même discipline que le lecteur de mbox de la phase 1.
    ///
    /// `uid_fetch` reste pour ce qui ne porte pas de corps — une moisson de drapeaux tient
    /// dans un `Vec` sans y penser.
    ///
    /// # Errors
    ///
    /// Voir [`Client::command`]. Une erreur rendue par `on` interrompt la moisson et remonte
    /// telle quelle : un échec d'écriture dans le store doit arrêter la lecture, pas être
    /// avalé pour continuer à télécharger dans le vide.
    pub fn uid_fetch_each(
        &mut self,
        set: &str,
        items: &str,
        on: &mut dyn FnMut(Fetched) -> Result<()>,
    ) -> Result<()> {
        let command = format!("UID FETCH {set} ({items})");
        self.command_each(command.as_bytes(), &mut |reply| {
            if !is_fetch(&reply.text) {
                return Ok(());
            }
            on(parse_fetch(&reply)?)
        })
    }

    /// `LOGOUT`. Les échecs sont ignorés : la connexion se ferme de toute façon.
    pub fn logout(&mut self) {
        let _ = self.command("LOGOUT");
    }

    /// Reprend le flux, en abandonnant l'état du dialogue.
    ///
    /// Sert au seul cas où c'est correct : `STARTTLS`, qui remplace le flux en clair par un
    /// flux chiffré et **repart d'un salut neuf**. Tout ce que le client croyait savoir —
    /// capacités comprises — est jeté avec lui, ce qui est le comportement exigé par la
    /// RFC 2595 : les capacités d'avant le chiffrement ont pu être modifiées en vol.
    ///
    /// Le tampon de lecture est perdu avec le `BufReader`. Ce n'est correct **que** parce
    /// qu'un serveur conforme n'envoie rien après son `OK STARTTLS` avant la poignée de main ;
    /// des octets en attente à ce moment-là seraient une injection, et les jeter est la bonne
    /// réponse.
    #[must_use]
    pub fn into_stream(self) -> S {
        self.io.into_inner()
    }

    /// `IDLE` : demande au serveur de parler quand il aura quelque chose à dire (RFC 2177).
    ///
    /// Rend l'étiquette de la commande, qu'il faudra passer à [`Client::idle_done`]. **La boîte
    /// doit être sélectionnée** : `IDLE` ne rapporte que ce qui arrive dans la boîte courante,
    /// et c'est la limite structurelle de l'extension — surveiller quatre-vingts dossiers
    /// demanderait quatre-vingts connexions, ce qu'aucun fournisseur n'accepte.
    ///
    /// ## Ce que la fonction refuse
    ///
    /// Un serveur qui n'annonce pas `IDLE` : [`Error::MissingCapability`]. Un serveur qui l'annonce et ne
    /// répond pas la demande de continuation : [`Error::Malformed`] — sans le `+`, il n'est pas
    /// en attente, et rester silencieux devant lui serait un blocage des deux côtés.
    ///
    /// # Errors
    ///
    /// Voir ci-dessus, plus [`Error::Network`].
    pub fn idle_start(&mut self) -> Result<String> {
        if !self.has("IDLE") {
            return Err(Error::MissingCapability {
                capability: "IDLE".to_owned(),
            });
        }
        self.tag += 1;
        let tag = format!("m{}", self.tag);
        self.raw_line(&format!("{tag} IDLE"))?;

        // Le serveur répond `+ idling`. Il peut aussi glisser des réponses non étiquetées
        // avant — un `* OK` de courtoisie — donc on lit jusqu'au `+` plutôt que d'exiger qu'il
        // arrive le premier.
        for _ in 0..8 {
            let reply = self.read_response()?;
            if reply.text.starts_with('+') {
                return Ok(tag);
            }
            if let Some((_, text)) = tagged(&reply.text, &tag) {
                return Err(Error::Refused {
                    command: "IDLE".to_owned(),
                    reason: text,
                });
            }
            self.absorb_capabilities(&reply.text);
        }
        Err(Error::Malformed {
            reason: "IDLE sans demande de continuation".to_owned(),
        })
    }

    /// Attend au plus `tick`, et rend ce que le serveur a dit — ou rien.
    ///
    /// `Ok(None)` veut dire **« il n'a rien dit »**, ce qui est le cas normal d'une boîte
    /// tranquille. L'appelant en profite pour regarder s'il doit s'arrêter, puis rappelle. Les
    /// tranches sont ce qui rend un démon arrêtable : sans elles, l'arrêt attendrait le
    /// prochain message, c'est-à-dire peut-être des heures.
    ///
    /// ## Une seule ligne à la fois, et pourquoi c'est suffisant
    ///
    /// Un événement d'`IDLE` tient sur une ligne — `* 4 EXISTS`, `* 2 EXPUNGE`, `* 3 FETCH
    /// (FLAGS …)`. L'appelant n'a pas à les collecter tous : le premier suffit à décider qu'il
    /// faut moissonner, et la moisson redemandera l'état de toute façon. Rendre la première
    /// ligne et laisser l'appelant sortir évite d'inventer une règle sur « combien
    /// d'événements attendre encore ».
    ///
    /// # Errors
    ///
    /// [`Error::Network`] sur le socket. [`Error::Malformed`] si l'attente expire **au milieu
    /// d'une ligne** : la connexion n'est alors plus synchronisée et doit être jetée.
    pub fn idle_wait(&mut self, tick: Duration) -> Result<Option<Raw>>
    where
        S: Timed,
    {
        let ordinary = self.io.get_ref().read_timeout()?;
        self.io.get_ref().set_read_timeout(Some(tick))?;
        let waited = self.read_line_or_quiet();
        // La borne est rendue **quoi qu'il arrive** : la laisser en place ferait expirer la
        // prochaine moisson au bout d'une tranche, et une tranche est bien plus courte que le
        // délai de lecture ordinaire.
        let restored = self.io.get_ref().set_read_timeout(ordinary);
        let Some(line) = waited? else {
            restored?;
            return Ok(None);
        };
        restored?;

        // La ligne peut annoncer un littéral — pas pour les événements d'`IDLE` connus, mais
        // un serveur a le droit d'en envoyer un, et le lire est la seule façon de rester
        // synchronisé. `read_response` s'en charge à partir de la ligne suivante ; ici on n'a
        // qu'une ligne, donc on refuse ce cas plutôt que de le lire à moitié.
        if literal_length(&line).is_some() {
            return Err(Error::Malformed {
                reason: "littéral inattendu dans une réponse d'IDLE".to_owned(),
            });
        }
        self.absorb_capabilities(&line);

        // **`* BYE` met fin à l'`IDLE`, il ne l'anime pas.** Le serveur ferme ; continuer à
        // attendre sur cette connexion attendrait pour toujours. C'est une erreur, donc une
        // reconnexion chez l'appelant.
        if line
            .strip_prefix("* ")
            .is_some_and(|rest| rest.len() >= 3 && rest[..3].eq_ignore_ascii_case("BYE"))
        {
            return Err(Error::Malformed {
                reason: format!("le serveur met fin à la connexion : {}", head(&line)),
            });
        }

        // **Tout ce que le serveur dit n'est pas un changement.** Dovecot envoie
        // `* OK Still here` toutes les deux minutes pendant un `IDLE`, par courtoisie. Rendre
        // cette ligne comme un événement faisait resynchroniser le compte toutes les deux
        // minutes — observé le 2026-09-09 sur `mail.perso.invalid`, cinq passages inutiles en
        // dix minutes, chacun sautant ses quinze dossiers pour ne rien trouver.
        if !announces_change(&line) {
            tracing::debug!(ligne = %head(&line), "IDLE : réponse sans changement, on attend");
            return Ok(None);
        }

        Ok(Some(Raw {
            text: line,
            literals: Vec::new(),
        }))
    }

    /// `DONE` : sort de l'`IDLE` et attend le `OK` étiqueté.
    ///
    /// `DONE` est **une ligne nue, sans étiquette** : ce n'est pas une commande, c'est la fin de
    /// celle qui est en cours. L'étiqueter ferait attendre le serveur pour toujours.
    ///
    /// Les réponses non étiquetées qui arrivent avant le `OK` sont rendues : le serveur a le
    /// droit d'annoncer un dernier événement pendant qu'on sort, et le jeter perdrait
    /// l'information qui justifiait de sortir.
    ///
    /// # Errors
    ///
    /// [`Error::Network`], ou [`Error::Refused`] si le serveur conclut par autre chose qu'un
    /// `OK`.
    pub fn idle_done(&mut self, tag: &str) -> Result<Vec<Raw>> {
        self.raw_line("DONE")?;
        let mut out = Vec::new();
        loop {
            let reply = self.read_response()?;
            if let Some((status, text)) = tagged(&reply.text, tag) {
                self.absorb_capabilities(&reply.text);
                return match status {
                    Status::Ok => Ok(out),
                    Status::No | Status::Bad => Err(Error::Refused {
                        command: "IDLE".to_owned(),
                        reason: text,
                    }),
                };
            }
            if let Some(other) = other_tag(&reply.text, tag) {
                return Err(Error::Malformed {
                    reason: format!("réponse étiquetée {other} alors qu'on attend {tag}"),
                });
            }
            self.absorb_capabilities(&reply.text);
            out.push(reply);
        }
    }

    /// Envoie une commande étiquetée et rend les réponses non étiquetées.
    ///
    /// # Errors
    ///
    /// [`Error::Refused`] sur un `NO` ou un `BAD`, [`Error::Network`] sur le socket,
    /// [`Error::Malformed`] si la réponse étiquetée ne suit pas la RFC.
    pub fn command(&mut self, command: &str) -> Result<Vec<Raw>> {
        self.command_bytes(command.as_bytes())
    }

    /// La même, sur des octets, pour les commandes qui portent un nom de boîte brut.
    ///
    /// # Errors
    ///
    /// Voir [`Client::command`].
    pub fn command_bytes(&mut self, command: &[u8]) -> Result<Vec<Raw>> {
        let mut out = Vec::new();
        self.command_each(command, &mut |reply| {
            out.push(reply);
            Ok(())
        })?;
        Ok(out)
    }

    /// Envoie une commande et passe chaque réponse non étiquetée à `on`, sans les accumuler.
    ///
    /// C'est la forme de base ; [`Client::command_bytes`] est celle qui collecte. La
    /// distinction existe pour la même raison que [`Client::uid_fetch_each`] : une moisson de
    /// corps ne doit pas tenir en mémoire.
    ///
    /// # Errors
    ///
    /// Voir [`Client::command`]. Une erreur rendue par `on` remonte telle quelle, et la
    /// commande est abandonnée — la connexion n'est plus synchronisée avec le dialogue, donc
    /// l'appelant ne doit plus la réutiliser.
    pub fn command_each(
        &mut self,
        command: &[u8],
        on: &mut dyn FnMut(Raw) -> Result<()>,
    ) -> Result<()> {
        self.tag += 1;
        let tag = format!("m{}", self.tag);

        let mut line = tag.clone().into_bytes();
        line.push(b' ');
        line.extend_from_slice(command);
        line.extend_from_slice(b"\r\n");
        self.io.get_mut().write_all(&line)?;
        self.io.get_mut().flush()?;

        loop {
            let reply = self.read_response()?;

            // Une demande de continuation n'a pas lieu d'arriver : aucune commande de cette
            // moisson n'envoie de littéral. En recevoir une veut dire que le serveur attend
            // des octets qu'on n'enverra jamais — donc un blocage, et il vaut mieux le dire.
            if reply.text.starts_with('+') {
                return Err(Error::Malformed {
                    reason: "demande de continuation inattendue".to_owned(),
                });
            }

            if let Some((status, text)) = tagged(&reply.text, &tag) {
                // **Un `OK [CAPABILITY …]` étiqueté compte autant qu'un `* CAPABILITY`.** La
                // RFC 3501 §6.2.3 recommande au serveur d'annoncer ainsi ses capacités
                // d'après authentification, exactement pour épargner un aller-retour — et
                // Dovecot le fait. Les jeter, c'était rester sur la liste d'avant le `LOGIN` :
                // mesuré le 2026-09-08 sur `mail.perso.invalid`, qui annonce `CONDSTORE`,
                // `QRESYNC` et `LIST-STATUS` une fois connecté et rien de tout ça avant. Le
                // compte s'est synchronisé des mois sans aucune extension, en croyant que le
                // serveur n'en avait pas.
                self.absorb_capabilities(&reply.text);
                return match status {
                    Status::Ok => Ok(()),
                    Status::No | Status::Bad => Err(Error::Refused {
                        command: String::from_utf8_lossy(command)
                            .split_whitespace()
                            .next()
                            .unwrap_or("?")
                            .to_owned(),
                        reason: text,
                    }),
                };
            }

            // Une réponse étiquetée d'une **autre** étiquette veut dire qu'on a perdu le fil
            // du dialogue. Continuer lirait la réponse d'une commande précédente comme celle
            // de la commande en cours.
            if let Some(other) = other_tag(&reply.text, &tag) {
                return Err(Error::Malformed {
                    reason: format!("réponse étiquetée {other} alors qu'on attend {tag}"),
                });
            }

            self.absorb_capabilities(&reply.text);
            on(reply)?;
        }
    }

    /// Lit une réponse complète, littéraux compris.
    fn read_response(&mut self) -> Result<Raw> {
        let mut text = String::new();
        let mut literals = Vec::new();

        loop {
            let line = self.read_line()?;
            match literal_length(&line) {
                Some(announced) => {
                    // **Le plafond est vérifié avant l'allocation.** C'est tout l'intérêt :
                    // `{4294967295}` ne doit pas faire réserver quatre gigaoctets.
                    let length = usize::try_from(announced)
                        .ok()
                        .filter(|it| *it <= self.limit)
                        .ok_or(Error::LiteralTooLarge {
                            announced,
                            limit: self.limit,
                        })?;

                    text.push_str(strip_literal(&line));
                    let mut bytes = vec![0_u8; length];
                    // `read_exact` **compte**. Un littéral tronqué échoue ici, ce qui est le
                    // comportement voulu : la panne devient une erreur, pas un blocage.
                    self.io.read_exact(&mut bytes).map_err(|source| {
                        if source.kind() == std::io::ErrorKind::UnexpectedEof {
                            Error::Malformed {
                                reason: format!(
                                    "littéral de {length} octets annoncé, la connexion s'est \
                                     fermée avant"
                                ),
                            }
                        } else {
                            Error::Network(source)
                        }
                    })?;
                    literals.push(bytes);
                    // La réponse continue **après** le littéral, sur la même réponse
                    // logique : on relit une ligne.
                }
                None => {
                    text.push_str(&line);
                    return Ok(Raw { text, literals });
                }
            }
        }
    }

    /// Lit une ligne, sans le `CRLF`, en refusant les lignes sans fin.
    fn read_line(&mut self) -> Result<String> {
        self.read_line_or_quiet()?.ok_or_else(|| Error::Malformed {
            reason: "délai de lecture dépassé alors qu'une réponse était attendue".to_owned(),
        })
    }

    /// La même, mais où **une attente qui expire sans un octet est un silence, pas une panne**.
    ///
    /// ## La distinction qui rend `IDLE` sûr
    ///
    /// Pendant un `IDLE`, le silence est le cas normal : il ne doit pas casser la connexion.
    /// Mais une expiration qui tombe **après** quelques octets d'une ligne est autre chose —
    /// ces octets sont consommés du socket et perdus, donc le dialogue n'est plus synchronisé,
    /// et continuer lirait la fin d'une ligne comme le début d'une autre.
    ///
    /// Les deux cas se distinguent par un seul fait : le tampon est-il vide ? `read_until` ne
    /// dit pas combien il a lu quand il échoue, mais il dit ce qu'il a écrit. Vide veut dire
    /// que rien n'a été pris au socket, donc que reprendre est sûr. Non vide est une erreur
    /// franche, et l'appelant doit jeter la connexion.
    fn read_line_or_quiet(&mut self) -> Result<Option<String>> {
        let mut raw = Vec::new();
        // `take` borne la lecture : un serveur qui n'envoie jamais de `\n` remplirait sinon
        // la mémoire disponible.
        let read = {
            let mut limited = (&mut self.io).take(LINE_LIMIT as u64 + 1);
            limited.read_until(b'\n', &mut raw)
        };
        let read = match read {
            Ok(read) => read,
            Err(source) if is_timeout(&source) && raw.is_empty() => return Ok(None),
            Err(source) if is_timeout(&source) => {
                return Err(Error::Malformed {
                    reason: format!(
                        "délai de lecture dépassé au milieu d'une ligne, après {} octets : la \
                         connexion n'est plus synchronisée",
                        raw.len()
                    ),
                });
            }
            Err(source) => return Err(Error::Network(source)),
        };
        if read == 0 {
            return Err(Error::Malformed {
                reason: "le serveur a fermé la connexion".to_owned(),
            });
        }
        if read > LINE_LIMIT {
            return Err(Error::Malformed {
                reason: format!("ligne de plus de {LINE_LIMIT} octets sans fin de ligne"),
            });
        }
        while raw.last().is_some_and(|it| *it == b'\n' || *it == b'\r') {
            raw.pop();
        }
        // Les octets non UTF-8 deviennent des caractères de remplacement. Les seuls endroits
        // où un octet arbitraire est légitime sont les littéraux — lus à part — et les noms
        // de boîte, extraits en octets par `parse_list` avant toute conversion.
        Ok(Some(String::from_utf8_lossy(&raw).into_owned()))
    }

    /// Ajoute les capacités trouvées dans une réponse.
    fn absorb_capabilities(&mut self, text: &str) {
        let upper = text.to_ascii_uppercase();
        let list = if let Some(at) = upper.find("[CAPABILITY ") {
            let rest = &upper[at + "[CAPABILITY ".len()..];
            rest.split(']').next().unwrap_or_default()
        } else if let Some(rest) = upper.strip_prefix("* CAPABILITY ") {
            rest
        } else {
            return;
        };
        self.announced = true;
        for word in list.split_whitespace() {
            let word = word.to_owned();
            if !self.capabilities.contains(&word) {
                self.capabilities.push(word);
            }
        }
    }
}

/// Cite une chaîne pour le protocole.
fn quote(value: &str) -> String {
    String::from_utf8_lossy(&quote_bytes(value.as_bytes())).into_owned()
}

/// Cite des octets pour le protocole, en déguisant ce qui doit l'être.
fn quote_bytes(value: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(value.len() + 2);
    out.push(b'"');
    for byte in value {
        if *byte == b'"' || *byte == b'\\' {
            out.push(b'\\');
        }
        out.push(*byte);
    }
    out.push(b'"');
    out
}

/// La longueur annoncée par un `{n}` terminant une ligne.
///
/// Rend un `u64` et non un `usize` : la valeur vient du réseau, et il faut pouvoir la comparer
/// au plafond **avant** de la convertir. Convertir d'abord perdrait l'information sur une
/// plateforme 32 bits, où `{5000000000}` deviendrait un petit nombre plausible.
fn literal_length(line: &str) -> Option<u64> {
    let inner = line.rsplit_once('{')?.1;
    // `{123+}` est la forme non synchronisante de LITERAL+. Acceptée à la lecture : un
    // serveur peut l'utiliser, et la refuser ferait échouer une réponse par ailleurs valide.
    let inner = inner.strip_suffix('}')?;
    let inner = inner.strip_suffix('+').unwrap_or(inner);
    inner.parse().ok()
}

/// La ligne sans son `{n}` final.
fn strip_literal(line: &str) -> &str {
    line.rsplit_once('{').map_or(line, |(before, _)| before)
}

/// L'état et le texte d'une réponse étiquetée, si l'étiquette est la bonne.
fn tagged(text: &str, tag: &str) -> Option<(Status, String)> {
    let rest = text.strip_prefix(tag)?.strip_prefix(' ')?;
    let (word, tail) = rest.split_once(' ').unwrap_or((rest, ""));
    let status = match word.to_ascii_uppercase().as_str() {
        "OK" => Status::Ok,
        "NO" => Status::No,
        "BAD" => Status::Bad,
        _ => return None,
    };
    Some((status, tail.to_owned()))
}

/// L'étiquette d'une réponse étiquetée qui n'est pas celle attendue.
fn other_tag(text: &str, expected: &str) -> Option<String> {
    if text.starts_with("* ") || text.starts_with('+') {
        return None;
    }
    let (tag, rest) = text.split_once(' ')?;
    if tag == expected {
        return None;
    }
    let word = rest.split_whitespace().next()?.to_ascii_uppercase();
    if matches!(word.as_str(), "OK" | "NO" | "BAD") {
        return Some(tag.to_owned());
    }
    None
}

/// Le nombre d'une réponse `* n MOT`.
fn untagged_number(text: &str, word: &str) -> Option<u32> {
    let rest = text.strip_prefix("* ")?;
    let (number, tail) = rest.split_once(' ')?;
    if !tail.to_ascii_uppercase().starts_with(word) {
        return None;
    }
    number.parse().ok()
}

/// La valeur d'un code entre crochets, `[UIDNEXT 42]`.
fn bracketed<'a>(text: &'a str, code: &str) -> Option<&'a str> {
    let upper = text.to_ascii_uppercase();
    let needle = format!("[{code} ");
    let at = upper.find(&needle)?;
    let rest = &text[at + needle.len()..];
    rest.split(']').next()
}

/// Vrai si la réponse est un `* n FETCH (…)`.
fn is_fetch(text: &str) -> bool {
    let Some(rest) = text.strip_prefix("* ") else {
        return false;
    };
    let Some((_, tail)) = rest.split_once(' ') else {
        return false;
    };
    tail.to_ascii_uppercase().starts_with("FETCH ")
}

/// Vrai si une réponse `* ENABLED …` nomme cette extension.
///
/// ## Pourquoi ce n'est pas une recherche de sous-chaîne
///
/// La première version cherchait `"ENABLED QRESYNC"` tel quel. Un serveur qui active les deux
/// extensions d'un coup répond `* ENABLED CONDSTORE QRESYNC` — la RFC 7162 §3.2.3 l'y encourage,
/// puisque `QRESYNC` implique `CONDSTORE` — et la sous-chaîne ne s'y trouve pas. Le client
/// concluait « refusé » et repartait sur le balayage, sans que rien ne le signale.
///
/// Attrapé par `mailfake` le 2026-09-08, parce qu'il répond comme la RFC le recommande plutôt
/// que comme le client l'attendait.
fn was_enabled(replies: &[Raw], extension: &str) -> bool {
    replies.iter().any(|reply| {
        let upper = reply.text.to_ascii_uppercase();
        let Some(rest) = upper.strip_prefix("* ENABLED") else {
            return false;
        };
        rest.split_whitespace().any(|token| token == extension)
    })
}

/// L'ensemble d'UID d'une réponse `* VANISHED [(EARLIER)] <ensemble>`.
///
/// ## `(EARLIER)` est accepté, et son absence aussi
///
/// Avec `(EARLIER)`, ce sont les purges d'avant notre reprise — la réponse au `QRESYNC`. Sans,
/// c'est une purge qui vient d'avoir lieu pendant la session. Les deux disent la même chose au
/// store : ces UID ne sont plus là. Les distinguer n'aurait d'intérêt que pour un affichage en
/// direct, que la phase 2 n'a pas.
fn vanished_set(text: &str) -> Option<&str> {
    let rest = text.strip_prefix("* ")?;
    let upper = rest.to_ascii_uppercase();
    let tail = upper.strip_prefix("VANISHED ")?;
    let offset = rest.len() - tail.len();
    let rest = rest[offset..].trim_start();
    Some(
        rest.strip_prefix("(EARLIER)")
            .or_else(|| rest.strip_prefix("(earlier)"))
            .map_or(rest, str::trim_start),
    )
}

/// Les UID d'un ensemble RFC 3501 : `41,43:116,118`.
///
/// ## Ce qui est refusé plutôt qu'interprété
///
/// **`*` n'est pas accepté.** Dans un `VANISHED`, il n'aurait aucun sens — on ne peut pas
/// purger « jusqu'au plus grand », le plus grand ayant justement disparu — et le traduire par
/// une borne inventée effacerait des messages que le serveur a toujours. Un élément
/// incompréhensible est **sauté**, pas deviné : perdre une purge fait garder un message de
/// trop, tandis qu'en inventer une le fait disparaître de la boîte de l'utilisateur.
///
/// Une plage à l'envers — `116:43` — est lue dans les deux sens : la RFC 3501 §9 dit
/// explicitement qu'un ensemble n'est pas ordonné, et c'est exactement le piège qui avait déjà
/// coûté un retéléchargement par dossier avec `n:*`.
///
/// Le nombre d'UID rendus est borné : une plage `1:4294967295` d'un serveur hostile ou cassé
/// remplirait la mémoire. Au-delà, la plage est ignorée et signalée.
fn parse_uid_set(set: &str) -> Vec<u32> {
    /// Au-delà, la plage vient d'un serveur qui ne dit pas la vérité : le plus gros dossier du
    /// corpus réel fait 19 176 messages.
    const RANGE_LIMIT: u32 = 1_000_000;

    let mut out = Vec::new();
    for part in set.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        match part.split_once(':') {
            None => {
                if let Ok(uid) = part.parse::<u32>() {
                    out.push(uid);
                }
            }
            Some((low, high)) => {
                let (Ok(low), Ok(high)) = (low.trim().parse::<u32>(), high.trim().parse::<u32>())
                else {
                    continue;
                };
                let (low, high) = if low <= high {
                    (low, high)
                } else {
                    (high, low)
                };
                if high - low >= RANGE_LIMIT {
                    tracing::warn!(low, high, "plage VANISHED démesurée, ignorée");
                    continue;
                }
                out.extend(low..=high);
            }
        }
    }
    out
}

/// Le nom de boîte d'une réponse `* LIST (…) "/" "nom"`.
///
/// Rendu en octets : c'est la suite qu'il faudra renvoyer au serveur. La ligne a déjà été
/// convertie en `String` avec remplacement, donc un nom non UTF-8 y a perdu ses octets — le
/// cas est traité dans [`crate::sync`], qui compare les noms tels que le serveur les donne.
/// ## Analysé vers l'avant, et pas depuis la fin
///
/// Chercher le dernier guillemet et remonter jusqu'au précédent **casse sur un nom déguisé** :
/// dans `"un\"nom"`, le guillemet trouvé en remontant est celui du déguisement, et le nom
/// ressort tronqué à `nom`. C'est le deuxième bug que la première version de ce module portait.
///
/// La forme est fixe — `* LIST (attributs) séparateur nom` — donc on la suit : lire les
/// attributs entre parenthèses, lire le séparateur, et ce qui reste est le nom.
fn parse_list(text: &str) -> Option<Listed> {
    let rest = text.strip_prefix("* ")?;
    let (word, tail) = rest.split_once(' ')?;
    if !matches!(word.to_ascii_uppercase().as_str(), "LIST" | "LSUB") {
        return None;
    }

    let tail = tail.trim_start();
    // Les attributs. Absents chez certains serveurs, d'où le cas `None`.
    let (attributes, after_flags) = match tail.strip_prefix('(') {
        Some(inner) => {
            let close = inner.find(')')?;
            (
                inner[..close]
                    .split_whitespace()
                    .map(str::to_owned)
                    .collect(),
                &inner[close + 1..],
            )
        }
        None => (Vec::new(), tail),
    };

    // Le séparateur : `"/"`, ou `NIL` pour une hiérarchie plate.
    let (delimiter, after_delimiter) = {
        let it = after_flags.trim_start();
        match it.strip_prefix('"') {
            Some(inner) => {
                let (value, rest) = take_quoted(inner)?;
                // Un séparateur est un caractère unique. Plus d'un octet veut dire qu'on n'a
                // pas compris la réponse, et deviner un séparateur découperait la hiérarchie
                // au mauvais endroit.
                let byte = match value.as_bytes() {
                    [single] => Some(*single),
                    _ => None,
                };
                (byte, rest)
            }
            None => (None, it.split_once(' ').map_or("", |(_, rest)| rest)),
        }
    };

    let name = after_delimiter.trim();
    let name = match name.strip_prefix('"') {
        Some(inner) => take_quoted(inner).map(|(value, _)| value.into_bytes())?,
        // Un atome est légal : tous les serveurs ne citent pas.
        None if !name.is_empty() => name.as_bytes().to_vec(),
        None => return None,
    };

    Some(Listed {
        name,
        delimiter,
        attributes,
    })
}

/// Lit une chaîne citée dont le guillemet ouvrant est déjà consommé.
///
/// Rend la valeur déguisement défait, et ce qui suit le guillemet fermant. `None` si la
/// chaîne n'est pas fermée — ce qui est une réponse malformée, pas une valeur vide.
fn take_quoted(inner: &str) -> Option<(String, &str)> {
    let mut out = String::with_capacity(inner.len());
    let mut escaped = false;
    for (at, character) in inner.char_indices() {
        match character {
            '\\' if !escaped => escaped = true,
            '"' if !escaped => return Some((out, &inner[at + 1..])),
            _ => {
                out.push(character);
                escaped = false;
            }
        }
    }
    None
}

/// Extrait l'état d'une boîte d'une réponse `* STATUS`.
///
/// Rend `None` pour tout ce qui n'est pas un `STATUS` exploitable : c'est une réponse parmi
/// d'autres dans le flot d'un `LIST-STATUS`, et les `* LIST` y sont majoritaires.
fn parse_status(text: &str) -> Option<MailboxStatus> {
    let rest = text.strip_prefix("* ")?;
    let (word, tail) = rest.split_once(' ')?;
    if !word.eq_ignore_ascii_case("STATUS") {
        return None;
    }

    let tail = tail.trim_start();
    let (name, after_name) = match tail.strip_prefix('"') {
        Some(inner) => {
            let (value, rest) = take_quoted(inner)?;
            (value.into_bytes(), rest)
        }
        // Un atome est légal : tous les serveurs ne citent pas.
        None => {
            let (value, rest) = tail.split_once(' ')?;
            if value.is_empty() {
                return None;
            }
            (value.as_bytes().to_vec(), rest)
        }
    };

    // Les paires `MOT valeur` entre parenthèses. Un serveur peut en rendre d'autres que celles
    // demandées, et dans n'importe quel ordre : on lit celles qu'on reconnaît.
    let inner = after_name.trim_start().strip_prefix('(')?;
    let inner = &inner[..inner.find(')')?];
    let mut out = MailboxStatus {
        name,
        ..MailboxStatus::default()
    };
    let mut words = inner.split_whitespace();
    while let (Some(key), Some(value)) = (words.next(), words.next()) {
        match key.to_ascii_uppercase().as_str() {
            "MESSAGES" => out.messages = value.parse().ok(),
            "UIDNEXT" => out.uidnext = value.parse().ok(),
            "UIDVALIDITY" => out.uidvalidity = value.parse().ok(),
            "HIGHESTMODSEQ" => out.highest_modseq = value.parse().ok(),
            _ => {}
        }
    }
    Some(out)
}

/// Extrait un message d'une réponse `FETCH`.
fn parse_fetch(reply: &Raw) -> Result<Fetched> {
    let text = &reply.text;
    let uid = fetch_number(text, "UID")
        .and_then(|it| u32::try_from(it).ok())
        .filter(|it| *it != 0)
        .ok_or_else(|| Error::Malformed {
            reason: format!("réponse FETCH sans UID exploitable : {}", head(text)),
        })?;

    let flags = fetch_flags(text);
    let modseq = fetch_parenthesised(text, "MODSEQ");

    // Le corps est le premier littéral. Dans un `UID FETCH … (BODY[])` il n'y en a qu'un ;
    // s'il en arrive plusieurs, prendre le premier est faux, donc on refuse.
    let body = match reply.literals.len() {
        0 => None,
        1 => Some(reply.literals[0].clone()),
        many => {
            return Err(Error::Malformed {
                reason: format!("{many} littéraux dans une réponse FETCH"),
            });
        }
    };

    Ok(Fetched {
        uid,
        flags,
        modseq,
        body,
        size: fetch_number(text, "RFC822.SIZE"),
    })
}

/// L'indice juste après un mot d'élément de `FETCH`.
///
/// ## Pourquoi ce n'est pas un `find(" MOT ")`
///
/// Le premier élément d'une réponse `FETCH` est collé à la parenthèse ouvrante :
/// `* 1 FETCH (UID 7 …)`. Chercher `" UID "` ne le trouve donc **jamais** — c'est le bug que
/// la première version de ce module portait, et il aurait cassé toutes les réponses réelles,
/// pas un cas limite.
///
/// Le mot doit être précédé d'une espace ou d'une parenthèse ouvrante, et suivi d'une espace.
/// Sans cette condition, chercher `UID` attraperait le `UID` de `X-GM-MSGID`… ou celui d'un
/// sujet, si un sujet pouvait arriver là.
fn item_at(text: &str, word: &str) -> Option<usize> {
    let upper = text.to_ascii_uppercase();
    let bytes = upper.as_bytes();
    let mut from = 0;
    while let Some(offset) = upper[from..].find(word) {
        let at = from + offset;
        let before_ok = at == 0 || matches!(bytes.get(at - 1), Some(b' ' | b'('));
        let after = at + word.len();
        let after_ok = matches!(bytes.get(after), Some(b' '));
        if before_ok && after_ok {
            return Some(after + 1);
        }
        from = at + word.len();
    }
    None
}

/// La valeur d'un `MOT n` dans une réponse `FETCH`.
fn fetch_number(text: &str, word: &str) -> Option<u64> {
    let at = item_at(text, word)?;
    let digits: String = text[at..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    digits.parse().ok()
}

/// La valeur d'un `MOT (n)` dans une réponse `FETCH`.
fn fetch_parenthesised(text: &str, word: &str) -> Option<u64> {
    let at = item_at(text, word)?;
    let inner = text[at..].strip_prefix('(')?;
    inner.split(')').next()?.trim().parse().ok()
}

/// Les drapeaux d'une réponse `FETCH`.
fn fetch_flags(text: &str) -> Vec<String> {
    let Some(at) = item_at(text, "FLAGS") else {
        return Vec::new();
    };
    let Some(inner) = text[at..].strip_prefix('(') else {
        return Vec::new();
    };
    let Some(list) = inner.split(')').next() else {
        return Vec::new();
    };
    list.split_whitespace().map(str::to_owned).collect()
}

/// Les premiers caractères d'un texte, pour un message d'erreur.
///
/// Borné, et pour une raison de vie privée autant que de lisibilité : une réponse du serveur
/// peut contenir un sujet de message, et `docs/PRIVACY.md` §8 interdit qu'un contenu de
/// message finisse dans un journal.
fn head(text: &str) -> String {
    let cut = text.char_indices().nth(60).map_or(text.len(), |(at, _)| at);
    text[..cut].to_owned()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn a_literal_length_is_read_as_u64_before_any_conversion() {
        // Sur une plateforme 32 bits, convertir d'abord ferait de `{5000000000}` un petit
        // nombre plausible, et le plafond ne verrait rien passer.
        assert_eq!(literal_length("* 1 FETCH (BODY[] {123}"), Some(123));
        assert_eq!(
            literal_length("* 1 FETCH (BODY[] {5000000000}"),
            Some(5_000_000_000)
        );
    }

    #[test]
    fn the_non_synchronising_literal_form_is_accepted() {
        // `{123+}` est du LITERAL+. Le refuser ferait échouer une réponse valide.
        assert_eq!(literal_length("* 1 FETCH (BODY[] {123+}"), Some(123));
    }

    #[test]
    fn a_line_without_a_literal_has_no_length() {
        assert_eq!(literal_length("m1 OK terminé"), None);
        assert_eq!(literal_length("* 1 FETCH (UID 2)"), None);
        // Une accolade sans nombre n'est pas un littéral.
        assert_eq!(literal_length("* OK {pas un nombre}"), None);
    }

    #[test]
    fn a_tagged_reply_is_recognised_by_its_status() {
        assert_eq!(
            tagged("m1 OK terminé", "m1"),
            Some((Status::Ok, "terminé".to_owned()))
        );
        assert_eq!(
            tagged("m1 NO refusé", "m1"),
            Some((Status::No, "refusé".to_owned()))
        );
        assert_eq!(tagged("m1 ok minuscule", "m1").unwrap().0, Status::Ok);
        assert!(tagged("m2 OK terminé", "m1").is_none());
        assert!(tagged("* OK quelque chose", "m1").is_none());
    }

    #[test]
    fn a_tagged_reply_with_no_text_is_still_a_reply() {
        // Un serveur peut répondre `m1 OK` tout court. Exiger un texte le refuserait.
        assert_eq!(tagged("m1 OK", "m1"), Some((Status::Ok, String::new())));
    }

    #[test]
    fn a_reply_from_another_tag_is_detected() {
        // Sinon on lirait la réponse d'une commande précédente comme celle de la commande
        // en cours, et tout ce qui suit serait décalé.
        assert_eq!(other_tag("m7 OK terminé", "m9"), Some("m7".to_owned()));
        assert!(other_tag("m9 OK terminé", "m9").is_none());
        assert!(other_tag("* 3 EXISTS", "m9").is_none());
        assert!(other_tag("+ continuez", "m9").is_none());
    }

    #[test]
    fn bracketed_codes_are_read_case_insensitively() {
        assert_eq!(
            bracketed("* OK [UIDVALIDITY 1000] valide", "UIDVALIDITY"),
            Some("1000")
        );
        assert_eq!(
            bracketed("* ok [uidnext 42] suivant", "UIDNEXT"),
            Some("42")
        );
        assert_eq!(bracketed("* OK rien", "UIDNEXT"), None);
    }

    #[test]
    fn exists_is_read_from_an_untagged_number() {
        assert_eq!(untagged_number("* 3 EXISTS", "EXISTS"), Some(3));
        assert_eq!(untagged_number("* 0 RECENT", "EXISTS"), None);
        assert_eq!(untagged_number("* OK autre", "EXISTS"), None);
    }

    #[test]
    fn a_fetch_response_is_recognised() {
        assert!(is_fetch("* 1 FETCH (UID 2)"));
        assert!(is_fetch("* 12 fetch (UID 2)"));
        assert!(!is_fetch("m1 OK UID FETCH terminé"));
        assert!(!is_fetch("* 3 EXISTS"));
    }

    #[test]
    fn a_fetch_without_a_uid_is_refused() {
        // **Sans UID, il n'y a aucun endroit sûr où ranger le message.** Le numéro de
        // séquence ne suffit pas : il change dès qu'un message est purgé.
        let reply = Raw {
            text: "* 1 FETCH (FLAGS (\\Seen))".to_owned(),
            literals: Vec::new(),
        };
        assert!(matches!(parse_fetch(&reply), Err(Error::Malformed { .. })));
    }

    #[test]
    fn a_uid_of_zero_is_refused() {
        // Zéro n'est pas un UID valide : la RFC 3501 les fait commencer à 1. L'accepter
        // écrirait une copie sous une clé que le serveur ne pourra jamais redemander.
        let reply = Raw {
            text: "* 1 FETCH (UID 0)".to_owned(),
            literals: Vec::new(),
        };
        assert!(matches!(parse_fetch(&reply), Err(Error::Malformed { .. })));
    }

    #[test]
    fn a_fetch_is_parsed_whatever_the_order_of_its_items() {
        // Les serveurs ne s'accordent pas sur l'ordre, et la RFC ne l'impose pas.
        let first = Raw {
            text: "* 1 FETCH (UID 7 FLAGS (\\Seen \\Flagged) MODSEQ (42) BODY[] )".to_owned(),
            literals: vec![b"corps".to_vec()],
        };
        let second = Raw {
            text: "* 1 FETCH (MODSEQ (42) BODY[] FLAGS (\\Seen \\Flagged) UID 7 )".to_owned(),
            literals: vec![b"corps".to_vec()],
        };
        let a = parse_fetch(&first).unwrap();
        let b = parse_fetch(&second).unwrap();
        assert_eq!(a, b);
        assert_eq!(a.uid, 7);
        assert_eq!(a.flags, vec!["\\Seen", "\\Flagged"]);
        assert_eq!(a.modseq, Some(42));
        assert_eq!(a.body.as_deref(), Some(b"corps".as_slice()));
    }

    #[test]
    fn the_first_fetch_item_is_glued_to_the_opening_parenthesis() {
        // **Le bug de la première version de ce module.** `find(" UID ")` ne trouve jamais
        // `(UID 7`, donc aucune réponse réelle n'était analysée — pas un cas limite, toutes.
        assert_eq!(fetch_number("* 1 FETCH (UID 7 FLAGS ())", "UID"), Some(7));
    }

    #[test]
    fn an_item_word_must_be_whole() {
        // `X-UID` n'est pas `UID`. Sans frontière de mot, un élément propre au serveur
        // fournirait une valeur à la place de la bonne.
        assert_eq!(
            fetch_number("* 1 FETCH (X-UID 9 UID 7 FLAGS ())", "UID"),
            Some(7)
        );
        assert_eq!(fetch_number("* 1 FETCH (UIDPLUS 9)", "UID"), None);
    }

    #[test]
    fn a_modseq_is_read_from_its_parentheses() {
        assert_eq!(
            fetch_parenthesised("* 1 FETCH (UID 7 MODSEQ (98765))", "MODSEQ"),
            Some(98_765)
        );
        // `MODSEQ 42` sans parenthèses n'est pas la forme de la RFC 7162 : la lire quand même
        // accepterait une réponse qu'on ne comprend pas.
        assert_eq!(fetch_parenthesised("* 1 FETCH (MODSEQ 42)", "MODSEQ"), None);
    }

    #[test]
    fn a_list_reply_with_a_nil_delimiter_still_yields_its_name() {
        // Une hiérarchie plate. Le séparateur est `NIL`, pas une chaîne citée.
        assert_eq!(
            parse_list("* LIST (\\HasNoChildren) NIL \"INBOX\"").map(|it| it.name),
            Some(b"INBOX".to_vec())
        );
    }

    #[test]
    fn a_list_reply_without_attributes_still_yields_its_name() {
        assert_eq!(
            parse_list("* LIST \"/\" \"INBOX\"").map(|it| it.name),
            Some(b"INBOX".to_vec())
        );
    }

    #[test]
    fn a_mailbox_name_containing_a_delimiter_like_string_is_not_confused() {
        // Un nom qui ressemble à un séparateur : l'analyse vers l'avant sait où elle en est,
        // l'analyse depuis la fin ne le sait pas.
        assert_eq!(
            parse_list("* LIST (\\HasChildren) \"/\" \"a/b/c\"").map(|it| it.name),
            Some(b"a/b/c".to_vec())
        );
    }

    #[test]
    fn an_unterminated_quoted_string_is_refused() {
        // Une réponse malformée, pas une valeur vide.
        assert!(parse_list("* LIST () \"/\" \"jamais fermé").is_none());
    }

    #[test]
    fn a_fetch_without_a_body_is_valid() {
        // Le cas d'une moisson de drapeaux : on ne retélécharge pas les corps.
        let reply = Raw {
            text: "* 1 FETCH (UID 7 FLAGS (\\Seen))".to_owned(),
            literals: Vec::new(),
        };
        let fetched = parse_fetch(&reply).unwrap();
        assert!(fetched.body.is_none());
        assert_eq!(fetched.flags, vec!["\\Seen"]);
    }

    #[test]
    fn several_literals_in_one_fetch_are_refused() {
        // Prendre le premier serait un choix arbitraire sur une réponse qu'on ne comprend
        // pas. Mieux vaut le dire.
        let reply = Raw {
            text: "* 1 FETCH (UID 7 BODY[HEADER] BODY[TEXT] )".to_owned(),
            literals: vec![b"a".to_vec(), b"b".to_vec()],
        };
        assert!(matches!(parse_fetch(&reply), Err(Error::Malformed { .. })));
    }

    #[test]
    fn flags_may_be_empty() {
        let reply = Raw {
            text: "* 1 FETCH (UID 7 FLAGS ())".to_owned(),
            literals: Vec::new(),
        };
        assert!(parse_fetch(&reply).unwrap().flags.is_empty());
    }

    #[test]
    fn a_quoted_mailbox_name_is_extracted_from_a_list_reply() {
        assert_eq!(
            parse_list("* LIST (\\HasNoChildren) \"/\" \"INBOX\"").map(|it| it.name),
            Some(b"INBOX".to_vec())
        );
        assert_eq!(
            parse_list("* LIST (\\HasChildren) \"/\" \"[Gmail]/Tous les messages\"")
                .map(|it| it.name),
            Some(b"[Gmail]/Tous les messages".to_vec())
        );
    }

    #[test]
    fn an_unquoted_mailbox_name_is_extracted_too() {
        // Un atome est légal : tous les serveurs ne citent pas.
        assert_eq!(
            parse_list("* LIST (\\HasNoChildren) \"/\" INBOX").map(|it| it.name),
            Some(b"INBOX".to_vec())
        );
    }

    #[test]
    fn an_escaped_quote_in_a_mailbox_name_survives() {
        assert_eq!(
            parse_list("* LIST () \"/\" \"un\\\"nom\"").map(|it| it.name),
            Some(b"un\"nom".to_vec())
        );
    }

    #[test]
    fn a_line_that_is_not_a_list_reply_yields_nothing() {
        assert!(parse_list("* 3 EXISTS").is_none());
        assert!(parse_list("m1 OK LIST terminé").is_none());
        assert!(parse_list("* LIST").is_none());
    }

    #[test]
    fn a_password_with_a_space_is_quoted() {
        // Les mots de passe applicatifs de Google en contiennent quatre. Sans guillemets, le
        // serveur lirait cinq arguments et refuserait.
        assert_eq!(quote("abcd efgh ijkl mnop"), "\"abcd efgh ijkl mnop\"");
    }

    #[test]
    fn a_password_cannot_break_out_of_its_quotes() {
        // Le même trou qu'une injection, sur un protocole plus vieux : sans déguisement, un
        // mot de passe contenant un guillemet coupe la commande en deux.
        assert_eq!(quote("a\"b"), "\"a\\\"b\"");
        assert_eq!(quote("a\\b"), "\"a\\\\b\"");
        assert_eq!(
            quote("x\" LOGOUT\""),
            "\"x\\\" LOGOUT\\\"\"",
            "une commande a pu être injectée"
        );
    }

    #[test]
    fn an_error_message_does_not_carry_a_whole_response() {
        // `docs/PRIVACY.md` §8 : un contenu de message ne finit pas dans un journal. Une
        // réponse de serveur peut porter un sujet.
        let long = "x".repeat(500);
        assert!(head(&long).len() <= 60);
    }

    #[test]
    fn cutting_an_error_message_does_not_split_a_character() {
        // Trancher à l'octet couperait un caractère multi-octets en deux et paniquerait.
        let accented = "é".repeat(200);
        let cut = head(&accented);
        assert!(cut.chars().count() <= 60);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod qresync_tests {
    use super::*;

    // ------------------------------------------------------------------
    // La ligne VANISHED. Elle vient du réseau, donc de l'entrée hostile.
    // ------------------------------------------------------------------

    #[test]
    fn a_vanished_line_yields_its_set_with_or_without_earlier() {
        assert_eq!(
            vanished_set("* VANISHED (EARLIER) 41,43:45"),
            Some("41,43:45")
        );
        assert_eq!(vanished_set("* VANISHED 41"), Some("41"));
        // La casse d'un mot-clé IMAP n'est pas garantie par la RFC.
        assert_eq!(vanished_set("* vanished (earlier) 7"), Some("7"));
    }

    #[test]
    fn what_is_not_a_vanished_line_is_not_read_as_one() {
        assert!(vanished_set("* 12 EXISTS").is_none());
        assert!(vanished_set("* OK [UIDNEXT 5]").is_none());
        // Le mot dans un sujet ne doit pas déclencher : le préfixe est ancré.
        assert!(vanished_set("* 1 FETCH (BODY[] {4}) VANISHED 9").is_none());
        assert!(vanished_set("VANISHED 9").is_none(), "sans le `* `");
        assert!(vanished_set("").is_none());
    }

    // ------------------------------------------------------------------
    // L'ensemble d'UID.
    // ------------------------------------------------------------------

    #[test]
    fn a_single_uid_and_a_range_both_parse() {
        assert_eq!(parse_uid_set("41"), vec![41]);
        assert_eq!(parse_uid_set("43:46"), vec![43, 44, 45, 46]);
        assert_eq!(parse_uid_set("1,3:5,9"), vec![1, 3, 4, 5, 9]);
    }

    #[test]
    fn a_backwards_range_is_read_in_both_directions() {
        // RFC 3501 §9 : un ensemble n'est pas ordonné. Le même piège que `n:*` avait déjà
        // coûté un retéléchargement par dossier le 2026-09-03.
        assert_eq!(parse_uid_set("46:43"), vec![43, 44, 45, 46]);
    }

    #[test]
    fn a_star_is_skipped_and_not_guessed() {
        // **Le point qui compte.** Inventer une borne ferait disparaître de la boîte de
        // l'utilisateur des messages que le serveur a toujours. Perdre une purge est le sens
        // sûr de l'erreur : on garde un message de trop.
        assert!(parse_uid_set("*").is_empty());
        assert!(parse_uid_set("5:*").is_empty());
        assert_eq!(parse_uid_set("3,5:*,7"), vec![3, 7]);
    }

    #[test]
    fn a_preposterous_range_is_refused_rather_than_allocated() {
        // Un serveur cassé ou hostile qui annonce `1:4294967295` remplirait la mémoire.
        assert!(parse_uid_set("1:4294967295").is_empty());
        // Et il ne fait pas perdre le reste de la ligne.
        assert_eq!(parse_uid_set("2,1:4294967295,4"), vec![2, 4]);
    }

    #[test]
    fn garbage_never_panics_and_never_invents_a_uid() {
        for candidate in [
            "", ",,,", ":", "a:b", "-1", "1:", ":9", "9,,8", "  ", "1:2:3", "٣",
        ] {
            let parsed = parse_uid_set(candidate);
            assert!(
                parsed.iter().all(|&uid| uid > 0),
                "{candidate} a produit un UID nul : {parsed:?}"
            );
        }
    }

    #[test]
    fn the_bound_is_a_span_not_a_value() {
        // Une plage étroite très haut dans les UID est parfaitement légitime : un dossier
        // ancien a des UID élevés. C'est l'étendue qui est bornée, pas la valeur.
        assert_eq!(
            parse_uid_set("4294967290:4294967295"),
            vec![
                4_294_967_290,
                4_294_967_291,
                4_294_967_292,
                4_294_967_293,
                4_294_967_294,
                4_294_967_295
            ]
        );
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod enabled_tests {
    use super::*;

    fn raw(text: &str) -> Raw {
        Raw {
            text: text.to_owned(),
            literals: Vec::new(),
        }
    }

    #[test]
    fn an_extension_is_found_whether_it_is_alone_or_with_others() {
        // **Le cas qui a cassé le 2026-09-08.** La RFC 7162 encourage le serveur à annoncer les
        // deux d'un coup, et la recherche de sous-chaîne `"ENABLED QRESYNC"` échouait dessus.
        assert!(was_enabled(
            &[raw("* ENABLED CONDSTORE QRESYNC")],
            "QRESYNC"
        ));
        assert!(was_enabled(
            &[raw("* ENABLED CONDSTORE QRESYNC")],
            "CONDSTORE"
        ));
        assert!(was_enabled(&[raw("* ENABLED QRESYNC")], "QRESYNC"));
        assert!(was_enabled(&[raw("* enabled qresync")], "QRESYNC"));
    }

    #[test]
    fn an_empty_or_absent_enabled_list_means_refused() {
        // RFC 5161 : une extension inconnue rend `OK` avec une liste vide. C'est un refus, pas
        // une erreur — et surtout pas un succès.
        assert!(!was_enabled(&[raw("* ENABLED")], "QRESYNC"));
        assert!(!was_enabled(&[], "QRESYNC"));
        assert!(!was_enabled(&[raw("* ENABLED CONDSTORE")], "QRESYNC"));
    }

    #[test]
    fn a_prefix_is_not_a_match() {
        // Sans découpage en jetons, `QRESYNC` serait trouvé dans `QRESYNCFOO`, et `CONDSTORE`
        // dans une hypothétique `CONDSTORE2`.
        assert!(!was_enabled(&[raw("* ENABLED QRESYNCFOO")], "QRESYNC"));
        assert!(!was_enabled(&[raw("* ENABLED XQRESYNC")], "QRESYNC"));
    }

    #[test]
    fn the_word_elsewhere_in_a_response_does_not_count() {
        // Le préfixe est ancré : un sujet de message qui contiendrait le mot ne doit pas faire
        // croire à une extension activée.
        assert!(!was_enabled(
            &[raw("* 1 FETCH (BODY[] {9}) ENABLED QRESYNC")],
            "QRESYNC"
        ));
        assert!(!was_enabled(&[raw("ENABLED QRESYNC")], "QRESYNC"));
    }
}

/// L'analyse d'un `* STATUS`, y compris quand il est de travers.
///
/// La règle du `CLAUDE.md` sur le parsing d'entrée hostile s'applique ici comme au MIME : ces
/// lignes viennent du réseau, et une réponse tronquée au milieu d'une parenthèse est une
/// panne qu'on a déjà vue chez un vrai serveur.
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod status_tests {
    use super::parse_status;

    #[test]
    fn a_quoted_name_and_its_three_numbers_are_read() {
        let found =
            parse_status("* STATUS \"INBOX\" (MESSAGES 231 UIDNEXT 44292 UIDVALIDITY 1)").unwrap();
        assert_eq!(found.name, b"INBOX");
        assert_eq!(found.messages, Some(231));
        assert_eq!(found.uidnext, Some(44292));
        assert_eq!(found.uidvalidity, Some(1));
        assert_eq!(found.highest_modseq, None);
    }

    #[test]
    fn an_unquoted_name_is_read_too() {
        // Tous les serveurs ne citent pas les noms d'atome.
        let found = parse_status("* STATUS INBOX (MESSAGES 3)").unwrap();
        assert_eq!(found.name, b"INBOX");
        assert_eq!(found.messages, Some(3));
    }

    #[test]
    fn a_name_with_a_space_survives_its_quotes() {
        let found = parse_status("* STATUS \"[Gmail]/Tous les messages\" (MESSAGES 9)").unwrap();
        assert_eq!(found.name, "[Gmail]/Tous les messages".as_bytes());
    }

    #[test]
    fn the_items_are_read_in_any_order_and_case() {
        let found = parse_status("* status \"A\" (uidvalidity 7 highestmodseq 42 messages 1)")
            .expect("le mot-clé et les éléments sont insensibles à la casse");
        assert_eq!(found.uidvalidity, Some(7));
        assert_eq!(found.highest_modseq, Some(42));
        assert_eq!(found.messages, Some(1));
        assert_eq!(found.uidnext, None);
    }

    #[test]
    fn an_unknown_item_is_ignored_and_does_not_shift_the_others() {
        // Un serveur a le droit de rendre plus que ce qu'on demande. Lire par paires plutôt
        // que par position est ce qui empêche `SIZE` de se faire lire comme un `UIDNEXT`.
        let found = parse_status("* STATUS \"A\" (SIZE 4096 MESSAGES 2 RECENT 0)").unwrap();
        assert_eq!(found.messages, Some(2));
        assert_eq!(found.uidnext, None);
    }

    #[test]
    fn an_item_without_its_value_ends_the_reading_rather_than_inventing_one() {
        let found = parse_status("* STATUS \"A\" (MESSAGES 2 UIDNEXT)").unwrap();
        assert_eq!(found.messages, Some(2));
        assert_eq!(found.uidnext, None, "une valeur a été inventée");
    }

    #[test]
    fn a_value_that_is_not_a_number_is_absent_not_zero() {
        // Zéro veut dire quelque chose — « la boîte est vide » — et une valeur illisible ne
        // doit pas se déguiser en boîte vide.
        let found = parse_status("* STATUS \"A\" (MESSAGES beaucoup)").unwrap();
        assert_eq!(found.messages, None);
    }

    #[test]
    fn a_message_count_beyond_a_u32_is_absent_rather_than_wrapped() {
        let found = parse_status("* STATUS \"A\" (MESSAGES 99999999999999)").unwrap();
        assert_eq!(found.messages, None);
    }

    #[test]
    fn what_is_not_a_status_line_is_not_read_as_one() {
        assert!(parse_status("* LIST (\\HasNoChildren) \"/\" \"INBOX\"").is_none());
        assert!(parse_status("* 3 EXISTS").is_none());
        assert!(parse_status("a1 OK STATUS terminé").is_none());
        assert!(parse_status("* STATUSES \"A\" (MESSAGES 1)").is_none());
    }

    #[test]
    fn a_truncated_line_yields_nothing_and_never_panics() {
        for line in [
            "* STATUS",
            "* STATUS ",
            "* STATUS \"A\"",
            "* STATUS \"A\" ",
            "* STATUS \"A\" (",
            "* STATUS \"A\" (MESSAGES 1",
            "* STATUS \"sans fin de guillemet (MESSAGES 1)",
            "* STATUS () ()",
        ] {
            let _ = parse_status(line);
        }
    }

    #[test]
    fn a_status_without_a_message_count_cannot_become_a_selected() {
        // `exists` vaudrait zéro par défaut, et zéro dit « tout a disparu ». Un champ absent
        // ne doit pas se déguiser en boîte vide : c'est la seule façon dont le raccourci du
        // critère 3 pourrait effacer des copies pour de bon.
        let found = parse_status("* STATUS \"A\" (UIDNEXT 5 UIDVALIDITY 1)").unwrap();
        assert!(found.as_selected().is_none());

        let complete = parse_status("* STATUS \"A\" (MESSAGES 0 UIDNEXT 5 UIDVALIDITY 1)").unwrap();
        let selected = complete.as_selected().expect("le compte est là");
        assert_eq!(selected.exists, 0);
        assert_eq!(selected.uidnext, Some(5));
    }
}

/// Vrai si cette réponse non étiquetée annonce un **changement de la boîte**.
///
/// ## Une liste blanche, et c'est un choix
///
/// Ce qui n'est pas reconnu est traité comme « rien de neuf ». À première vue c'est le mauvais
/// sens — un changement qu'on ne reconnaîtrait pas serait manqué — et c'est pourtant le bon,
/// pour une raison précise : **l'échéance du veilleur est le filet**. Une synchronisation part
/// de toute façon au bout de vingt-quatre minutes, donc une ligne mal classée coûte au pire un
/// retard borné.
///
/// La liste noire aurait le défaut inverse, et il est réel : `* OK Still here` de Dovecot,
/// arrivant toutes les deux minutes, a fait resynchroniser un compte toutes les deux minutes
/// pendant que ce code n'en tenait pas compte. Un faux positif se paie **en continu**, un faux
/// négatif se paie une fois et se rattrape tout seul.
///
/// ## Ce qui est reconnu
///
/// - `* n EXISTS` — le nombre de messages a changé ;
/// - `* n RECENT` — des messages sont arrivés depuis la dernière session ;
/// - `* n EXPUNGE` — un message a été retiré ;
/// - `* n FETCH (…)` — des drapeaux ont changé ;
/// - `* VANISHED …` — des messages ont disparu (RFC 7162).
///
/// Tout le reste — `* OK`, `* NO`, `* BAD`, `* CAPABILITY`, `* FLAGS` — est informatif.
fn announces_change(text: &str) -> bool {
    let Some(rest) = text.strip_prefix("* ") else {
        return false;
    };
    let rest = rest.trim_start();
    // `VANISHED` n'est pas numéroté : il vient juste après l'astérisque. Le **mot entier**, pas
    // son préfixe — sans quoi un `VANISHEDX` qu'une extension future introduirait se ferait
    // lire comme celui-ci. C'est la même exigence que `an_item_word_must_be_whole` plus haut.
    let first = rest.split(' ').next().unwrap_or_default();
    if first.eq_ignore_ascii_case("VANISHED") {
        return true;
    }
    // Les autres sont numérotés : `* 4 EXISTS`. Le numéro est un numéro de séquence, dont on ne
    // fait rien — c'est le mot qui suit qui porte l'information.
    let Some((number, word)) = rest.split_once(' ') else {
        return false;
    };
    if number.parse::<u64>().is_err() {
        return false;
    }
    let word = word.split(['(', ' ']).next().unwrap_or_default();
    matches!(
        word.to_ascii_uppercase().as_str(),
        "EXISTS" | "RECENT" | "EXPUNGE" | "FETCH"
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod change_tests {
    use super::announces_change;

    #[test]
    fn the_five_announcements_of_a_change_are_recognised() {
        assert!(announces_change("* 4 EXISTS"));
        assert!(announces_change("* 1 RECENT"));
        assert!(announces_change("* 2 EXPUNGE"));
        assert!(announces_change("* 3 FETCH (FLAGS (\\Seen))"));
        assert!(announces_change("* VANISHED (EARLIER) 41,42"));
    }

    #[test]
    fn the_word_is_read_whatever_its_case() {
        assert!(announces_change("* 4 exists"));
        assert!(announces_change("* vanished 7"));
    }

    #[test]
    fn a_courtesy_line_is_not_a_change() {
        // **Le cas qui a coûté une resynchronisation toutes les deux minutes.** Dovecot envoie
        // celle-là pendant un `IDLE` pour dire qu'il est toujours là.
        assert!(!announces_change("* OK Still here"));
    }

    #[test]
    fn what_is_informative_is_not_a_change() {
        for line in [
            "* OK [UIDVALIDITY 1] valide",
            "* NO quelque chose",
            "* BAD ligne illisible",
            "* CAPABILITY IMAP4rev1 IDLE",
            "* FLAGS (\\Seen \\Answered)",
            "* BYE au revoir",
            "+ idling",
            "a1 OK IDLE terminé",
        ] {
            assert!(!announces_change(line), "pour {line:?}");
        }
    }

    #[test]
    fn a_truncated_line_is_not_a_change_and_never_panics() {
        for line in [
            "",
            "*",
            "* ",
            "* 4",
            "* EXISTS",
            "* x EXISTS",
            "*  4 EXISTS",
        ] {
            let _ = announces_change(line);
        }
        assert!(
            !announces_change("* 4"),
            "sans le mot, il n'y a rien à lire"
        );
        assert!(
            !announces_change("* EXISTS"),
            "sans numéro, ce n'est pas la forme de la RFC"
        );
    }

    #[test]
    fn a_word_that_merely_starts_like_one_of_them_is_not_a_change() {
        // `EXISTSNT` n'existe pas, mais comparer par préfixe le laisserait passer — et le jour
        // où une extension ajoute un mot qui commence pareil, on le prendrait pour l'autre.
        assert!(!announces_change("* 4 EXISTSNT"));
        assert!(!announces_change("* VANISHEDX 7"));
    }
}
