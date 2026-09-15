//! Le type d'erreur de l'envoi.
//!
//! ## Pourquoi il distingue plus de cas que les autres
//!
//! Le critère 8 de `docs/PHASE-3.md` demande que l'utilisateur voie **quoi faire**, pas un code
//! numérique. « Erreur SMTP 552 » n'aide personne ; « le message dépasse la taille que ce
//! serveur accepte » dit quoi corriger.
//!
//! Et l'appelant, lui, a besoin de deux réponses de plus : **est-ce que je réessaie ?** et
//! **est-ce que le message est peut-être parti ?** La deuxième est propre à l'envoi et elle n'a
//! pas d'équivalent en lecture. C'est [`Error::stage`] qui y répond.

use crate::client::Stage;
use crate::reply::Reply;

/// Le résultat d'un envoi.
pub type Result<T> = std::result::Result<T, Error>;

/// Ce qui peut empêcher un message de partir — ou laisser un doute.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Le réseau, ou le socket.
    #[error("erreur réseau à l'étape {stage} : {source}")]
    Network {
        /// Où on en était. **C'est ce qui décide d'un réessai.**
        stage: Stage,
        /// La cause système.
        source: std::io::Error,
    },

    /// TLS n'a pas pu être établi ou vérifié.
    ///
    /// **Jamais contourné.** Un `SUBMIT` en clair transporte le mot de passe et le message.
    #[error("échec TLS : {reason}")]
    Tls {
        /// Ce que la couche TLS a refusé.
        reason: String,
    },

    /// Le nom d'hôte n'est pas un nom valide pour TLS.
    #[error("nom d'hôte invalide pour TLS : {host}")]
    InvalidHost {
        /// L'hôte refusé.
        host: String,
    },

    /// Le serveur a refusé l'authentification.
    ///
    /// **Pas réessayable sans intervention.** Insister sur un mot de passe refusé fait bloquer
    /// le compte chez le fournisseur.
    #[error("authentification refusée : {reason}")]
    AuthRefused {
        /// Ce que le serveur a répondu, sans le secret.
        reason: String,
    },

    /// Le serveur n'annonce pas ce dont on a besoin.
    ///
    /// Un serveur sans `STARTTLS` sur un port en clair, ou sans le mécanisme
    /// d'authentification que le compte déclare.
    #[error("le serveur n'annonce pas {capability}")]
    MissingCapability {
        /// Ce qui manque.
        capability: String,
    },

    /// Le serveur a refusé une commande, et le refus est **passager** — `4xx`.
    ///
    /// Quota momentané, serveur qui se recharge, limitation de débit. À réessayer plus tard.
    #[error("refus passager à l'étape {stage} ({code}) : {reason}")]
    Transient {
        /// Où on en était.
        stage: Stage,
        /// Le code rendu.
        code: u16,
        /// Le texte du serveur.
        reason: String,
    },

    /// Le serveur a refusé une commande **définitivement** — `5xx`.
    ///
    /// Réessayer ne changera rien. Le message doit revenir à l'utilisateur.
    #[error("refus définitif à l'étape {stage} ({code}) : {reason}")]
    Rejected {
        /// Où on en était.
        stage: Stage,
        /// Le code rendu.
        code: u16,
        /// Le texte du serveur.
        reason: String,
    },

    /// Le message dépasse la taille annoncée par le serveur.
    ///
    /// Séparé d'un `Rejected` parce que c'est le seul refus que l'utilisateur peut corriger
    /// **avant** d'envoyer, et parce qu'on peut le voir venir : `SIZE` est annoncé à l'`EHLO`.
    #[error("message de {size} octets, le serveur accepte au plus {limit}")]
    TooLarge {
        /// La taille du message.
        size: u64,
        /// Ce que le serveur annonce.
        limit: u64,
    },

    /// La réponse du serveur ne suit pas la RFC — y compris « il n'a pas répondu ».
    ///
    /// **L'étape en fait partie**, et ce n'était pas le cas au premier jet. Une coupure après le
    /// point final rend cette erreur-là — le serveur a fermé sans répondre — et sans l'étape,
    /// `may_have_been_sent` répondait faux : la file d'envoi aurait renvoyé le message.
    /// Trouvé par le test `a_server_that_cuts_after_the_final_dot_leaves_a_doubt`, qui est
    /// écrit pour ça.
    #[error("réponse hors spécification à l'étape {stage} : {reason}", stage = stage.map_or_else(|| "?".to_owned(), |it| it.to_string()))]
    Malformed {
        /// Où on en était, quand c'est connu. `None` quand l'analyseur est appelé hors dialogue.
        stage: Option<Stage>,
        /// Ce qui n'allait pas.
        reason: String,
    },

    /// Le message à envoyer n'est pas envoyable.
    ///
    /// Aucun destinataire, une adresse qui contient un retour à la ligne, un en-tête qui
    /// tenterait d'en injecter un autre. **Refusé avant d'ouvrir une connexion** : c'est un
    /// défaut de l'appelant, pas du serveur.
    #[error("message non envoyable : {reason}")]
    Unsendable {
        /// Ce qui manque, ou ce qui est en trop.
        reason: String,
    },
}

impl Error {
    /// L'étape où l'échec est survenu, quand elle est connue.
    ///
    /// ## Ce que l'appelant en fait, et pourquoi ça n'a pas d'équivalent en lecture
    ///
    /// Un échec **après** l'acceptation du `DATA` laisse un doute : le serveur a peut-être pris
    /// le message. Réessayer, c'est risquer un doublon chez le destinataire ; abandonner, c'est
    /// risquer de perdre le message. Le protocole ne permet pas de lever le doute.
    ///
    /// La file d'envoi tranche, et pour trancher elle a besoin de savoir **où** ça s'est
    /// arrêté. C'est tout ce que ce crate peut lui donner d'honnête.
    #[must_use]
    pub const fn stage(&self) -> Option<Stage> {
        match self {
            Self::Network { stage, .. }
            | Self::Transient { stage, .. }
            | Self::Rejected { stage, .. } => Some(*stage),
            Self::Malformed { stage, .. } => *stage,
            _ => None,
        }
    }

    /// Vrai si réessayer plus tard a une chance de marcher.
    ///
    /// **Ne dit rien du doublon.** Un échec réessayable après l'acceptation du `DATA` est
    /// réessayable *du point de vue du réseau* et dangereux du point de vue du destinataire.
    /// Les deux questions sont distinctes, et les mélanger dans un seul booléen est exactement
    /// comment on envoie deux fois.
    #[must_use]
    pub const fn retryable(&self) -> bool {
        match self {
            Self::Network { .. } | Self::Transient { .. } | Self::Tls { .. } => true,
            Self::AuthRefused { .. }
            | Self::Rejected { .. }
            | Self::TooLarge { .. }
            | Self::Malformed { .. }
            | Self::MissingCapability { .. }
            | Self::InvalidHost { .. }
            | Self::Unsendable { .. } => false,
        }
    }

    /// Vrai si le message a **peut-être** été accepté malgré l'erreur.
    ///
    /// C'est la question que la file d'envoi doit poser avant de réessayer. Elle est vraie dès
    /// que l'échec survient après l'envoi du point final du `DATA` : à partir de là, le serveur
    /// a le message, et son silence ne dit pas s'il l'a gardé.
    #[must_use]
    pub const fn may_have_been_sent(&self) -> bool {
        matches!(self.stage(), Some(Stage::Committing))
    }

    /// La famille du refus, quand cet échec en est un — **le critère 8**.
    ///
    /// ## Ce que ça donne à l'appelant
    ///
    /// Une phrase actionnable par [`Refusal::advice`], et une décision par
    /// [`Refusal::worth_retrying`]. Sans ça, la file d'envoi ne pouvait rendre que l'affichage
    /// de cette erreur : « refus définitif à l'étape RCPT (550) : … », qui nomme l'étape et le
    /// code mais ne dit pas quoi faire — et le critère demande précisément le contraire.
    ///
    /// `None` pour ce qui n'est pas un refus du serveur : une panne réseau, un échec TLS, un
    /// brouillon invalide. Ceux-là ont déjà leur propre message, et les ranger dans une famille
    /// de refus laisserait croire que le serveur a dit quelque chose.
    #[must_use]
    pub fn refusal(&self) -> Option<Refusal> {
        match self {
            Self::Transient { stage, code, .. } | Self::Rejected { stage, code, .. } => {
                Some(Refusal::classify(*stage, *code))
            }
            // Le seul refus qu'on voit venir sans que le serveur ait parlé : la taille annoncée
            // à l'`EHLO` suffit à savoir que le message ne passera pas.
            Self::TooLarge { .. } => Some(Refusal::TooLarge),
            Self::AuthRefused { .. } => Some(Refusal::Authentication),
            _ => None,
        }
    }

    /// Construit l'erreur qui correspond à un refus du serveur.
    ///
    /// Le code décide, pas le texte : un texte est écrit par chaque serveur à sa façon, un code
    /// est normalisé.
    #[must_use]
    pub fn from_reply(stage: Stage, reply: &Reply) -> Self {
        if reply.transient() {
            return Self::Transient {
                stage,
                code: reply.code,
                reason: reply.text(),
            };
        }
        Self::Rejected {
            stage,
            code: reply.code,
            reason: reply.text(),
        }
    }
}

/// Ce qu'un refus demande à l'utilisateur de faire — **le critère 8**.
///
/// ## Pourquoi une énumération et pas seulement un texte
///
/// Le critère demande que « l'utilisateur voie **quoi** faire : quota, destinataire refusé,
/// message trop gros, authentification. Pas un code numérique ». Un texte suffirait à
/// l'afficher, mais pas à ce qu'une interface **décide** : la coquille grise « Réessayer » sur
/// un refus définitif, la CLI rend un code de sortie différent, et un jour un client proposera
/// « retirer la pièce jointe » sur un message trop gros. Une famille se lit par le
/// compilateur ; une phrase ne se lit qu'à l'œil.
///
/// ## Les familles viennent des codes, pas du texte du serveur
///
/// Le texte est écrit par l'administrateur du serveur, dans sa langue, avec sa formulation. Le
/// code est normalisé — RFC 5321 §4.2.3 et RFC 3463 pour les codes étendus. Classer sur le
/// texte marcherait sur un serveur et échouerait au suivant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// Une boîte pleine, un quota, une limitation de débit : ça repassera tout seul.
    Quota,
    /// Le destinataire n'existe pas, ou le serveur refuse de lui remettre.
    Recipient,
    /// Le message est trop gros pour ce serveur.
    TooLarge,
    /// L'authentification a été refusée : le secret n'est plus valable.
    Authentication,
    /// Le serveur refuse d'expédier pour cet expéditeur — relais interdit, adresse non
    /// autorisée.
    Sender,
    /// Un refus dont la famille n'est pas reconnue.
    Other,
}

impl Refusal {
    /// Classe un refus depuis le code du serveur et l'étape où il est tombé.
    ///
    /// ## L'étape compte autant que le code
    ///
    /// `552` à l'étape `Recipient` est un quota de destinataire — « boîte pleine » — alors que le
    /// même `552` à l'étape `Data` est un message trop gros. Un `550` à l'étape `MailFrom` est
    /// un expéditeur refusé, à l'étape `Rcpt` un destinataire inconnu. Classer sur le seul code
    /// dirait au destinataire de vider sa boîte quand c'est notre pièce jointe qui est trop
    /// grosse.
    #[must_use]
    pub fn classify(stage: Stage, code: u16) -> Self {
        match (stage, code) {
            (Stage::Auth, _) => Self::Authentication,
            // RFC 5321 : 452 « insufficient system storage », 552 « exceeded storage
            // allocation ». À l'étape d'un destinataire, les deux parlent de sa boîte.
            (Stage::Recipient, 452 | 552) => Self::Quota,
            (Stage::Recipient, 450 | 550 | 551 | 553) => Self::Recipient,
            // À l'étape du message, ces mêmes codes parlent de sa taille — et 523 est le code
            // étendu que certains serveurs rendent pour « trop gros ».
            (Stage::Data | Stage::Sender, 552 | 523) => Self::TooLarge,
            (Stage::Sender, 550 | 553) => Self::Sender,
            // 421 « service not available », 450/451 « mailbox busy / local error » : le
            // serveur demande de repasser.
            (_, 421 | 450 | 451 | 452) => Self::Quota,
            _ => Self::Other,
        }
    }

    /// La phrase que l'utilisateur lit : **ce qui s'est passé, et quoi faire**.
    ///
    /// En français, sans code numérique, et sans le texte du serveur — celui-ci est gardé à
    /// côté par la file d'envoi pour qui veut regarder, mais il est écrit par un administrateur
    /// pour un administrateur.
    ///
    /// ## `retrying` n'est pas un détail de mise en forme
    ///
    /// « L'envoi est réessayé automatiquement ; rien à faire » est vrai **tant que la file
    /// réessaie**. Les tentatives épuisées, la ligne passe à `failed` et la même phrase reste
    /// affichée : l'utilisateur lit qu'il n'a rien à faire sur un message qui ne repartira plus
    /// jamais, alors qu'il est désormais le seul qui puisse le faire partir. C'est le critère 8
    /// pris à l'envers, et c'est le défaut qu'a trouvé
    /// `a_transient_refusal_that_gave_up_stops_promising_an_automatic_retry`.
    ///
    /// La phrase se compose donc de deux morceaux : **ce que le serveur a dit**, qui ne dépend
    /// que de la famille, et **qui agit ensuite** — la file ou l'utilisateur. Deux morceaux
    /// plutôt que douze phrases : la cause ne change pas parce qu'une tentative reste.
    #[must_use]
    pub fn advice(self, retrying: bool) -> String {
        let next = if retrying {
            RETRY_COMING
        } else {
            self.action()
        };
        format!("{} {next}", self.cause())
    }

    /// Ce que le serveur a refusé, dit en français.
    ///
    /// Ne dépend que de la famille : un serveur occupé est occupé qu'il reste une tentative ou
    /// non.
    #[must_use]
    pub fn cause(self) -> &'static str {
        match self {
            Self::Quota => "Le serveur est occupé, ou la boîte du destinataire est pleine.",
            Self::Recipient => {
                "Le serveur refuse ce destinataire : l'adresse n'existe probablement pas."
            }
            Self::TooLarge => "Le message est trop gros pour ce serveur.",
            Self::Authentication => {
                "Le serveur a refusé l'authentification : le mot de passe ou le jeton n'est \
                 peut-être plus valable."
            }
            Self::Sender => "Le serveur refuse d'expédier depuis cette adresse.",
            Self::Other => "Le serveur a refusé le message sans que la raison soit reconnue.",
        }
    }

    /// Ce qu'il reste à faire **à l'utilisateur**, la file ayant renoncé.
    ///
    /// Chaque famille nomme un geste concret, et un seul. « Réessayez » sur une adresse fausse
    /// n'est pas un geste : l'adresse serait la même.
    #[must_use]
    pub fn action(self) -> &'static str {
        match self {
            Self::Quota => "Renvoyez le message plus tard.",
            Self::Recipient => "Vérifiez l'adresse, puis renvoyez le message.",
            Self::TooLarge => "Retirez une pièce jointe, ou envoyez-la par un autre moyen.",
            Self::Authentication => "Reconfigurez le compte — `mail account submission`.",
            Self::Sender => "Vérifiez que le compte est autorisé à envoyer par ce serveur.",
            Self::Other => "Le détail rendu par le serveur est conservé à côté.",
        }
    }

    /// Vrai si renvoyer le message **tel quel** a une chance de marcher.
    ///
    /// ## À quoi ça sert, et pourquoi c'est écrit ici
    ///
    /// Une ligne `failed` peut être remise en file — le serveur n'a jamais pris le message,
    /// donc il n'y a pas de risque de doublon, contrairement à un envoi douteux. Mais offrir
    /// « Renvoyer » n'a de sens que si **rien n'a besoin de changer d'abord** : sur une adresse
    /// qui n'existe pas, sur une pièce jointe trop grosse ou sur un mot de passe périmé, le
    /// bouton renverrait exactement ce que le serveur vient de refuser. Un bouton qui échoue à
    /// tous les coups est ce que le critère 8 interdit autant qu'un code numérique.
    ///
    /// `Other` rend `false` : la raison n'est pas reconnue, donc l'effet d'un renvoi n'est pas
    /// prévisible, et proposer un geste dont on ignore l'effet est pire que de n'en proposer
    /// aucun.
    #[must_use]
    pub fn worth_retrying(self) -> bool {
        matches!(self, Self::Quota)
    }
}

/// Ce qui suit quand la file, et non l'utilisateur, agit ensuite.
///
/// « Pour l'instant » est là exprès : la phrase ne promet pas que l'envoi réussira, seulement
/// qu'il repartira sans qu'on s'en occupe.
const RETRY_COMING: &str = "L'envoi est réessayé automatiquement ; rien à faire pour l'instant.";

#[cfg(test)]
mod tests {
    use super::Error;
    use crate::client::Stage;
    use crate::reply::Reply;

    fn reply(code: u16) -> Reply {
        Reply {
            code,
            lines: vec!["texte du serveur".to_owned()],
        }
    }

    #[test]
    fn a_four_hundred_is_transient_and_a_five_hundred_is_not() {
        let soft = Error::from_reply(Stage::Recipient, &reply(452));
        let hard = Error::from_reply(Stage::Recipient, &reply(550));
        assert!(soft.retryable());
        assert!(!hard.retryable());
    }

    #[test]
    fn the_stage_survives_the_error() {
        // C'est la seule information qui permette à la file d'envoi de décider sans deviner.
        let it = Error::from_reply(Stage::Committing, &reply(451));
        assert_eq!(it.stage(), Some(Stage::Committing));
    }

    #[test]
    fn only_a_failure_after_the_final_dot_leaves_a_doubt() {
        // **La distinction qui évite d'envoyer deux fois.** Avant le point final, le serveur
        // n'a rien pris ; après, son silence ne dit pas s'il a gardé.
        for stage in [
            Stage::Greeting,
            Stage::Ehlo,
            Stage::StartTls,
            Stage::Auth,
            Stage::Sender,
            Stage::Recipient,
            Stage::Data,
        ] {
            assert!(
                !Error::from_reply(stage, &reply(451)).may_have_been_sent(),
                "{stage} ne devrait pas laisser de doute"
            );
        }
        assert!(
            Error::from_reply(Stage::Committing, &reply(451)).may_have_been_sent(),
            "un échec après le point final laisse un doute"
        );
    }

    #[test]
    fn a_retryable_error_can_still_leave_a_doubt() {
        // Les deux questions sont distinctes, et les mélanger est exactement comment on envoie
        // deux fois : celle-ci est réessayable **et** douteuse.
        let it = Error::from_reply(Stage::Committing, &reply(451));
        assert!(it.retryable());
        assert!(it.may_have_been_sent());
    }

    #[test]
    fn what_the_caller_did_wrong_is_never_retryable() {
        let it = Error::Unsendable {
            reason: "aucun destinataire".to_owned(),
        };
        assert!(!it.retryable());
        assert_eq!(it.stage(), None);
        assert!(!it.may_have_been_sent());
    }
}
