//! Le jeton d'un compte OAuth2 : obtenu une fois, rafraîchi tout seul, gardé au trousseau.
//!
//! ## Ce qui est rangé, et pourquoi c'est une seule entrée
//!
//! Quatre choses vont ensemble : l'identifiant client, le secret client, le jeton de
//! rafraîchissement et le jeton d'accès en cache. Elles sont écrites comme **un** objet JSON
//! sous **une** entrée de trousseau, pas comme quatre entrées.
//!
//! La raison est l'atomicité. Un rafraîchissement remplace le jeton d'accès et parfois le
//! jeton de rafraîchissement ; en deux entrées, une interruption entre les deux écritures
//! laisse un compte à moitié à jour, dont l'un des deux jetons ne correspond plus à l'autre.
//! Le trousseau ne connaît pas la transaction, donc la seule façon d'être atomique est de
//! n'écrire qu'une fois.
//!
//! ## Pourquoi le jeton d'accès est mis en cache
//!
//! Un jeton d'accès vit une heure. Sans cache, chaque `mail sync` en redemande un, ce qui fait
//! un aller-retour HTTPS avant chaque synchronisation et compte dans les quotas du
//! fournisseur.
//!
//! Le mettre en cache **au même endroit** que le jeton de rafraîchissement n'ajoute aucune
//! exposition : qui peut lire l'un peut lire l'autre, et l'autre est bien plus puissant — un
//! jeton d'accès périme en une heure, un jeton de rafraîchissement dure jusqu'à révocation.
//! Ranger l'un ailleurs, dans le store ou dans un fichier, serait le vrai relâchement.
//!
//! ## Ce que ce module ne fait pas
//!
//! Il ne journalise **aucun** jeton, ni entier ni tronqué. Un préfixe de jeton dans un journal
//! est encore un fragment de secret, et il suffit à identifier un compte dans une fuite de
//! journaux. Les traces disent l'hôte, l'identifiant, et si un rafraîchissement a eu lieu.

use crate::http;
use crate::oauth::{self, ClientCredentials, Provider, Tokens};
use crate::{Error, Result, SERVICE};

/// Le préfixe des entrées OAuth2.
///
/// Distinct de celui des mots de passe : un compte peut changer de mécanisme
/// d'authentification — Google a coupé les mots de passe applicatifs pour certains comptes — et
/// les deux entrées ne doivent pas s'écraser pendant la migration.
const PREFIX: &str = "oauth2";

/// Le port du point de terminaison de jeton. **Toujours 443.**
///
/// Pas un paramètre : un port configurable sur le chemin qui transporte un jeton de
/// rafraîchissement est une porte, pas une souplesse.
const TOKEN_PORT: u16 = 443;

/// Ce qu'on garde pour un compte OAuth2.
///
/// `Debug` est écrit à la main : la dérivation afficherait les jetons, et un `Debug` de
/// structure finit toujours par passer dans un `tracing::debug!`.
#[derive(Clone, PartialEq, Eq)]
pub struct Oauth2Record {
    /// L'identifiant client de l'application.
    pub client_id: String,
    /// Le secret client, quand le fournisseur en impose un.
    ///
    /// Google en exige un même pour une application de bureau, où il n'est **pas** un secret
    /// au sens cryptographique : il est dans le binaire de tout client installé. C'est pour ça
    /// que PKCE existe, et pour ça qu'il n'est pas dans ce dépôt — il vient de l'utilisateur,
    /// qui crée son propre client dans la console du fournisseur.
    pub client_secret: Option<String>,
    /// Le jeton de rafraîchissement. C'est **lui** qui vaut le compte.
    pub refresh: String,
    /// Le jeton d'accès en cache, s'il en reste un d'utilisable.
    pub access: Option<String>,
    /// L'instant Unix à partir duquel le jeton d'accès doit être rafraîchi.
    ///
    /// `None` veut dire « on ne sait pas » : le fournisseur n'a pas annoncé de durée de vie. Le
    /// jeton est alors utilisé jusqu'à ce que le serveur le refuse, ce qui est le seul
    /// comportement correct quand on ignore l'échéance.
    pub refresh_after: Option<i64>,
}

impl std::fmt::Debug for Oauth2Record {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // L'identifiant client n'est pas un secret — il est visible dans l'URL de
        // consentement, que l'utilisateur lit dans son navigateur. Les trois autres champs ne
        // sont jamais affichés, même par leur présence tronquée.
        f.debug_struct("Oauth2Record")
            .field("client_id", &self.client_id)
            .field("client_secret", &self.client_secret.as_ref().map(|_| "…"))
            .field("refresh", &"…")
            .field("access", &self.access.as_ref().map(|_| "…"))
            .field("refresh_after", &self.refresh_after)
            .finish()
    }
}

impl Oauth2Record {
    /// Vrai si le jeton d'accès en cache est encore bon à `now`.
    ///
    /// Sans échéance connue, rend `true` : on essaie, et un refus du serveur déclenchera le
    /// rafraîchissement. L'inverse — rafraîchir à chaque fois par prudence — transformerait un
    /// fournisseur silencieux sur `expires_in` en un aller-retour supplémentaire par
    /// synchronisation.
    #[must_use]
    pub fn access_usable_at(&self, now: i64) -> bool {
        self.access.as_ref().is_some_and(|it| !it.is_empty())
            && self.refresh_after.is_none_or(|after| now < after)
    }

    /// L'objet JSON écrit au trousseau.
    fn to_json(&self) -> String {
        let mut object = serde_json::Map::new();
        object.insert("client_id".to_owned(), self.client_id.clone().into());
        if let Some(secret) = &self.client_secret {
            object.insert("client_secret".to_owned(), secret.clone().into());
        }
        object.insert("refresh_token".to_owned(), self.refresh.clone().into());
        if let Some(access) = &self.access {
            object.insert("access_token".to_owned(), access.clone().into());
        }
        if let Some(after) = self.refresh_after {
            object.insert("refresh_after".to_owned(), after.into());
        }
        serde_json::Value::Object(object).to_string()
    }

    /// Relit l'objet JSON du trousseau.
    ///
    /// ## L'erreur ne cite pas le contenu
    ///
    /// Un JSON abîmé est un incident de trousseau, mais ce JSON **est** le secret : recopier ce
    /// qu'on n'a pas su lire dans un message d'erreur le ferait sortir par le journal, qui est
    /// exactement le chemin que le critère 6 interdit.
    fn from_json(text: &str) -> Result<Self> {
        let json: serde_json::Value = serde_json::from_str(text).map_err(|_| Error::Protocol {
            reason: "entrée de trousseau OAuth2 illisible".to_owned(),
        })?;
        let field = |name: &str| {
            json.get(name)
                .and_then(|it| it.as_str())
                .filter(|it| !it.is_empty())
                .map(str::to_owned)
        };
        Ok(Self {
            client_id: field("client_id").ok_or_else(|| Error::Protocol {
                reason: "entrée de trousseau OAuth2 sans `client_id`".to_owned(),
            })?,
            client_secret: field("client_secret"),
            refresh: field("refresh_token").ok_or_else(|| Error::Protocol {
                reason: "entrée de trousseau OAuth2 sans `refresh_token`".to_owned(),
            })?,
            access: field("access_token"),
            refresh_after: json.get("refresh_after").and_then(|it| it.as_i64()),
        })
    }

    /// Les identifiants client, pour un échange ou un rafraîchissement.
    #[must_use]
    pub fn credentials(&self) -> ClientCredentials {
        ClientCredentials {
            client_id: self.client_id.clone(),
            client_secret: self.client_secret.clone(),
        }
    }
}

/// Enregistre ce qu'on garde d'un compte OAuth2.
///
/// # Errors
///
/// [`Error::Empty`] si le jeton de rafraîchissement est vide — un enregistrement qui réussit
/// sans jeton exploitable est le pire des deux mondes. [`Error::Unavailable`] si le trousseau
/// ne répond pas.
pub fn store_oauth2(host: &str, username: &str, record: &Oauth2Record) -> Result<()> {
    if record.refresh.is_empty() || record.client_id.is_empty() {
        return Err(Error::Empty);
    }
    entry(host, username)?
        .set_password(&record.to_json())
        .map_err(|source| Error::Unavailable(source.to_string()))?;
    tracing::info!(
        host,
        username,
        "jetons OAuth2 enregistrés dans le trousseau"
    );
    Ok(())
}

/// Relit ce qu'on garde d'un compte OAuth2.
///
/// # Errors
///
/// [`Error::NotFound`] si le compte n'a jamais consenti, [`Error::Protocol`] si l'entrée est
/// abîmée, [`Error::Unavailable`] si le trousseau ne répond pas.
pub fn load_oauth2(host: &str, username: &str) -> Result<Oauth2Record> {
    match entry(host, username)?.get_password() {
        Ok(text) => Oauth2Record::from_json(&text),
        Err(keyring::Error::NoEntry) => Err(Error::NotFound {
            host: host.to_owned(),
            username: username.to_owned(),
        }),
        Err(source) => Err(Error::Unavailable(source.to_string())),
    }
}

/// Oublie ce qu'on garde d'un compte OAuth2. Idempotent.
///
/// ## Ça ne révoque rien chez le fournisseur
///
/// Le jeton de rafraîchissement reste valide côté serveur : on l'a seulement effacé de cette
/// machine. La révocation se fait dans le compte du fournisseur, et le CLI le dit à
/// l'utilisateur plutôt que de laisser croire le contraire.
///
/// # Errors
///
/// [`Error::Unavailable`] si le trousseau ne répond pas.
pub fn forget_oauth2(host: &str, username: &str) -> Result<()> {
    match entry(host, username)?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => {
            tracing::info!(host, username, "jetons OAuth2 oubliés");
            Ok(())
        }
        Err(source) => Err(Error::Unavailable(source.to_string())),
    }
}

/// Vrai si un compte a des jetons OAuth2 enregistrés.
///
/// Ne rend pas les jetons, pour la même raison qu'[`crate::is_stored`].
#[must_use]
pub fn has_oauth2(host: &str, username: &str) -> bool {
    load_oauth2(host, username).is_ok()
}

/// Rend un jeton d'accès utilisable, en le rafraîchissant si besoin.
///
/// C'est l'unique point d'entrée pour synchroniser un compte OAuth2 : l'appelant ne sait pas si
/// un rafraîchissement a eu lieu, et n'a pas à le savoir.
///
/// ## Le nouveau jeton est réécrit avant d'être rendu
///
/// Si l'écriture au trousseau échoue, l'appel **échoue** au lieu de rendre le jeton quand même.
/// Un jeton rendu mais pas enregistré marche une fois, puis la synchronisation suivante en
/// redemande un : le compte fonctionne en apparence tout en consommant un rafraîchissement par
/// passe, et le quota du fournisseur finit par répondre à la place du diagnostic.
///
/// ## Le jeton de rafraîchissement n'est remplacé que s'il en vient un nouveau
///
/// Google n'en renvoie pas au rafraîchissement. Le remplacer par `None` effacerait le seul
/// secret qui vaille, et le compte demanderait un consentement complet à la passe suivante.
///
/// # Errors
///
/// [`Error::NotFound`] si le compte n'a jamais consenti, [`Error::Consent`] si le jeton de
/// rafraîchissement est mort — l'utilisateur doit reconsentir, réessayer ne sert à rien.
pub fn access_token(host: &str, username: &str, now: i64) -> Result<String> {
    let mut record = load_oauth2(host, username)?;
    if record.access_usable_at(now) {
        // Pas de trace : dire « jeton pris au cache » à chaque synchronisation ne renseigne
        // personne et rend le journal illisible là où le rafraîchissement, lui, compte.
        return record.access.ok_or(Error::Empty);
    }

    let provider = Provider::for_host(host).ok_or_else(|| Error::Provider {
        code: "unknown_provider".to_owned(),
        description: format!(
            "aucun fournisseur OAuth2 connu pour {host} — un compte OAuth2 a besoin d'un point \
             de terminaison de jeton, qui n'est pas configurable"
        ),
    })?;

    let credentials = record.credentials();
    let fields = oauth::refresh_fields(&credentials, &record.refresh);
    let response = http::post_form_tls(
        provider.token_host,
        TOKEN_PORT,
        provider.token_path,
        &fields,
    )?;
    let access = apply_tokens(&mut record, response.status, &response.body, now)?;
    store_oauth2(host, username, &record)?;
    tracing::info!(host, username, "jeton d'accès rafraîchi");
    Ok(access)
}

/// Range la réponse du fournisseur dans l'enregistrement, et rend le jeton d'accès.
///
/// ## Pourquoi c'est une fonction à part
///
/// Parce que le reste du chemin ne l'est pas testable : `access_token` ouvre le trousseau puis
/// parle en TLS à un hôte réel. Cette fonction-là ne fait que la partie qui décide — quoi
/// garder, quoi remplacer, quoi ne pas toucher — et c'est celle qui a un comportement
/// **propre à chaque fournisseur**.
///
/// ## Le jeton de rafraîchissement de Microsoft tourne, celui de Google non
///
/// Google ne renvoie pas de nouveau jeton de rafraîchissement : celui du premier consentement
/// vaut jusqu'à révocation. Microsoft en renvoie un **à chaque rafraîchissement**, et
/// l'ancien cesse de valoir. Ne pas garder le nouveau donne une panne qui n'arrive pas tout
/// de suite : le compte marche, puis s'arrête un jour, sans que rien ait changé de notre
/// côté.
///
/// C'est la raison d'être de cette fonction et de ses tests. Le chemin Microsoft n'a jamais
/// tourné contre un vrai serveur — `docs/PHASE-2.md` — et c'est le maximum de ce qu'on peut
/// en éprouver sans compte.
///
/// ## Une réponse en échec ne touche à rien
///
/// L'enregistrement est laissé **intact** : un rafraîchissement refusé par le fournisseur ne
/// doit pas effacer le jeton qui, la seconde d'avant, marchait. C'est ce qui fait qu'une panne
/// passagère du fournisseur ne se transforme pas en consentement à redemander.
///
/// # Errors
///
/// Ce que rend [`oauth::parse_token_response`].
fn apply_tokens(record: &mut Oauth2Record, status: u16, body: &str, now: i64) -> Result<String> {
    let tokens = oauth::parse_token_response(status, body)?;

    record.access = Some(tokens.access.clone());
    record.refresh_after = tokens.refresh_after(now);
    if let Some(new_refresh) = tokens.refresh {
        record.refresh = new_refresh;
    }
    Ok(tokens.access)
}

/// Le secret à présenter au serveur, quel que soit le mécanisme.
///
/// Rend un mot de passe pour un compte `password`, un jeton d'accès frais pour un compte
/// `oauth2`. C'est ce que le CLI et le job de fond du démon appellent tous les deux, et c'est
/// **le seul** endroit où le mécanisme décide de l'entrée de trousseau à ouvrir.
///
/// ## Pourquoi une étiquette et pas un type
///
/// L'étiquette est celle de `mailcore::AuthKind::as_str`, qui est une fonction totale sur une
/// énumération déjà validée à la lecture du store : elle ne peut valoir que `password` ou
/// `oauth2`. Prendre un type demanderait à ce crate de dépendre de `mailcore`, donc de tirer
/// SQLite, tantivy et l'analyseur MIME dans l'arbre de dépendances du crate qui garde les
/// secrets — l'inverse de ce que dit l'en-tête de `lib.rs`.
///
/// Le prix est une étiquette au lieu d'un type à la frontière. Il est payé en refusant
/// explicitement tout ce qui n'est pas connu, exactement comme `AuthKind::parse` : **aucun
/// repli**, parce que se tromper de mécanisme, c'est envoyer un secret dans un champ qui ne
/// l'attend pas.
///
/// # Errors
///
/// [`Error::UnknownMechanism`] sur une étiquette inconnue. Sinon, ce que rendent
/// [`crate::load`] ou [`access_token`].
pub fn secret_for(host: &str, username: &str, mechanism: &str, now: i64) -> Result<String> {
    match mechanism {
        "password" => crate::load(host, username),
        "oauth2" => access_token(host, username, now),
        other => Err(Error::UnknownMechanism {
            found: other.to_owned(),
        }),
    }
}

/// Vrai si le trousseau porte déjà de quoi authentifier ce compte.
///
/// ## Pourquoi une question séparée de [`secret_for`]
///
/// Elle sert à **ne pas redemander** ce qu'on a déjà. Le cas est arrivé le 2026-09-11 : le store
/// avait été vidé, les cinq entrées du trousseau étaient intactes, et redéclarer un compte
/// aurait refait un consentement OAuth2 complet — navigateur, identifiant client, écran du
/// fournisseur — pour aboutir au jeton déjà rangé.
///
/// Elle ne rend **pas** le secret, comme [`crate::is_stored`] : afficher l'état d'un compte ne
/// demande pas de le lire, et une fonction qui lit est une fonction qui peut le laisser fuir.
///
/// Un mécanisme inconnu rend `false` plutôt qu'une erreur : pour un affichage, « on ne sait
/// pas » et « il n'y en a pas » se présentent pareil, et l'appel qui compte — [`secret_for`] —
/// refusera clairement. C'est le même arbitrage que [`crate::is_stored`] sur un trousseau
/// indisponible.
#[must_use]
pub fn has_secret(host: &str, username: &str, mechanism: &str) -> bool {
    match mechanism {
        "password" => crate::is_stored(host, username),
        "oauth2" => has_oauth2(host, username),
        _ => false,
    }
}

/// Ce qu'un consentement demande à l'appelant d'afficher.
///
/// Un `&dyn Fn` et non un journal : cette URL doit arriver **sous les yeux** de l'utilisateur,
/// parce que l'ouverture du navigateur peut échouer et qu'il devra alors la coller à la main.
/// `tracing` peut être configuré pour n'écrire nulle part ; la sortie du CLI, non.
pub type Announce<'a> = &'a dyn Fn(&str);

/// Déroule un consentement complet et rend des jetons neufs.
///
/// Ouvre un port de bouclage, construit l'URL, ouvre le navigateur, attend le code, l'échange.
///
/// ## L'absence de jeton de rafraîchissement est une erreur, pas un détail
///
/// Un premier échange sans `refresh_token` donne un compte qui marche une heure puis s'arrête,
/// et le diagnostic arrive une heure plus tard sous la forme d'un refus d'authentification.
/// Chez Google ça vient d'un `access_type` manquant ou d'un consentement déjà accordé sans
/// `prompt=consent` — les deux sont couverts par [`oauth::authorization_url`], donc si ça
/// arrive quand même, il faut le dire tout de suite.
///
/// # Errors
///
/// [`Error::Consent`] si l'utilisateur refuse, si le délai passe, ou si le fournisseur ne rend
/// pas de jeton de rafraîchissement. [`Error::Network`], [`Error::Tls`], [`Error::Protocol`],
/// [`Error::Provider`] selon l'étape.
/// ## `redirect_port`
///
/// `None` — le cas normal — laisse le système choisir un port éphémère, ce que Google autorise
/// explicitement pour le bouclage. `Some(port)` est la sortie de secours pour un fournisseur qui
/// comparerait l'URI de redirection **avec** son port : voir [`crate::consent::Loopback::open`].
pub fn authorize(
    provider: &Provider,
    credentials: &ClientCredentials,
    login_hint: &str,
    announce: Announce<'_>,
    redirect_port: Option<u16>,
) -> Result<Tokens> {
    let pkce = oauth::Pkce::new()?;
    let state = oauth::random_state()?;
    let loopback = crate::consent::Loopback::open(redirect_port)?;
    let redirect_uri = loopback.redirect_uri();

    let url = oauth::authorization_url(
        provider,
        credentials,
        &redirect_uri,
        &pkce,
        &state,
        login_hint,
    );
    announce(&url);

    // Un navigateur qui ne s'ouvre pas n'est **pas** un échec du consentement : l'URL vient
    // d'être affichée, l'utilisateur peut la coller. Échouer ici casserait le cas d'une session
    // distante ou d'une machine sans navigateur par défaut.
    if let Err(error) = crate::consent::open_browser(&url) {
        tracing::warn!(%error, "navigateur non ouvert — l'URL a été affichée");
    }

    let code = loopback.wait_for_code(&state)?;

    let fields = oauth::exchange_fields(credentials, &code, &redirect_uri, &pkce);
    let response = http::post_form_tls(
        provider.token_host,
        TOKEN_PORT,
        provider.token_path,
        &fields,
    )?;
    let tokens = oauth::parse_token_response(response.status, &response.body)?;

    if tokens.refresh.is_none() {
        return Err(Error::Consent {
            reason: "le fournisseur n'a pas rendu de jeton de rafraîchissement : le compte \
                     cesserait de fonctionner dans une heure. Révoquer l'accès de cette \
                     application dans le compte du fournisseur, puis recommencer."
                .to_owned(),
        });
    }
    tracing::info!(
        provider = provider.token_host,
        "consentement obtenu, jetons échangés"
    );
    Ok(tokens)
}

/// Enregistre le résultat d'un consentement.
///
/// # Errors
///
/// [`Error::Empty`] si les jetons n'ont pas de quoi tenir dans le temps, [`Error::Unavailable`]
/// si le trousseau ne répond pas.
pub fn store_tokens(
    host: &str,
    username: &str,
    credentials: &ClientCredentials,
    tokens: &Tokens,
    now: i64,
) -> Result<()> {
    let record = Oauth2Record {
        client_id: credentials.client_id.clone(),
        client_secret: credentials.client_secret.clone(),
        refresh: tokens.refresh.clone().ok_or(Error::Empty)?,
        access: Some(tokens.access.clone()),
        refresh_after: tokens.refresh_after(now),
    };
    store_oauth2(host, username, &record)
}

/// L'entrée de trousseau OAuth2 d'un compte.
fn entry(host: &str, username: &str) -> Result<keyring::Entry> {
    let key = format!("{PREFIX}:{username}@{host}");
    keyring::Entry::new(SERVICE, &key).map_err(|source| Error::Unavailable(source.to_string()))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn record() -> Oauth2Record {
        Oauth2Record {
            client_id: "123-abc.apps.googleusercontent.com".to_owned(),
            client_secret: Some("GOCSPX-secret".to_owned()),
            refresh: "1//refresh-token".to_owned(),
            access: Some("ya29.access".to_owned()),
            refresh_after: Some(1_000),
        }
    }

    /// Un compte jetable, qui **se nettoie même si le test panique**.
    ///
    /// La même raison que le garde de `lib.rs` : ces tests écrivent dans le vrai trousseau de
    /// la machine, et 34 entrées de test y traînaient le 2026-09-03 parce qu'un `forget` en
    /// dernière ligne est sauté dès qu'une assertion tombe.
    struct Scratch {
        host: String,
        /// Le **même** verrou que celui des tests de `lib.rs`, et pas un par module : le
        /// trousseau de Windows perd des suppressions sous accès concurrent, et deux verrous
        /// laisseraient `session` et `lib` se marcher dessus. Voir [`crate::KEYRING_LOCK`].
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl Scratch {
        fn new() -> Self {
            Self {
                _lock: crate::keyring_lock(),
                host: format!(
                    "test-jetable-{}-{}.invalid",
                    std::process::id(),
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map_or(0, |d| d.as_nanos())
                ),
            }
        }

        fn available(&self) -> bool {
            if forget_oauth2(&self.host, "sonde").is_err() {
                eprintln!("trousseau indisponible, test ignoré");
                return false;
            }
            true
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            for username in ["marie", "jean"] {
                let _ = forget_oauth2(&self.host, username);
                // **Et l'entrée mot de passe.** Un des tests d'ici en pose une, pour vérifier
                // que les deux mécanismes ne s'écrasent pas ; l'oublier laisserait ce que le
                // test lui-même dénonce.
                let _ = crate::forget(&self.host, username);
            }
        }
    }

    // ------------------------------------------------------------------
    // L'aller-retour JSON. C'est ce format qui est écrit au trousseau : il
    // doit se relire, y compris quand les champs facultatifs manquent.
    // ------------------------------------------------------------------

    #[test]
    fn a_full_record_survives_a_json_round_trip() {
        let original = record();
        let back = Oauth2Record::from_json(&original.to_json()).unwrap();
        assert_eq!(back, original);
    }

    #[test]
    fn a_record_without_a_client_secret_survives_the_round_trip() {
        // Microsoft n'en impose pas pour une application publique. Un `None` qui reviendrait en
        // `Some("")` ferait envoyer un `client_secret` vide, refusé par le fournisseur.
        let mut original = record();
        original.client_secret = None;
        let back = Oauth2Record::from_json(&original.to_json()).unwrap();
        assert_eq!(back.client_secret, None);
        assert_eq!(back, original);
    }

    #[test]
    fn a_record_without_an_expiry_survives_the_round_trip() {
        let mut original = record();
        original.refresh_after = None;
        original.access = None;
        let back = Oauth2Record::from_json(&original.to_json()).unwrap();
        assert_eq!(back, original);
    }

    #[test]
    fn a_record_without_a_refresh_token_is_refused_at_read() {
        // Une entrée sans jeton de rafraîchissement ne sert à rien : mieux vaut le dire que
        // laisser une synchronisation échouer à l'authentification.
        let error = Oauth2Record::from_json(r#"{"client_id":"abc"}"#).unwrap_err();
        assert!(matches!(error, Error::Protocol { .. }), "{error:?}");
        assert!(error.to_string().contains("refresh_token"), "{error}");
    }

    #[test]
    fn a_record_without_a_client_id_is_refused_at_read() {
        let error = Oauth2Record::from_json(r#"{"refresh_token":"1//x"}"#).unwrap_err();
        assert!(matches!(error, Error::Protocol { .. }), "{error:?}");
    }

    #[test]
    fn an_empty_string_counts_as_absent() {
        // Un `refresh_token: ""` réussirait la lecture et échouerait au rafraîchissement, une
        // heure plus tard, sur un message du fournisseur.
        let error =
            Oauth2Record::from_json(r#"{"client_id":"abc","refresh_token":""}"#).unwrap_err();
        assert!(matches!(error, Error::Protocol { .. }), "{error:?}");
    }

    #[test]
    fn a_damaged_entry_is_never_quoted_in_the_error() {
        // **Cette entrée est le secret.** Recopier ce qu'on n'a pas su lire le ferait sortir
        // par le journal, ce qui est exactement le chemin que le critère 6 interdit.
        let secret = "1//jeton-qui-ne-doit-pas-fuiter";
        let damaged = format!("{{\"refresh_token\":\"{secret}\", ceci n'est pas du JSON");
        let message = Oauth2Record::from_json(&damaged).unwrap_err().to_string();
        assert!(!message.contains(secret), "un secret a fui : {message}");
        assert!(!message.contains("1//"), "un fragment a fui : {message}");
    }

    #[test]
    fn reading_never_panics_whatever_the_entry_holds() {
        for candidate in [
            "",
            "null",
            "[]",
            "42",
            "{}",
            r#"{"client_id":42,"refresh_token":true}"#,
            r#"{"client_id":"a","refresh_token":"b","refresh_after":"pas un nombre"}"#,
            "{\u{0}\u{1}",
        ] {
            let _ = Oauth2Record::from_json(candidate);
        }
    }

    // ------------------------------------------------------------------
    // La péremption. C'est elle qui décide s'il faut sortir sur le réseau.
    // ------------------------------------------------------------------

    #[test]
    fn a_token_is_usable_before_its_deadline_and_not_after() {
        let record = record();
        assert!(record.access_usable_at(999));
        assert!(!record.access_usable_at(1_000), "l'échéance est incluse");
        assert!(!record.access_usable_at(10_000));
    }

    #[test]
    fn a_token_without_a_known_deadline_is_tried_rather_than_refreshed() {
        // Rafraîchir par prudence à chaque fois transformerait un fournisseur silencieux sur
        // `expires_in` en un aller-retour de plus par synchronisation.
        let mut record = record();
        record.refresh_after = None;
        assert!(record.access_usable_at(i64::MAX));
    }

    #[test]
    fn an_absent_or_empty_access_token_is_not_usable() {
        let mut record = record();
        record.access = None;
        assert!(!record.access_usable_at(0));
        record.access = Some(String::new());
        assert!(!record.access_usable_at(0));
    }

    // ------------------------------------------------------------------
    // Le trousseau.
    // ------------------------------------------------------------------

    #[test]
    fn a_record_survives_a_keyring_round_trip() {
        let scratch = Scratch::new();
        if !scratch.available() {
            return;
        }
        store_oauth2(&scratch.host, "marie", &record()).unwrap();
        assert_eq!(load_oauth2(&scratch.host, "marie").unwrap(), record());
        assert!(has_oauth2(&scratch.host, "marie"));

        forget_oauth2(&scratch.host, "marie").unwrap();
        assert!(matches!(
            load_oauth2(&scratch.host, "marie"),
            Err(Error::NotFound { .. })
        ));
    }

    #[test]
    fn has_secret_answers_for_each_mechanism_and_refuses_to_guess() {
        let scratch = Scratch::new();
        if !scratch.available() {
            return;
        }
        // Rien au départ, pour les deux mécanismes.
        assert!(!has_secret(&scratch.host, "marie", "password"));
        assert!(!has_secret(&scratch.host, "marie", "oauth2"));

        store_oauth2(&scratch.host, "marie", &record()).unwrap();
        assert!(has_secret(&scratch.host, "marie", "oauth2"));
        // **Le contrôle qui compte** : un jeton OAuth2 n'est pas un mot de passe. Répondre
        // « oui » ici ferait déclarer un compte en `password` qui échouerait à la connexion.
        assert!(!has_secret(&scratch.host, "marie", "password"));

        crate::store(&scratch.host, "jean", "secret").unwrap();
        assert!(has_secret(&scratch.host, "jean", "password"));
        assert!(!has_secret(&scratch.host, "jean", "oauth2"));

        // Un mécanisme inconnu ne se replie sur rien.
        assert!(!has_secret(&scratch.host, "marie", "ntlm"));

        forget_oauth2(&scratch.host, "marie").unwrap();
        let _ = crate::forget(&scratch.host, "jean");
    }

    #[test]
    fn two_accounts_on_the_same_host_keep_their_own_tokens() {
        let scratch = Scratch::new();
        if !scratch.available() {
            return;
        }
        let mut other = record();
        other.refresh = "1//celui-de-jean".to_owned();

        store_oauth2(&scratch.host, "marie", &record()).unwrap();
        store_oauth2(&scratch.host, "jean", &other).unwrap();

        assert_eq!(load_oauth2(&scratch.host, "marie").unwrap(), record());
        assert_eq!(load_oauth2(&scratch.host, "jean").unwrap(), other);

        forget_oauth2(&scratch.host, "marie").unwrap();
        assert!(
            has_oauth2(&scratch.host, "jean"),
            "oublier un compte a effacé l'autre"
        );
    }

    #[test]
    fn an_oauth2_entry_does_not_collide_with_a_password_entry() {
        // Un compte peut passer du mot de passe applicatif à OAuth2 — Google a coupé les
        // premiers pour certains comptes. Les deux entrées doivent coexister pendant la
        // migration.
        let scratch = Scratch::new();
        if !scratch.available() {
            return;
        }
        crate::store(&scratch.host, "marie", "mot-de-passe").unwrap();
        store_oauth2(&scratch.host, "marie", &record()).unwrap();

        assert_eq!(crate::load(&scratch.host, "marie").unwrap(), "mot-de-passe");
        assert_eq!(load_oauth2(&scratch.host, "marie").unwrap(), record());

        forget_oauth2(&scratch.host, "marie").unwrap();
        assert!(
            crate::is_stored(&scratch.host, "marie"),
            "oublier les jetons a effacé le mot de passe"
        );
        crate::forget(&scratch.host, "marie").unwrap();
    }

    #[test]
    fn an_empty_refresh_token_is_refused_at_write() {
        let scratch = Scratch::new();
        let mut empty = record();
        empty.refresh = String::new();
        assert!(matches!(
            store_oauth2(&scratch.host, "marie", &empty),
            Err(Error::Empty)
        ));
    }

    #[test]
    fn a_cached_token_is_returned_without_touching_the_network() {
        // Le contrôle est indirect mais net : l'hôte est en `.invalid`, donc **aucune**
        // connexion ne peut aboutir. Un `access_token` qui rendrait le jeton du cache prouve
        // qu'il n'est pas sorti ; s'il sortait, l'appel échouerait.
        let scratch = Scratch::new();
        if !scratch.available() {
            return;
        }
        store_oauth2(&scratch.host, "marie", &record()).unwrap();
        assert_eq!(
            access_token(&scratch.host, "marie", 999).unwrap(),
            "ya29.access"
        );
    }

    #[test]
    fn an_unknown_host_is_refused_rather_than_guessed() {
        // Le point de terminaison de jeton n'est **pas** configurable : c'est le moyen le plus
        // simple de faire envoyer un jeton de rafraîchissement ailleurs. Un hôte inconnu doit
        // donc être refusé, pas deviné.
        let scratch = Scratch::new();
        if !scratch.available() {
            return;
        }
        let mut expired = record();
        expired.refresh_after = Some(0);
        store_oauth2(&scratch.host, "marie", &expired).unwrap();

        let error = access_token(&scratch.host, "marie", 10_000).unwrap_err();
        match error {
            Error::Provider { ref code, .. } => assert_eq!(code, "unknown_provider"),
            other => panic!("issue inattendue : {other:?}"),
        }
        assert!(!error.retryable(), "un hôte inconnu ne se réessaie pas");
    }

    #[test]
    fn an_absent_account_says_it_rather_than_refreshing_nothing() {
        let scratch = Scratch::new();
        assert!(matches!(
            access_token(&scratch.host, "marie", 0),
            Err(Error::NotFound { .. }) | Err(Error::Unavailable(_))
        ));
    }

    // ------------------------------------------------------------------
    // Ce qui ne doit jamais sortir.
    // ------------------------------------------------------------------

    #[test]
    fn the_debug_impl_shows_no_token() {
        // Un `Debug` de structure finit toujours par passer dans un `tracing::debug!`. La
        // dérivation afficherait les trois secrets.
        let shown = format!("{:?}", record());
        for secret in ["1//refresh-token", "ya29.access", "GOCSPX-secret"] {
            assert!(!shown.contains(secret), "{secret} apparaît dans {shown}");
        }
        // Le contrôle positif : un `Debug` vide passerait l'assertion ci-dessus sans rien
        // prouver. L'identifiant client, qui n'est pas un secret, doit être là.
        assert!(shown.contains("apps.googleusercontent.com"), "{shown}");
    }

    #[test]
    fn a_token_is_never_in_an_error_message() {
        let record = record();
        for message in [
            Error::NotFound {
                host: "imap.gmail.com".to_owned(),
                username: "marie@gmail.com".to_owned(),
            }
            .to_string(),
            Error::Empty.to_string(),
            Error::Consent {
                reason: "invalid_grant : Token has been expired or revoked.".to_owned(),
            }
            .to_string(),
        ] {
            assert!(!message.contains(&record.refresh), "{message}");
            assert!(
                !message.contains(record.access.as_deref().unwrap_or("")),
                "{message}"
            );
        }
    }

    #[test]
    fn a_dead_refresh_token_asks_for_a_consent_and_not_a_retry() {
        // Insister sur un jeton révoqué fait bloquer le client chez certains fournisseurs. La
        // distinction est portée par `Error::retryable`, et c'est elle que la synchronisation
        // périodique lit.
        let error = oauth::parse_token_response(
            400,
            r#"{"error":"invalid_grant","error_description":"Token has been expired or revoked."}"#,
        )
        .unwrap_err();
        assert!(matches!(error, Error::Consent { .. }), "{error:?}");
        assert!(!error.retryable());
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod rotation_tests {
    //! Ce qu'une réponse du fournisseur fait à l'enregistrement — le **seul** endroit du chemin
    //! de rafraîchissement qui ait un comportement propre à chaque fournisseur.
    //!
    //! Le reste — ouvrir le trousseau, parler en TLS à un hôte réel — n'est pas testable, et
    //! c'est exactement pourquoi la décision est isolée ici.

    use super::{Oauth2Record, apply_tokens};

    fn record() -> Oauth2Record {
        Oauth2Record {
            client_id: "app-id".to_owned(),
            client_secret: None,
            refresh: "ANCIEN-jeton-de-rafraichissement".to_owned(),
            access: Some("ancien-acces".to_owned()),
            refresh_after: Some(1_000),
        }
    }

    #[test]
    fn a_rotated_refresh_token_replaces_the_old_one() {
        // **Le cas Microsoft.** Il renvoie un nouveau jeton de rafraîchissement à chaque
        // rafraîchissement, et l'ancien cesse de valoir. Garder l'ancien donne une panne
        // différée : le compte marche, puis s'arrête un jour sans que rien ait changé chez
        // nous. C'est la panne la moins déboguable qui soit.
        let mut it = record();
        let body =
            r#"{"access_token":"nouveau-acces","refresh_token":"NOUVEAU-jeton","expires_in":3599}"#;

        let access = apply_tokens(&mut it, 200, body, 10_000).unwrap();

        assert_eq!(access, "nouveau-acces");
        assert_eq!(it.access.as_deref(), Some("nouveau-acces"));
        assert_eq!(
            it.refresh, "NOUVEAU-jeton",
            "le jeton de rafraîchissement de Microsoft n'a pas été gardé"
        );
        assert!(
            it.refresh_after.is_some_and(|at| at > 10_000),
            "l'échéance n'a pas été reportée : {:?}",
            it.refresh_after
        );
    }

    #[test]
    fn an_absent_refresh_token_leaves_the_old_one_in_place() {
        // **Le cas Google.** Il n'en renvoie pas : celui du premier consentement vaut jusqu'à
        // révocation. L'effacer demanderait un nouveau consentement à chaque heure.
        let mut it = record();
        let body = r#"{"access_token":"ya29.nouveau","expires_in":3599}"#;

        apply_tokens(&mut it, 200, body, 10_000).unwrap();

        assert_eq!(it.refresh, "ANCIEN-jeton-de-rafraichissement");
        assert_eq!(it.access.as_deref(), Some("ya29.nouveau"));
    }

    #[test]
    fn a_refusal_leaves_the_record_untouched() {
        // Un rafraîchissement refusé ne doit pas effacer le jeton qui marchait la seconde
        // d'avant : une panne passagère du fournisseur se transformerait en consentement à
        // redemander à l'utilisateur.
        let mut it = record();
        let before = (it.refresh.clone(), it.access.clone(), it.refresh_after);
        let body = r#"{"error":"temporarily_unavailable"}"#;

        let refused = apply_tokens(&mut it, 503, body, 10_000);

        assert!(refused.is_err(), "un 503 devait remonter");
        assert_eq!(
            (it.refresh.clone(), it.access.clone(), it.refresh_after),
            before,
            "l'enregistrement a été abîmé par un refus"
        );
    }

    #[test]
    fn an_empty_rotated_token_does_not_erase_the_old_one() {
        // Un fournisseur qui rend `"refresh_token": ""` dit « rien », pas « oublie le tien ».
        // C'est `parse_token_response` qui le normalise ; ce test verrouille la conséquence
        // ici, là où l'effacement aurait lieu.
        let mut it = record();
        let body = r#"{"access_token":"a","refresh_token":"","expires_in":60}"#;

        apply_tokens(&mut it, 200, body, 0).unwrap();

        assert_eq!(it.refresh, "ANCIEN-jeton-de-rafraichissement");
    }

    #[test]
    fn a_body_that_is_not_json_leaves_the_record_untouched_and_never_panics() {
        let mut it = record();
        let before = it.refresh.clone();
        for body in ["", "<html>503</html>", "{", "null", "[]"] {
            let _ = apply_tokens(&mut it, 200, body, 0);
        }
        assert_eq!(it.refresh, before);
    }
}
