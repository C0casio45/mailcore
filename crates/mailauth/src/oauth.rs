//! OAuth2 pour un client de bureau : consentement, échange, rafraîchissement.
//!
//! ## Le mot de passe ne passe jamais par ici
//!
//! C'est tout l'intérêt, et c'est ce qui distingue ce chemin du mot de passe applicatif :
//! l'utilisateur s'authentifie **sur la page du fournisseur**, dans son navigateur. Mailcore
//! reçoit un code à usage unique, l'échange contre un jeton, et ne voit à aucun moment le mot
//! de passe du compte.
//!
//! ## Quatre pièges, et ils cassent tous **plus tard**
//!
//! Aucun de ces quatre ne se manifeste pendant le développement. Chacun donne une
//! authentification qui marche, puis qui s'arrête — une heure après, une semaine après, ou à
//! la deuxième autorisation. C'est la raison d'être des tests de ce module.
//!
//! **1. Sans `access_type=offline`, il n'y a pas de jeton de rafraîchissement.** On obtient un
//! jeton d'accès valable une heure, tout fonctionne, et l'application cesse de synchroniser
//! au bout de cette heure sans que rien n'ait changé.
//!
//! **2. Sans `prompt=consent`, la deuxième autorisation ne rend pas de jeton de
//! rafraîchissement.** Google n'en émet un qu'au premier consentement. Un utilisateur qui
//! refait l'opération — parce qu'il a perdu son trousseau, par exemple — obtient une réponse
//! sans `refresh_token`, et un client qui ne le remarque pas enregistre un compte cassé.
//!
//! **3. Le défi PKCE est un base64url *sans bourrage*.** Avec bourrage, ou en base64 standard,
//! le fournisseur refuse l'échange en parlant de vérificateur invalide.
//!
//! **4. `invalid_grant` n'est pas une erreur réseau.** Il veut dire que le jeton de
//! rafraîchissement est mort — révoqué, expiré, ou périmé par les 7 jours du mode « test » de
//! Google. Le réessayer en boucle ne le ressuscite pas ; il faut un nouveau consentement, donc
//! une action de l'utilisateur.
//!
//! ## Ce que ce module ne fait pas
//!
//! Il ne garde rien. Le jeton de rafraîchissement va dans le trousseau du système, par les
//! fonctions du module racine ; l'identifiant client aussi. Un module qui à la fois obtient et
//! garde les secrets serait le seul endroit à compromettre.

use base64::Engine as _;

use crate::{Error, Result, http};

/// L'encodeur du défi PKCE : base64url, **sans bourrage**.
///
/// La RFC 7636 l'exige, et un bourrage en trop fait refuser l'échange avec un message qui
/// parle de vérificateur invalide — pas d'encodage.
const B64URL: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::URL_SAFE_NO_PAD;

/// Marge avant l'expiration d'un jeton d'accès.
///
/// Deux minutes. Un jeton qui expire pendant une moisson fait échouer le dossier en cours ;
/// le rafraîchir un peu tôt coûte un appel et rend la moisson insensible à l'horloge du
/// serveur, qui n'est pas la nôtre.
pub const EXPIRY_MARGIN: u64 = 120;

/// Les points de terminaison d'un fournisseur.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Provider {
    /// L'URL de la page de consentement.
    pub authorize: &'static str,
    /// L'hôte du point de terminaison de jeton.
    pub token_host: &'static str,
    /// Le chemin du point de terminaison de jeton.
    pub token_path: &'static str,
    /// Le périmètre demandé.
    pub scope: &'static str,
}

impl Provider {
    /// Google.
    ///
    /// Le périmètre `https://mail.google.com/` est « restreint » au sens de Google : il donne
    /// l'accès IMAP complet, et c'est le seul qui le donne. Les périmètres plus étroits de
    /// l'API Gmail ne servent pas IMAP.
    ///
    /// C'est aussi ce qui rend la validation de l'application nécessaire pour servir d'autres
    /// utilisateurs, et ce qui impose le mode « test » — donc l'expiration à 7 jours — à un
    /// projet non publié. Voir `docs/PHASE-2.md`.
    pub const GOOGLE: Self = Self {
        authorize: "https://accounts.google.com/o/oauth2/v2/auth",
        token_host: "oauth2.googleapis.com",
        token_path: "/token",
        scope: "https://mail.google.com/",
    };

    /// Microsoft, pour les comptes Outlook et Office 365.
    ///
    /// Le profil réel en a un — `outlook.office365.com`. Les points de terminaison sont ceux
    /// du locataire `common`, qui accepte les comptes personnels et professionnels.
    ///
    /// `offline_access` est ici un **périmètre**, pas un paramètre : c'est lui qui obtient le
    /// jeton de rafraîchissement, là où Google veut `access_type=offline`. Les deux
    /// fournisseurs demandent la même chose de deux façons, et confondre les deux donne le
    /// piège 1.
    pub const MICROSOFT: Self = Self {
        authorize: "https://login.microsoftonline.com/common/oauth2/v2.0/authorize",
        token_host: "login.microsoftonline.com",
        token_path: "/common/oauth2/v2.0/token",
        scope: "https://outlook.office.com/IMAP.AccessAsUser.All offline_access",
    };

    /// Le fournisseur d'un hôte IMAP, quand on le reconnaît.
    ///
    /// Reconnu par suffixe, pas par égalité : `imap.gmail.com` et `imap.googlemail.com`
    /// désignent le même service, et un utilisateur peut avoir configuré l'un ou l'autre.
    #[must_use]
    pub fn for_host(host: &str) -> Option<Self> {
        let host = host.trim().to_ascii_lowercase();
        if host.ends_with("gmail.com") || host.ends_with("googlemail.com") {
            return Some(Self::GOOGLE);
        }
        if host.ends_with("outlook.com")
            || host.ends_with("office365.com")
            || host.ends_with("outlook.office365.com")
        {
            return Some(Self::MICROSOFT);
        }
        None
    }
}

/// L'identifiant client, tel que l'utilisateur l'a créé chez le fournisseur.
///
/// ## Pourquoi il n'est pas dans le dépôt
///
/// Thunderbird embarque le sien parce que c'est une application validée. Ce dépôt sera publié,
/// et un secret client dans un dépôt public est un secret publié — Google le considère non
/// confidentiel pour une application de bureau, mais il se fait détourner pour le quota.
///
/// Il vit donc dans le trousseau, à côté du jeton de rafraîchissement.
#[derive(Debug, Clone)]
pub struct ClientCredentials {
    /// L'identifiant client.
    pub client_id: String,
    /// Le secret client. Absent pour un client public au sens de la RFC 8252.
    pub client_secret: Option<String>,
}

/// Un couple vérificateur / défi PKCE.
///
/// ## Pourquoi PKCE alors qu'il y a un secret client
///
/// La redirection passe par le bouclage, et **un autre processus de la machine peut écouter**
/// un port et courser le nôtre. PKCE rend le code intercepté inutilisable sans le
/// vérificateur, qui ne quitte jamais notre processus.
///
/// Ce n'est pas une redondance avec le secret client : le secret protège l'échange contre un
/// tiers, PKCE protège le code contre un voisin.
#[derive(Debug, Clone)]
pub struct Pkce {
    verifier: String,
    challenge: String,
}

impl Pkce {
    /// Fabrique un couple neuf.
    ///
    /// Le vérificateur fait 43 caractères — le minimum de la RFC 7636 — tirés de l'alphabet
    /// base64url appliqué à 32 octets aléatoires. 32 octets, c'est 256 bits : de quoi rendre
    /// une devinette hors de question.
    ///
    /// # Errors
    ///
    /// [`Error::Entropy`] si le système ne rend pas d'aléa. **Refusé, jamais remplacé** : un
    /// vérificateur prévisible désarme PKCE sans que rien ne le signale.
    pub fn new() -> Result<Self> {
        let mut seed = [0_u8; 32];
        getrandom::fill(&mut seed).map_err(|source| Error::Entropy {
            reason: source.to_string(),
        })?;
        let verifier = B64URL.encode(seed);

        let digest = ring::digest::digest(&ring::digest::SHA256, verifier.as_bytes());
        let challenge = B64URL.encode(digest.as_ref());
        Ok(Self {
            verifier,
            challenge,
        })
    }

    /// Le défi, à mettre dans l'URL de consentement.
    #[must_use]
    pub fn challenge(&self) -> &str {
        &self.challenge
    }

    /// Le vérificateur, à envoyer à l'échange. **Ne quitte pas le processus autrement.**
    #[must_use]
    pub fn verifier(&self) -> &str {
        &self.verifier
    }
}

/// Un état anti-rejeu, à comparer au retour de la redirection.
///
/// Sans lui, n'importe qui pourrait faire ouvrir notre URL de redirection avec un code de son
/// choix — un code d'un autre compte, par exemple, pour faire synchroniser une boîte qui n'est
/// pas la nôtre.
///
/// # Errors
///
/// [`Error::Entropy`] si le système ne rend pas d'aléa. Un état prévisible se devine, donc ne
/// protège de rien.
pub fn random_state() -> Result<String> {
    let mut seed = [0_u8; 16];
    getrandom::fill(&mut seed).map_err(|source| Error::Entropy {
        reason: source.to_string(),
    })?;
    Ok(B64URL.encode(seed))
}

/// L'URL de la page de consentement.
///
/// `login_hint` évite à l'utilisateur de choisir un compte quand il en a plusieurs — c'est une
/// commodité, pas une sécurité : le fournisseur peut l'ignorer, et le compte réellement
/// autorisé est vérifié après l'échange.
#[must_use]
pub fn authorization_url(
    provider: &Provider,
    credentials: &ClientCredentials,
    redirect_uri: &str,
    pkce: &Pkce,
    state: &str,
    login_hint: &str,
) -> String {
    // **`access_type=offline` et `prompt=consent` sont les pièges 1 et 2.** Sans le premier,
    // pas de jeton de rafraîchissement du tout ; sans le second, pas de jeton de
    // rafraîchissement à la deuxième autorisation.
    //
    // Les deux sont propres à Google et ignorés ailleurs : Microsoft obtient la même chose par
    // le périmètre `offline_access`. Les envoyer aux deux est sans effet de bord et évite un
    // branchement de plus.
    let query = http::form_encode(&[
        ("client_id", credentials.client_id.as_str()),
        ("redirect_uri", redirect_uri),
        ("response_type", "code"),
        ("scope", provider.scope),
        ("access_type", "offline"),
        ("prompt", "consent"),
        ("code_challenge", pkce.challenge()),
        ("code_challenge_method", "S256"),
        ("state", state),
        ("login_hint", login_hint),
    ]);
    format!("{}?{query}", provider.authorize)
}

/// Ce qu'un point de terminaison de jeton a rendu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tokens {
    /// Le jeton d'accès, à passer à `AUTHENTICATE XOAUTH2`.
    pub access: String,
    /// Le jeton de rafraîchissement, quand le fournisseur en émet un.
    ///
    /// **Absent au rafraîchissement, et c'est normal** : on garde celui qu'on a. Absent au
    /// *premier* échange, c'est le piège 2, et l'appelant doit le traiter comme une erreur.
    pub refresh: Option<String>,
    /// Durée de vie du jeton d'accès, en secondes.
    pub expires_in: Option<u64>,
}

impl Tokens {
    /// L'instant, en secondes Unix, à partir duquel il faut rafraîchir.
    ///
    /// Compté depuis `now` et diminué de [`EXPIRY_MARGIN`]. Sans durée annoncée, rend `None` :
    /// on rafraîchira quand le serveur refusera, ce qui est le seul comportement correct quand
    /// on ne sait pas.
    #[must_use]
    pub fn refresh_after(&self, now: i64) -> Option<i64> {
        let lifetime = self.expires_in?;
        let usable = lifetime.saturating_sub(EXPIRY_MARGIN);
        Some(now.saturating_add(i64::try_from(usable).unwrap_or(i64::MAX)))
    }
}

/// Lit une réponse de point de terminaison de jeton.
///
/// ## Le corps est lu même sur une erreur
///
/// Un `400` porte **le** diagnostic : `invalid_grant` pour un jeton mort, `invalid_client`
/// pour un identifiant client faux, `invalid_scope` pour un périmètre refusé. Les trois se
/// corrigent autrement, et le code d'état seul ne les distingue pas.
///
/// # Errors
///
/// [`Error::Consent`] quand le jeton de rafraîchissement est mort — l'utilisateur doit
/// reconsentir, et réessayer ne sert à rien. [`Error::Provider`] pour les autres refus.
/// [`Error::Protocol`] si la réponse n'est pas du JSON exploitable.
pub fn parse_token_response(status: u16, body: &str) -> Result<Tokens> {
    let json: serde_json::Value = serde_json::from_str(body).map_err(|_| Error::Protocol {
        reason: format!("réponse de jeton illisible ({status})"),
    })?;

    if let Some(code) = json.get("error").and_then(|it| it.as_str()) {
        let description = json
            .get("error_description")
            .and_then(|it| it.as_str())
            .unwrap_or("sans description");
        // **`invalid_grant` est une classe à part.** Il veut dire que le jeton de
        // rafraîchissement est mort : révoqué, ou périmé par les 7 jours du mode « test ».
        // Le réessayer en boucle ne le ressuscite pas, et sur certains fournisseurs ça fait
        // bloquer le client.
        if code == "invalid_grant" {
            return Err(Error::Consent {
                reason: format!("{code} : {description}"),
            });
        }
        return Err(Error::Provider {
            code: code.to_owned(),
            description: description.to_owned(),
        });
    }

    // Pas d'erreur annoncée mais un code d'état d'échec : un fournisseur qui répond de travers.
    if !(200..300).contains(&status) {
        return Err(Error::Protocol {
            reason: format!("état {status} sans champ `error`"),
        });
    }

    let access = json
        .get("access_token")
        .and_then(|it| it.as_str())
        .filter(|it| !it.is_empty())
        .ok_or_else(|| Error::Protocol {
            reason: "réponse sans `access_token`".to_owned(),
        })?
        .to_owned();

    Ok(Tokens {
        access,
        refresh: json
            .get("refresh_token")
            .and_then(|it| it.as_str())
            .filter(|it| !it.is_empty())
            .map(str::to_owned),
        // `expires_in` arrive en nombre chez Google, en chaîne chez d'autres. Les deux sont
        // acceptés : refuser une chaîne ferait échouer un échange par ailleurs valide.
        expires_in: json.get("expires_in").and_then(|it| {
            it.as_u64()
                .or_else(|| it.as_str().and_then(|text| text.parse().ok()))
        }),
    })
}

/// Les champs du `POST` qui échange un code d'autorisation contre des jetons.
#[must_use]
pub fn exchange_fields<'a>(
    credentials: &'a ClientCredentials,
    code: &'a str,
    redirect_uri: &'a str,
    pkce: &'a Pkce,
) -> Vec<(&'a str, &'a str)> {
    let mut fields = vec![
        ("client_id", credentials.client_id.as_str()),
        ("code", code),
        ("redirect_uri", redirect_uri),
        ("grant_type", "authorization_code"),
        ("code_verifier", pkce.verifier()),
    ];
    if let Some(secret) = credentials.client_secret.as_deref() {
        fields.push(("client_secret", secret));
    }
    fields
}

/// Les champs du `POST` qui rafraîchit un jeton d'accès.
#[must_use]
pub fn refresh_fields<'a>(
    credentials: &'a ClientCredentials,
    refresh_token: &'a str,
) -> Vec<(&'a str, &'a str)> {
    let mut fields = vec![
        ("client_id", credentials.client_id.as_str()),
        ("refresh_token", refresh_token),
        ("grant_type", "refresh_token"),
    ];
    if let Some(secret) = credentials.client_secret.as_deref() {
        fields.push(("client_secret", secret));
    }
    fields
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn credentials() -> ClientCredentials {
        ClientCredentials {
            client_id: "123-abc.apps.googleusercontent.com".to_owned(),
            client_secret: Some("GOCSPX-secret".to_owned()),
        }
    }

    // ------------------------------------------------------------------
    // PKCE. Le piège 3 : le bourrage.
    // ------------------------------------------------------------------

    #[test]
    fn the_challenge_is_the_sha256_of_the_verifier_in_base64url_without_padding() {
        let pkce = Pkce::new().unwrap();

        // Recalculé indépendamment : c'est ce qui vérifie qu'on n'a pas haché autre chose —
        // les octets aléatoires au lieu du vérificateur encodé, par exemple, qui est l'erreur
        // symétrique et tout aussi silencieuse.
        let expected = B64URL.encode(
            ring::digest::digest(&ring::digest::SHA256, pkce.verifier().as_bytes()).as_ref(),
        );
        assert_eq!(pkce.challenge(), expected);
    }

    #[test]
    fn neither_the_verifier_nor_the_challenge_is_padded() {
        // **Le piège 3.** Avec bourrage, le fournisseur refuse l'échange en parlant de
        // vérificateur invalide, et rien ne dit que c'est un `=` en trop.
        let pkce = Pkce::new().unwrap();
        assert!(!pkce.verifier().contains('='), "{}", pkce.verifier());
        assert!(!pkce.challenge().contains('='), "{}", pkce.challenge());
    }

    #[test]
    fn the_challenge_uses_the_url_safe_alphabet() {
        // `+` et `/` du base64 standard sont des séparateurs dans une URL : ils cassent la
        // requête sans que le fournisseur puisse dire pourquoi.
        for _ in 0..50 {
            let pkce = Pkce::new().unwrap();
            assert!(
                !pkce.challenge().contains('+') && !pkce.challenge().contains('/'),
                "{}",
                pkce.challenge()
            );
            assert!(!pkce.verifier().contains('+') && !pkce.verifier().contains('/'));
        }
    }

    #[test]
    fn the_verifier_is_long_enough_for_the_rfc() {
        // La RFC 7636 veut entre 43 et 128 caractères. En dessous, le fournisseur refuse.
        let pkce = Pkce::new().unwrap();
        assert!(
            (43..=128).contains(&pkce.verifier().len()),
            "longueur {}",
            pkce.verifier().len()
        );
    }

    #[test]
    fn two_verifiers_are_never_the_same() {
        // Un vérificateur prévisible désarme PKCE sans que rien ne le signale.
        let mut seen = std::collections::HashSet::new();
        for _ in 0..100 {
            assert!(
                seen.insert(Pkce::new().unwrap().verifier().to_owned()),
                "un vérificateur est sorti deux fois"
            );
        }
    }

    #[test]
    fn two_states_are_never_the_same() {
        let mut seen = std::collections::HashSet::new();
        for _ in 0..100 {
            assert!(seen.insert(random_state().unwrap()));
        }
    }

    // ------------------------------------------------------------------
    // L'URL de consentement. Les pièges 1 et 2.
    // ------------------------------------------------------------------

    #[test]
    fn the_url_asks_for_offline_access_and_forces_the_consent_screen() {
        // **Les pièges 1 et 2, verrouillés.** Sans `access_type=offline`, pas de jeton de
        // rafraîchissement du tout ; sans `prompt=consent`, pas de jeton de rafraîchissement
        // à la deuxième autorisation. Les deux donnent une application qui marche puis
        // s'arrête.
        let pkce = Pkce::new().unwrap();
        let url = authorization_url(
            &Provider::GOOGLE,
            &credentials(),
            "http://127.0.0.1:54321/",
            &pkce,
            "etat",
            "marie@gmail.com",
        );
        assert!(url.contains("access_type=offline"), "{url}");
        assert!(url.contains("prompt=consent"), "{url}");
    }

    #[test]
    fn the_url_carries_the_challenge_and_its_method() {
        let pkce = Pkce::new().unwrap();
        let url = authorization_url(
            &Provider::GOOGLE,
            &credentials(),
            "http://127.0.0.1:1/",
            &pkce,
            "etat",
            "",
        );
        assert!(url.contains("code_challenge_method=S256"), "{url}");
        assert!(
            url.contains(&format!("code_challenge={}", pkce.challenge())),
            "{url}"
        );
    }

    #[test]
    fn the_url_never_carries_the_verifier_nor_the_client_secret() {
        // **Le vérificateur ne quitte pas le processus** avant l'échange, et le secret client
        // n'a rien à faire dans une URL que le navigateur va journaliser dans son historique.
        let pkce = Pkce::new().unwrap();
        let url = authorization_url(
            &Provider::GOOGLE,
            &credentials(),
            "http://127.0.0.1:1/",
            &pkce,
            "etat",
            "",
        );
        assert!(
            !url.contains(pkce.verifier()),
            "le vérificateur est dans l'URL"
        );
        assert!(
            !url.contains("GOCSPX"),
            "le secret client est dans l'URL : {url}"
        );
    }

    #[test]
    fn the_redirect_uri_is_encoded() {
        // Non encodée, la partie après `://` est lue comme la suite de la requête.
        let pkce = Pkce::new().unwrap();
        let url = authorization_url(
            &Provider::GOOGLE,
            &credentials(),
            "http://127.0.0.1:54321/",
            &pkce,
            "etat",
            "",
        );
        assert!(
            url.contains("redirect_uri=http%3A%2F%2F127.0.0.1%3A54321%2F"),
            "{url}"
        );
    }

    #[test]
    fn the_google_scope_is_the_one_that_serves_imap() {
        // Les périmètres étroits de l'API Gmail ne donnent pas l'accès IMAP. Se tromper donne
        // un consentement accordé et un `AUTHENTICATE` refusé.
        assert_eq!(Provider::GOOGLE.scope, "https://mail.google.com/");
    }

    #[test]
    fn microsoft_gets_its_refresh_token_through_a_scope_not_a_parameter() {
        // Le même besoin, exprimé autrement. Confondre les deux donne le piège 1 chez l'un
        // ou chez l'autre.
        assert!(Provider::MICROSOFT.scope.contains("offline_access"));
        assert!(!Provider::GOOGLE.scope.contains("offline_access"));
    }

    #[test]
    fn a_provider_is_recognised_by_suffix() {
        // `imap.gmail.com` et `imap.googlemail.com` sont le même service, et un utilisateur
        // peut avoir configuré l'un ou l'autre.
        assert_eq!(Provider::for_host("imap.gmail.com"), Some(Provider::GOOGLE));
        assert_eq!(
            Provider::for_host("imap.googlemail.com"),
            Some(Provider::GOOGLE)
        );
        assert_eq!(
            Provider::for_host("outlook.office365.com"),
            Some(Provider::MICROSOFT)
        );
        assert_eq!(Provider::for_host("IMAP.GMAIL.COM"), Some(Provider::GOOGLE));
        assert_eq!(Provider::for_host("mail.perso.invalid"), None);
        assert_eq!(Provider::for_host(""), None);
    }

    #[test]
    fn a_lookalike_host_is_not_recognised() {
        // `gmail.com.attaquant.fr` ne se termine pas par `gmail.com`, et c'est ce qui compte.
        assert_eq!(Provider::for_host("gmail.com.attaquant.fr"), None);
    }

    // ------------------------------------------------------------------
    // Les réponses du fournisseur. Le piège 4.
    // ------------------------------------------------------------------

    #[test]
    fn a_first_exchange_yields_both_tokens() {
        let body = r#"{"access_token":"ya29.abc","expires_in":3599,
                       "refresh_token":"1//0xyz","scope":"https://mail.google.com/",
                       "token_type":"Bearer"}"#;
        let tokens = parse_token_response(200, body).unwrap();
        assert_eq!(tokens.access, "ya29.abc");
        assert_eq!(tokens.refresh.as_deref(), Some("1//0xyz"));
        assert_eq!(tokens.expires_in, Some(3599));
    }

    #[test]
    fn a_refresh_yields_no_new_refresh_token_and_that_is_normal() {
        // Le fournisseur ne le réémet pas : on garde celui qu'on a. Traiter cette absence
        // comme une erreur ferait échouer tous les rafraîchissements.
        let body = r#"{"access_token":"ya29.def","expires_in":3599,"token_type":"Bearer"}"#;
        let tokens = parse_token_response(200, body).unwrap();
        assert!(tokens.refresh.is_none());
        assert_eq!(tokens.access, "ya29.def");
    }

    #[test]
    fn an_empty_refresh_token_counts_as_absent() {
        // **Le piège 2 sous sa forme discrète.** Un `""` enregistré serait un compte qui
        // s'authentifie une heure puis plus jamais, et rien dans le trousseau ne le dirait.
        let body = r#"{"access_token":"a","refresh_token":"","expires_in":10}"#;
        assert!(parse_token_response(200, body).unwrap().refresh.is_none());
    }

    #[test]
    fn an_invalid_grant_asks_for_a_new_consent_not_a_retry() {
        // **Le piège 4.** C'est ce que Google renvoie quand le jeton de rafraîchissement est
        // mort — révoqué, ou périmé par les 7 jours du mode « test ». Le réessayer en boucle
        // ne le ressuscite pas.
        let body =
            r#"{"error":"invalid_grant","error_description":"Token has been expired or revoked."}"#;
        let error = parse_token_response(400, body).unwrap_err();
        assert!(matches!(error, Error::Consent { .. }), "{error:?}");
        assert!(
            !error.retryable(),
            "un consentement mort ne se réessaie pas"
        );
        assert!(
            error.to_string().contains("expired or revoked"),
            "la description du fournisseur est la seule information utile : {error}"
        );
    }

    #[test]
    fn other_provider_errors_are_distinguished() {
        for (code, expected_retryable) in [
            ("invalid_client", false),
            ("invalid_scope", false),
            ("unauthorized_client", false),
        ] {
            let body = format!(r#"{{"error":"{code}","error_description":"d"}}"#);
            let error = parse_token_response(400, &body).unwrap_err();
            assert!(
                matches!(error, Error::Provider { .. }),
                "{code} : {error:?}"
            );
            assert_eq!(error.retryable(), expected_retryable, "{code}");
            assert!(error.to_string().contains(code), "{error}");
        }
    }

    #[test]
    fn an_error_is_read_even_on_a_two_hundred() {
        // Un fournisseur qui met une erreur dans un `200` est hors spécification, mais son
        // message reste la seule information utile.
        let body = r#"{"error":"invalid_grant","error_description":"d"}"#;
        assert!(matches!(
            parse_token_response(200, body),
            Err(Error::Consent { .. })
        ));
    }

    #[test]
    fn a_failure_status_without_an_error_field_is_a_protocol_error() {
        assert!(matches!(
            parse_token_response(500, r#"{"quelque":"chose"}"#),
            Err(Error::Protocol { .. })
        ));
    }

    #[test]
    fn a_response_without_an_access_token_is_refused() {
        // Enregistrer un compte sur une réponse vide donnerait un compte qui échoue à chaque
        // synchronisation sans qu'on sache pourquoi.
        for body in [r#"{}"#, r#"{"access_token":""}"#, r#"{"expires_in":10}"#] {
            assert!(
                matches!(parse_token_response(200, body), Err(Error::Protocol { .. })),
                "accepté : {body}"
            );
        }
    }

    #[test]
    fn an_expires_in_given_as_a_string_is_accepted() {
        // Google le met en nombre, d'autres en chaîne. Refuser la chaîne ferait échouer un
        // échange par ailleurs valide.
        let body = r#"{"access_token":"a","expires_in":"3599"}"#;
        assert_eq!(
            parse_token_response(200, body).unwrap().expires_in,
            Some(3599)
        );
    }

    #[test]
    fn a_body_that_is_not_json_never_panics() {
        // L'entrée vient du réseau — et d'un portail captif, parfois, qui répond du HTML.
        for body in [
            "",
            "pas du json",
            "<html>portail captif</html>",
            "[]",
            "null",
            "42",
        ] {
            assert!(
                matches!(parse_token_response(200, body), Err(Error::Protocol { .. })),
                "accepté : {body:?}"
            );
        }
    }

    // ------------------------------------------------------------------
    // L'expiration.
    // ------------------------------------------------------------------

    #[test]
    fn the_refresh_deadline_keeps_a_margin() {
        // Un jeton qui expire pendant une moisson fait échouer le dossier en cours.
        let tokens = Tokens {
            access: "a".to_owned(),
            refresh: None,
            expires_in: Some(3600),
        };
        assert_eq!(tokens.refresh_after(1000), Some(1000 + 3600 - 120));
    }

    #[test]
    fn a_lifetime_shorter_than_the_margin_expires_immediately() {
        // Le rafraîchir tout de suite est le bon comportement : on ne veut pas d'un jeton
        // qu'on sait déjà trop court.
        let tokens = Tokens {
            access: "a".to_owned(),
            refresh: None,
            expires_in: Some(30),
        };
        assert_eq!(tokens.refresh_after(1000), Some(1000));
    }

    #[test]
    fn no_announced_lifetime_means_no_deadline() {
        // On rafraîchira quand le serveur refusera. C'est le seul comportement correct quand
        // on ne sait pas, et inventer une durée ferait rafraîchir trop tôt ou trop tard.
        let tokens = Tokens {
            access: "a".to_owned(),
            refresh: None,
            expires_in: None,
        };
        assert_eq!(tokens.refresh_after(1000), None);
    }

    #[test]
    fn an_absurd_lifetime_does_not_overflow() {
        let tokens = Tokens {
            access: "a".to_owned(),
            refresh: None,
            expires_in: Some(u64::MAX),
        };
        assert_eq!(tokens.refresh_after(1000), Some(i64::MAX));
    }

    // ------------------------------------------------------------------
    // Les champs des deux échanges.
    // ------------------------------------------------------------------

    #[test]
    fn the_exchange_carries_the_verifier_and_the_secret() {
        let pkce = Pkce::new().unwrap();
        let credentials = credentials();
        let fields = exchange_fields(&credentials, "le-code", "http://127.0.0.1:1/", &pkce);
        let map: std::collections::HashMap<&str, &str> = fields.iter().copied().collect();

        assert_eq!(map.get("grant_type"), Some(&"authorization_code"));
        assert_eq!(map.get("code"), Some(&"le-code"));
        assert_eq!(map.get("code_verifier"), Some(&pkce.verifier()));
        assert_eq!(map.get("client_secret"), Some(&"GOCSPX-secret"));
    }

    #[test]
    fn a_public_client_sends_no_secret() {
        // La RFC 8252 décrit des clients publics, sans secret. En envoyer un vide serait
        // refusé par le fournisseur.
        let pkce = Pkce::new().unwrap();
        let public = ClientCredentials {
            client_id: "id".to_owned(),
            client_secret: None,
        };
        let fields = exchange_fields(&public, "c", "r", &pkce);
        assert!(!fields.iter().any(|(key, _)| *key == "client_secret"));
    }

    #[test]
    fn the_refresh_carries_no_verifier() {
        // PKCE ne protège que l'échange du code. Envoyer un vérificateur au rafraîchissement
        // serait refusé.
        let credentials = credentials();
        let fields = refresh_fields(&credentials, "1//0xyz");
        let map: std::collections::HashMap<&str, &str> = fields.iter().copied().collect();

        assert_eq!(map.get("grant_type"), Some(&"refresh_token"));
        assert_eq!(map.get("refresh_token"), Some(&"1//0xyz"));
        assert!(!map.contains_key("code_verifier"));
        assert!(!map.contains_key("code"));
    }
}
