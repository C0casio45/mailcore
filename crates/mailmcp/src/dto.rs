//! Ce que les outils rendent au modèle.
//!
//! Des types à part, et pas ceux de `mailcore`. Trois raisons :
//!
//! - **La surface exposée est un contrat.** La renommer ou la restreindre ne doit pas
//!   obliger à toucher au cœur, et changer un type du cœur ne doit pas casser un client.
//! - **Ce qui part vers un modèle est choisi, pas hérité.** Un `MessageId` interne devient
//!   un simple entier ; un `BlobHash` ne sort pas du tout, parce qu'un modèle n'a rien à en
//!   faire et qu'il révélerait la structure du stockage.
//! - **Les descriptions comptent autant que les données.** Un champ nommé `date` sans unité
//!   se fait mal interpréter ; les `schemars` portent l'explication.

use rmcp::schemars;
use serde::Serialize;

/// Un dossier et ses compteurs.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct Folder {
    /// Identifiant interne du dossier, à passer aux autres outils.
    pub id: i64,
    /// Le compte auquel il appartient, tel que nommé par l'utilisateur.
    pub account: String,
    /// Chemin complet, séparé par `/`. Exemple : `[Gmail]/Tous les messages`.
    pub path: String,
    /// Rôle deviné : `inbox`, `sent`, `drafts`, `trash`, `junk`, `archive`, `other`.
    pub kind: String,
    /// Nombre de messages référencés dans ce dossier.
    pub total: u64,
    /// Nombre de messages non lus.
    pub unread: u64,
}

/// Un message, tel qu'il apparaît dans une liste ou un résultat de recherche.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct MessageSummary {
    /// Identifiant interne, à passer à `get_message` ou `get_thread`.
    pub id: i64,
    /// Date d'envoi, au format ISO 8601 `AAAA-MM-JJ`. Absente si l'en-tête `Date` manquait.
    pub date: Option<String>,
    /// Adresse de l'expéditeur, en minuscules.
    pub from: String,
    /// Nom affiché de l'expéditeur, s'il y en avait un.
    pub from_name: Option<String>,
    /// Sujet, décodé.
    pub subject: String,
    /// Vrai si le message déclare des pièces jointes.
    pub has_attachments: bool,
    /// Pertinence, pour un résultat de recherche. Comparable au sein d'une même requête
    /// seulement — pas entre deux requêtes différentes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub score: Option<f32>,
}

/// Une pièce jointe : décrite, jamais rendue.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct Attachment {
    /// Le nom de fichier déclaré, s'il y en a un.
    pub name: Option<String>,
    /// Le type MIME déclaré. À traiter comme une déclaration, pas comme une vérité.
    pub mime: String,
    /// La taille en octets, après décodage.
    pub size: u64,
}

/// Un message ouvert.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct Message {
    /// Le résumé, mêmes champs que dans une liste.
    pub summary: MessageSummary,
    /// L'en-tête `Message-ID`, s'il était présent.
    pub message_id: Option<String>,
    /// Les destinataires (`To` et `Cc`), décodés.
    pub to: Vec<String>,
    /// Le corps du message, **aplati en texte**. Jamais du HTML : le balisage, les URL et
    /// les scripts sont retirés avant de sortir d'ici.
    pub body: String,
    /// Vrai si le corps a été tronqué parce qu'il dépassait la taille maximale.
    pub body_truncated: bool,
    /// Les pièces jointes, listées et non ouvertes.
    pub attachments: Vec<Attachment>,
    /// Les dossiers qui contiennent ce message. Plusieurs entrées veulent dire que le même
    /// contenu est référencé à plusieurs endroits — c'est normal, pas une duplication.
    pub folders: Vec<String>,
}

/// Un fil de discussion.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct Thread {
    /// Les messages, du plus ancien au plus récent.
    pub messages: Vec<MessageSummary>,
    /// Nombre de messages dans le fil.
    pub count: usize,
}

/// Une liste de résultats, avec de quoi savoir si elle est complète.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct Results {
    /// Les messages trouvés.
    pub messages: Vec<MessageSummary>,
    /// Nombre de résultats rendus.
    pub count: usize,
    /// Vrai si la limite demandée a été atteinte : il y a probablement d'autres résultats.
    pub truncated: bool,
}
