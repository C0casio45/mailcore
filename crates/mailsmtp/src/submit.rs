//! Le transport réel : la connexion chiffrée branchée sur la file d'envoi.
//!
//! ## Ce module ne lit aucun secret
//!
//! Il en **reçoit** un. C'est la même règle que `mailsync` : le trousseau du système est
//! l'affaire de `mailauth`, et le brancher ici mettrait `keyring` dans l'arbre de dépendances de
//! tous les tests SMTP — y compris ceux d'un compte à mot de passe applicatif, qui n'en a pas
//! besoin.
//!
//! Conséquence à connaître : un jeton OAuth2 est **rafraîchi par l'appelant**, avant de
//! construire le [`Submitter`]. Un envoi long — une pièce jointe de 25 Mo sur une liaison
//! montante ordinaire — peut dépasser la validité du jeton, et c'est à la connexion qu'il est
//! vérifié, pas pendant le transfert. Le cas n'a donc pas de conséquence : le jeton sert à
//! l'`AUTH`, qui a lieu avant le premier octet du message.
//!
//! ## La frontière du doute est appelée ici, et à un seul endroit
//!
//! [`Transport::deliver`] doit appeler son rappel entre l'acceptation du `DATA` et le point
//! final. C'est la ligne entre [`Client::open_data`] et [`Client::finish_data`], et c'est pour
//! elle que ces deux fonctions sont séparées.
//!
//! [`Client::open_data`]: crate::Client::open_data
//! [`Client::finish_data`]: crate::Client::finish_data

use mailcore::{Outgoing, Server};

use crate::client::Credential;
use crate::error::{Error, Result};
use crate::queue::Transport;

/// De quoi remettre un message à un serveur de soumission.
///
/// Vit le temps d'un envoi : il n'y a **pas de connexion réutilisée entre deux messages**. Deux
/// raisons, dans cet ordre :
///
/// **Le doute ne doit pas se propager.** Une connexion partagée dont le serveur ferme au milieu
/// d'un message rendrait le suivant douteux aussi ; un envoi par connexion borne le doute au
/// message qui l'a rencontré.
///
/// **Le coût est celui d'une poignée de main TLS par message**, et un utilisateur envoie
/// quelques messages par heure. C'est le sens inverse de la moisson IMAP, où une connexion sert
/// des milliers de messages et où la réutiliser est ce qui rend la synchronisation possible.
#[derive(Debug)]
pub struct Submitter<'a> {
    /// Le serveur de soumission du compte.
    pub server: &'a Server,
    /// Le secret, déjà rafraîchi. Voir la documentation du module.
    pub credential: Credential<'a>,
}

impl Transport for Submitter<'_> {
    /// Le dialogue complet, avec la frontière du doute au bon endroit.
    ///
    /// # Errors
    ///
    /// Ce que le serveur, le réseau ou TLS refuse. **L'étape portée par l'erreur décide de la
    /// suite** : voir [`Error::may_have_been_sent`].
    ///
    /// [`Error::may_have_been_sent`]: crate::Error::may_have_been_sent
    fn deliver(
        &mut self,
        job: &Outgoing,
        body: &mut dyn std::io::Read,
        frontier: &mut dyn FnMut() -> Result<()>,
    ) -> Result<()> {
        let mut client = crate::tls::connect(self.server)?;
        client.auth(&self.server.username, self.credential)?;

        // La taille est annoncée : un serveur qui connaît la limite refuse **avant** le
        // transfert. Sans elle, 25 Mo montent pour se faire refuser à la fin.
        //
        // Elle vient de la ligne de file et **non du corps** : depuis que le corps est un flux,
        // sa longueur n'est connue qu'après l'avoir lu — donc trop tard. Voir la migration v7.
        // Zéro veut dire « inconnue » pour une ligne écrite avant cette migration, et
        // `mail_from` ne l'annonce alors pas.
        let size = Some(job.size).filter(|it| *it > 0);
        client.mail_from(&job.sender, size)?;

        if job.recipients.is_empty() {
            // Le protocole refuse un `DATA` sans destinataire, mais avec un message qui ne dit
            // rien. Le dire ici, avant, et sans étape : rien n'est parti.
            return Err(Error::Unsendable {
                reason: "aucun destinataire dans l'enveloppe".to_owned(),
            });
        }
        for recipient in &job.recipients {
            client.rcpt_to(recipient)?;
        }

        client.open_data()?;

        // **La frontière.** Après cette ligne, le serveur peut avoir le message ; avant, il a
        // une transaction vide qu'il abandonnera à son propre délai.
        //
        // Une erreur ici renonce **sans** écrire le point final, et c'est le contrat de
        // `Transport::deliver` : sans la frontière sur le disque, envoyer serait indéfendable.
        // La connexion est fermée par le destructeur ; le serveur voit une coupure et abandonne.
        frontier()?;

        client.finish_data_from(body)?;
        client.quit();
        tracing::info!(
            job = job.id.0,
            host = %self.server.host,
            recipients = job.recipients.len(),
            "message remis au serveur de soumission"
        );
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::Submitter;
    use crate::client::Credential;
    use mailcore::{AuthKind, Security, Server};

    fn server() -> Server {
        Server {
            host: "smtp.exemple.fr".to_owned(),
            port: 465,
            username: "marie@exemple.fr".to_owned(),
            auth: AuthKind::Password,
            security: Security::Tls,
        }
    }

    #[test]
    fn the_debug_output_never_carries_the_secret() {
        // `Submitter` porte un secret, et il dérive `Debug`. Un `tracing::debug!(?submitter)`
        // ailleurs dans le programme le mettrait dans un journal — `docs/PRIVACY.md` §8. Ce
        // test est ce qui empêche `Credential` de devenir affichable en clair sans qu'on le
        // remarque.
        let server = server();
        let it = Submitter {
            server: &server,
            credential: Credential::Password("mot-de-passe-tres-secret"),
        };
        let shown = format!("{it:?}");
        assert!(
            !shown.contains("mot-de-passe-tres-secret"),
            "le secret apparaît dans le Debug : {shown}"
        );
    }
}
