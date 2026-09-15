//! **Critère 8, étage 2** : le message piégé rendu dans un **vrai moteur de rendu**.
//!
//! ## Ce que cet étage prouve, et que l'étage 1 ne prouve pas
//!
//! `docs/PRIVACY.md` le dit : « la protection est appliquée par le moteur de rendu (CSP,
//! `sandbox`), jamais par du code applicatif qu'on pourrait oublier d'exécuter sur un chemin
//! détourné. Le code applicatif (assainissement) est la deuxième ceinture, pas la première. »
//!
//! L'étage 1 met à l'épreuve la deuxième ceinture. Celui-ci met à l'épreuve **la première**, et
//! il le fait dans le moteur du système — WebView2 sur Windows, WKWebView sur macOS, WebKitGTK
//! sur Linux. Aucune simulation : le document est confié au moteur, avec la CSP et le `sandbox`
//! que `mailhtml::csp` fournit, et le serveur instrumenté compte ce qui sort.
//!
//! ## Trois phases, et l'ordre n'est pas décoratif
//!
//! | Phase | Document | CSP | `sandbox` | Attendu |
//! |---|---|---|---|---|
//! | **A** — contrôle | brut, non assaini | aucune | permissif | **au moins une requête** |
//! | **B** — la CSP seule | brut, non assaini | `MESSAGE_CSP` | `MESSAGE_SANDBOX` | **zéro** |
//! | **C** — les deux ceintures | assaini | `MESSAGE_CSP` | `MESSAGE_SANDBOX` | **zéro** |
//!
//! **A est le contrôle positif, et il est indispensable.** Sans lui, un zéro en B et en C
//! pourrait vouloir dire « le moteur n'a rien chargé parce qu'il n'a rien rendu » — un webview
//! mal monté, une page jamais chargée, un port fermé. A prouve que le moteur *irait* chercher
//! ces ressources et que le serveur *les verrait*. Une exécution où A ne compte rien est une
//! exécution invalide, et le programme le dit au lieu de se féliciter.
//!
//! **B est la phase qui prouve le critère.** Le document y est celui de l'attaquant, intact :
//! seule la CSP l'empêche de sortir. C'est exactement ce que `docs/PRIVACY.md` demande de
//! vérifier, et c'est ce que l'étage 1 ne peut pas dire.
//!
//! **C est la configuration réelle** — celle que les coquilles servent, avec les deux
//! ceintures. Elle doit être à zéro aussi, évidemment, mais son zéro est moins informatif que
//! celui de B.
//!
//! ## Ce que cet étage ne couvre pas
//!
//! Il met à l'épreuve le **document du message**, pas l'application autour. « Zéro requête vers
//! une destination autre que le démon » — l'autre moitié du critère 8 — se vérifie ailleurs :
//! par les tests de parité des politiques de `mail-ui`, et par le fait que la coquille native
//! n'embarque aucun code capable d'émettre une requête (`docs/PRIVACY.md`, §5).
//!
//! Il ne couvre qu'**un moteur à la fois** : celui du système où il tourne. Le relevé nomme
//! lequel, et la CI devra le lancer sur les trois plates-formes pour que le critère soit tenu
//! partout.
//!
//! ## Pourquoi un binaire et pas un `#[test]`
//!
//! Un webview veut un fil d'événements sur le **fil principal** du processus. Le harnais de
//! test de Rust exécute chaque test sur un fil secondaire. La CI lance donc ce programme comme
//! une étape, et son code de sortie bloque la fusion exactement comme un test qui tombe.

#![forbid(unsafe_code)]

use std::time::{Duration, Instant};

use mailhtml::csp::{MESSAGE_CSP, MESSAGE_SANDBOX};
use mailhtml::sanitize::{self, Policy};
use mailprivacy::Spy;
use tao::event::{Event, StartCause, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoop, EventLoopBuilder, EventLoopProxy};
use tao::window::WindowBuilder;
use wry::WebViewBuilder;

/// Temps laissé au moteur pour aller chercher ce qu'il croit pouvoir charger.
///
/// Généreux : une requête sur le bouclage part en microsecondes, mais le moteur peut décider de
/// différer un chargement d'image ou une police. Un délai trop court transformerait un vrai
/// blocage en faux négatif — et le contrôle positif de la phase A est ce qui vérifie que ce
/// délai suffit.
const SETTLE: Duration = Duration::from_millis(2_500);

/// Au-delà, le moteur n'a pas répondu et l'exécution est invalide.
/// Le contrôle final doit lui aussi charger : si le moteur meurt en cours de route, la
/// phase D le dit.
const PATIENCE: Duration = Duration::from_secs(60);

/// Les phases, dans l'ordre. Voir l'en-tête du module.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Control,
    CspOnly,
    BothBelts,
    /// Le contrôle refait **en dernier**, et il n'est pas décoratif.
    ///
    /// Le premier contrôle prouve que le moteur chargeait *au début* de l'exécution. Celui-ci
    /// prouve qu'il chargeait encore *à la fin* — donc que les zéros des phases gardées ne
    /// viennent pas d'un moteur mort en cours de route, d'une fenêtre fermée, ou d'un
    /// séquencement de phases cassé. C'est le contrôle qui aurait attrapé le défaut de
    /// séquencement trouvé en relecture le 2026-09-02.
    ControlAgain,
}

impl Phase {
    /// Le nom court utilisé dans le relevé, et le préfixe des URL de la phase.
    const fn label(self) -> &'static str {
        match self {
            Self::Control => "A-controle",
            Self::CspOnly => "B-csp-seule",
            Self::BothBelts => "C-deux-ceintures",
            Self::ControlAgain => "D-controle-final",
        }
    }

    /// Vrai si la phase doit charger — les deux contrôles.
    const fn is_control(self) -> bool {
        matches!(self, Self::Control | Self::ControlAgain)
    }

    /// La phase suivante, ou `None` à la fin.
    const fn next(self) -> Option<Self> {
        match self {
            Self::Control => Some(Self::CspOnly),
            Self::CspOnly => Some(Self::BothBelts),
            Self::BothBelts => Some(Self::ControlAgain),
            Self::ControlAgain => None,
        }
    }

    /// Retrouve une phase depuis son nom, tel que la page le renvoie.
    fn from_label(label: &str) -> Option<Self> {
        [
            Self::Control,
            Self::CspOnly,
            Self::BothBelts,
            Self::ControlAgain,
        ]
        .into_iter()
        .find(|phase| phase.label() == label)
    }
}

/// Ce que le fil d'événements reçoit de la page hôte et du minuteur.
///
/// Tout passe par des événements utilisateur, et c'est structurel : le gestionnaire d'IPC ne
/// peut pas évaluer un script, puisque c'est le fil d'événements qui possède le webview. Il
/// signale, le fil d'événements agit.
/// **Chaque événement porte sa phase**, et c'est la correction d'un défaut réel.
///
/// Sans étiquette, voici ce qui se passait — et c'est passé au vert pendant une exécution
/// entière avant qu'une relecture le trouve. Le message piégé contient un
/// `<meta http-equiv="refresh">`. En phase de contrôle, il est autorisé, donc le cadre
/// **navigue**, donc l'élément `<iframe>` émet un **deuxième** événement `load`. Deux `load`,
/// deux minuteurs. Le premier clôt la phase A et lance la phase B ; le second, quelques
/// millisecondes plus tard, clôt **la phase B** — dont le cadre venait à peine d'être créé.
///
/// La phase B rendait donc zéro *quoi que fasse la CSP*. C'est exactement la phase dont ce
/// programme dit qu'elle « prouve le critère ».
#[derive(Debug, Clone, Copy)]
enum Wake {
    /// La page hôte est prête à recevoir une phase.
    Ready,
    /// L'`<iframe>` de cette phase a fini de charger.
    Loaded(Phase),
    /// Le délai de décantation de cette phase est écoulé.
    Settled(Phase),
}

/// Écrit la raison et sort en échec.
///
/// **Toute impossibilité est un échec, jamais un test ignoré.** Un webview qu'on n'arrive pas à
/// monter ne prouve pas l'absence de requête : il prouve qu'on n'a rien mesuré. Le critère 8 ne
/// doit pas pouvoir être coché par une exécution qui n'a rien fait.
fn fail(reason: &str) -> ! {
    eprintln!("{reason}");
    eprintln!("Exécution invalide : ne pas cocher le critère 8.");
    std::process::exit(1)
}

/// Le moteur de rendu du système, nommé pour le relevé.
const fn engine() -> &'static str {
    if cfg!(target_os = "windows") {
        "WebView2"
    } else if cfg!(target_os = "macos") {
        "WKWebView"
    } else {
        "WebKitGTK"
    }
}

/// Échappe une chaîne pour l'insérer dans un littéral JavaScript entre apostrophes doubles.
///
/// Écrit à la main et volontairement paranoïaque : le document injecté est **le message de
/// l'attaquant**. Une évasion de littéral ici lui donnerait l'exécution dans la page hôte, ce
/// qui invaliderait la mesure — le moteur chargerait alors les ressources depuis un contexte
/// qui n'est pas celui qu'on teste.
fn as_js_string(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len() + 16);
    out.push('"');
    for character in raw.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            // `</script>` dans une chaîne fermerait la balise qui la contient. Couper le
            // chevron le rend inoffensif sans changer ce que le moteur parsera dans l'iframe.
            '<' => out.push_str("\\u003c"),
            '>' => out.push_str("\\u003e"),
            '&' => out.push_str("\\u0026"),
            // Les séparateurs de ligne Unicode terminent un littéral en JavaScript.
            '\u{2028}' => out.push_str("\\u2028"),
            '\u{2029}' => out.push_str("\\u2029"),
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

/// Le document que le moteur charge : la page hôte, qui monte l'`<iframe>` du message.
///
/// C'est **le montage des coquilles**, reproduit : le corps n'a pas d'URL, il arrive dans une
/// chaîne, donc il est injecté par `srcdoc` ; et comme il n'y a pas de réponse HTTP pour porter
/// la CSP, elle passe par un `<meta http-equiv>` en tête du document injecté.
///
/// La page hôte, elle, ne porte pas de CSP. C'est délibéré et c'est nommé dans l'en-tête du
/// module : ce qui est mis à l'épreuve ici est le document du message, pas l'application.
fn host_page() -> String {
    // `sandbox` est posé en attribut de l'`<iframe>` et non dans la CSP : `sandbox` et
    // `frame-ancestors` sont les deux directives qui ne fonctionnent pas en `<meta>`.
    r#"<!doctype html>
<html><head><meta charset="utf-8"><title>critère 8 étage 2</title></head>
<body style="margin:0">
<script>
  window.onerror = function (message, source, ligne) {
    window.ipc.postMessage('erreur: ' + message + ' @ ' + ligne);
    return false;
  };
  function poser(phase, corps, csp, bac) {
    window.ipc.postMessage('pose-debut:' + phase);
    const ancien = document.getElementById('message');
    if (ancien) { ancien.remove(); }

    const tete = csp
      ? '<meta http-equiv="Content-Security-Policy" content="' + csp + '">'
      : '';
    const document_injecte =
      '<!doctype html><html><head><meta charset="utf-8">' + tete + '</head><body>' +
      corps + '</body></html>';

    const cadre = document.createElement('iframe');
    cadre.id = 'message';
    cadre.setAttribute('sandbox', bac);
    cadre.style.width = '100%';
    cadre.style.height = '600px';
    cadre.style.border = '0';
    cadre.addEventListener('load', () => {
      window.ipc.postMessage('pose:' + phase);
    });
    cadre.srcdoc = document_injecte;
    document.body.appendChild(cadre);
  }
  window.ipc.postMessage('pret');
</script>
</body></html>"#
        .to_owned()
}

/// Le relevé d'une phase.
#[derive(Debug)]
struct Result {
    phase: Phase,
    /// Connexions acceptées, tous vecteurs confondus.
    requests: usize,
    /// Les vecteurs qui ont ouvert au moins une connexion, et combien.
    leaks: Vec<(&'static str, usize)>,
    /// Durée réellement laissée au moteur, du chargement du cadre à la clôture.
    ///
    /// **Relevée et vérifiée.** Une phase close trop tôt rend zéro sans rien prouver ; c'est
    /// exactement le défaut qu'une relecture a trouvé le 2026-09-02, et le seul moyen qu'il ne
    /// revienne pas est de mesurer la durée au lieu de la supposer.
    settled: Duration,
}

impl Result {
    /// Vrai si la phase a fait ce qu'on attendait d'elle.
    fn passed(&self) -> bool {
        if self.phase.is_control() {
            // Un contrôle doit charger, et largement : un seul vecteur qui passe voudrait dire
            // que le moteur est à moitié mort.
            return self.leaks.len() >= CONTROL_MINIMUM;
        }
        // Un zéro n'est un succès que si le moteur a eu le temps de demander…
        if self.settled < SETTLE {
            return false;
        }
        // … et une fuite n'est tolérée que si elle est **connue et nommée**.
        self.leaks
            .iter()
            .all(|(name, _)| ENGINE_LEAKS.contains(name))
    }

    /// Les fuites de cette phase qui ne sont pas dans la liste connue.
    fn unexpected(&self) -> Vec<&'static str> {
        self.leaks
            .iter()
            .filter(|(name, _)| !ENGINE_LEAKS.contains(name))
            .map(|(name, _)| *name)
            .collect()
    }
}

/// Vecteurs qu'un contrôle doit au minimum avoir chargés.
///
/// Le message compte vingt-cinq vecteurs, dont plusieurs ne partent pas d'eux-mêmes — un
/// formulaire attend un clic, un lien attend un clic. Le seuil est donc bien en dessous du
/// total, mais assez haut pour qu'un moteur à demi mort ne passe pas.
const CONTROL_MINIMUM: usize = 10;

/// Les fuites que le **moteur** laisse passer malgré la CSP, et que seul l'assainisseur ferme.
///
/// ## Ce que cette liste dit du modèle de sécurité
///
/// `docs/PRIVACY.md` posait une hiérarchie : « la protection est appliquée par le moteur de
/// rendu (CSP, `sandbox`) ; le code applicatif (assainissement) est la deuxième ceinture, pas la
/// première ». Cette liste est l'ensemble des cas où **c'est faux**.
///
/// `iframe` : sous `frame-src 'none'`, WebView2 refuse bien la requête — mais il a déjà **ouvert
/// la connexion TCP** vers l'hôte visé avant de l'abandonner. Le serveur de l'expéditeur voit
/// donc une connexion venant de l'adresse du lecteur, à l'instant où il ouvre le message : c'est
/// exactement le signal qu'un pixel espion cherche à obtenir. Mesuré le 2026-09-03, vecteur
/// isolé par `MAILPRIVACY_ONLY=iframe`.
///
/// Ce que ça change : pour ce vecteur, **l'assainisseur est la première barrière et non la
/// seconde**. Il retire `<iframe>` entièrement, ce que la phase C vérifie.
///
/// Cette liste est **fermée** : une fuite d'un vecteur qui n'y figure pas fait échouer
/// l'exécution. Une régression du moteur, ou un vecteur nouveau, ne peut donc pas se glisser
/// dans un « c'est comme ça ».
const ENGINE_LEAKS: &[&str] = &["iframe"];

/// L'état du programme entre deux réveils du fil d'événements.
struct Run {
    /// **Un serveur instrumenté par couple (phase, vecteur)**, chacun sur son port.
    ///
    /// C'est ce qui rend l'attribution exacte et complète. Deux étapes ont été nécessaires pour
    /// y arriver, et les deux ont été imposées par une mesure :
    ///
    /// - un préfixe de chemin par phase ne suffit pas : une URL **racine-relative** l'ignore, et
    ///   le vecteur devient invisible ;
    /// - un serveur par phase ne suffit pas non plus : une connexion **sans requête HTTP**
    ///   lisible fait monter un compteur sans dire d'où elle vient. C'est arrivé, et il a fallu
    ///   vingt-cinq exécutions d'isolement pour nommer le coupable.
    ///
    /// Un port par vecteur répond aux deux : une connexion arrivée là vient de là, requête ou
    /// pas. Effet de bord bienvenu : autant d'origines distinctes, donc aucun partage de cache.
    spies: Vec<(Phase, Vec<(&'static str, Spy)>)>,
    phase: Phase,
    /// Quand le cadre de la phase en cours a fini de charger.
    loaded_at: Option<Instant>,
    /// La phase pour laquelle un minuteur a déjà été armé. **Un seul par phase.**
    armed: Option<Phase>,
    results: Vec<Result>,
    started: Instant,
}

impl Run {
    /// Les serveurs d'une phase, un par vecteur.
    fn spies_of(&self, phase: Phase) -> &[(&'static str, Spy)] {
        self.spies
            .iter()
            .find(|(it, _)| *it == phase)
            .map_or(&[], |(_, spies)| spies.as_slice())
    }

    /// Le corps, la CSP et le `sandbox` de la phase en cours.
    ///
    /// Le corps est **reconstruit par phase** : chaque vecteur y vise le port qui lui est propre
    /// pour cette phase.
    fn document(&self) -> (String, &'static str, &'static str) {
        let hosts: Vec<(&str, String)> = self
            .spies_of(self.phase)
            .iter()
            .map(|(name, spy)| (*name, spy.host()))
            .collect();
        let raw = mailprivacy::message_per_host(&hosts);
        match self.phase {
            // Les contrôles doivent pouvoir charger : document intact, aucune politique, et un
            // `sandbox` qui autorise ce que les phases gardées interdisent.
            Phase::Control | Phase::ControlAgain => {
                (raw, "", "allow-scripts allow-same-origin allow-forms")
            }
            Phase::CspOnly => (raw, MESSAGE_CSP, MESSAGE_SANDBOX),
            Phase::BothBelts => (
                sanitize::clean(&raw, Policy::default()).html,
                MESSAGE_CSP,
                MESSAGE_SANDBOX,
            ),
        }
    }

    /// Le script qui monte l'`<iframe>` de la phase en cours.
    fn script(&mut self) -> String {
        let (body, csp, sandbox) = self.document();
        self.loaded_at = None;
        self.armed = None;
        format!(
            "poser({}, {}, {}, {})",
            as_js_string(self.phase.label()),
            as_js_string(&body),
            as_js_string(csp),
            as_js_string(sandbox),
        )
    }

    /// Clôt la phase en cours et rend `true` s'il en reste une.
    fn close_phase(&mut self) -> bool {
        // **Les connexions acceptées, pas les requêtes lisibles.** Une connexion TCP ouverte
        // vers l'hôte d'un expéditeur est déjà une fuite, même si aucune requête HTTP complète
        // ne suit — et c'est exactement la forme que prend la fuite du vecteur `iframe`.
        let leaks: Vec<(&'static str, usize)> = self
            .spies_of(self.phase)
            .iter()
            .filter_map(|(name, spy)| {
                let count = spy.count();
                (count > 0).then_some((*name, count))
            })
            .collect();
        let settled = self.loaded_at.map(|at| at.elapsed()).unwrap_or_default();

        let result = Result {
            phase: self.phase,
            requests: leaks.iter().map(|(_, count)| count).sum(),
            leaks,
            settled,
        };
        println!(
            "  {:<18} {:>3} connexion(s) sur {:>2} vecteur(s)   décantation {:>6} ms   {}",
            result.phase.label(),
            result.requests,
            result.leaks.len(),
            result.settled.as_millis(),
            if result.passed() { "ok" } else { "ÉCHEC" },
        );
        for (name, count) in &result.leaks {
            println!("       ← {name} ({count})");
        }
        self.results.push(result);

        match self.phase.next() {
            Some(next) => {
                self.phase = next;
                true
            }
            None => false,
        }
    }
}

fn main() {
    // **Un serveur par couple (phase, vecteur).** Ils démarrent tous avant la fenêtre : un port
    // qu'on n'arrive pas à ouvrir doit faire échouer l'exécution avant qu'elle prétende mesurer
    // quoi que ce soit.
    //
    // `MAILPRIVACY_ONLY=<nom>` restreint à un seul vecteur — l'outil d'isolement, celui qui a
    // permis de nommer la fuite du moteur.
    let only = std::env::var("MAILPRIVACY_ONLY").ok();
    let selected: Vec<&'static str> = match &only {
        Some(name) => match mailprivacy::vector(name) {
            Some((name, _)) => vec![*name],
            None => fail(&format!(
                "vecteur inconnu : {name}. Les noms sont dans `mailprivacy::VECTORS`."
            )),
        },
        None => mailprivacy::VECTORS.iter().map(|(name, _)| *name).collect(),
    };

    let spies: Vec<(Phase, Vec<(&'static str, Spy)>)> = [
        Phase::Control,
        Phase::CspOnly,
        Phase::BothBelts,
        Phase::ControlAgain,
    ]
    .into_iter()
    .map(|phase| {
        let per_vector = selected
            .iter()
            .map(|name| (*name, Spy::start()))
            .collect::<Vec<_>>();
        (phase, per_vector)
    })
    .collect();

    println!("Critère 8, étage 2 — le message piégé dans un vrai moteur de rendu");
    println!("  Moteur           {}", engine());
    println!(
        "  Vecteurs         {} — un port instrumenté par vecteur et par phase",
        selected.len()
    );
    if let Some(name) = &only {
        println!("  Isolement        {name}");
    }
    println!("  Décantation      {} ms par phase", SETTLE.as_millis());
    println!("  Fuites connues   {}", ENGINE_LEAKS.join(", "));
    println!();

    // `EventLoopBuilder` et non `EventLoop::new` : c'est le constructeur qui porte le type
    // d'événement utilisateur, par lequel la page hôte et le minuteur parlent au fil.
    let event_loop: EventLoop<Wake> = EventLoopBuilder::<Wake>::with_user_event().build();
    let proxy = event_loop.create_proxy();

    let window = match WindowBuilder::new()
        .with_title("mailcore — critère 8 étage 2")
        .with_inner_size(tao::dpi::LogicalSize::new(900.0, 700.0))
        // Invisible : rien ici n'est à regarder, et une fenêtre qui s'ouvre au milieu d'une
        // CI vole le premier plan. Le moteur charge ses ressources qu'elle soit visible ou non.
        .with_visible(false)
        .build(&event_loop)
    {
        Ok(window) => window,
        Err(source) => fail(&format!("fenêtre non créée : {source}")),
    };

    let mut run = Run {
        spies,
        phase: Phase::Control,
        loaded_at: None,
        armed: None,
        results: Vec::new(),
        started: Instant::now(),
    };

    let handler = proxy.clone();
    let builder = WebViewBuilder::new()
        .with_html(host_page())
        .with_ipc_handler(move |request| on_message(request.body(), &handler));

    // **La fenêtre invisible n'a pas de poignée sur Linux, et c'est GTK qui l'exige.**
    //
    // Sur Windows et sur macOS, une fenêtre cachée a quand même son `HWND` ou sa `NSView` :
    // `build(&window)` marche. Sur Linux, une fenêtre GTK qu'on n'a jamais montrée n'est pas
    // *réalisée*, donc elle n'a aucune poignée native, et `wry` refuse avec « the underlying
    // handle is not available ». C'est ce que la CI a rendu au premier passage sur Linux.
    //
    // Le remède n'est pas de montrer la fenêtre — un banc qui vole le premier plan est une
    // nuisance, et la visibilité ne change rien à ce que le moteur charge. C'est de construire
    // le webview dans le **conteneur GTK** de la fenêtre, qui existe sans réalisation. C'est le
    // chemin que `wry` documente pour Linux, et il ne concerne que Linux.
    #[cfg(target_os = "linux")]
    let built = {
        use tao::platform::unix::WindowExtUnix;
        use wry::WebViewBuilderExtUnix;

        match window.default_vbox() {
            Some(vbox) => builder.build_gtk(vbox),
            None => fail("la fenêtre GTK n'a pas de conteneur : rien à quoi accrocher le moteur"),
        }
    };
    #[cfg(not(target_os = "linux"))]
    let built = builder.build(&window);

    let webview = match built {
        Ok(webview) => webview,
        Err(source) => fail(&format!("webview non créé : {source}")),
    };

    // `EventLoop::run` ne rend jamais la main — elle diverge. La sortie se fait donc depuis
    // l'intérieur, par `std::process::exit`, et c'est aussi ce qui rend le code de sortie
    // exploitable par la CI.
    event_loop.run(move |event, _target, control_flow| {
        // **`WaitUntil` et non `Wait`.** Le garde-fou ci-dessous ne s'exécute que quand le fil
        // d'événements tourne : avec `Wait` et un moteur muet — IPC cassé, page jamais
        // chargée — aucun événement n'arrive, le gestionnaire ne tourne pas, et le processus
        // reste planté jusqu'au délai du runner de CI. Le réveil périodique est ce qui rend la
        // promesse du garde-fou vraie.
        *control_flow = ControlFlow::WaitUntil(Instant::now() + Duration::from_millis(250));

        if run.started.elapsed() > PATIENCE {
            fail(&format!(
                "le moteur n'a pas répondu en {} s",
                PATIENCE.as_secs()
            ));
        }

        match event {
            // La page hôte est chargée et son script s'est exécuté : la première phase peut
            // partir. C'est le seul déclencheur — attendre `StartCause::Init` poserait une
            // `<iframe>` dans une page qui n'existe pas encore.
            Event::UserEvent(Wake::Ready) => {
                let script = run.script();
                if let Err(source) = webview.evaluate_script(&script) {
                    fail(&format!(
                        "script de la première phase non évalué : {source}"
                    ));
                }
            }
            // L'`<iframe>` a fini de charger : c'est **maintenant** que la décantation
            // commence. Avant, le moteur n'a pas encore eu l'occasion de demander quoi que ce
            // soit, et un délai armé trop tôt mesurerait le chargement au lieu de la fuite.
            //
            // **Un seul minuteur par phase, et seulement pour la phase en cours.** Un
            // `<meta refresh>` autorisé fait naviguer le cadre, donc émettre un deuxième
            // `load` : sans ces deux gardes, ce deuxième événement armait un minuteur qui
            // clôturait la phase *suivante* après quelques millisecondes.
            Event::UserEvent(Wake::Loaded(phase)) => {
                if phase == run.phase {
                    if run.loaded_at.is_none() {
                        run.loaded_at = Some(Instant::now());
                    }
                    if run.armed != Some(phase) {
                        run.armed = Some(phase);
                        arm(&proxy, phase);
                    }
                }
            }
            Event::UserEvent(Wake::Settled(phase)) => {
                // Un minuteur d'une phase révolue n'a rien à clôturer.
                if phase == run.phase {
                    if run.close_phase() {
                        // Phase suivante : le même webview, une `<iframe>` neuve.
                        let script = run.script();
                        if let Err(source) = webview.evaluate_script(&script) {
                            fail(&format!("script de phase non évalué : {source}"));
                        }
                    } else {
                        std::process::exit(report(&run.results));
                    }
                }
            }
            Event::WindowEvent {
                event: WindowEvent::CloseRequested,
                ..
            } => fail("fenêtre fermée avant la fin des phases"),
            Event::NewEvents(StartCause::Init) | _ => {}
        }
    });
}

/// Traite un message de la page hôte.
fn on_message(body: &str, proxy: &EventLoopProxy<Wake>) {
    if let Some(rest) = body.strip_prefix("erreur:") {
        // La page hôte est **notre** code : une erreur dedans invalide la mesure, elle ne se
        // journalise pas en passant.
        fail(&format!("erreur JavaScript dans la page hôte :{rest}"));
    }
    if body.starts_with("pose-debut:") {
        // Trace de mise au point : elle ne pilote rien. Muette sauf demande explicite, parce
        // qu'une sortie bavarde dans une CI cache le relevé qui compte.
        if std::env::var_os("MAILPRIVACY_TRACE").is_some() {
            eprintln!("  [page] {body}");
        }
        return;
    }
    let wake = if body == "pret" {
        Wake::Ready
    } else if let Some(label) = body.strip_prefix("pose:") {
        // La page renvoie le nom de la phase qu'elle vient de poser. Un nom inconnu est ignoré :
        // mieux vaut ne pas réagir que réagir pour la mauvaise phase.
        match Phase::from_label(label) {
            Some(phase) => Wake::Loaded(phase),
            None => return,
        }
    } else {
        return;
    };
    // Un envoi qui échoue veut dire que le fil d'événements est parti : il n'y a plus personne
    // à prévenir, et le processus est en train de sortir.
    let _ = proxy.send_event(wake);
}

/// Arme le minuteur de décantation de la phase en cours.
///
/// Un fil qui dort puis réveille le fil d'événements, et non un `sleep` dans le fil
/// d'événements : celui-ci doit rester libre pour que le moteur charge, sans quoi on mesurerait
/// notre propre blocage.
fn arm(proxy: &EventLoopProxy<Wake>, phase: Phase) {
    let proxy = proxy.clone();
    std::thread::spawn(move || {
        std::thread::sleep(SETTLE);
        let _ = proxy.send_event(Wake::Settled(phase));
    });
}

/// Écrit le verdict et rend le code de sortie du processus.
fn report(results: &[Result]) -> i32 {
    println!();

    if !results.iter().any(|it| it.phase == Phase::Control) {
        println!("Verdict : INVALIDE — le contrôle positif n'a pas été exécuté.");
        return 1;
    }
    // Les deux contrôles doivent avoir chargé : celui du début prouve que le moteur marchait
    // quand on a commencé, celui de la fin qu'il marchait encore quand on a conclu.
    for control in results.iter().filter(|it| it.phase.is_control()) {
        if !control.passed() {
            println!(
                "Verdict : INVALIDE — {} n'a chargé que {} vecteur(s) sur un minimum de \
                 {CONTROL_MINIMUM}.",
                control.phase.label(),
                control.leaks.len()
            );
            println!("Le moteur n'a donc pas rendu, ou les serveurs instrumentés ne voient");
            println!("rien : les zéros des phases gardées ne prouvent rien. Ne pas cocher le");
            println!("critère 8 sur cette exécution.");
            return 1;
        }
    }

    let guarded: Vec<&Result> = results.iter().filter(|it| !it.phase.is_control()).collect();
    let unexpected: Vec<(&Result, Vec<&'static str>)> = guarded
        .iter()
        .map(|result| (*result, result.unexpected()))
        .filter(|(_, leaks)| !leaks.is_empty())
        .collect();
    let too_fast: Vec<&Result> = guarded
        .iter()
        .filter(|it| it.settled < SETTLE)
        .copied()
        .collect();

    for result in &too_fast {
        println!(
            "Verdict : INVALIDE — {} n'a eu que {} ms de décantation au lieu de {}. Son zéro ne",
            result.phase.label(),
            result.settled.as_millis(),
            SETTLE.as_millis()
        );
        println!("prouve rien : le moteur n'a pas eu le temps de demander.");
    }
    for (result, leaks) in &unexpected {
        println!(
            "Verdict : ÉCHOUÉ — {} a laissé sortir des vecteurs non répertoriés : {}",
            result.phase.label(),
            leaks.join(", ")
        );
        println!("Soit la barrière a régressé, soit c'est une fuite du moteur à documenter dans");
        println!("`ENGINE_LEAKS` — mais pas en silence.");
    }
    if !too_fast.is_empty() || !unexpected.is_empty() {
        return 1;
    }

    println!(
        "Verdict : passé — les contrôles ont chargé {} et {} vecteur(s) ; les phases gardées",
        results
            .iter()
            .find(|it| it.phase == Phase::Control)
            .map_or(0, |it| it.leaks.len()),
        results
            .iter()
            .find(|it| it.phase == Phase::ControlAgain)
            .map_or(0, |it| it.leaks.len()),
    );
    let tolerated: Vec<&'static str> = guarded
        .iter()
        .flat_map(|it| it.leaks.iter().map(|(name, _)| *name))
        .collect();
    if tolerated.is_empty() {
        println!("n'ont rien laissé sortir du tout.");
    } else {
        println!(
            "n'ont laissé sortir que des fuites connues du moteur : {}.",
            tolerated.join(", ")
        );
        println!("Pour celles-là, **c'est l'assainisseur qui est la première barrière** — voir");
        println!("`ENGINE_LEAKS` et `docs/PRIVACY.md`.");
    }
    0
}
