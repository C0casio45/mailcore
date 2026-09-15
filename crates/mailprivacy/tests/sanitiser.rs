//! **Critère 8, étage 1** : l'assainisseur, sans moteur de rendu.
//!
//! Un serveur HTTP local instrumenté, le message piégé de [`mailprivacy::trapped_message`],
//! l'assainissement, et deux assertions : plus aucune URL distante là où un moteur irait la
//! chercher, et le serveur a reçu **exactement zéro requête**.
//!
//! ## Ce que cet étage prouve, et ce qu'il ne prouve pas
//!
//! Il prouve la **deuxième ceinture** — celle que nous écrivons. Il ne dit rien de la première,
//! qui est la CSP appliquée par le moteur : c'est l'étage 2 (`src/main.rs`) qui s'en charge, en
//! rendant le même message dans un vrai webview.
//!
//! Ne pas cocher le critère 8 en voyant seulement ce fichier au vert.
//!
//! ## Pourquoi ce fichier a déménagé
//!
//! Il vivait dans `crates/mailhtml/tests/no_network.rs`. Les deux étages doivent piéger **le
//! même** message : un vecteur ajouté d'un côté et pas de l'autre laisserait un trou que
//! personne ne verrait. `mailhtml` n'a pas à dépendre d'un moteur de rendu, donc c'est le
//! message piégé qui est remonté ici, et cet étage l'a suivi.
//!
//! ## Un zéro qui ne peut pas être autre chose ne prouve rien
//!
//! Un compteur à zéro parce que le serveur est mort dirait exactement la même chose qu'un
//! compteur à zéro parce que rien n'a été demandé. Le dernier test de ce fichier fait donc une
//! requête volontaire et vérifie que le compteur monte. Sans lui, tout le reste serait un test
//! qui se félicite tout seul.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::{Read, Write};
use std::net::TcpStream;

use mailhtml::sanitize::{self, Policy};
use mailhtml::trackers;
use mailprivacy::{Spy, trapped_message};

#[test]
fn a_trapped_message_leaves_no_loadable_remote_url_behind() {
    let spy = Spy::start();
    let host = spy.host();
    let cleaned = sanitize::clean(&trapped_message(&host), Policy::default());

    // **Ce que le critère 8 demande vraiment.** Pas « l'URL n'apparaît nulle part » — un mail
    // a le droit de citer une URL en toutes lettres, et personne ne la charge pour autant —
    // mais « l'URL n'est nulle part où le moteur irait la chercher ».
    //
    // La distinction n'est pas théorique, cette entrée-ci la produit. La contrebande
    // `<scr<script>ipt src="…">` ressort en **texte**, chevron final échappé en `&gt;`. Une
    // assertion textuelle ne saurait pas la distinguer d'un attribut : les guillemets d'un
    // texte ne sont pas échappés, donc `src="http://…"` se lit pareil dans les deux cas.
    // Il faut relire la sortie comme un moteur la relirait, pas comme une chaîne.
    let remaining = trackers::scan(&cleaned.html);
    assert!(
        remaining.is_clean(),
        "il reste de quoi charger : {remaining:?}\n{}",
        cleaned.html
    );

    // Le fragment de contrebande, lui, doit être du texte et le rester. Le `&gt;` est la
    // preuve que l'analyseur l'a traité comme du contenu et non comme du balisage — un
    // chevron encore vivant voudrait dire qu'une balise a été reconstruite.
    if let Some(reste) = cleaned.html.find("imbrique.js") {
        let apres = &cleaned.html[reste..];
        assert!(
            apres.starts_with("imbrique.js\"&gt;"),
            "le fragment de contrebande n'est pas inerte : {:?}",
            &apres[..apres.len().min(40)]
        );
    }

    // Le CSS ne peut plus rien charger : les blocs `<style>` partent entiers et aucune
    // propriété autorisée en attribut ne porte d'`url()`.
    for forme in [
        format!("url(http://{host}"),
        format!("url('http://{host}"),
        format!("url(\"http://{host}"),
    ] {
        assert!(
            !cleaned.html.contains(&forme),
            "une url() CSS a survécu\n{}",
            cleaned.html
        );
    }

    assert!(
        cleaned.html.contains("lien-clique"),
        "le lien a été retiré : il doit rester lisible avant le clic\n{}",
        cleaned.html
    );

    // Le texte, lui, survit : assainir n'est pas mutiler.
    assert!(cleaned.html.contains("voici votre facture"));

    // Et rien n'est parti pendant qu'on assainissait.
    assert_eq!(
        spy.count(),
        0,
        "l'assainissement a lui-même fait une requête"
    );
}

#[test]
fn nothing_executable_survives_the_sanitiser() {
    let spy = Spy::start();
    let cleaned = sanitize::clean(&trapped_message(&spy.host()), Policy::default());
    let html = cleaned.html.to_lowercase();

    for interdit in [
        "<script",
        "<iframe",
        "<form",
        "<object",
        "<embed",
        "<link",
        "<meta",
        "<style",
        "<base",
        "<input",
        "<video",
        "<audio",
        "onload",
        "javascript:",
        "fetch(",
        "@import",
        "@font-face",
        "position:fixed",
        "position: fixed",
    ] {
        assert!(
            !html.contains(interdit),
            "survivant : {interdit}\n{}",
            cleaned.html
        );
    }

    assert_eq!(spy.count(), 0);
}

#[test]
fn the_blocked_resources_are_counted_so_the_user_can_be_told() {
    let spy = Spy::start();
    let raw = trapped_message(&spy.host());

    // Ce que l'assainisseur a retiré.
    let cleaned = sanitize::clean(&raw, Policy::default());
    assert!(
        cleaned.blocked_images >= 3,
        "images bloquées comptées : {}",
        cleaned.blocked_images
    );

    // Ce que le message contenait, pour le bandeau : « bloqué — N traceurs détectés ».
    let report = trackers::scan(&raw);
    assert!(!report.is_clean());
    assert!(
        report
            .trackers
            .iter()
            .any(|t| t.kind == trackers::Kind::Pixel),
        "pixel espion non détecté : {report:?}"
    );
    assert!(
        report
            .trackers
            .iter()
            .any(|t| t.kind == trackers::Kind::CorrelatedId),
        "identifiant corrélé non détecté : {report:?}"
    );

    // Le rapport ne recopie pas l'adresse du destinataire qu'il vient de dénoncer.
    assert!(!format!("{report:?}").contains("marie"));

    assert_eq!(spy.count(), 0, "le comptage a fait une requête");
}

#[test]
fn unblocking_restores_the_images_and_nothing_else() {
    // « Afficher les images » ne doit pas être une porte dérobée pour le reste.
    let spy = Spy::start();
    let host = spy.host();
    let cleaned = sanitize::clean(
        &trapped_message(&host),
        Policy {
            allow_remote_images: true,
        },
    );

    assert!(
        cleaned.html.contains("logo.png"),
        "l'image n'est pas revenue"
    );

    for toujours_interdit in [
        "feuille.css",
        "import.css",
        "police.woff2",
        "script.js",
        "formulaire",
        "cadre",
        "video.mp4",
    ] {
        assert!(
            !cleaned.html.contains(toujours_interdit),
            "le déblocage des images a laissé passer {toujours_interdit}"
        );
    }

    assert_eq!(spy.count(), 0);
}

#[test]
fn the_spy_would_actually_notice_a_request() {
    // Le test qui valide les autres. Sans lui, tous les `assert_eq!(spy.count(), 0)`
    // ci-dessus passeraient aussi bien avec un serveur mort.
    let spy = Spy::start();
    assert_eq!(spy.count(), 0);

    let mut stream = TcpStream::connect(spy.address).expect("connexion au serveur instrumenté");
    stream
        .write_all(b"GET /pixel.gif HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
        .expect("requête");
    stream.flush().expect("vidage");
    let mut response = Vec::new();
    let _ = stream.read_to_end(&mut response);

    assert_eq!(spy.count(), 1, "le compteur ne compte rien");
}
