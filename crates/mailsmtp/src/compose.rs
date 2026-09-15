//! Composer un message RFC 5322. **Purement local, donc entièrement testable.**
//!
//! ## Ce que ce module refuse, et pourquoi c'est ici
//!
//! Un en-tête est une ligne. Une valeur d'en-tête qui contient un retour à la ligne en crée
//! **deux**, et la seconde est écrite par qui a fourni la valeur : un sujet, un nom affiché, une
//! adresse. C'est une injection d'en-tête, et elle sert à ajouter un `Bcc` invisible ou à
//! couper le corps du message.
//!
//! Le refus est ici plutôt que dans le client SMTP parce que c'est ici qu'on connaît la
//! structure : le client, lui, voit des octets et ne peut que faire confiance.
//!
//! ## Rien ne s'ajoute en secret
//!
//! `docs/PHASE-3.md` : pas d'en-tête `X-Mailer`, pas d'identifiant de suivi, pas de pixel. Ce
//! module écrit les en-têtes que la RFC impose, ceux que l'utilisateur a remplis, et rien
//! d'autre. La liste est courte exprès, et elle est lisible dans [`Draft::headers`].

use std::io::{Read, Write};

use mailcore::BlobHash;

use crate::error::{Error, Result};

/// Longueur maximale d'une ligne d'en-tête avant pliage.
///
/// La RFC 5322 §2.1.1 recommande 78 caractères et impose 998. On plie à 76 pour laisser la
/// place au `=?UTF-8?B?…?=` d'un mot encodé sans franchir la recommandation.
const FOLD_AT: usize = 76;

/// Une adresse, validée.
///
/// ## Pourquoi un type et pas une `String`
///
/// Parce qu'une adresse non validée qui traverse trois fonctions finit dans un `MAIL FROM`, et
/// qu'un retour à la ligne y injecte une commande SMTP. Le type est la preuve que la
/// vérification a eu lieu, et il n'y a qu'un seul endroit qui puisse en fabriquer un.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Address {
    /// La partie adresse, sans chevrons.
    addr: String,
    /// Le nom affiché, s'il y en a un.
    name: Option<String>,
}

impl Address {
    /// Valide une adresse et son nom affiché.
    ///
    /// ## Ce qui est refusé
    ///
    /// - un **retour à la ligne**, dans l'adresse comme dans le nom : c'est l'injection ;
    /// - une adresse **sans `@`**, ou dont une des deux parties est vide ;
    /// - une adresse qui contient une **espace**, un chevron ou une virgule — trois caractères
    ///   qui changent le sens de la liste où elle sera écrite ;
    /// - le **NUL**, qui tronque la chaîne dès qu'elle traverse une interface C.
    ///
    /// Ce qui n'est **pas** vérifié : que le domaine existe, ou que la boîte existe. Aucune des
    /// deux ne se vérifie sans requête réseau, et `docs/PRIVACY.md` ne le permettrait pas de
    /// toute façon. C'est le serveur qui refusera, et le critère 8 dira pourquoi.
    ///
    /// # Errors
    ///
    /// [`Error::Unsendable`], en nommant ce qui n'allait pas — jamais en recopiant l'adresse
    /// entière dans le message, qui finit dans un journal.
    pub fn parse(addr: &str, name: Option<&str>) -> Result<Self> {
        let addr = addr.trim();
        let refuse = |reason: &str| {
            Err(Error::Unsendable {
                reason: reason.to_owned(),
            })
        };

        if addr.is_empty() {
            return refuse("adresse vide");
        }
        if addr.contains(['\r', '\n']) {
            return refuse("adresse contenant un retour à la ligne");
        }
        if addr.contains('\0') {
            return refuse("adresse contenant un octet nul");
        }
        if addr.contains([' ', '\t', '<', '>', ',', ';']) {
            return refuse("adresse contenant un séparateur : espace, chevron ou virgule");
        }
        // `rsplit_once` et non `split_once` : une partie locale peut contenir un `@` si elle
        // est citée, et c'est le **dernier** qui sépare le domaine.
        let Some((local, domain)) = addr.rsplit_once('@') else {
            return refuse("adresse sans `@`");
        };
        if local.is_empty() {
            return refuse("adresse sans partie locale avant le `@`");
        }
        if domain.is_empty() || !domain.contains('.') {
            return refuse("adresse sans domaine exploitable après le `@`");
        }

        let name = match name.map(str::trim).filter(|it| !it.is_empty()) {
            Some(name) if name.contains(['\r', '\n', '\0']) => {
                return refuse("nom affiché contenant un retour à la ligne");
            }
            other => other.map(str::to_owned),
        };

        Ok(Self {
            addr: addr.to_owned(),
            name,
        })
    }

    /// La partie adresse, telle qu'elle va dans l'enveloppe SMTP.
    #[must_use]
    pub fn addr(&self) -> &str {
        &self.addr
    }

    /// La forme d'en-tête : `Nom <adresse>`, ou l'adresse seule.
    ///
    /// Le nom est encodé s'il n'est pas ASCII, et **cité** s'il contient un caractère spécial.
    #[must_use]
    pub fn to_header(&self) -> String {
        match &self.name {
            None => self.addr.clone(),
            Some(name) => format!("{} <{}>", display_name(name), self.addr),
        }
    }
}

/// Encode une valeur d'en-tête si elle n'est pas ASCII imprimable — RFC 2047.
///
/// ## Le piège à deux étages
///
/// Encoder l'en-tête **entier** casse les adresses : un `To: =?UTF-8?B?...?=` où le base64
/// contient `<jean@exemple.fr>` n'est plus une adresse pour le destinataire, c'est du texte. Ne
/// rien encoder fait arriver du charabia dès qu'un sujet est en arabe — et le corpus en a, mesuré
/// à la phase 1.
///
/// La règle est donc : on encode **ce mot-là**, et seulement s'il en a besoin. Les mots ASCII
/// restent lisibles, ce qui est aussi ce qui rend un en-tête débogable à l'œil.
#[must_use]
pub fn encoded_word(value: &str) -> String {
    if is_plain_ascii(value) {
        return value.to_owned();
    }
    encode_word(value)
}

/// Un **nom d'affichage** prêt à mettre devant une adresse : encodé, ou cité s'il le faut.
///
/// ## Pourquoi ce n'est pas la même fonction que [`encoded_word`]
///
/// Les deux emplois n'obéissent pas à la même règle de la RFC 5322, et les avoir confondus a
/// produit un défaut visible chez **tous** les destinataires d'une réponse :
///
/// - un nom d'affichage est un `phrase` dans un champ **structuré**. Les caractères spéciaux y
///   ont un sens, donc `Jean, Dupont <…>` sans guillemets serait lu comme deux adresses : il
///   faut citer ;
/// - un `Subject` est un champ **non structuré** — RFC 5322 §3.6.5. Aucun caractère n'y est
///   spécial, donc citer n'est pas seulement inutile : ça ajoute des guillemets que le
///   destinataire **lit**.
///
/// Une seule fonction servait les deux, et tout sujet contenant un deux-points — donc **toute
/// réponse**, puisqu'elle commence par « Re: » — partait en `Subject: "Re: Facture"`. Le banc
/// du critère 1 l'a trouvé à sa première exécution sur le corpus, sur 1 164 messages.
#[must_use]
pub fn display_name(value: &str) -> String {
    if is_plain_ascii(value) {
        if value.contains([',', ';', '<', '>', '"', '@', ':']) {
            return format!("\"{}\"", value.replace('\\', r"\\").replace('"', "\\\""));
        }
        return value.to_owned();
    }
    encode_word(value)
}

/// Vrai si cette valeur peut partir telle quelle dans un en-tête.
///
/// Les `\r` et `\n` sont exclus parce qu'ils termineraient l'en-tête — c'est l'injection que
/// [`Address::parse`] refuse déjà — et `?` et `=` parce qu'ils pourraient être relus comme le
/// début d'un mot encodé.
fn is_plain_ascii(value: &str) -> bool {
    value.is_ascii() && !value.contains(['\r', '\n', '?', '='])
}

/// Un `Message-ID` sous la forme que l'en-tête demande : `<quelque-chose@domaine>`.
///
/// ## Ce qu'elle accepte, et ce qu'elle jette
///
/// Les chevrons sont ajoutés s'ils manquent, et jamais doublés. Un identifiant **vide** ou qui
/// contient un blanc rend `None` : un `msg-id` n'en contient pas — RFC 5322 §3.6.4 — et l'écrire
/// quand même couperait l'en-tête en deux jetons, dont le second serait lu comme un identifiant
/// inventé. C'est le même refus que celui de [`Address::parse`] devant un retour à la ligne :
/// mieux vaut un fil non rattaché qu'un en-tête corrompu.
///
/// Un `<` ou un `>` **au milieu** est jeté pour la même raison.
///
/// Publique parce que le banc du critère 1 en a besoin : il doit comparer la chaîne écrite à
/// celle qui **pouvait légitimement** être écrite, et refaire la règle de son côté serait en
/// avoir deux.
#[must_use]
pub fn canonical_msg_id(value: &str) -> Option<String> {
    let bare = value.trim().trim_start_matches('<').trim_end_matches('>');
    if bare.is_empty()
        || bare.contains(char::is_whitespace)
        || bare.contains(['<', '>'])
        || !bare.is_ascii()
    {
        return None;
    }
    Some(format!("<{bare}>"))
}

/// Une valeur destinée à l'**intérieur** d'un paramètre MIME déjà entre guillemets.
///
/// Elle est échappée, pas citée : les guillemets sont posés par l'appelant, et en ajouter une
/// paire donnerait `filename=""rapport""`. Un nom non ASCII part en mot encodé, ce qui ne
/// contient ni guillemet ni blanc.
#[must_use]
fn quoted_parameter(value: &str) -> String {
    if is_plain_ascii(value) {
        return value.replace('\\', r"\\").replace('"', "\\\"");
    }
    encode_word(value)
}

/// Encode une valeur en mot encodé RFC 2047.
fn encode_word(value: &str) -> String {
    use base64::Engine as _;
    // `B` et non `Q` : le base64 est plus court dès qu'un mot sur trois est non ASCII, et
    // surtout il ne peut pas produire d'espace — le `Q` en produit, et une espace dans un mot
    // encodé le termine.
    let encoded = base64::engine::general_purpose::STANDARD.encode(value.as_bytes());
    format!("=?UTF-8?B?{encoded}?=")
}

/// Plie une ligne d'en-tête trop longue, sur les espaces.
///
/// La RFC 5322 §2.2.3 : une ligne de repli commence par une espace ou une tabulation. Un mot
/// plus long que la limite — une URL, un `Message-ID` — **n'est pas coupé** : le couper le
/// rendrait faux, et la RFC autorise 998 caractères.
#[must_use]
pub fn fold(header: &str) -> String {
    let mut out = String::with_capacity(header.len() + 8);
    let mut column = 0_usize;
    for (index, word) in header.split(' ').enumerate() {
        if index > 0 {
            if column + 1 + word.len() > FOLD_AT {
                out.push_str("\r\n ");
                column = 1;
            } else {
                out.push(' ');
                column += 1;
            }
        }
        out.push_str(word);
        column += word.len();
    }
    out
}

/// Les en-têtes de fil d'une réponse : `In-Reply-To` et `References`.
///
/// ## Une seule implémentation, parce que se tromper casse le fil de tout le monde
///
/// RFC 5322 §3.6.4 : `In-Reply-To` porte le `Message-ID` du message auquel on répond, et
/// `References` **prolonge** la chaîne existante. La reconstruire depuis un fil local donnerait
/// une chaîne différente de celle des autres clients, et un lecteur qui regroupe sur
/// `References` verrait deux fils au lieu d'un.
///
/// Cette règle vivait dans la coquille, donc invérifiable ailleurs. Elle est ici : `mail reply`
/// le jour où il existera, et le banc du critère 1 qui la vérifie sur les vrais fils du corpus,
/// appellent la **même** fonction que la fenêtre de rédaction.
///
/// ## Un parent sans `Message-ID` ne produit pas d'en-tête inventé
///
/// Ça existe — le corpus en a — et il n'y a alors rien d'honnête à mettre dans `In-Reply-To`.
/// `References` garde quand même la chaîne du parent : elle rattache la réponse au fil même
/// sans le dernier maillon.
///
/// ## Un identifiant déjà présent n'est pas ajouté deux fois
///
/// Certains clients écrivent leur propre `Message-ID` dans leur `References`. L'ajouter à
/// nouveau ferait grossir la chaîne à chaque échange sans rien dire de plus.
#[must_use]
pub fn reply_threading(parent_id: Option<&str>, parent_references: &[String]) -> Threading {
    let mut references: Vec<String> = parent_references
        .iter()
        .map(|it| it.trim().to_owned())
        .filter(|it| !it.is_empty())
        .collect();
    let parent = parent_id
        .map(str::trim)
        .filter(|it| !it.is_empty())
        .map(ToOwned::to_owned);
    if let Some(parent) = &parent
        && !references.contains(parent)
    {
        references.push(parent.clone());
    }
    Threading {
        in_reply_to: parent,
        references,
    }
}

/// Ce qu'une réponse porte comme en-têtes de fil.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Threading {
    /// Le `Message-ID` du message auquel on répond, s'il en avait un.
    pub in_reply_to: Option<String>,
    /// La chaîne `References`, du plus ancien au plus récent.
    pub references: Vec<String>,
}

/// Le sujet d'une réponse : celui du parent, préfixé une seule fois.
///
/// ## « Re: » une fois, jamais deux
///
/// Un « Re: Re: Re: » vient d'un client qui ne vérifie pas, et le corpus en est plein. La
/// comparaison ignore la casse — « RE: », « re: », « Re : » avec une espace avant le
/// deux-points sont tous des préfixes de réponse écrits par des clients réels.
///
/// Les préfixes d'autres langues ne sont **pas** reconnus : « Antw: », « Odp: », « SV: »
/// existent, mais décider qu'un sujet commence par une abréviation étrangère demande une liste
/// qui vieillit, et se tromper produit « Re: Antw: … » — ce que fait aussi tout le reste du
/// monde. Un sujet vide donne « Re: » seul, ce qui est ce qu'attend un lecteur devant une
/// réponse à un message sans sujet.
#[must_use]
pub fn reply_subject(parent_subject: &str) -> String {
    let trimmed = parent_subject.trim();
    let lowered = trimmed.to_lowercase();
    if lowered.starts_with("re:") || lowered.starts_with("re :") {
        return trimmed.to_owned();
    }
    format!("Re: {trimmed}")
}

/// Fabrique un `Message-ID` unique.
///
/// ## Pourquoi il vient de nous
///
/// Laisser le serveur en poser un — certains le font — casse le fil dès que le message revient
/// par IMAP dans les envoyés : on ne le reconnaît pas, et la réponse qu'on reçoit ensuite pointe
/// vers un identifiant qu'on n'a jamais vu.
///
/// La partie aléatoire fait 128 bits, tirés du système. Le domaine est celui de l'expéditeur :
/// un identifiant doit être unique **globalement**, et la RFC 5322 §3.6.4 obtient cette
/// garantie du domaine.
///
/// # Errors
///
/// [`Error::Unsendable`] si le système ne peut pas fournir d'aléa — auquel cas fabriquer un
/// identifiant devinable serait pire que refuser.
pub fn message_id(sender: &Address) -> Result<String> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).map_err(|_| Error::Unsendable {
        reason: "le système ne fournit pas d'aléa pour un Message-ID".to_owned(),
    })?;
    let hex: String = bytes.iter().map(|it| format!("{it:02x}")).collect();
    // Le domaine, pas l'adresse entière : mettre la partie locale dans un `Message-ID` la
    // publierait dans tous les fils où le message passe, y compris chez des tiers.
    let domain = sender.addr.rsplit_once('@').map_or("localhost", |(_, d)| d);
    Ok(format!("<{hex}@{domain}>"))
}

/// La date d'un en-tête `Date`, au format de la RFC 5322 §3.3.
///
/// ## Pourquoi c'est écrit à la main
///
/// Il n'y a aucune bibliothèque de date dans le dépôt, et en ajouter une pour formater une
/// ligne serait un arbre de dépendances pour quinze lignes de calendrier. Le calcul est celui
/// de l'algorithme de Howard Hinnant — jours civils depuis l'époque — qui tient en une fonction
/// et n'a pas de cas particulier.
///
/// ## Le décalage est toujours `+0000`
///
/// Et ce n'est pas un raccourci : c'est ce qui évite de publier le fuseau de l'utilisateur.
/// `Date: … +0200` dit à tout destinataire dans quel pays on se trouve, et la RFC n'exige rien
/// de plus qu'un décalage valide. L'instant est le même ; seule l'indiscrétion disparaît.
#[must_use]
pub fn rfc5322_date(unix_seconds: i64) -> String {
    const DAYS: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];

    // `div_euclid` et `rem_euclid` et non `/` et `%` : une date **avant** 1970 donne un
    // quotient négatif, et la division entière de Rust tronque vers zéro — ce qui décalerait
    // d'un jour. Un `Date` négatif ne devrait pas arriver, et « ne devrait pas » n'est pas une
    // raison de rendre faux.
    let days = unix_seconds.div_euclid(86_400);
    let seconds_of_day = unix_seconds.rem_euclid(86_400);

    let (year, month, day) = civil_from_days(days);
    // Le 1er janvier 1970 était un jeudi, soit l'indice 3 de `DAYS`.
    let weekday = (days + 3).rem_euclid(7) as usize;

    format!(
        "{}, {} {} {} {:02}:{:02}:{:02} +0000",
        DAYS[weekday],
        day,
        MONTHS[(month - 1) as usize],
        year,
        seconds_of_day / 3600,
        (seconds_of_day % 3600) / 60,
        seconds_of_day % 60,
    )
}

/// Jours depuis l'époque → année, mois, jour. L'algorithme de Howard Hinnant.
///
/// Sans cas particulier pour les années bissextiles : il décale l'origine à mars, ce qui met le
/// 29 février en fin d'année et fait disparaître l'exception.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Un message à envoyer, avant d'être assemblé.
///
/// ## Ce qui n'y est pas, et qui n'y sera pas
///
/// Pas de champ `X-Mailer`, pas d'identifiant de campagne, pas de pixel. `docs/PHASE-3.md` :
/// tout ce que le message porte doit être lisible par l'utilisateur, et la seule façon de le
/// garantir est que la liste des en-têtes soit **courte et fixe**. Elle est ici, en entier.
#[derive(Debug, Clone)]
pub struct Draft {
    /// L'expéditeur.
    pub from: Address,
    /// Les destinataires visibles.
    pub to: Vec<Address>,
    /// En copie, visibles aussi.
    pub cc: Vec<Address>,
    /// En copie **cachée**. Voir [`Draft::assemble`] : ils ne sont pas dans les en-têtes.
    pub bcc: Vec<Address>,
    /// Le sujet, tel que l'utilisateur l'a tapé.
    pub subject: String,
    /// Le corps en texte brut. **Toujours présent** : voir [`Draft::assemble`].
    pub text: String,
    /// Le corps en HTML, s'il y en a un.
    pub html: Option<String>,
    /// Le `Message-ID` auquel ce message répond.
    pub in_reply_to: Option<String>,
    /// La chaîne des `Message-ID` du fil, du plus ancien au plus récent.
    pub references: Vec<String>,
    /// Les pièces jointes, désignées par leur contenu. Voir [`Attachment`].
    pub attachments: Vec<Attachment>,
}

impl Draft {
    /// Un brouillon minimal : un expéditeur, un destinataire, un sujet, du texte.
    #[must_use]
    pub fn new(from: Address, to: Vec<Address>, subject: &str, text: &str) -> Self {
        Self {
            from,
            to,
            cc: Vec::new(),
            bcc: Vec::new(),
            subject: subject.to_owned(),
            text: text.to_owned(),
            html: None,
            in_reply_to: None,
            references: Vec::new(),
            attachments: Vec::new(),
        }
    }

    /// Ajoute une signature au corps, dans les deux parties.
    ///
    /// ## Pourquoi l'assemblage est ici et pas chez chaque client
    ///
    /// Parce qu'il y a deux moteurs — `mail send` et `outbox.send` — et que « comment une
    /// signature rejoint un message » doit avoir **une** implémentation. C'est la même raison
    /// que `mailsmtp::queue::stage`, qui assemble et met en file pour les deux : deux copies
    /// divergent, et celle qui divergerait ici enverrait à quelqu'un un message dont la
    /// signature est doublée, ou absente.
    ///
    /// ## Les deux parties disent la même chose, ou il n'y en a qu'une
    ///
    /// `multipart/alternative` veut deux versions du **même** message. La signature est donc
    /// ajoutée au texte **et** au HTML, par le même chemin — `mailhtml::rich` — et la partie
    /// HTML n'apparaît que si le document dit quelque chose qu'un texte nu ne peut pas dire :
    /// un gras, un italique, un lien, une puce. Un corps et une signature en texte nu partent
    /// en une seule partie.
    ///
    /// ## Ce qu'elle écrase
    ///
    /// Un `html` déjà posé par l'appelant est **remplacé**, parce que la seule façon de garder
    /// les deux d'accord est de les produire ensemble. Aucun appelant n'en pose aujourd'hui
    /// avant de signer ; celui qui voudrait le faire devra signer d'abord.
    ///
    /// Une signature vide ne fait rien : un compte sans signature ne doit pas ajouter deux
    /// lignes blanches en fin de message.
    /// ## Le corps est normalisé avant d'être signé, et il faut le faire ici
    ///
    /// Un corps arrive avec les fins de ligne de sa source : `\r\n` d'un fichier lu par la CLI,
    /// `\n` d'un champ de texte. Le modèle de document compte ses lignes sur `\n` seul, donc un
    /// `\r` resté dedans devient un caractère **dans** la ligne — il ressortirait tel quel dans
    /// le HTML envoyé. Et un corps qui finit par une fin de ligne donnerait **deux** lignes
    /// blanches avant la signature au lieu d'une, puisque le séparateur en ajoute une.
    ///
    /// Les deux se corrigent au même endroit : ici, où le corps entre dans un document. Le
    /// premier jet ne le faisait pas, et le test l'a montré sur un corps en `\r\n`.
    pub fn sign_with(&mut self, signature: &mailhtml::rich::Document) {
        if signature.is_empty() {
            return;
        }
        let body = self.text.replace("\r\n", "\n").replace('\r', "\n");
        let mut whole = mailhtml::rich::Document::plain(body.trim_end_matches('\n'));
        whole.append(signature);
        self.text = whole.to_text();
        self.html = whole.is_formatted().then(|| whole.to_html());
    }

    /// Tous les destinataires de l'enveloppe : `to`, `cc` **et** `bcc`.
    ///
    /// L'enveloppe SMTP et les en-têtes ne disent pas la même chose, et c'est le mécanisme même
    /// de la copie cachée : un `bcc` reçoit le message parce qu'il est dans un `RCPT TO`, et
    /// personne ne le sait parce qu'il n'est dans aucun en-tête.
    #[must_use]
    pub fn envelope_recipients(&self) -> Vec<&Address> {
        self.to
            .iter()
            .chain(self.cc.iter())
            .chain(self.bcc.iter())
            .collect()
    }

    /// Assemble le message RFC 5322 complet.
    ///
    /// ## La copie cachée n'est pas dans les en-têtes
    ///
    /// C'est **la** règle de correction de cette fonction. Écrire un en-tête `Bcc:` enverrait la
    /// liste des destinataires cachés à tous les destinataires, ce qui est l'inverse exact de ce
    /// que l'utilisateur a demandé. Le champ existe dans le brouillon, il sort par
    /// [`Draft::envelope_recipients`], et il ne franchit jamais cette fonction.
    ///
    /// ## Le texte brut est toujours là, même avec du HTML
    ///
    /// Un `multipart/alternative` sans partie texte est illisible pour un lecteur qui ne rend
    /// pas le HTML — et pour un lecteur qui le rend mais refuse de le charger, ce que
    /// `docs/PRIVACY.md` demande à la coquille de faire par défaut. Envoyer du HTML seul, c'est
    /// exiger du destinataire ce qu'on refuse à soi-même.
    ///
    /// ## La frontière ne peut pas apparaître dans le corps
    ///
    /// Elle porte 128 bits d'aléa. Une frontière devinable ou fixe qui apparaîtrait dans le
    /// texte couperait le message en deux au mauvais endroit — et un expéditeur qui choisit son
    /// texte pourrait provoquer exprès la coupure.
    ///
    /// # Errors
    ///
    /// [`Error::Unsendable`] s'il n'y a aucun destinataire, ou si le système ne fournit pas
    /// d'aléa pour le `Message-ID` ou la frontière.
    pub fn assemble(&self, now: i64) -> Result<Vec<u8>> {
        let mut out = Vec::with_capacity(self.text.len() + 1024);
        self.write_to(&mut out, now, &mut |_| {
            Err(Error::Unsendable {
                reason: "un message avec pièces jointes ne s'assemble pas en mémoire : \
                         utiliser `write_to`"
                    .to_owned(),
            })
        })?;
        Ok(out)
    }

    /// Écrit le message RFC 5322 dans un puits, **sans jamais le tenir en entier en mémoire**.
    ///
    /// ## C'est la forme qui tient le critère 3
    ///
    /// Une pièce jointe de 25 Mo devient 34 Mo de base64. Assembler en mémoire tiendrait les
    /// deux vivants en même temps, plus le `Vec` qui grossit par doublements — de l'ordre de
    /// cent mégaoctets pour un seul message. `docs/PHASE-3.md` borne la crête à 200 Mo, et la
    /// règle 4 du `CLAUDE.md` interdit de charger un contenu entier de toute façon.
    ///
    /// Ici, chaque pièce jointe passe du disque au puits par un tampon de quelques kilooctets :
    /// l'encodage base64 est **streamé**, et la seule chose de la taille du message qui existe
    /// est ce que le puits en fait.
    ///
    /// ## Les pièces jointes sont désignées par leur contenu, pas par un chemin
    ///
    /// `open` reçoit le hachage d'une pièce et rend un flux de lecture. C'est ce qui permet au
    /// démon d'assembler un message pour un client **sans qu'aucun client ne nomme un chemin** :
    /// une pièce jointe doit d'abord être dans le magasin de blobs, et un client ne peut donc
    /// référencer que du contenu déjà présent. C'est la même règle que `jobs.start`, qui prend
    /// un rang et jamais un chemin.
    ///
    /// ## La structure MIME dépend de ce qu'il y a
    ///
    /// | texte | HTML | pièces | structure |
    /// |---|---|---|---|
    /// | oui | non | non | `text/plain` |
    /// | oui | oui | non | `multipart/alternative` |
    /// | oui | non | oui | `multipart/mixed` [ `text/plain`, pièces… ] |
    /// | oui | oui | oui | `multipart/mixed` [ `multipart/alternative`, pièces… ] |
    ///
    /// Le `mixed` **enveloppe** l'`alternative` et ne le remplace pas : mettre les deux corps et
    /// les pièces au même niveau ferait afficher le texte *et* le HTML l'un après l'autre chez
    /// tous les lecteurs, ce qui est le défaut MIME le plus courant.
    ///
    /// # Errors
    ///
    /// [`Error::Unsendable`] s'il n'y a aucun destinataire, si le système ne fournit pas d'aléa,
    /// ou si `open` refuse une pièce. [`Error::Network`] — étape `Data` — sur une erreur du
    /// puits : c'est le seul type d'erreur du crate qui porte une cause d'entrée-sortie, et
    /// l'étape dit qu'aucun octet n'a pu partir de travers.
    pub fn write_to<W: Write>(
        &self,
        sink: &mut W,
        now: i64,
        open: &mut dyn FnMut(BlobHash) -> Result<Box<dyn Read>>,
    ) -> Result<()> {
        if self.envelope_recipients().is_empty() {
            return Err(Error::Unsendable {
                reason: "aucun destinataire".to_owned(),
            });
        }

        let mut head = String::with_capacity(512);
        let mut line = |header: &str| {
            head.push_str(&fold(header));
            head.push_str("\r\n");
        };

        line(&format!("From: {}", self.from.to_header()));
        line(&format!("To: {}", list(&self.to)));
        if !self.cc.is_empty() {
            line(&format!("Cc: {}", list(&self.cc)));
        }
        // **Aucun en-tête `Bcc`.** Voir la doc de cette fonction.
        line(&format!("Subject: {}", encoded_word(&self.subject)));
        line(&format!("Date: {}", rfc5322_date(now)));
        line(&format!("Message-ID: {}", message_id(&self.from)?));
        // **Les identifiants sont remis en forme `<…>` ici, à l'écriture.**
        //
        // Ils arrivent des deux formes selon leur source : un en-tête brut les porte avec leurs
        // chevrons, `mail-parser` les rend **sans**. La coquille lit l'API, qui lit
        // `mail-parser` : ses réponses partaient donc avec `In-Reply-To: abc@x.fr`, ce que la
        // RFC 5322 §3.6.4 n'autorise pas — un `msg-id` s'écrit entre chevrons — et qu'un client
        // qui regroupe sur cet en-tête ne rattache pas.
        //
        // La remise en forme est faite **au point d'écriture** et pas chez l'appelant : c'est
        // une propriété de l'en-tête, pas de qui le remplit, et une barrière par défaut vaut
        // mieux que la vigilance de trois appelants. Le banc du critère 1 a trouvé le défaut sur
        // 415 fils du corpus.
        if let Some(parent) = self.in_reply_to.as_deref().and_then(canonical_msg_id) {
            line(&format!("In-Reply-To: {parent}"));
        }
        let chain: Vec<String> = self
            .references
            .iter()
            .filter_map(|it| canonical_msg_id(it))
            .collect();
        if !chain.is_empty() {
            line(&format!("References: {}", chain.join(" ")));
        }
        line("MIME-Version: 1.0");
        put(sink, head.as_bytes())?;

        if self.attachments.is_empty() {
            return self.write_body(sink);
        }

        // Le `mixed` enveloppe le corps, quel qu'il soit.
        let outer = boundary()?;
        put(
            sink,
            format!("Content-Type: multipart/mixed; boundary=\"{outer}\"\r\n\r\n").as_bytes(),
        )?;
        put(sink, format!("--{outer}\r\n").as_bytes())?;
        self.write_body(sink)?;

        for attachment in &self.attachments {
            put(sink, format!("\r\n--{outer}\r\n").as_bytes())?;
            self.write_attachment(sink, attachment, open)?;
        }
        put(sink, format!("\r\n--{outer}--\r\n").as_bytes())
    }

    /// Le corps : `text/plain` seul, ou `multipart/alternative`.
    fn write_body<W: Write>(&self, sink: &mut W) -> Result<()> {
        match &self.html {
            None => {
                put(sink, b"Content-Type: text/plain; charset=utf-8\r\n")?;
                put(sink, b"Content-Transfer-Encoding: 8bit\r\n\r\n")?;
                put(sink, self.text.as_bytes())
            }
            Some(html) => {
                let inner = boundary()?;
                put(
                    sink,
                    format!("Content-Type: multipart/alternative; boundary=\"{inner}\"\r\n\r\n")
                        .as_bytes(),
                )?;
                // L'ordre compte : la RFC 2046 §5.1.4 dit que la **dernière** partie est la
                // préférée. Le texte d'abord, le HTML ensuite.
                put(sink, format!("--{inner}\r\n").as_bytes())?;
                put(sink, b"Content-Type: text/plain; charset=utf-8\r\n")?;
                put(sink, b"Content-Transfer-Encoding: 8bit\r\n\r\n")?;
                put(sink, self.text.as_bytes())?;
                put(sink, format!("\r\n--{inner}\r\n").as_bytes())?;
                put(sink, b"Content-Type: text/html; charset=utf-8\r\n")?;
                put(sink, b"Content-Transfer-Encoding: 8bit\r\n\r\n")?;
                put(sink, html.as_bytes())?;
                put(sink, format!("\r\n--{inner}--\r\n").as_bytes())
            }
        }
    }

    /// Une pièce jointe : ses en-têtes, puis son contenu en base64 streamé.
    fn write_attachment<W: Write>(
        &self,
        sink: &mut W,
        attachment: &Attachment,
        open: &mut dyn FnMut(BlobHash) -> Result<Box<dyn Read>>,
    ) -> Result<()> {
        // **Le nom de fichier est encodé, jamais recopié tel quel.** Un nom accentué en octets
        // bruts dans un en-tête est hors RFC 5322, et un nom qui porterait un guillemet ou un
        // retour à la ligne casserait la structure MIME — ou y injecterait un en-tête.
        // `quoted_parameter` refuse les deux par construction : il n'émet que de l'ASCII sûr,
        // et il **échappe** au lieu de citer — les guillemets sont déjà dans le format
        // ci-dessous, et en ajouter une paire produirait `filename=""x""`.
        let name = quoted_parameter(&attachment.filename);
        put(
            sink,
            format!("Content-Type: {}; name=\"{name}\"\r\n", attachment.mime).as_bytes(),
        )?;
        put(
            sink,
            format!("Content-Disposition: attachment; filename=\"{name}\"\r\n").as_bytes(),
        )?;
        put(sink, b"Content-Transfer-Encoding: base64\r\n\r\n")?;

        let mut source = open(attachment.blob)?;
        // Le puits est enveloppé : l'encodeur base64 écrit dans le replieur, qui coupe à
        // 76 caractères, qui écrit dans le puits final. Rien n'accumule.
        let mut wrapped = Wrapping::new(sink);
        {
            let mut encoder = base64::write::EncoderWriter::new(
                &mut wrapped,
                &base64::engine::general_purpose::STANDARD,
            );
            std::io::copy(&mut source, &mut encoder).map_err(|source| Error::Network {
                stage: crate::Stage::Data,
                source,
            })?;
            encoder.finish().map_err(|source| Error::Network {
                stage: crate::Stage::Data,
                source,
            })?;
        }
        wrapped.finish()
    }
}

/// Une pièce jointe, désignée par son **contenu**.
///
/// ## Pourquoi un hachage et pas un chemin
///
/// Un chemin dans une demande de client donnerait, à quiconque détient le jeton du démon, la
/// lecture de n'importe quel fichier de la machine — rangé dans un message, puis envoyé où il
/// veut. C'est la même raison qui fait que `jobs.start` prend un rang et jamais un chemin.
///
/// Avec un hachage, un client ne peut référencer que du contenu **déjà** dans le magasin de
/// blobs. Y mettre un fichier est une opération locale, faite par celui qui a le fichier.
///
/// ## La taille est portée, et elle sert avant d'ouvrir
///
/// `MAIL FROM ... SIZE=` s'annonce avant le transfert, et un serveur qui connaît la limite
/// refuse alors **avant** que 25 Mo ne montent. Sans la taille dans le brouillon, il faudrait
/// ouvrir chaque pièce pour la mesurer, donc lire deux fois.
///
/// Elle est celle du contenu **brut**. Le base64 en fait un tiers de plus, et
/// [`Attachment::encoded_len`] le calcule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attachment {
    /// Le nom montré au destinataire. Encodé à l'écriture, jamais recopié tel quel.
    pub filename: String,
    /// Le type MIME déclaré, `application/octet-stream` à défaut.
    pub mime: String,
    /// Le contenu, dans le magasin de blobs.
    pub blob: BlobHash,
    /// La taille du contenu brut, en octets.
    pub size: u64,
}

impl Attachment {
    /// La taille de cette pièce **une fois encodée**, sauts de ligne compris.
    ///
    /// Sert à annoncer `SIZE` : annoncer la taille brute ferait passer un message sous une
    /// limite qu'il dépasse d'un tiers, et le refus arriverait après le transfert — donc au
    /// pire moment, celui où le doute commence.
    ///
    /// Base64 : quatre caractères pour trois octets, arrondi au supérieur, plus un `CRLF` par
    /// ligne de 76 caractères.
    #[must_use]
    pub const fn encoded_len(&self) -> u64 {
        let groups = self.size.div_ceil(3);
        let chars = groups.saturating_mul(4);
        let lines = chars.div_ceil(BASE64_LINE as u64);
        chars.saturating_add(lines.saturating_mul(2))
    }
}

/// Le type MIME d'un nom de fichier, d'après son extension.
///
/// Une liste courte : les types qu'un humain joint à un mail. Tout le reste est
/// `application/octet-stream`, ce qui est **correct** et non un aveu d'échec — la RFC 2046 §4.5.1
/// en fait le défaut, et le client du destinataire déduit de son côté.
#[must_use]
pub fn mime_for(filename: &str) -> &'static str {
    let extension = filename
        .rsplit_once('.')
        .map(|(_, ext)| ext.to_ascii_lowercase())
        .unwrap_or_default();
    match extension.as_str() {
        "pdf" => "application/pdf",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "txt" | "log" => "text/plain; charset=utf-8",
        "csv" => "text/csv; charset=utf-8",
        "md" => "text/markdown; charset=utf-8",
        "html" | "htm" => "text/html; charset=utf-8",
        "json" => "application/json",
        "xml" => "application/xml",
        "zip" => "application/zip",
        "odt" => "application/vnd.oasis.opendocument.text",
        "ods" => "application/vnd.oasis.opendocument.spreadsheet",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        _ => "application/octet-stream",
    }
}

/// La longueur d'une ligne de base64, en caractères.
///
/// 76, ce que la RFC 2045 §6.8 impose comme maximum. Pas 64, qui est la valeur de PEM et qu'on
/// voit parfois recopiée par erreur : elle est valide mais gaspille 15 % de lignes.
const BASE64_LINE: usize = 76;

/// Un puits qui coupe ce qu'on lui écrit en lignes de [`BASE64_LINE`] caractères.
///
/// ## Pourquoi il faut replier
///
/// La RFC 2045 §6.8 borne une ligne de base64 à 76 caractères, et la RFC 5322 §2.1.1 borne
/// **toute** ligne d'un message à 998 octets. Un base64 non replié dépasse les deux dès
/// 750 octets de pièce jointe : des serveurs le refusent, d'autres le coupent où ils veulent —
/// et couper au mauvais endroit corrompt la pièce.
///
/// ## Il compte les caractères, pas les octets écrits
///
/// Le compteur est le reste de la ligne courante, gardé **entre** les appels à `write` :
/// l'encodeur base64 écrit par blocs de taille arbitraire, et un replieur qui repartirait de
/// zéro à chaque appel produirait des lignes de longueur aléatoire. C'est le seul état de cette
/// structure, et c'est celui qui est facile à oublier.
#[derive(Debug)]
struct Wrapping<'a, W: Write> {
    sink: &'a mut W,
    /// Combien de caractères ont déjà été écrits sur la ligne courante.
    on_line: usize,
}

impl<'a, W: Write> Wrapping<'a, W> {
    const fn new(sink: &'a mut W) -> Self {
        Self { sink, on_line: 0 }
    }

    /// Termine la dernière ligne, si elle n'est pas vide.
    ///
    /// Sans ce `CRLF` final, la frontière `--…` qui suit se collerait à la fin du base64 et ne
    /// serait pas reconnue : le lecteur verrait une seule partie, tronquée.
    fn finish(self) -> Result<()> {
        if self.on_line > 0 {
            put(self.sink, b"\r\n")?;
        }
        Ok(())
    }
}

impl<W: Write> Write for Wrapping<'_, W> {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        let mut written = 0;
        while written < buffer.len() {
            let room = BASE64_LINE.saturating_sub(self.on_line);
            if room == 0 {
                self.sink.write_all(b"\r\n")?;
                self.on_line = 0;
                continue;
            }
            let take = room.min(buffer.len() - written);
            self.sink.write_all(&buffer[written..written + take])?;
            self.on_line += take;
            written += take;
        }
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.sink.flush()
    }
}

/// Écrit des octets dans un puits, en traduisant l'erreur.
///
/// L'étape est `Data` : une erreur d'écriture pendant l'assemblage arrive **avant** tout octet
/// réseau, donc elle ne doit surtout pas porter `Committing` — ce serait un doute inventé.
fn put<W: Write>(sink: &mut W, bytes: &[u8]) -> Result<()> {
    sink.write_all(bytes).map_err(|source| Error::Network {
        stage: crate::Stage::Data,
        source,
    })
}

/// Une liste d'adresses pour un en-tête.
fn list(addresses: &[Address]) -> String {
    addresses
        .iter()
        .map(Address::to_header)
        .collect::<Vec<_>>()
        .join(", ")
}

/// Une frontière `multipart`, imprévisible.
fn boundary() -> Result<String> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).map_err(|_| Error::Unsendable {
        reason: "le système ne fournit pas d'aléa pour une frontière MIME".to_owned(),
    })?;
    let hex: String = bytes.iter().map(|it| format!("{it:02x}")).collect();
    Ok(format!("----mailcore-{hex}"))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::{Address, encoded_word, fold, message_id};
    use crate::error::Error;

    #[test]
    fn a_plain_address_is_accepted() {
        let it = Address::parse("jean@exemple.fr", None).unwrap();
        assert_eq!(it.addr(), "jean@exemple.fr");
        assert_eq!(it.to_header(), "jean@exemple.fr");
    }

    #[test]
    fn a_display_name_is_kept_and_bracketed() {
        let it = Address::parse("jean@exemple.fr", Some("Jean Dupont")).unwrap();
        assert_eq!(it.to_header(), "Jean Dupont <jean@exemple.fr>");
    }

    #[test]
    fn a_newline_in_an_address_is_refused() {
        // **L'injection.** Une adresse qui porte un retour à la ligne écrit une deuxième
        // commande SMTP, ou un deuxième en-tête.
        for bad in [
            "jean@exemple.fr\r\nBcc: victime@ailleurs.fr",
            "jean@exemple.fr\nRCPT TO:<victime@ailleurs.fr>",
            "jean\r@exemple.fr",
        ] {
            assert!(
                matches!(Address::parse(bad, None), Err(Error::Unsendable { .. })),
                "accepté : {bad:?}"
            );
        }
    }

    #[test]
    fn a_newline_in_a_display_name_is_refused() {
        // Le nom affiché est du texte fourni par l'utilisateur, et il va dans un en-tête.
        assert!(matches!(
            Address::parse("jean@exemple.fr", Some("Jean\r\nBcc: victime@ailleurs.fr")),
            Err(Error::Unsendable { .. })
        ));
    }

    #[test]
    fn an_address_without_a_usable_domain_is_refused() {
        for bad in ["jean", "jean@", "@exemple.fr", "jean@localhost", ""] {
            assert!(
                matches!(Address::parse(bad, None), Err(Error::Unsendable { .. })),
                "accepté : {bad:?}"
            );
        }
    }

    #[test]
    fn a_separator_in_an_address_is_refused() {
        // Une virgule ou un chevron change le sens de la liste où l'adresse sera écrite.
        for bad in [
            "jean@exemple.fr, victime@ailleurs.fr",
            "<jean@exemple.fr>",
            "jean @exemple.fr",
            "jean@exemple.fr;victime@ailleurs.fr",
        ] {
            assert!(
                matches!(Address::parse(bad, None), Err(Error::Unsendable { .. })),
                "accepté : {bad:?}"
            );
        }
    }

    #[test]
    fn a_nul_byte_is_refused() {
        assert!(matches!(
            Address::parse("jean\0@exemple.fr", None),
            Err(Error::Unsendable { .. })
        ));
    }

    #[test]
    fn the_last_at_separates_the_domain() {
        // **Pourquoi `rsplit_once` et non `split_once`.** Une partie locale citée peut contenir
        // un `@` — la RFC 5322 §3.4.1 l'autorise — et prendre le *premier* séparateur donnerait
        // ici le domaine `b"@exemple.fr`, qui n'en est pas un.
        let quoted = Address::parse("\"a@b\"@exemple.fr", None).unwrap();
        assert_eq!(quoted.addr(), "\"a@b\"@exemple.fr");

        let plain = Address::parse("a.b+c@sous.exemple.fr", None).unwrap();
        assert_eq!(plain.addr(), "a.b+c@sous.exemple.fr");
    }

    #[test]
    fn an_ascii_word_is_left_readable() {
        // Un en-tête débogable à l'œil vaut mieux qu'un en-tête tout encodé.
        assert_eq!(encoded_word("Facture septembre"), "Facture septembre");
    }

    #[test]
    fn a_non_ascii_word_is_encoded_as_an_encoded_word() {
        let out = encoded_word("Réunion");
        assert!(out.starts_with("=?UTF-8?B?"), "{out}");
        assert!(out.ends_with("?="), "{out}");
        use base64::Engine as _;
        let inner = out.trim_start_matches("=?UTF-8?B?").trim_end_matches("?=");
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(inner)
            .unwrap();
        assert_eq!(String::from_utf8(decoded).unwrap(), "Réunion");
    }

    #[test]
    fn a_special_ascii_name_is_quoted_not_encoded() {
        // `Dupont, Jean <…>` sans guillemets serait lu comme deux adresses.
        let it = Address::parse("jean@exemple.fr", Some("Dupont, Jean")).unwrap();
        assert_eq!(it.to_header(), "\"Dupont, Jean\" <jean@exemple.fr>");
    }

    #[test]
    fn a_quote_inside_a_quoted_name_is_escaped() {
        let it = Address::parse("jean@exemple.fr", Some("Jean \"Le Grand\", Dupont")).unwrap();
        let header = it.to_header();
        assert!(header.starts_with('"'), "{header}");
        assert!(
            header.contains("\\\""),
            "le guillemet n'est pas déguisé : {header}"
        );
    }

    #[test]
    fn a_name_with_an_arabic_subject_survives() {
        // Le corpus en a — mesuré à la phase 1. Ne rien encoder ferait arriver du charabia.
        let out = encoded_word("فاتورة");
        assert!(out.starts_with("=?UTF-8?B?"));
    }

    #[test]
    fn a_long_header_is_folded_on_spaces() {
        let header = format!("Subject: {}", "mot ".repeat(40));
        let folded = fold(&header);
        assert!(folded.contains("\r\n "), "aucun repli");
        for line in folded.split("\r\n") {
            assert!(
                line.trim_end().len() <= super::FOLD_AT,
                "ligne trop longue : {} — {line}",
                line.len()
            );
        }
    }

    #[test]
    fn a_word_longer_than_the_limit_is_not_cut() {
        // Une URL ou un `Message-ID` coupé est faux, et la RFC autorise 998 caractères.
        let long = "x".repeat(200);
        let folded = fold(&format!("References: <{long}@exemple.fr>"));
        assert!(folded.contains(&long), "le mot a été coupé");
    }

    #[test]
    fn a_short_header_is_left_alone() {
        assert_eq!(fold("Subject: court"), "Subject: court");
    }

    #[test]
    fn a_message_id_is_unique_and_carries_only_the_domain() {
        let sender = Address::parse("jean@exemple.fr", None).unwrap();
        let first = message_id(&sender).unwrap();
        let second = message_id(&sender).unwrap();

        assert_ne!(first, second, "deux identifiants identiques");
        assert!(first.starts_with('<') && first.ends_with('>'), "{first}");
        assert!(first.ends_with("@exemple.fr>"), "{first}");
        // **La partie locale n'y est pas** : un `Message-ID` circule dans tous les fils où le
        // message passe, y compris chez des tiers.
        assert!(!first.contains("jean"), "la partie locale a fui : {first}");
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod draft_tests {
    use super::{Address, Draft, reply_subject, reply_threading, rfc5322_date};
    use crate::error::Error;

    fn who(addr: &str) -> Address {
        Address::parse(addr, None).unwrap()
    }

    fn draft() -> Draft {
        Draft::new(
            who("marie@exemple.fr"),
            vec![who("jean@ailleurs.fr")],
            "Facture",
            "Bonjour.\r\n",
        )
    }

    fn assembled(it: &Draft) -> String {
        String::from_utf8(it.assemble(1_788_000_000).unwrap()).unwrap()
    }

    /// Vrai si le **bloc d'en-têtes** porte cet en-tête, quelle que soit sa casse.
    ///
    /// Le bloc s'arrête à la première ligne vide : ce qui vient après est le corps, et un corps
    /// qui contient le mot « bcc » est du texte ordinaire, pas une fuite. Les lignes de
    /// continuation — celles qui commencent par un blanc, RFC 5322 §2.2.3 — ne sont pas des
    /// noms d'en-tête et sont donc sautées.
    fn has_header(message: &str, name: &str) -> bool {
        let headers = message.split("\r\n\r\n").next().unwrap_or_default();
        headers
            .split("\r\n")
            .filter(|line| !line.starts_with([' ', '\t']))
            .filter_map(|line| line.split_once(':'))
            .any(|(found, _)| found.trim().eq_ignore_ascii_case(name))
    }

    #[test]
    fn a_minimal_message_has_the_headers_the_rfc_requires() {
        let out = assembled(&draft());
        assert!(out.contains("From: marie@exemple.fr\r\n"), "{out}");
        assert!(out.contains("To: jean@ailleurs.fr\r\n"), "{out}");
        assert!(out.contains("Subject: Facture\r\n"), "{out}");
        assert!(out.contains("Date: "), "{out}");
        assert!(out.contains("Message-ID: <"), "{out}");
        assert!(out.contains("MIME-Version: 1.0\r\n"), "{out}");
        // Le corps est séparé des en-têtes par une ligne vide, et une seule.
        assert!(out.contains("\r\n\r\nBonjour.\r\n"), "{out}");
    }

    #[test]
    fn a_blind_copy_never_reaches_the_headers() {
        // **La règle de correction de cette fonction.** Un en-tête `Bcc:` enverrait la liste des
        // destinataires cachés à tous les destinataires, ce qui est l'inverse exact de ce que
        // l'utilisateur a demandé.
        let mut it = draft();
        it.bcc.push(who("discret@ailleurs.fr"));
        let out = assembled(&it);

        // **Le contrôle porte sur les en-têtes, ligne par ligne, et pas sur le message
        // entier.** Chercher « bcc » n'importe où était un test *instable* : le `Message-ID`
        // est seize octets d'aléa rendus en hexadécimal, `b` et `c` sont des chiffres
        // hexadécimaux, et un identifiant sur cent porte « bcc » par hasard. Il échouait donc
        // une fois sur cent cinquante environ — sur le test qui garde la règle la plus
        // importante de tout l'envoi, ce qui est le pire endroit pour apprendre à relancer.
        assert!(
            !has_header(&out, "bcc"),
            "un en-tête Bcc a été écrit : {out}"
        );
        // Le contrôle négatif, sans lequel le précédent ne prouverait rien : le prédicat doit
        // voir un vrai `Bcc:`, y compris replié sur la ligne suivante comme la RFC 5322 §2.2.3
        // l'autorise.
        assert!(has_header(
            "To: jean@ailleurs.fr\r\nBcc: discret@ailleurs.fr\r\n\r\ncorps",
            "bcc"
        ));
        assert!(has_header(
            "To: jean@ailleurs.fr\r\nbcc:\r\n discret@ailleurs.fr\r\n\r\ncorps",
            "bcc"
        ));
        // Et l'adresse cachée n'est nulle part, corps compris. Celle-ci se cherche partout sans
        // risque d'instabilité : aucun aléa hexadécimal ne produit une adresse.
        assert!(
            !out.contains("discret@ailleurs.fr"),
            "le destinataire caché apparaît dans le message : {out}"
        );
        // Et il reçoit quand même : il est dans l'enveloppe.
        let envelope: Vec<&str> = it.envelope_recipients().iter().map(|a| a.addr()).collect();
        assert!(envelope.contains(&"discret@ailleurs.fr"), "{envelope:?}");
    }

    #[test]
    fn the_bcc_check_is_not_fooled_by_a_random_message_id() {
        // **Pourquoi le test précédent ne cherche pas « bcc » dans le message entier.** Un
        // `Message-ID` est seize octets d'aléa en hexadécimal ; `b` et `c` en sont des chiffres,
        // donc « bcc » y apparaît par hasard — environ sept identifiants sur mille, ce qui a
        // fait échouer le test sur du code juste.
        //
        // Ce message n'a pas d'en-tête `Bcc`. L'ancien contrôle le condamnait quand même ; le
        // nouveau voit qu'il n'y a pas d'en-tête de ce nom.
        let unlucky = "From: marie@exemple.fr\r\n\
                       To: jean@ailleurs.fr\r\n\
                       Message-ID: <9f3bcc41d2e07a5b8c6104fe2a3d97bb@exemple.fr>\r\n\
                       \r\n\
                       Bonjour.\r\n";
        assert!(
            unlucky.to_ascii_lowercase().contains("bcc"),
            "l'exemple ne reproduit plus le hasard qu'il documente"
        );
        assert!(!has_header(unlucky, "bcc"));

        // Et un corps qui *parle* de Bcc n'est pas une fuite non plus : le bloc d'en-têtes
        // s'arrête à la première ligne vide.
        let discussing = "To: jean@ailleurs.fr\r\n\r\nJe t'ai mis en Bcc: exprès.\r\n";
        assert!(!has_header(discussing, "bcc"));
    }

    #[test]
    fn a_visible_copy_is_in_the_headers_and_in_the_envelope() {
        let mut it = draft();
        it.cc.push(who("temoin@ailleurs.fr"));
        let out = assembled(&it);

        assert!(out.contains("Cc: temoin@ailleurs.fr\r\n"), "{out}");
        assert_eq!(it.envelope_recipients().len(), 2);
    }

    #[test]
    fn a_message_without_a_recipient_is_refused() {
        let mut it = draft();
        it.to.clear();
        assert!(matches!(it.assemble(0), Err(Error::Unsendable { .. })));
    }

    #[test]
    fn a_reply_stays_in_its_thread() {
        // Sans `In-Reply-To` ni `References`, la réponse ouvre un fil neuf chez le destinataire.
        let mut it = draft();
        it.in_reply_to = Some("<parent@exemple.fr>".to_owned());
        it.references = vec![
            "<aieul@exemple.fr>".to_owned(),
            "<parent@exemple.fr>".to_owned(),
        ];
        let out = assembled(&it);

        assert!(
            out.contains("In-Reply-To: <parent@exemple.fr>\r\n"),
            "{out}"
        );
        assert!(
            out.contains("References: <aieul@exemple.fr> <parent@exemple.fr>\r\n"),
            "{out}"
        );
    }

    #[test]
    fn a_non_ascii_subject_is_encoded() {
        let mut it = draft();
        it.subject = "Réunion de septembre".to_owned();
        let out = assembled(&it);
        assert!(out.contains("Subject: =?UTF-8?B?"), "{out}");
        assert!(!out.contains("Réunion"), "le sujet est parti brut : {out}");
    }

    #[test]
    fn an_html_body_keeps_a_text_alternative() {
        // Envoyer du HTML seul, c'est exiger du destinataire ce que `docs/PRIVACY.md` refuse à
        // notre propre coquille : rendre du HTML par défaut.
        let mut it = draft();
        it.html = Some("<p>Bonjour.</p>".to_owned());
        let out = assembled(&it);

        assert!(out.contains("multipart/alternative"), "{out}");
        assert!(out.contains("Content-Type: text/plain"), "{out}");
        assert!(out.contains("Content-Type: text/html"), "{out}");
        // La RFC 2046 §5.1.4 : la **dernière** partie est la préférée. Le HTML vient après.
        let plain_at = out.find("text/plain").unwrap();
        let html_at = out.find("text/html").unwrap();
        assert!(plain_at < html_at, "le HTML doit venir en dernier");
    }

    #[test]
    fn the_multipart_ends_with_its_closing_boundary() {
        // Un `multipart` sans frontière fermante est un message tronqué pour la plupart des
        // lecteurs, et la dernière partie disparaît.
        let mut it = draft();
        it.html = Some("<p>x</p>".to_owned());
        let out = assembled(&it);

        let boundary = out
            .split("boundary=\"")
            .nth(1)
            .and_then(|it| it.split('"').next())
            .unwrap()
            .to_owned();
        assert!(out.ends_with(&format!("--{boundary}--\r\n")), "{out}");
        assert_eq!(
            out.matches(&format!("--{boundary}\r\n")).count(),
            2,
            "il faut exactement deux ouvertures de partie"
        );
    }

    #[test]
    fn two_messages_never_share_a_boundary() {
        // Une frontière fixe ou devinable qui apparaîtrait dans le texte couperait le message au
        // mauvais endroit — et un expéditeur qui choisit son texte pourrait le provoquer.
        let mut it = draft();
        it.html = Some("<p>x</p>".to_owned());
        let first = assembled(&it);
        let second = assembled(&it);
        let of = |out: &str| {
            out.split("boundary=\"")
                .nth(1)
                .and_then(|it| it.split('"').next())
                .unwrap()
                .to_owned()
        };
        assert_ne!(of(&first), of(&second));
    }

    #[test]
    fn the_date_is_the_one_the_rfc_asks_for() {
        // 1970-01-01 00:00:00 UTC était un jeudi.
        assert_eq!(rfc5322_date(0), "Thu, 1 Jan 1970 00:00:00 +0000");
        // Un instant vérifié à la main : 1 788 000 000 = 20 694 jours pleins + 38 400 s, et le
        // 29 août 2026 est un samedi — onze jours avant le mercredi 9 septembre. L'attendu de la
        // première version était une supposition, et c'est le code qui avait raison.
        assert_eq!(
            rfc5322_date(1_788_000_000),
            "Sat, 29 Aug 2026 10:40:00 +0000"
        );
    }

    #[test]
    fn a_leap_day_is_not_off_by_one() {
        // L'algorithme décale l'origine à mars pour que le 29 février tombe en fin d'année et
        // n'ait pas de cas particulier. Un décalage d'un jour ne se verrait que là.
        assert!(rfc5322_date(1_709_164_800).starts_with("Thu, 29 Feb 2024"));
    }

    #[test]
    fn a_date_before_the_epoch_does_not_shift_by_a_day() {
        // La division entière de Rust tronque vers zéro, ce qui décalerait d'un jour dans le
        // passé. `div_euclid` est ce qui l'évite, et ce test est ce qui le retient.
        assert_eq!(rfc5322_date(-1), "Wed, 31 Dec 1969 23:59:59 +0000");
    }

    #[test]
    fn the_offset_never_reveals_the_local_timezone() {
        // `Date: … +0200` dit à tout destinataire dans quel pays on se trouve. L'instant est le
        // même en UTC ; seule l'indiscrétion disparaît.
        for at in [0, 1_788_000_000, -86_400] {
            assert!(rfc5322_date(at).ends_with(" +0000"), "{at}");
        }
    }

    /// Une signature riche : un nom en gras, une puce, un lien.
    fn signature() -> mailhtml::rich::Document {
        let text = "Éloïse Durand\ndirectrice\nexemple.fr";
        let name = "Éloïse Durand";
        let site = "exemple.fr";
        let mut it = mailhtml::rich::Document::plain(text);
        // Les bornes sont calculées : « Éloïse Durand » fait quinze octets pour treize
        // caractères, et un nombre écrit à la main désignerait autre chose.
        it.apply(
            0,
            name.len(),
            &mailhtml::rich::Style {
                bold: true,
                ..mailhtml::rich::Style::default()
            },
        );
        let at = text.find(site).unwrap();
        it.apply(
            at,
            at + site.len(),
            &mailhtml::rich::Style {
                link: Some("https://exemple.fr".to_owned()),
                ..mailhtml::rich::Style::default()
            },
        );
        it.toggle_bullet(1);
        it
    }

    #[test]
    fn signing_adds_the_signature_to_both_parts_and_they_say_the_same_thing() {
        // **La règle de correction de `multipart/alternative`** : les deux parties portent le
        // même message. Une signature ajoutée à l'une et pas à l'autre est la façon la plus
        // simple de faire lire deux choses différentes à deux destinataires.
        let mut it = draft();
        it.text = "Bonjour,\nvoici le rapport.\r\n".to_owned();
        it.sign_with(&signature());

        assert_eq!(
            it.text,
            "Bonjour,\nvoici le rapport.\n\nÉloïse Durand\n- directrice\n\
             exemple.fr <https://exemple.fr>"
        );
        let html = it.html.clone().unwrap();
        assert_eq!(
            html,
            "<p>Bonjour,</p><p>voici le rapport.</p><p><br></p><p><b>Éloïse Durand</b></p>\
             <ul><li>directrice</li></ul><p><a href=\"https://exemple.fr\">exemple.fr</a></p>"
        );

        // Et le message assemblé porte bien les deux parties.
        let out = assembled(&it);
        assert!(out.contains("multipart/alternative"), "{out}");
        assert!(out.contains("Éloïse") || out.contains("=?utf-8?"), "{out}");
    }

    #[test]
    fn a_plain_signature_leaves_the_message_with_a_single_text_part() {
        // **Pas de partie HTML pour rien.** Deux parties identiques n'apportent rien et donnent
        // un lecteur de plus à qui faire confiance pour choisir.
        let mut it = draft();
        it.text = "Bonjour.".to_owned();
        it.sign_with(&mailhtml::rich::Document::plain("Cordialement,\nÉloïse"));

        assert_eq!(it.text, "Bonjour.\n\nCordialement,\nÉloïse");
        assert_eq!(
            it.html, None,
            "une partie HTML est partie sans rien dire de plus"
        );
        let out = assembled(&it);
        assert!(!out.contains("multipart/alternative"), "{out}");
    }

    #[test]
    fn an_empty_signature_changes_nothing() {
        // Un compte sans signature — ou dont la signature n'est que des blancs — ne doit pas
        // ajouter deux lignes blanches en fin de message.
        let mut it = draft();
        let before = (it.text.clone(), it.html.clone());
        it.sign_with(&mailhtml::rich::Document::plain(""));
        assert_eq!((it.text.clone(), it.html.clone()), before);
        it.sign_with(&mailhtml::rich::Document::plain("  \n\t"));
        assert_eq!((it.text, it.html), before);
    }

    #[test]
    fn a_signature_on_an_empty_body_does_not_start_with_blank_lines() {
        // Le cas d'un message qu'on n'a pas encore écrit : la signature ne doit pas être poussée
        // en bas d'une page blanche.
        let mut it = draft();
        it.text = String::new();
        it.sign_with(&signature());
        assert!(it.text.starts_with("Éloïse Durand"), "{:?}", it.text);
    }

    #[test]
    fn a_body_that_looks_like_markup_is_escaped_by_the_signing() {
        // Le corps traverse le même chemin d'échappement que la signature. Sans lui, un `<`
        // tapé dans le corps casserait le HTML **chez le destinataire**, où personne ne peut
        // plus le corriger.
        let mut it = draft();
        it.text = "3 < 5 & \"vrai\"".to_owned();
        it.sign_with(&signature());
        let html = it.html.unwrap();
        assert!(html.contains("3 &lt; 5 &amp; &quot;vrai&quot;"), "{html}");
    }

    #[test]
    fn signing_twice_would_double_the_signature_and_that_is_why_the_flag_exists() {
        // **Le mode de panne que le drapeau de `SendParams` écarte.** Rien ici n'empêche de
        // signer deux fois : c'est l'appelant qui décide, et il y a un seul appelant par moteur.
        // Ce test fige la conséquence, pour que personne n'ajoute un `sign_with` « au cas où »
        // dans un deuxième endroit du chemin d'envoi.
        let mut it = draft();
        it.text = "Bonjour.".to_owned();
        it.sign_with(&signature());
        it.sign_with(&signature());
        assert_eq!(it.text.matches("Éloïse Durand").count(), 2, "{:?}", it.text);
    }

    #[test]
    fn a_subject_is_never_quoted_because_it_is_an_unstructured_field() {
        // **Le défaut que le banc du critère 1 a trouvé, sur 1 164 messages du corpus.** Une
        // seule fonction servait les noms d'affichage et les sujets : tout sujet contenant un
        // deux-points partait entre guillemets, donc **toute réponse**, puisqu'elle commence par
        // « Re: ». Le destinataire lisait `"Re: Facture"`, guillemets compris.
        //
        // RFC 5322 §3.6.5 : `Subject` est un champ non structuré. Aucun caractère n'y est
        // spécial, donc rien n'y est à citer.
        for subject in [
            "Re: Facture",
            "Tr: Facture",
            "Un sujet, avec une virgule",
            "a@b : deux points et une arobase",
            "guillemet \" au milieu",
            "<chevrons>",
        ] {
            let mut it = draft();
            it.subject = subject.to_owned();
            let out = assembled(&it);
            assert!(
                out.contains(&format!("Subject: {subject}\r\n")),
                "sujet transformé : {out}"
            );
        }
    }

    #[test]
    fn a_display_name_is_still_quoted_when_it_has_to_be() {
        // **Le contrôle inverse, et il est indispensable.** Un nom d'affichage est un `phrase`
        // dans un champ **structuré** : sans guillemets, `Durand, Éloïse <e@x.fr>` serait lu
        // comme deux adresses, dont une invalide. La correction du sujet ne devait pas emporter
        // cette règle avec elle.
        let name = Address::parse("e@x.fr", Some("Durand, Éloïse")).unwrap();
        // Non ASCII : il part en mot encodé, ce qui ne contient ni virgule ni blanc.
        assert!(
            name.to_header().contains("=?UTF-8?B?"),
            "{}",
            name.to_header()
        );

        let ascii = Address::parse("e@x.fr", Some("Durand, Eloise")).unwrap();
        assert_eq!(
            ascii.to_header(),
            "\"Durand, Eloise\" <e@x.fr>",
            "un nom ASCII à virgule doit être cité"
        );
        // Et un guillemet dans le nom est échappé, pas laissé tel quel.
        let quoted = Address::parse("e@x.fr", Some("Jean \"Le Grand\"")).unwrap();
        assert_eq!(quoted.to_header(), "\"Jean \\\"Le Grand\\\"\" <e@x.fr>");
    }

    #[test]
    fn a_reply_subject_round_trips_through_a_reader() {
        // Le contrôle de bout en bout : assembler, relire avec un vrai analyseur, comparer. Un
        // sujet qui ne se relit pas identique est un sujet qui arrivera de travers.
        for subject in [
            "Re: Facture",
            "Re: Réunion été",
            "Re: فاتورة",
            "Re: Une ligne de sujet assez longue pour dépasser les soixante-dix-huit caractères \
             de la RFC 5322",
        ] {
            let mut it = draft();
            it.subject = subject.to_owned();
            let bytes = it.assemble(1_789_041_600).unwrap();
            let parsed = mail_parser::MessageParser::default().parse(&bytes).unwrap();
            let read = parsed.subject().unwrap_or_default();
            // Blancs repliés : la RFC 5322 §2.2.3 autorise à couper un en-tête long sur un
            // blanc, et un lecteur peut restituer le pli comme une espace.
            let fold = |value: &str| value.split_whitespace().collect::<Vec<_>>().join(" ");
            assert_eq!(fold(read), fold(subject), "sujet abîmé : {read:?}");
        }
    }

    #[test]
    fn a_message_id_without_its_brackets_is_put_back_in_shape() {
        // **Le second défaut que le banc du critère 1 a trouvé, sur 415 fils.** Les
        // identifiants arrivent des deux formes selon leur source : un en-tête brut les porte
        // avec chevrons, `mail-parser` les rend sans. La coquille lit l'API, qui lit
        // `mail-parser` : ses réponses partaient donc avec `In-Reply-To: abc@x.fr`, que la RFC
        // 5322 §3.6.4 n'autorise pas et qu'un client qui regroupe sur cet en-tête ne rattache
        // pas.
        let mut it = draft();
        it.in_reply_to = Some("sans-chevrons@exemple.fr".to_owned());
        it.references = vec![
            "premier@exemple.fr".to_owned(),
            "<deja-en-forme@exemple.fr>".to_owned(),
        ];
        let out = assembled(&it);

        assert!(
            out.contains("In-Reply-To: <sans-chevrons@exemple.fr>\r\n"),
            "chevrons non remis : {out}"
        );
        assert!(
            out.contains("References: <premier@exemple.fr> <deja-en-forme@exemple.fr>\r\n"),
            "chaîne mal écrite : {out}"
        );
        // Et jamais doublés sur celui qui les avait déjà.
        assert!(!out.contains("<<"), "{out}");
    }

    #[test]
    fn an_identifier_that_cannot_be_written_is_dropped_rather_than_corrupting_the_header() {
        // Un `msg-id` ne contient pas de blanc — RFC 5322 §3.6.4. L'écrire quand même couperait
        // l'en-tête en deux jetons, dont le second serait relu comme un identifiant inventé :
        // le fil du destinataire pointerait vers un message qui n'existe pas. Le même refus que
        // celui d'une adresse à retour à la ligne.
        let mut it = draft();
        it.in_reply_to = Some("avec un blanc@exemple.fr".to_owned());
        it.references = vec![
            "bon@exemple.fr".to_owned(),
            "mauvais avec blanc@exemple.fr".to_owned(),
            String::new(),
        ];
        let out = assembled(&it);

        assert!(
            !out.contains("In-Reply-To"),
            "un identifiant fautif est parti : {out}"
        );
        assert!(
            out.contains("References: <bon@exemple.fr>\r\n"),
            "la chaîne devait garder le bon et jeter le reste : {out}"
        );
    }

    #[test]
    fn the_thread_rule_appends_the_parent_without_repeating_it() {
        // La règle de fil elle-même, qui vivait dans la coquille et n'était donc vérifiable
        // nulle part ailleurs.
        let chain = vec!["<a@x.fr>".to_owned(), "<b@x.fr>".to_owned()];
        let it = reply_threading(Some("<c@x.fr>"), &chain);
        assert_eq!(it.in_reply_to.as_deref(), Some("<c@x.fr>"));
        assert_eq!(it.references, vec!["<a@x.fr>", "<b@x.fr>", "<c@x.fr>"]);

        // Un parent déjà présent dans la chaîne — certains clients y mettent leur propre
        // identifiant — n'est pas ajouté deux fois.
        let it = reply_threading(Some("<b@x.fr>"), &chain);
        assert_eq!(it.references, vec!["<a@x.fr>", "<b@x.fr>"]);

        // Un parent sans `Message-ID` ne produit pas d'en-tête inventé, et la chaîne survit.
        let it = reply_threading(None, &chain);
        assert_eq!(it.in_reply_to, None);
        assert_eq!(it.references, chain);
    }

    #[test]
    fn a_reply_subject_gains_exactly_one_prefix() {
        assert_eq!(reply_subject("Facture"), "Re: Facture");
        assert_eq!(reply_subject("Re: Facture"), "Re: Facture");
        assert_eq!(reply_subject("RE: Facture"), "RE: Facture");
        assert_eq!(reply_subject("re : Facture"), "re : Facture");
        // Un sujet qui portait déjà deux préfixes n'en gagne pas un troisième : le corpus en a
        // 29, écrits par des clients qui ne vérifient pas.
        assert_eq!(reply_subject("Re: Re: Facture"), "Re: Re: Facture");
        // Un sujet vide donne « Re: » seul, ce qu'attend un lecteur devant une réponse à un
        // message sans sujet.
        assert_eq!(reply_subject("   "), "Re: ");
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod attachment_tests {
    use super::{Address, Attachment, BASE64_LINE, Draft, Wrapping};
    use mailcore::BlobHash;
    use std::io::{Read, Write};

    fn who(addr: &str) -> Address {
        Address::parse(addr, None).unwrap()
    }

    fn draft() -> Draft {
        Draft::new(
            who("marie@exemple.fr"),
            vec![who("jean@ailleurs.fr")],
            "Facture",
            "Bonjour.\r\n",
        )
    }

    /// Écrit le message avec un contenu de pièce jointe fourni en mémoire.
    fn written(it: &Draft, content: &[u8]) -> String {
        let mut out = Vec::new();
        let owned = content.to_vec();
        it.write_to(&mut out, 1_788_000_000, &mut |_| {
            Ok(Box::new(std::io::Cursor::new(owned.clone())) as Box<dyn Read>)
        })
        .unwrap();
        String::from_utf8(out).unwrap()
    }

    fn attached(name: &str, size: u64) -> Attachment {
        Attachment {
            filename: name.to_owned(),
            mime: "application/pdf".to_owned(),
            blob: BlobHash::from_bytes([3_u8; 32]),
            size,
        }
    }

    #[test]
    fn a_message_without_attachments_is_unchanged_by_the_streaming_path() {
        // Le contrôle de non-régression : `assemble` passe maintenant par `write_to`. Un
        // message sans pièce jointe doit sortir exactement comme avant, sans `multipart/mixed`.
        let it = draft();
        let out = written(&it, b"");
        assert!(
            out.contains("Content-Type: text/plain; charset=utf-8"),
            "{out}"
        );
        assert!(!out.contains("multipart/mixed"), "{out}");
        assert!(out.ends_with("Bonjour.\r\n"), "{out}");
    }

    #[test]
    fn an_attachment_wraps_the_body_in_a_mixed_part() {
        let mut it = draft();
        it.attachments.push(attached("facture.pdf", 9));
        let out = written(&it, b"123456789");

        assert!(out.contains("Content-Type: multipart/mixed;"), "{out}");
        assert!(out.contains("Content-Disposition: attachment;"), "{out}");
        assert!(
            out.contains("Content-Transfer-Encoding: base64"),
            "la pièce n'est pas encodée : {out}"
        );
        // Le corps texte est **dedans**, pas remplacé.
        assert!(out.contains("Bonjour."), "{out}");
        // `123456789` en base64.
        assert!(out.contains("MTIzNDU2Nzg5"), "{out}");
    }

    #[test]
    fn html_and_attachments_nest_instead_of_flattening() {
        // **Le défaut MIME le plus courant.** Mettre le texte, le HTML et les pièces au même
        // niveau fait afficher le texte *et* le HTML l'un après l'autre chez tous les lecteurs.
        // Le `mixed` doit envelopper l'`alternative`.
        let mut it = draft();
        it.html = Some("<p>Bonjour.</p>".to_owned());
        it.attachments.push(attached("facture.pdf", 3));
        let out = written(&it, b"abc");

        let mixed = out.find("multipart/mixed").expect("pas de mixed");
        let alternative = out
            .find("multipart/alternative")
            .expect("pas d'alternative");
        assert!(
            mixed < alternative,
            "l'alternative n'est pas dans le mixed : {out}"
        );
        assert!(out.contains("text/html"), "{out}");
    }

    #[test]
    fn the_two_boundaries_are_different() {
        // Une frontière partagée entre le `mixed` et l'`alternative` ferait fermer les deux
        // niveaux d'un coup : le lecteur perdrait les pièces jointes.
        let mut it = draft();
        it.html = Some("<p>x</p>".to_owned());
        it.attachments.push(attached("f.pdf", 1));
        let out = written(&it, b"a");

        let boundaries: Vec<&str> = out
            .lines()
            .filter_map(|line| line.strip_prefix("Content-Type: multipart/"))
            .filter_map(|line| line.split("boundary=\"").nth(1))
            .filter_map(|line| line.split('"').next())
            .collect();
        assert_eq!(boundaries.len(), 2, "{boundaries:?}");
        assert_ne!(boundaries[0], boundaries[1], "une frontière partagée");
    }

    #[test]
    fn an_accented_filename_is_encoded_not_copied() {
        // Un nom en octets bruts dans un en-tête est hors RFC 5322. Et un nom qui porterait un
        // guillemet ou un retour à la ligne casserait la structure MIME, ou y injecterait un
        // en-tête.
        let mut it = draft();
        it.attachments.push(attached("relevé \"été\".pdf", 1));
        let out = written(&it, b"a");

        let header = out
            .lines()
            .find(|line| line.starts_with("Content-Disposition:"))
            .expect("pas de disposition");
        assert!(header.is_ascii(), "{header}");
        assert!(!header.contains("relev"), "{header}");
        // Un seul niveau de guillemets : celui de `filename="…"`.
        assert_eq!(header.matches('"').count(), 2, "{header}");
    }

    #[test]
    fn a_filename_that_could_inject_a_header_cannot() {
        // Le même piège que sur les adresses, un cran plus loin dans le message.
        let mut it = draft();
        it.attachments.push(attached("a.pdf\r\nX-Injecte: oui", 1));
        let out = written(&it, b"a");

        assert!(
            !out.contains("X-Injecte"),
            "un en-tête a été injecté par un nom de fichier : {out}"
        );
    }

    #[test]
    fn base64_lines_never_exceed_the_rfc_limit() {
        // **La RFC 2045 §6.8 borne à 76 caractères, et la RFC 5322 §2.1.1 borne toute ligne à
        // 998 octets.** Un base64 non replié dépasse les deux dès 750 octets de pièce jointe :
        // des serveurs le refusent, d'autres le coupent où ils veulent — et couper au mauvais
        // endroit corrompt la pièce.
        let mut it = draft();
        let content = vec![0xAB_u8; 5_000];
        it.attachments
            .push(attached("gros.bin", content.len() as u64));
        let out = written(&it, &content);

        // Seules les lignes **du base64** sont bornées à 76. Un en-tête peut légitimement être
        // plus long — la RFC 5322 §2.1.1 le borne à 998, et une déclaration de frontière fait
        // dans les 87 caractères sans que ce soit un défaut. Confondre les deux limites était
        // le premier jet de ce test, et il échouait sur le `Content-Type`.
        let body = out
            .split("base64\r\n\r\n")
            .nth(1)
            .expect("pas de partie base64");
        let encoded = body.split("\r\n--").next().expect("pas de frontière après");
        assert!(
            encoded.lines().count() > 50,
            "trop peu de lignes pour juger"
        );
        for line in encoded.lines() {
            assert!(
                line.len() <= BASE64_LINE,
                "ligne de base64 de {} caractères : {line}",
                line.len()
            );
        }
    }

    #[test]
    fn the_wrapper_keeps_its_count_across_writes() {
        // **L'état facile à oublier.** L'encodeur base64 écrit par blocs de taille arbitraire ;
        // un replieur qui repartirait de zéro à chaque appel produirait des lignes de longueur
        // aléatoire — valides à l'œil, hors RFC en réalité.
        let mut out = Vec::new();
        {
            let mut wrapping = Wrapping::new(&mut out);
            // Trois écritures de 40, donc 120 caractères : la coupure doit tomber à 76.
            for _ in 0..3 {
                wrapping.write_all(&[b'x'; 40]).unwrap();
            }
            wrapping.finish().unwrap();
        }
        let text = String::from_utf8(out).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0].len(), BASE64_LINE, "{lines:?}");
        assert_eq!(lines[1].len(), 120 - BASE64_LINE, "{lines:?}");
    }

    #[test]
    fn the_last_line_is_terminated_so_the_boundary_is_recognised() {
        // Sans le `CRLF` final, la frontière `--…` se collerait à la fin du base64 et ne serait
        // pas reconnue : le lecteur verrait une seule partie, tronquée.
        let mut it = draft();
        it.attachments.push(attached("f.bin", 3));
        let out = written(&it, b"abc");
        assert!(out.ends_with("--\r\n"), "{out}");
        // La frontière fermante est seule sur sa ligne. Elle s'écrit `--` **plus** la
        // frontière — qui commence elle-même par des tirets — plus `--` : six tirets en tête.
        // Le premier jet de ce test comptait mal les tirets et ne trouvait donc rien.
        let closing = out.lines().last().expect("un message a au moins une ligne");
        assert!(closing.starts_with("--"), "{closing}");
        assert!(closing.ends_with("--"), "{closing}");
        assert!(closing.contains("mailcore-"), "{closing}");
    }

    #[test]
    fn an_empty_attachment_is_valid_and_terminates_cleanly() {
        // Un fichier vide existe. Sans le garde sur `on_line`, `finish` écrirait un `CRLF` de
        // trop — inoffensif ici, mais le cas doit être dit.
        let mut it = draft();
        it.attachments.push(attached("vide.txt", 0));
        let out = written(&it, b"");
        assert!(out.contains("filename="), "{out}");
        assert!(out.ends_with("--\r\n"), "{out}");
    }

    #[test]
    fn assembling_in_memory_refuses_an_attachment_rather_than_dropping_it() {
        // **Le refus qui évite un message amputé en silence.** `assemble` ne peut pas ouvrir un
        // blob — il n'a pas le magasin — donc un appelant qui l'utiliserait avec des pièces
        // jointes obtiendrait un message sans elles. Refuser est la seule réponse honnête.
        let mut it = draft();
        it.attachments.push(attached("f.pdf", 1));
        let outcome = it.assemble(1_788_000_000);
        assert!(
            matches!(outcome, Err(crate::Error::Unsendable { .. })),
            "un message avec pièce jointe a été assemblé sans elle"
        );
    }

    #[test]
    fn the_announced_size_accounts_for_base64_and_its_line_breaks() {
        // Annoncer la taille brute ferait passer un message sous une limite qu'il dépasse d'un
        // tiers, et le refus arriverait **après** le transfert — au pire moment, celui où le
        // doute commence.
        let it = attached("f.bin", 3_000);
        let encoded = it.encoded_len();
        assert!(encoded > 3_000, "{encoded}");
        // 3000 octets → 1000 groupes → 4000 caractères → 53 lignes → 4106.
        assert_eq!(encoded, 4_000 + 53 * 2);

        // Et la mesure doit correspondre à ce qui est réellement écrit.
        let mut draft = draft();
        draft.attachments.push(attached("f.bin", 3_000));
        let out = written(&draft, &vec![0x5A_u8; 3_000]);
        let body = out
            .split("base64\r\n\r\n")
            .nth(1)
            .expect("pas de partie base64");
        let actual = body
            .split("\r\n--")
            .next()
            .expect("pas de frontière après")
            .len();
        assert_eq!(
            u64::try_from(actual).unwrap(),
            encoded,
            "la taille annoncée ne correspond pas aux octets écrits"
        );
    }

    #[test]
    fn a_huge_size_never_overflows_the_announced_length() {
        let it = attached("f.bin", u64::MAX);
        assert!(it.encoded_len() > 0, "débordement en zéro ou négatif");
    }

    #[test]
    fn an_attachment_name_with_a_quote_does_not_break_its_header() {
        // Le paramètre est déjà entre guillemets dans le format : y ajouter une paire donnerait
        // `filename=""x""`, et un guillemet non échappé terminerait la valeur au milieu — ce
        // qui casse la structure MIME chez le destinataire.
        //
        // C'est le pendant du sujet non cité : une même fonction servait les deux emplois, et
        // les séparer demandait de vérifier les deux.
        let mut it = draft();
        it.attachments.push(attached(r#"rapport "final".pdf"#, 7));
        let out = written(&it, b"contenu");
        assert!(
            out.contains(r#"filename="rapport \"final\".pdf""#),
            "guillemet non échappé : {out}"
        );
        // Et un nom accentué part en mot encodé, qui ne contient ni guillemet ni blanc.
        let mut it = draft();
        it.attachments.push(attached("réglé.pdf", 7));
        let out = written(&it, b"contenu");
        assert!(out.contains("filename=\"=?UTF-8?B?"), "{out}");
    }
}
