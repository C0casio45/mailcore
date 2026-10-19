//! Le geste qu'une phrase du service propose, rendu exécutable sur cette machine.
//!
//! ## Pourquoi, depuis le 2026-10-19
//!
//! La bannière de la recherche propose une commande — `mail vectors --bert`, `mail model
//! install` — avec un bouton « Copier ». Copiée telle quelle, elle échouait deux fois : `mail`
//! n'est dans le `PATH` de personne, puisque l'application se lance depuis le menu Démarrer, et
//! sans `--store` la commande visait le store par défaut, pas celui que la fenêtre a ouvert. *Dire
//! quoi faire oblige à rendre le geste possible* — la leçon du 2026-09-10.
//!
//! ## Pourquoi ici et pas dans le service
//!
//! Le service compose la phrase et le geste **sans aucun chemin** (`maild::semantic::describe`) :
//! sa réponse part aussi vers un client distant, et un chemin y dirait l'arborescence — le nom
//! d'utilisateur compris — d'une machine à une autre. Seule la coquille embarquée sait à la fois
//! où est son store et où est son exécutable ; c'est donc elle qui complète, et pour elle seule.
//! En mode distant, le geste reste tel quel : il se lance sur la machine du démon, que cette
//! coquille ne connaît pas.

use camino::Utf8PathBuf;

/// Ce que la coquille embarquée sait de sa machine.
#[derive(Debug, Clone)]
pub struct Local {
    /// Le store que la fenêtre a ouvert.
    pub store: Utf8PathBuf,
    /// L'outil en ligne de commande posé à côté de la coquille, s'il y est. Sinon on écrit
    /// `mail` et on compte sur le `PATH` : c'est le cas d'une installation qui l'y a mis.
    pub mail: Option<Utf8PathBuf>,
}

impl Local {
    /// Le store ouvert, et `mail` cherché à côté de l'exécutable qui tourne.
    pub fn beside_this_executable(store: Utf8PathBuf) -> Self {
        let name = format!("mail{}", std::env::consts::EXE_SUFFIX);
        let mail = std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(|dir| dir.join(&name)))
            .filter(|path| path.is_file())
            .and_then(|path| Utf8PathBuf::from_path_buf(path).ok());
        Self { store, mail }
    }
}

/// Le geste, prêt à coller dans un terminal de cette machine.
///
/// Seul un geste qui commence par `mail ` est complété : c'est la seule commande que le service
/// propose, et toucher à une autre serait deviner ce qu'elle veut dire.
pub fn runnable(gesture: &str, local: Option<&Local>) -> String {
    let (Some(local), Some(rest)) = (local, gesture.strip_prefix("mail ")) else {
        return gesture.to_owned();
    };
    let program = local
        .mail
        .as_deref()
        .map_or_else(|| "mail".to_owned(), |path| quoted(path.as_str()));
    // Sous PowerShell — le terminal par défaut de Windows —, un programme entre guillemets est
    // une chaîne, pas une commande : il faut l'opérateur d'appel devant.
    let call = if cfg!(windows) && program.starts_with('\'') {
        "& "
    } else {
        ""
    };
    format!(
        "{call}{program} --store {} {rest}",
        quoted(local.store.as_str())
    )
}

/// Un argument tel qu'un terminal le relira, des guillemets seulement s'il en faut.
///
/// Sans guillemets dans le cas courant — `F:\dev\…` —, la commande marche aussi bien dans
/// PowerShell que dans `cmd`. Avec, ce sont des guillemets simples, que PowerShell et un shell
/// POSIX lisent tous deux sans rien interpréter dedans ; seule l'apostrophe s'y écrit
/// différemment.
fn quoted(text: &str) -> String {
    let plain = !text.is_empty()
        && text
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '\\' | '/' | ':' | '.' | '_' | '-'));
    if plain {
        return text.to_owned();
    }
    let mut out = String::with_capacity(text.len() + 2);
    out.push('\'');
    for c in text.chars() {
        if cfg!(windows) && is_powershell_quote(c) {
            // PowerShell ferme aussi une chaîne simple sur les apostrophes typographiques — un
            // nom de dossier tapé dans l'Explorateur en porte. Doublée, chacune reste du texte.
            out.push(c);
            out.push(c);
        } else if !cfg!(windows) && c == '\'' {
            out.push_str(r"'\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

/// Les caractères que PowerShell lit comme une apostrophe de chaîne.
fn is_powershell_quote(c: char) -> bool {
    matches!(c, '\'' | '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}')
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn local(store: &str, mail: Option<&str>) -> Local {
        Local {
            store: Utf8PathBuf::from(store),
            mail: mail.map(Utf8PathBuf::from),
        }
    }

    #[test]
    fn a_gesture_names_the_open_store_and_the_tool_beside_the_window() {
        let here = local(
            r"F:\dev\mailcore\measurements\store-imap",
            Some(r"F:\dev\mailcore\target\release\mail.exe"),
        );
        assert_eq!(
            runnable("mail vectors --bert", Some(&here)),
            r"F:\dev\mailcore\target\release\mail.exe --store F:\dev\mailcore\measurements\store-imap vectors --bert"
        );
    }

    #[test]
    fn without_the_tool_beside_it_the_gesture_counts_on_the_path() {
        let here = local("/home/marie/.local/share/mailcore", None);
        assert_eq!(
            runnable("mail model install", Some(&here)),
            "mail --store /home/marie/.local/share/mailcore model install"
        );
    }

    #[test]
    fn a_path_with_a_space_is_quoted_and_called() {
        let here = local(
            r"C:\Users\Marie Curie\AppData\Roaming\mailcore\data",
            Some(r"C:\Program Files\mailcore\mail.exe"),
        );
        let gesture = runnable("mail vectors --bert", Some(&here));
        if cfg!(windows) {
            assert_eq!(
                gesture,
                r"& 'C:\Program Files\mailcore\mail.exe' --store 'C:\Users\Marie Curie\AppData\Roaming\mailcore\data' vectors --bert"
            );
        } else {
            assert_eq!(
                gesture,
                r"'C:\Program Files\mailcore\mail.exe' --store 'C:\Users\Marie Curie\AppData\Roaming\mailcore\data' vectors --bert"
            );
        }
    }

    #[test]
    fn an_apostrophe_in_a_path_cannot_close_the_quotes() {
        // « L'été » : sans l'échappement, l'apostrophe fermerait la chaîne et la suite du chemin
        // deviendrait des arguments — ou une commande.
        let quoted = quoted("/data/L'été; rm -rf ~");
        if cfg!(windows) {
            assert_eq!(quoted, "'/data/L''été; rm -rf ~'");
            // L'apostrophe typographique ferme aussi une chaîne PowerShell.
            assert_eq!(super::quoted("L\u{2019}été"), "'L\u{2019}\u{2019}été'");
        } else {
            assert_eq!(quoted, r"'/data/L'\''été; rm -rf ~'");
        }
    }

    #[test]
    fn a_remote_shell_or_another_command_leaves_the_gesture_alone() {
        // Le contrôle inverse : en mode distant le geste se lance chez le démon, et ce qui n'est
        // pas une commande `mail` n'est pas deviné.
        assert_eq!(runnable("mail vectors --bert", None), "mail vectors --bert");
        let here = local(r"F:\store", None);
        assert_eq!(runnable("mailx --help", Some(&here)), "mailx --help");
    }
}
