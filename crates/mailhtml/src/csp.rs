//! La politique de sécurité de contenu du rendu d'un message. **Source unique.**
//!
//! Cette constante est la première des deux barrières de `docs/PRIVACY.md`. Elle est
//! appliquée par le moteur de rendu, pas par du code applicatif qu'on pourrait oublier
//! d'exécuter sur un chemin détourné.
//!
//! Elle vit ici, et nulle part ailleurs. Une deuxième copie dans le front — même
//! identique le jour où elle est écrite — est exactement le point de défaillance unique
//! que `docs/PRIVACY.md` cherche à éviter. Le front la lit depuis le démon.

/// La CSP appliquée à l'`<iframe>` qui rend le corps d'un message.
///
/// Conséquence de `default-src 'none'` plus `connect-src 'none'` : images distantes,
/// feuilles de style externes, polices web, pixels espions et requêtes de fond ne sont
/// pas « non affichés », ils ne sont **pas requêtés**. Aucun paquet ne quitte la machine.
pub const MESSAGE_CSP: &str = "default-src 'none'; \
     img-src 'self' data: blob:; \
     style-src 'unsafe-inline'; \
     script-src 'none'; \
     frame-src 'none'; \
     connect-src 'none'; \
     font-src 'self'; \
     form-action 'none'";

/// Les attributs `sandbox` autorisés sur cette `<iframe>`, et rien d'autre.
///
/// Pas de `allow-scripts`, pas de `allow-same-origin`, pas de `allow-forms`.
/// `allow-popups-to-escape-sandbox` est nécessaire pour qu'un clic sur un lien s'ouvre
/// dans le navigateur système plutôt que dans le webview du client.
pub const MESSAGE_SANDBOX: &str = "allow-popups allow-popups-to-escape-sandbox";

#[cfg(test)]
mod tests {
    use super::*;

    /// Verrou. Si une directive disparaît, ce test tombe avant la fusion.
    #[test]
    fn csp_blocks_every_egress_vector() {
        for directive in [
            "default-src 'none'",
            "script-src 'none'",
            "connect-src 'none'",
            "frame-src 'none'",
            "form-action 'none'",
        ] {
            assert!(
                MESSAGE_CSP.contains(directive),
                "directive manquante dans la CSP : {directive}"
            );
        }
    }

    #[test]
    fn csp_never_allows_a_remote_origin() {
        // Aucune source distante ne doit pouvoir se glisser dans la politique.
        assert!(!MESSAGE_CSP.contains("http:"));
        assert!(!MESSAGE_CSP.contains("https:"));
        assert!(!MESSAGE_CSP.contains('*'));
    }

    #[test]
    fn sandbox_grants_nothing_but_popups() {
        for forbidden in [
            "allow-scripts",
            "allow-same-origin",
            "allow-forms",
            "allow-modals",
            "allow-downloads",
        ] {
            assert!(
                !MESSAGE_SANDBOX.contains(forbidden),
                "permission interdite accordée par le sandbox : {forbidden}"
            );
        }
    }
}
