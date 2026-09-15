//! Le dialogue SMTP, du salut au point final.
//!
//! ## L'ordre des étapes n'est pas une convention, c'est le protocole
//!
//! `EHLO` avant tout, `STARTTLS` avant l'authentification, l'authentification avant
//! l'enveloppe, l'enveloppe avant les données. Chaque étape a son nom dans [`Stage`], et ce nom
//! **survit à l'erreur** : c'est la seule information qui permette à la file d'envoi de décider
//! si un message a pu partir.
//!
//! ## Un deuxième `EHLO` après `STARTTLS`, et ce n'est pas une politesse
//!
//! Les capacités d'avant le chiffrement ont pu être modifiées en vol par quelqu'un qui voulait
//! faire disparaître `STARTTLS` de la liste, ou y ajouter un mécanisme d'authentification
//! faible. La RFC 3207 §4.2 exige donc de tout réapprendre après la poignée de main : c'est
//! [`crate::tls::connect`] qui envoie le second `EHLO`, et [`Client::over`] qui repart d'une
//! liste de capacités vide.
//!
//! C'est la même leçon que la phase 2 a payée sur l'IMAP : les capacités demandées au mauvais
//! moment valent une réponse fausse.

use std::io::{BufRead, BufReader, Read, Write};

use crate::error::{Error, Result};
use crate::reply::{self, Reply};

/// Où en est le dialogue.
///
/// ## Pourquoi une étape et pas un booléen « envoyé »
///
/// Parce qu'entre « rien n'est parti » et « c'est parti », il y a un troisième état :
/// [`Stage::Committing`], après le point final du `DATA`. Le serveur a le message, et s'il se
/// tait à cet instant, **rien dans le protocole ne dit s'il l'a gardé**.
///
/// La file d'envoi tranche. Pour trancher, elle a besoin de savoir où ça s'est arrêté, et c'est
/// tout ce que le protocole permet de lui dire honnêtement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// Le salut du serveur, avant toute commande.
    Greeting,
    /// `EHLO` — la découverte des capacités.
    Ehlo,
    /// `STARTTLS` et la poignée de main.
    StartTls,
    /// `AUTH` — l'authentification.
    Auth,
    /// `MAIL FROM` — l'expéditeur de l'enveloppe.
    Sender,
    /// `RCPT TO` — un destinataire de l'enveloppe.
    Recipient,
    /// `DATA`, puis les octets du message.
    Data,
    /// **Après le point final.** Le serveur a tout reçu et n'a pas encore répondu.
    ///
    /// C'est la seule étape où un échec laisse un doute sur ce que le destinataire recevra.
    Committing,
}

impl std::fmt::Display for Stage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            Self::Greeting => "salut",
            Self::Ehlo => "EHLO",
            Self::StartTls => "STARTTLS",
            Self::Auth => "AUTH",
            Self::Sender => "MAIL FROM",
            Self::Recipient => "RCPT TO",
            Self::Data => "DATA",
            Self::Committing => "remise",
        };
        f.write_str(name)
    }
}

/// De quoi s'authentifier, sans dire d'où ça vient.
///
/// La même forme que `mailsync::Credential`, et pour la même raison : les deux secrets ne se
/// placent pas au même endroit dans le protocole. Un jeton d'accès envoyé dans un `AUTH PLAIN`
/// part dans le champ mot de passe, et un serveur journalise les échecs d'authentification avec
/// l'identifiant.
#[derive(Clone, Copy)]
pub enum Credential<'a> {
    /// Un mot de passe, ou un mot de passe applicatif : `AUTH PLAIN`.
    Password(&'a str),
    /// Un jeton d'accès OAuth2 : `AUTH XOAUTH2`.
    Bearer(&'a str),
}

/// Le `Debug` est **écrit à la main**, et il ne montre que le mécanisme.
///
/// ## Ce n'était pas le cas au premier jet
///
/// La dérivation affichait `Password("mot-de-passe")`, donc un `tracing::debug!(?credential)`
/// ailleurs dans le programme aurait mis le secret dans un journal — `docs/PRIVACY.md` §8.
/// Trouvé par `submit::tests::the_debug_output_never_carries_the_secret`, qui est écrit pour ça
/// et qui a échoué du premier coup.
///
/// La dérivation est le défaut dangereux ici : elle est correcte pour tout le reste du projet,
/// et fausse pour exactement les deux types qui portent un secret.
impl<'a> Credential<'a> {
    /// La pièce qui correspond au mécanisme déclaré par le compte.
    ///
    /// C'est **le seul** endroit du crate où un secret est aiguillé vers une variante. Le même
    /// aiguillage existe dans `mailsync::Credential::for_auth`, et pour la même raison : une
    /// deuxième copie chez chaque appelant finirait par envoyer un jeton dans le champ mot de
    /// passe — où un serveur le journalise avec l'identifiant quand l'authentification échoue.
    #[must_use]
    pub const fn for_auth(auth: mailcore::AuthKind, secret: &'a str) -> Self {
        match auth {
            mailcore::AuthKind::Password => Self::Password(secret),
            mailcore::AuthKind::OAuth2 => Self::Bearer(secret),
        }
    }
}

impl std::fmt::Debug for Credential<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mechanism = match self {
            Self::Password(_) => "Password",
            Self::Bearer(_) => "Bearer",
        };
        write!(f, "Credential::{mechanism}(<masqué>)")
    }
}

/// Une connexion SMTP, à un stade quelconque du dialogue.
#[derive(Debug)]
pub struct Client<S> {
    io: BufReader<S>,
    /// Ce que le dernier `EHLO` a annoncé. Vide avant lui, et **jeté** par `STARTTLS`.
    capabilities: Vec<String>,
}

impl<S: Read + Write> Client<S> {
    /// Prend un flux déjà établi et lit le salut.
    ///
    /// # Errors
    ///
    /// [`Error::Rejected`] si le serveur ouvre par un `554` — il refuse la connexion, et le
    /// dire tout de suite vaut mieux que d'envoyer un `EHLO` dans le vide.
    pub fn greet(stream: S) -> Result<Self> {
        let mut client = Self {
            io: BufReader::new(stream),
            capabilities: Vec::new(),
        };
        let reply = client.read_reply(Stage::Greeting)?;
        if !reply.accepted() {
            return Err(Error::from_reply(Stage::Greeting, &reply));
        }
        Ok(client)
    }

    /// Prend un flux **sans attendre de salut**.
    ///
    /// Le seul cas où c'est correct : après une poignée de main `STARTTLS`, où le serveur ne
    /// renvoie pas de salut — la RFC 3207 §4.2 dit de renvoyer un `EHLO`, pas d'attendre.
    #[must_use]
    pub fn over(stream: S) -> Self {
        Self {
            io: BufReader::new(stream),
            capabilities: Vec::new(),
        }
    }

    /// Les capacités du dernier `EHLO`.
    #[must_use]
    pub fn capabilities(&self) -> &[String] {
        &self.capabilities
    }

    /// Vrai si le serveur a annoncé cette extension.
    #[must_use]
    pub fn has(&self, keyword: &str) -> bool {
        self.capabilities
            .iter()
            .any(|line| line.eq_ignore_ascii_case(keyword))
    }

    /// La taille maximale annoncée par `SIZE`, si le serveur en annonce une.
    ///
    /// `SIZE 0` veut dire « pas de limite annoncée » (RFC 1870 §6) et non « rien n'est
    /// accepté » : le rendre tel quel ferait refuser tous les messages.
    #[must_use]
    pub fn size_limit(&self) -> Option<u64> {
        self.capabilities
            .iter()
            .find_map(|line| line.strip_prefix("SIZE "))
            .and_then(|it| it.trim().parse::<u64>().ok())
            .filter(|it| *it > 0)
    }

    /// `EHLO`, et retient ce que le serveur annonce.
    ///
    /// Le nom annoncé est celui que l'appelant donne. **Pas le nom d'hôte de la machine** : il
    /// part sur le fil, il finit dans les en-têtes `Received` du destinataire, et le nom d'un
    /// poste de travail n'a rien à y faire. `docs/PRIVACY.md` : rien ne s'ajoute au message qui
    /// ne soit visible.
    ///
    /// # Errors
    ///
    /// [`Error::Rejected`] ou [`Error::Transient`] si le serveur refuse, [`Error::Network`] sur
    /// le socket.
    pub fn ehlo(&mut self, client_name: &str) -> Result<&[String]> {
        let reply = self.command(Stage::Ehlo, &format!("EHLO {client_name}"))?;
        // La première ligne d'une réponse à `EHLO` est le nom du serveur, pas une extension.
        self.capabilities = reply
            .lines
            .iter()
            .skip(1)
            .map(|it| it.trim().to_owned())
            .collect();
        tracing::debug!(
            extensions = self.capabilities.len(),
            "capacités SMTP annoncées"
        );
        Ok(&self.capabilities)
    }

    /// `AUTH`, avec le mécanisme qui correspond à la pièce fournie.
    ///
    /// ## Le mécanisme n'est pas négociable à la baisse
    ///
    /// Un compte déclaré `oauth2` s'authentifie en `XOAUTH2` ou pas du tout. Retomber sur
    /// `PLAIN` enverrait le **jeton d'accès dans le champ mot de passe**, où un serveur le
    /// journalise avec l'identifiant en cas d'échec — le jeton finirait écrit sur le disque de
    /// quelqu'un d'autre. C'est la même raison qui a fait deux variantes dans `mailsync`.
    ///
    /// ## La commande n'est pas journalisée
    ///
    /// Sa ligne porte le secret en base64. `docs/PRIVACY.md` §8 interdit qu'un secret finisse
    /// dans un journal, même trivialement réversible.
    ///
    /// # Errors
    ///
    /// [`Error::MissingCapability`] si le serveur n'annonce pas le mécanisme,
    /// [`Error::AuthRefused`] s'il refuse, [`Error::Network`] sur le socket.
    pub fn auth(&mut self, username: &str, credential: Credential<'_>) -> Result<()> {
        let mechanisms = self
            .capabilities
            .iter()
            .find_map(|line| line.strip_prefix("AUTH "))
            .unwrap_or_default()
            .to_ascii_uppercase();

        let (wanted, response) = match credential {
            Credential::Password(secret) => ("PLAIN", plain(username, secret)),
            Credential::Bearer(token) => ("XOAUTH2", xoauth2(username, token)),
        };
        if !mechanisms.split_whitespace().any(|it| it == wanted) {
            return Err(Error::MissingCapability {
                capability: format!("AUTH {wanted}"),
            });
        }

        // La réponse initiale part dans la commande — RFC 4954 §4 l'autorise, et tous les
        // serveurs du corpus l'acceptent. Un aller-retour de continuation en moins.
        self.line(Stage::Auth, &format!("AUTH {wanted} {response}"))?;
        let reply = self.read_reply(Stage::Auth)?;

        if reply.accepted() {
            return Ok(());
        }
        // Un `334` ici veut dire que le serveur veut *dialoguer* : sur `XOAUTH2`, c'est le
        // message d'erreur du fournisseur. Il attend une ligne vide pour conclure, et sans elle
        // le dialogue reste en l'air.
        if reply.wants_more() {
            self.line(Stage::Auth, "")?;
            let closing = self.read_reply(Stage::Auth)?;
            return Err(Error::AuthRefused {
                reason: closing.text(),
            });
        }
        Err(Error::AuthRefused {
            reason: reply.text(),
        })
    }

    /// `MAIL FROM`, avec la taille annoncée si le serveur sait la lire.
    ///
    /// ## Annoncer la taille est une politesse qui rend service
    ///
    /// RFC 1870 : un serveur qui connaît la taille d'avance refuse **avant** le transfert. Sans
    /// elle, un message de 30 Mo se téléverse en entier pour se faire refuser à la fin — et sur
    /// une liaison montante ordinaire, c'est plusieurs minutes perdues.
    ///
    /// # Errors
    ///
    /// [`Error::TooLarge`] si le serveur annonce une limite que le message dépasse — vérifié
    /// **avant** d'ouvrir l'enveloppe. Sinon, ce que le serveur refuse.
    pub fn mail_from(&mut self, sender: &str, size: Option<u64>) -> Result<()> {
        if let (Some(size), Some(limit)) = (size, self.size_limit())
            && size > limit
        {
            return Err(Error::TooLarge { size, limit });
        }
        let announce = match size.filter(|_| self.has("SIZE") || self.size_limit().is_some()) {
            Some(size) => format!(" SIZE={size}"),
            None => String::new(),
        };
        self.command(Stage::Sender, &format!("MAIL FROM:<{sender}>{announce}"))?;
        Ok(())
    }

    /// `RCPT TO`, un destinataire à la fois.
    ///
    /// Un par commande, parce que le protocole est ainsi : c'est ce qui permet à un serveur
    /// d'accepter trois destinataires et d'en refuser un quatrième, et à l'appelant de savoir
    /// **lequel**.
    ///
    /// # Errors
    ///
    /// Ce que le serveur refuse, avec l'étape [`Stage::Recipient`].
    pub fn rcpt_to(&mut self, recipient: &str) -> Result<()> {
        self.command(Stage::Recipient, &format!("RCPT TO:<{recipient}>"))?;
        Ok(())
    }

    /// `DATA`, puis les octets du message, puis le point final.
    ///
    /// ## Le point en début de ligne est doublé, et c'est le piège classique
    ///
    /// RFC 5321 §4.5.2 : la fin des données est une ligne contenant un seul point. Un message
    /// dont une ligne **commence** par un point terminerait donc le transfert au milieu — le
    /// destinataire reçoit un message tronqué, et le reste est interprété comme des commandes
    /// SMTP.
    ///
    /// Un point en début de ligne est donc doublé à l'écriture, et le serveur en retire un. Ce
    /// n'est pas une précaution : c'est le protocole, et l'oublier corrompt silencieusement tout
    /// message contenant une ligne commençant par un point — ce qui arrive dans du texte cité et
    /// dans du code.
    ///
    /// ## L'étape change après le point final
    ///
    /// Avant, un échec veut dire « rien n'est parti ». Après, il veut dire « peut-être ». C'est
    /// [`Stage::Committing`], et c'est toute la raison d'être de cette énumération.
    ///
    /// # Errors
    ///
    /// Ce que le serveur refuse. Une erreur portant [`Stage::Committing`] veut dire que le
    /// message a **peut-être** été accepté — voir [`Error::may_have_been_sent`].
    ///
    /// [`Error::may_have_been_sent`]: crate::Error::may_have_been_sent
    pub fn data(&mut self, message: &[u8]) -> Result<Reply> {
        self.open_data()?;
        self.finish_data(message)
    }

    /// La commande `DATA` seule : le serveur ouvre le transfert et ne prend encore rien.
    ///
    /// ## Pourquoi elle est séparée de [`Client::finish_data`]
    ///
    /// Entre les deux se trouve le seul instant du projet où une écriture locale doit
    /// **précéder** une action réseau : la file d'envoi y rend durable le fait qu'elle est sur
    /// le point de commettre. Sans cette couture, l'appelant ne peut pas s'insérer entre le
    /// `354` et le point final, et le critère 2 n'est pas tenable.
    ///
    /// Après cet appel, le serveur a une transaction **vide**. Un client qui disparaît la lui
    /// laisse ouverte, et il l'abandonne à son propre délai — rien n'est remis.
    ///
    /// # Errors
    ///
    /// [`Error::Malformed`] si le serveur ne rend pas un `354`, sinon ce qu'il refuse. **Aucune
    /// de ces erreurs ne porte [`Stage::Committing`]** : à ce point, rien n'est parti.
    pub fn open_data(&mut self) -> Result<()> {
        let opened = self.command(Stage::Data, "DATA")?;
        if !opened.wants_more() {
            return Err(Error::Malformed {
                stage: Some(Stage::Data),
                reason: format!("DATA attendait un 354, le serveur a rendu {}", opened.code),
            });
        }
        Ok(())
    }

    /// Le corps du message, puis le point final, puis la réponse.
    ///
    /// **Tout échec à partir d'ici porte [`Stage::Committing`]**, donc laisse un doute. Appeler
    /// cette fonction est le point de non-retour de l'envoi.
    ///
    /// # Errors
    ///
    /// Ce que le serveur refuse, ou son silence. Voir [`Error::may_have_been_sent`].
    ///
    /// [`Error::may_have_been_sent`]: crate::Error::may_have_been_sent
    pub fn finish_data(&mut self, message: &[u8]) -> Result<Reply> {
        self.finish_data_from(&mut std::io::Cursor::new(message))
    }

    /// Le corps **lu d'un flux**, puis le point final, puis la réponse.
    ///
    /// ## C'est la forme qui tient le critère 3
    ///
    /// Un message de 25 Mo passe du disque au socket par un tampon de quelques kilooctets. Le
    /// doublage des points et la normalisation des fins de ligne se font **au passage** — voir
    /// [`Stuffing`] — donc rien de la taille du message n'existe en mémoire.
    ///
    /// [`Client::finish_data`] en dérive plutôt que l'inverse : le doublage des points corrompt
    /// silencieusement quand il est faux, et deux implémentations finiraient par ne plus
    /// corriger la même chose.
    ///
    /// ## Le terminateur dépend de ce qui a été écrit, pas de ce qu'on croit
    ///
    /// `Stuffing::finish` rend vrai si la sortie finit sur une fin de ligne. Un message qui n'y
    /// finit pas prend un `CRLF` avant le point : sans lui, le point se collerait à la dernière
    /// ligne et ne terminerait rien — le serveur attendrait indéfiniment.
    ///
    /// Le tester sur les octets d'entrée serait faux : un message qui finit par un `\n` nu
    /// devient un `CRLF` en sortie, et le test d'entrée aurait ajouté un `CRLF` de trop.
    ///
    /// # Errors
    ///
    /// Ce que le serveur refuse, ou son silence. Une erreur de lecture du flux porte
    /// [`Stage::Data`] et non `Committing` : rien n'a été commis. Voir
    /// [`Error::may_have_been_sent`].
    ///
    /// [`Error::may_have_been_sent`]: crate::Error::may_have_been_sent
    pub fn finish_data_from(&mut self, source: &mut dyn Read) -> Result<Reply> {
        let ended_on_newline = {
            let mut stuffing = Stuffing::new(self.io.get_mut());
            std::io::copy(source, &mut stuffing).map_err(|source| Error::Network {
                stage: Stage::Data,
                source,
            })?;
            stuffing.finish().map_err(|source| Error::Network {
                stage: Stage::Data,
                source,
            })?
        };

        let terminator: &[u8] = if ended_on_newline {
            b".\r\n"
        } else {
            b"\r\n.\r\n"
        };
        self.write_all(Stage::Data, terminator)?;

        // **À partir d'ici, le serveur a tout.** Son silence ne dit plus si le message est
        // parti.
        let reply = self.read_reply(Stage::Committing)?;
        if reply.accepted() {
            return Ok(reply);
        }
        Err(Error::from_reply(Stage::Committing, &reply))
    }

    /// Reprend le flux, pour passer en TLS.
    ///
    /// ## Un tampon non vide est un refus, pas une perte
    ///
    /// `BufReader::into_inner` jette ce qui est déjà lu. Ici, ce qui est déjà lu est ce que le
    /// serveur a envoyé **après** son `220 Ready` et **avant** la poignée de main : du texte en
    /// clair qu'un attaquant en position d'écrire sur le fil peut choisir. La RFC 3207 §6 demande
    /// d'oublier tout ce qui précède le chiffrement ; le jeter en silence suffirait pour la
    /// lettre de la RFC, mais laisserait passer sans bruit une tentative d'injection.
    ///
    /// Donc c'est une erreur. Un serveur correct n'a rien envoyé de plus, et le tampon est vide.
    ///
    /// # Errors
    ///
    /// [`Error::Tls`] si des octets non lus attendent dans le tampon.
    pub fn into_stream(self) -> Result<S> {
        if !self.io.buffer().is_empty() {
            return Err(Error::Tls {
                reason: format!(
                    "{} octets en clair envoyés après le 220 de STARTTLS",
                    self.io.buffer().len()
                ),
            });
        }
        Ok(self.io.into_inner())
    }

    /// `QUIT`, en ignorant ce que le serveur répond.
    ///
    /// Il n'y a rien à faire d'un `QUIT` refusé : le message est parti ou non, et cette réponse
    /// n'y change rien. Insister ferait échouer un envoi réussi sur une politesse.
    pub fn quit(&mut self) {
        let _ = self.line(Stage::Committing, "QUIT");
    }

    /// Envoie une commande et rend la réponse, quelle qu'elle soit.
    ///
    /// **Ne juge pas le code** : c'est l'appelant qui sait si un `250` était attendu ou un
    /// `354`. Les helpers au-dessus, eux, jugent.
    ///
    /// # Errors
    ///
    /// [`Error::Network`] sur le socket, [`Error::Malformed`] si la réponse ne suit pas la RFC.
    pub fn command(&mut self, stage: Stage, command: &str) -> Result<Reply> {
        self.line(stage, command)?;
        let reply = self.read_reply(stage)?;
        if reply.accepted() || reply.wants_more() {
            return Ok(reply);
        }
        Err(Error::from_reply(stage, &reply))
    }

    /// Écrit une ligne, terminée par `CRLF`.
    ///
    /// ## Le `CRLF` est ajouté ici, et la commande ne doit pas en contenir
    ///
    /// Une commande qui porterait un retour à la ligne injecterait une **deuxième** commande.
    /// Sur un `MAIL FROM` construit à partir d'une adresse venue de l'utilisateur, c'est une
    /// injection de commande SMTP — et c'est ce que [`crate::compose`] refuse en amont.
    fn line(&mut self, stage: Stage, command: &str) -> Result<()> {
        debug_assert!(
            !command.contains('\r') && !command.contains('\n'),
            "une commande SMTP ne peut pas porter de retour à la ligne"
        );
        let mut bytes = command.as_bytes().to_vec();
        bytes.extend_from_slice(b"\r\n");
        self.write_all(stage, &bytes)
    }

    /// Écrit des octets bruts, sans rien y ajouter.
    fn write_all(&mut self, stage: Stage, bytes: &[u8]) -> Result<()> {
        self.io
            .get_mut()
            .write_all(bytes)
            .and_then(|()| self.io.get_mut().flush())
            .map_err(|source| Error::Network { stage, source })
    }

    /// Lit une réponse complète.
    fn read_reply(&mut self, stage: Stage) -> Result<Reply> {
        // **L'étape est recollée sur l'erreur de l'analyseur.** `reply::read` ne la connaît pas —
        // il est pur — et sans elle un serveur qui ferme après le point final rendrait une
        // erreur sans étape, donc sans doute, donc un message renvoyé. Voir `Error::Malformed`.
        reply::read(|| self.next_line(stage)).map_err(|it| match it {
            Error::Malformed { reason, .. } => Error::Malformed {
                stage: Some(stage),
                reason,
            },
            other => other,
        })
    }

    /// Lit une ligne, sans son `CRLF`.
    fn next_line(&mut self, stage: Stage) -> Result<Option<String>> {
        let mut raw = Vec::new();
        // `take` borne la lecture : un serveur qui n'envoie jamais de `\n` remplirait sinon la
        // mémoire disponible. La borne est celle de `reply`, plus un octet pour la détecter.
        let read = {
            let mut limited = (&mut self.io).take(reply::LINE_LIMIT as u64 + 1);
            limited
                .read_until(b'\n', &mut raw)
                .map_err(|source| Error::Network { stage, source })?
        };
        if read == 0 {
            return Ok(None);
        }
        while raw.last().is_some_and(|it| *it == b'\n' || *it == b'\r') {
            raw.pop();
        }
        // Les octets non UTF-8 deviennent des caractères de remplacement : une réponse SMTP est
        // du texte, et un serveur qui envoie autre chose n'a rien à nous dire d'exploitable.
        Ok(Some(String::from_utf8_lossy(&raw).into_owned()))
    }
}

/// La réponse `AUTH PLAIN` — RFC 4616.
///
/// Trois champs séparés par un octet **nul** : identité d'autorisation vide, identifiant, mot de
/// passe. Le premier est vide parce qu'on ne s'autorise pas au nom de quelqu'un d'autre.
fn plain(username: &str, password: &str) -> String {
    use base64::Engine as _;
    let mut raw = Vec::with_capacity(username.len() + password.len() + 2);
    raw.push(0);
    raw.extend_from_slice(username.as_bytes());
    raw.push(0);
    raw.extend_from_slice(password.as_bytes());
    base64::engine::general_purpose::STANDARD.encode(&raw)
}

/// La réponse `AUTH XOAUTH2` de Google et Microsoft.
///
/// Ce n'est pas un standard IETF, c'est une convention que les deux fournisseurs partagent :
/// `user=<identifiant>^Aauth=Bearer <jeton>^A^A`, où `^A` est l'octet 1. Les deux `^A` finaux
/// sont **obligatoires** — un seul, et le serveur refuse sans dire pourquoi.
fn xoauth2(username: &str, token: &str) -> String {
    use base64::Engine as _;
    let raw = format!("user={username}\x01auth=Bearer {token}\x01\x01");
    base64::engine::general_purpose::STANDARD.encode(raw.as_bytes())
}

/// Un puits qui double les points en début de ligne et normalise les fins de ligne.
///
/// ## Pourquoi un puits et pas une fonction
///
/// Parce qu'un message de 25 Mo ne doit pas exister deux fois en mémoire. La version en tampon
/// allouait une copie complète de la sortie ; celle-ci transforme au passage, par blocs de
/// quelques kilooctets, et [`Client::finish_data_from`] la branche entre le fichier et le
/// socket.
///
/// **C'est la seule implémentation du doublage.** `stuffed` en dérive plutôt que de la recopier :
/// deux copies d'une transformation qui corrompt silencieusement quand elle est fausse finiraient
/// par ne plus corriger la même chose.
///
/// ## L'état porté entre deux écritures
///
/// Deux choses, et les deux sont faciles à oublier :
///
/// **`at_line_start`** — sans lui, un bloc qui commence par un point ne saurait pas s'il est en
/// début de ligne, et le point ne serait pas doublé. Le message arriverait tronqué chez le
/// destinataire, et la suite serait lue comme des commandes SMTP.
///
/// **`pending_cr`** — un bloc peut finir sur un `\r` dont le `\n` arrive dans le suivant.
/// Traiter le `\r` tout de suite émettrait un `CRLF`, puis le `\n` du bloc suivant en émettrait
/// un second : le message gagnerait une ligne vide toutes les 8 Kio, exactement aux frontières
/// de tampon. C'est le genre de défaut qui ne se voit que sur les gros messages.
#[derive(Debug)]
struct Stuffing<W: Write> {
    sink: W,
    at_line_start: bool,
    pending_cr: bool,
}

impl<W: Write> Stuffing<W> {
    const fn new(sink: W) -> Self {
        Self {
            sink,
            at_line_start: true,
            // Faux au départ : rien n'a été écrit, donc aucun retour chariot n'attend son
            // saut de ligne. À vrai, tout message gagnait un CRLF en tête.
            pending_cr: false,
        }
    }

    /// Vide un `\r` resté en attente, et dit si la sortie finit sur une fin de ligne.
    ///
    /// La réponse décide du terminateur : un message qui finit déjà par `CRLF` prend `.\r\n`,
    /// les autres `\r\n.\r\n`. Sans le `CRLF` intercalaire, le point se collerait à la dernière
    /// ligne et ne terminerait rien — le serveur attendrait indéfiniment.
    fn finish(mut self) -> std::io::Result<bool> {
        if self.pending_cr {
            self.sink.write_all(b"\r\n")?;
            self.at_line_start = true;
        }
        self.sink.flush()?;
        Ok(self.at_line_start)
    }
}

impl<W: Write> Write for Stuffing<W> {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        // Le tampon de sortie est dimensionné pour le pire cas d'un bloc : chaque octet peut
        // devenir deux — un message d'une ligne de points.
        let mut out = Vec::with_capacity(buffer.len() * 2 + 2);
        for byte in buffer {
            if self.pending_cr {
                // Le `\r` gardé du bloc précédent, ou du caractère précédent.
                out.extend_from_slice(b"\r\n");
                self.at_line_start = true;
                self.pending_cr = false;
                if *byte == b'\n' {
                    // Le `\n` appartenait au `CRLF` déjà émis.
                    continue;
                }
            }
            match *byte {
                b'\r' => self.pending_cr = true,
                // Un `LF` seul devient un `CRLF` : la RFC 5321 §2.3.8 l'exige, et un serveur
                // qui reçoit un `LF` nu le corrige, le refuse ou le transmet tel quel selon
                // l'humeur — auquel cas le message arrive abîmé.
                b'\n' => {
                    out.extend_from_slice(b"\r\n");
                    self.at_line_start = true;
                }
                b'.' if self.at_line_start => {
                    out.extend_from_slice(b"..");
                    self.at_line_start = false;
                }
                other => {
                    out.push(other);
                    self.at_line_start = false;
                }
            }
        }
        self.sink.write_all(&out)?;
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.sink.flush()
    }
}

/// Double les points en début de ligne, et normalise les fins de ligne en `CRLF`.
///
/// ## Les deux transformations vont ensemble
///
/// Le doublage du point ne se décide que sur un **début de ligne**, et un début de ligne ne se
/// reconnaît qu'après un `CRLF`. Un message dont les fins de ligne sont des `LF` nus — ce que
/// produit tout éditeur sur Unix — n'a donc pas de début de ligne au sens du protocole, et le
/// doublage y raterait exactement les lignes qu'il doit protéger.
///
/// La RFC 5321 §2.3.8 exige `CRLF` de toute façon : un `LF` nu dans les données est une
/// violation que certains serveurs corrigent, d'autres refusent, et d'autres transmettent telle
/// quelle — auquel cas le message arrive abîmé chez le destinataire.
#[cfg(test)]
fn stuffed(message: &[u8]) -> Vec<u8> {
    // **Une enveloppe sur [`Stuffing`], et pas une deuxième implémentation.** La transformation
    // corrompt silencieusement quand elle est fausse ; deux copies finiraient par ne plus
    // corriger la même chose, et c'est le chemin en tampon — celui des tests — qui divergerait
    // du chemin streamé, celui de la production.
    //
    // Une réserve d'un huitième : le pire cas réel est du texte cité, où une ligne sur dix
    // commence par un point. Prévoir le double serait du gâchis sur 25 Mo de pièce jointe.
    let mut out = Vec::with_capacity(message.len() + message.len() / 8 + 16);
    {
        let mut stuffing = Stuffing::new(&mut out);
        // Un `Vec` n'échoue pas à l'écriture, et un `Stuffing` n'ajoute pas de cause d'échec :
        // les deux `Result` sont donc structurellement `Ok`. Les ignorer plutôt que de faire
        // remonter une erreur impossible dans la signature.
        let _ = stuffing.write_all(message);
        let _ = stuffing.finish();
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::{Client, Stage};

    #[test]
    fn every_stage_has_a_readable_name() {
        // Ces noms partent dans un message d'erreur que l'utilisateur lit — critère 8.
        for stage in [
            Stage::Greeting,
            Stage::Ehlo,
            Stage::StartTls,
            Stage::Auth,
            Stage::Sender,
            Stage::Recipient,
            Stage::Data,
            Stage::Committing,
        ] {
            let name = stage.to_string();
            assert!(!name.is_empty());
            assert!(!name.contains("Stage"), "nom de variante rendu tel quel");
        }
    }

    /// Un flux qui ne rend rien et avale tout.
    ///
    /// Sert aux fonctions qui ne parlent pas — celles qui ne lisent que les capacités déjà
    /// apprises. Un `Cursor` ne suffit pas : [`Client`] demande `Read + Write`, et un
    /// `Cursor<&[u8]>` n'écrit pas.
    struct Silent;

    impl std::io::Read for Silent {
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            Ok(0)
        }
    }

    impl std::io::Write for Silent {
        fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
            Ok(buffer.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Un client monté sur ce flux muet.
    fn on() -> Client<Silent> {
        Client::over(Silent)
    }

    #[test]
    fn a_size_of_zero_means_no_announced_limit() {
        // RFC 1870 §6 : `SIZE 0` veut dire « pas de limite annoncée ». Le rendre tel quel
        // ferait refuser tous les messages.
        let mut client = on();
        client.capabilities = vec!["SIZE 0".to_owned()];
        assert_eq!(client.size_limit(), None);

        client.capabilities = vec!["SIZE 35882577".to_owned()];
        assert_eq!(client.size_limit(), Some(35_882_577));
    }

    #[test]
    fn an_unparsable_size_is_absent_rather_than_zero() {
        let mut client = on();
        client.capabilities = vec!["SIZE beaucoup".to_owned()];
        assert_eq!(client.size_limit(), None);
        client.capabilities = vec!["SIZE".to_owned()];
        assert_eq!(client.size_limit(), None);
    }

    #[test]
    fn a_capability_is_matched_whole_and_case_insensitively() {
        let mut client = on();
        client.capabilities = vec!["STARTTLS".to_owned(), "8BITMIME".to_owned()];
        assert!(client.has("starttls"));
        assert!(client.has("8BITMIME"));
        assert!(!client.has("START"), "un préfixe n'est pas une capacité");
        assert!(!client.has("TLS"));
    }

    /// Un flux dont les lectures viennent d'un tampon fixe et dont les écritures sont jetées.
    ///
    /// **Pourquoi pas le serveur scripté de `tests/dialogue.rs`.** Ce qu'il faut éprouver ici
    /// est l'état du tampon de lecture à un instant précis, et sur un socket cet instant
    /// dépend de l'ordonnanceur : les octets injectés arrivent peut-être après l'appel. Le
    /// premier jet du test était sur socket, et il passait en déclarant le tampon vide alors
    /// que le serveur avait bien envoyé les deux réponses. En mémoire, il n'y a pas de course.
    struct Given(std::io::Cursor<Vec<u8>>);

    impl Given {
        fn new(bytes: &[u8]) -> Self {
            Self(std::io::Cursor::new(bytes.to_vec()))
        }
    }

    impl std::io::Read for Given {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            self.0.read(buffer)
        }
    }

    impl std::io::Write for Given {
        fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
            Ok(buffer.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn an_injection_after_the_starttls_go_ahead_is_refused() {
        // **La fenêtre d'injection de `STARTTLS`.** Ce que le serveur envoie après son
        // `220 Ready` et avant la poignée de main est du texte en clair qu'un attaquant en
        // position d'écrire sur le fil peut choisir. Il attend dans le tampon de lecture, et il
        // serait lu **après** le passage en TLS comme s'il en venait — donc comme s'il était
        // authentifié par le certificat.
        //
        // La RFC 3207 §6 demande d'oublier ce qui précède le chiffrement. Le jeter en silence
        // suffirait pour la lettre de la RFC ; en faire une erreur est ce qui fait qu'une
        // tentative se voit au lieu de passer.
        let mut client = Client::greet(Given::new(b"220 pret\r\n")).unwrap();
        let ready = client.command(Stage::StartTls, "STARTTLS");
        // Rien de plus à lire : le salut a été consommé, la commande n'a pas de réponse.
        assert!(ready.is_err());

        let mut client =
            Client::greet(Given::new(b"220 pret\r\n220 Ready\r\n250 injecte\r\n")).unwrap();
        let ready = client.command(Stage::StartTls, "STARTTLS").unwrap();
        assert_eq!(ready.code, 220);
        match client.into_stream() {
            Err(crate::Error::Tls { reason }) => {
                assert!(reason.contains("en clair"), "{reason}");
                assert!(reason.contains("13"), "la taille doit être dite : {reason}");
            }
            Err(other) => panic!("attendu une erreur TLS, obtenu {other}"),
            Ok(_) => panic!("les octets injectés sont passés dans le tunnel"),
        }
    }

    #[test]
    fn a_stream_with_nothing_left_to_read_is_handed_over() {
        // Le contrôle négatif : une implémentation qui refuserait toujours passerait le test
        // ci-dessus et casserait tout `STARTTLS`. Celui-là le rattrape.
        let mut client = Client::greet(Given::new(b"220 pret\r\n220 Ready\r\n")).unwrap();
        client.command(Stage::StartTls, "STARTTLS").unwrap();
        assert!(client.into_stream().is_ok());
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod wire_tests {
    //! Le doublage du point et la normalisation des fins de ligne — les deux transformations
    //! qui corrompent un message quand elles manquent, **sans rien signaler**.

    use super::{plain, stuffed, xoauth2};

    fn text(bytes: &[u8]) -> String {
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    #[test]
    fn a_leading_dot_is_doubled() {
        // **Le piège.** Sans doublage, cette ligne termine le transfert : le destinataire reçoit
        // un message tronqué, et le reste part au serveur comme des commandes SMTP.
        assert_eq!(
            text(&stuffed(b"avant\r\n.\r\napres\r\n")),
            "avant\r\n..\r\napres\r\n"
        );
        assert_eq!(
            text(&stuffed(b".bonjour\r\n")),
            "..bonjour\r\n",
            "un point en tout début de message est aussi un début de ligne"
        );
    }

    #[test]
    fn a_dot_that_is_not_at_a_line_start_is_left_alone() {
        // Doubler tous les points casserait toutes les adresses et tous les nombres.
        assert_eq!(
            text(&stuffed(b"jean.dupont@a.fr\r\n")),
            "jean.dupont@a.fr\r\n"
        );
        assert_eq!(text(&stuffed(b"fin de phrase.\r\n")), "fin de phrase.\r\n");
    }

    #[test]
    fn a_bare_newline_becomes_a_crlf() {
        // Tout éditeur sur Unix produit des `LF` nus, et la RFC 5321 §2.3.8 exige `CRLF`. Un
        // serveur qui les transmet tels quels fait arriver un message abîmé.
        assert_eq!(text(&stuffed(b"un\ndeux\n")), "un\r\ndeux\r\n");
        assert_eq!(text(&stuffed(b"un\rdeux\r")), "un\r\ndeux\r\n");
    }

    #[test]
    fn a_dot_after_a_bare_newline_is_doubled_too() {
        // **Les deux transformations vont ensemble.** Sans normalisation, ce début de ligne
        // n'en serait pas un au sens du protocole, et le doublage raterait exactement la ligne
        // qu'il doit protéger.
        assert_eq!(
            text(&stuffed(b"avant\n.\napres\n")),
            "avant\r\n..\r\napres\r\n"
        );
    }

    #[test]
    fn an_already_correct_message_is_unchanged() {
        let clean = b"From: a@b.fr\r\nSubject: s\r\n\r\ncorps\r\n";
        assert_eq!(stuffed(clean), clean.to_vec());
    }

    #[test]
    fn an_empty_message_stays_empty() {
        assert!(stuffed(b"").is_empty());
    }

    #[test]
    fn the_plain_response_has_its_two_nul_separators() {
        use base64::Engine as _;
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(plain("marie", "secret"))
            .unwrap();
        // RFC 4616 : identité d'autorisation vide, puis l'identifiant, puis le mot de passe.
        assert_eq!(decoded, b"\0marie\0secret");
    }

    #[test]
    fn the_xoauth2_response_ends_with_two_control_ones() {
        use base64::Engine as _;
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(xoauth2("marie@exemple.fr", "ya29.jeton"))
            .unwrap();
        let text = String::from_utf8(decoded).unwrap();
        assert_eq!(
            text,
            "user=marie@exemple.fr\x01auth=Bearer ya29.jeton\x01\x01"
        );
        // Un seul `\x01` final et le serveur refuse sans dire pourquoi.
        assert!(text.ends_with("\x01\x01"), "il manque un séparateur final");
    }

    #[test]
    fn a_secret_never_appears_in_clear_in_the_response() {
        // Le base64 n'est pas du chiffrement, mais la ligne qui part sur le fil ne doit pas
        // porter le secret en clair : c'est ce que le critère 7 vérifiera pour de bon.
        assert!(!plain("marie", "secret").contains("secret"));
        assert!(!xoauth2("marie", "ya29.jeton").contains("ya29"));
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod stuffing_tests {
    use super::Stuffing;
    use std::io::Write;

    /// Transforme en écrivant par blocs de `chunk` octets.
    ///
    /// C'est le paramètre qui compte : le chemin streamé écrit par blocs de quelques
    /// kilooctets, et un défaut d'état ne se voit **qu'aux frontières**.
    fn through(input: &[u8], chunk: usize) -> (String, bool) {
        let mut out = Vec::new();
        let ended = {
            let mut stuffing = Stuffing::new(&mut out);
            for block in input.chunks(chunk.max(1)) {
                stuffing.write_all(block).unwrap();
            }
            stuffing.finish().unwrap()
        };
        (String::from_utf8(out).unwrap(), ended)
    }

    #[test]
    fn a_carriage_return_split_across_two_blocks_makes_one_line_break() {
        // **Le défaut que l'état `pending_cr` évite.** Un bloc peut finir sur un `\r` dont le
        // `\n` arrive dans le suivant. Traiter le `\r` tout de suite émettrait un `CRLF`, puis
        // le `\n` du bloc suivant un second : le message gagnerait une ligne vide **toutes les
        // 8 Kio**, exactement aux frontières de tampon. Invisible sur un petit message.
        let (whole, _) = through(b"avant\r\napres", 100);
        for chunk in 1..=12 {
            let (split, _) = through(b"avant\r\napres", chunk);
            assert_eq!(
                split, whole,
                "un découpage par {chunk} donne autre chose qu'un seul bloc"
            );
        }
        assert_eq!(whole, "avant\r\napres");
    }

    #[test]
    fn a_dot_at_the_start_of_a_block_is_still_doubled() {
        // **L'autre état porté.** Sans `at_line_start`, un bloc qui commence par un point ne
        // saurait pas qu'il est en début de ligne : le point ne serait pas doublé, le message
        // arriverait tronqué, et la suite serait lue comme des commandes SMTP.
        //
        // Le découpage à 7 fait tomber la frontière juste avant le point.
        let (split, _) = through(b"avant\r\n.cache\r\napres\r\n", 7);
        assert!(split.contains("..cache"), "{split}");
        let (whole, _) = through(b"avant\r\n.cache\r\napres\r\n", 100);
        assert_eq!(split, whole);
    }

    #[test]
    fn every_chunk_size_gives_the_same_output() {
        // Le test général : la transformation ne doit dépendre que de son entrée. Un message
        // qui mêle les trois pièges — points en tête, `CR` seuls, `LF` seuls.
        let input = b".un\rdeux\ntrois\r\n.quatre\n.\r\n";
        let (reference, ended) = through(input, 4_096);
        for chunk in 1..=input.len() {
            let (got, got_ended) = through(input, chunk);
            assert_eq!(got, reference, "découpage par {chunk}");
            assert_eq!(got_ended, ended, "découpage par {chunk}");
        }
    }

    #[test]
    fn a_lone_carriage_return_at_the_very_end_is_terminated() {
        // Un message qui finit sur un `\r` nu : `finish` doit le vider en `CRLF`, sinon le
        // point final se collerait à lui et ne terminerait rien.
        let (out, ended) = through(b"texte\r", 3);
        assert_eq!(out, "texte\r\n");
        assert!(ended, "la sortie finit sur une fin de ligne");
    }

    #[test]
    fn the_end_of_line_verdict_follows_the_output_not_the_input() {
        // **Le piège du terminateur.** Un message qui finit par un `\n` nu devient un `CRLF` en
        // sortie : tester l'entrée aurait ajouté un `CRLF` de trop, donc une ligne vide avant
        // le point final.
        assert!(through(b"texte\n", 2).1, "un LF nu finit bien une ligne");
        assert!(through(b"texte\r\n", 2).1);
        assert!(!through(b"texte", 2).1, "sans fin de ligne");
        // Un message vide : rien n'a été écrit, donc on est en début de ligne.
        assert!(through(b"", 1).1);
    }

    #[test]
    fn a_dot_in_the_middle_of_a_line_is_never_doubled() {
        // **Le contrôle négatif du test précédent, et il manquait.** Vérifier qu'un point en
        // début de bloc est doublé ne prouve rien : une implémentation qui remettrait
        // `at_line_start` à vrai à chaque écriture le doublerait **aussi**, et doublerait en
        // plus les points de milieu de ligne. Le destinataire lirait `www..exemple.fr`.
        //
        // Trouvé en cassant `at_line_start` exprès : les tests passaient quand même.
        let input = b"voir www.exemple.fr\r\n";
        let (reference, _) = through(input, 4_096);
        assert_eq!(reference, "voir www.exemple.fr\r\n");
        for chunk in 1..=input.len() {
            let (got, _) = through(input, chunk);
            assert_eq!(got, reference, "découpage par {chunk} a doublé un point");
        }
    }

    #[test]
    fn a_message_of_only_dots_never_loses_one() {
        let (out, _) = through(b".\r\n.\r\n.\r\n", 2);
        assert_eq!(out, "..\r\n..\r\n..\r\n");
    }
}
