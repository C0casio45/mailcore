//! Ce qu'une panique laisse dans un journal : **où**, jamais **quoi**.
//!
//! Le message d'une panique est souvent formaté à partir des données en cause — un
//! `panic!("… {valeur}")` dans une dépendance qui refuse une entrée, donc du courrier. La
//! bibliothèque standard citait elle-même la chaîne qu'on coupait hors d'une frontière de
//! caractère ; Rust 1.96 ne le fait plus (vérifié le 2026-10-19), mais rien ne promet qu'aucune
//! dépendance ne le fera. Le crochet par défaut écrit le message tel quel sur la sortie
//! d'erreur ; celle d'un enfant
//! est recopiée dans le journal de son parent, et celui de la coquille va sur disque. Ce serait
//! le « deuxième exemplaire du mail, en clair, dans un fichier que personne ne surveille » que
//! `docs/PRIVACY.md` §8 interdit.
//!
//! **La règle tient au type du message, pas à son texte.** Un message écrit dans le source —
//! `panic!("…")` sans argument, que Rust passe en `&'static str` — vient du code : il est gardé.
//! Un message formaté à l'exécution arrive en `String`, et peut venir des données : il est
//! remplacé par sa longueur. L'emplacement et le nom du fil suffisent à retrouver la ligne.
//!
//! L'emplacement lui-même est raccourci : une panique dans une dépendance porte le chemin absolu
//! de son source, sous le répertoire personnel, donc le nom du compte Windows.

/// Remplace le crochet de panique du processus. À appeler une fois, sitôt le journal monté.
pub fn install() {
    std::panic::set_hook(Box::new(|info| {
        let location = info.location().map_or_else(
            || "inconnu".to_owned(),
            |at| format!("{}:{}:{}", short_file(at.file()), at.line(), at.column()),
        );
        let current = std::thread::current();
        let thread = current.name().unwrap_or("sans nom");
        let payload = info.payload();
        if let Some(message) = payload.downcast_ref::<&'static str>() {
            tracing::error!(%location, fil = thread, message, "panique");
        } else {
            let length = payload.downcast_ref::<String>().map_or(0, String::len);
            tracing::error!(
                %location,
                fil = thread,
                octets = length,
                "panique — message formaté à l'exécution, non recopié"
            );
        }
    }));
}

/// Le chemin d'un source, sans ce qui précède la racine de son crate.
///
/// Les crates du workspace arrivent en chemin relatif, `crates/maild/src/…`, et la bibliothèque
/// standard sous `/rustc/<empreinte>/` : rien à retirer. Une dépendance arrive en chemin absolu
/// sous `~/.cargo/registry/src/<index>/` ou `~/.cargo/git/checkouts/<dépôt>/<révision>/` : on
/// garde ce qui suit, `<crate>-<version>/src/…`, ce qui suffit à la retrouver. Tout autre chemin
/// absolu garde son seul nom de fichier — deviner une racine dans un chemin inconnu finirait par
/// prendre un répertoire personnel pour elle.
fn short_file(file: &str) -> String {
    let normalised = file.replace('\\', "/");
    let absolute = normalised.starts_with('/') || normalised.as_bytes().get(1) == Some(&b':');
    if !absolute || normalised.starts_with("/rustc/") {
        return normalised;
    }
    // Ce qui suit le marqueur, moins les composants qui nomment l'index ou le dépôt.
    for (marker, skipped) in [("/registry/src/", 1), ("/git/checkouts/", 2)] {
        if let Some(at) = normalised.rfind(marker) {
            let rest: Vec<&str> = normalised[at + marker.len()..].split('/').collect();
            if rest.len() > skipped {
                return rest[skipped..].join("/");
            }
        }
    }
    normalised
        .rsplit('/')
        .next()
        .map_or_else(String::new, str::to_owned)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::short_file;

    #[test]
    fn a_dependency_loses_the_home_directory() {
        assert_eq!(
            short_file(
                r"C:\Users\quelqu'un\.cargo\registry\src\index.crates.io-1949cf8c6b5b557f\tantivy-0.26.1\src\indexer\merger.rs"
            ),
            "tantivy-0.26.1/src/indexer/merger.rs"
        );
        assert_eq!(
            short_file(
                "/home/quelqu'un/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/mail-parser-0.11.0/src/lib.rs"
            ),
            "mail-parser-0.11.0/src/lib.rs"
        );
    }

    #[test]
    fn a_workspace_path_and_the_standard_library_are_kept() {
        assert_eq!(
            short_file(r"crates\maild\src\vectors.rs"),
            "crates/maild/src/vectors.rs"
        );
        assert_eq!(
            short_file("/rustc/ac68faa20/library/core/src/str/mod.rs"),
            "/rustc/ac68faa20/library/core/src/str/mod.rs"
        );
    }

    #[test]
    fn a_git_dependency_loses_the_home_directory_too() {
        assert_eq!(
            short_file(
                "/home/quelqu'un/.cargo/git/checkouts/candle-0c2b1a/4f3e2d1/candle-core/src/tensor.rs"
            ),
            "candle-core/src/tensor.rs"
        );
    }

    #[test]
    fn an_unknown_absolute_path_keeps_only_its_file_name() {
        // Un dépôt cloné sous le répertoire personnel — et un compte dont le nom ressemble à
        // une version, que deviner une racine aurait gardé.
        assert_eq!(
            short_file(r"C:\Users\jean-2.0\projets\outil\src\main.rs"),
            "main.rs"
        );
        assert_eq!(short_file("/home/quelqu'un/outil/src/main.rs"), "main.rs");
    }
}
