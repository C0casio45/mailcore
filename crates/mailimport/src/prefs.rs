//! Lecture de `prefs.js` : mettre une **adresse** sur chaque compte au lieu d'un nom de serveur.
//!
//! ## Pourquoi c'est nécessaire
//!
//! Thunderbird nomme les répertoires de comptes d'après le serveur IMAP, en dédupliquant :
//! `ImapMail/imap.gmail.com`, `imap.gmail-1.com`, `imap.gmail-2.com`… Sur le profil réel, ça
//! donne onze comptes dont quatre s'appellent une variation de `imap.gmail.com`, et chacun a
//! son `INBOX`. Le nom de répertoire ne permet donc **pas** de savoir de quelle boîte on
//! regarde la réception.
//!
//! L'adresse, elle, est dans la configuration du profil. Ce module va la chercher.
//!
//! ## La clé de jointure
//!
//! `mail.server.serverN.directory-rel` vaut `[ProfD]ImapMail/<répertoire>` : c'est exactement
//! le répertoire que le parcours de [`crate::tree`] rencontre. La jointure est donc exacte,
//! pas heuristique — et elle survit aux suffixes de déduplication, contrairement à une
//! comparaison de noms d'hôtes.
//!
//! ## L'ordre des sources
//!
//! 1. `mail.identity.idN.useremail` — l'adresse déclarée de l'identité. Sémantiquement la
//!    bonne : c'est « l'adresse de ce compte ».
//! 2. `mail.server.serverN.userName` — souvent l'adresse elle aussi, mais pas toujours : sur
//!    un serveur personnel, l'identifiant peut être un simple `tom`.
//! 3. `mail.server.serverN.name` — le libellé que l'utilisateur a donné.
//! 4. Rien : l'appelant garde le nom de répertoire. `Local Folders` et les dossiers
//!    virtuels n'ont pas d'adresse, et leur inventer une serait pire que de ne rien changer.
//!
//! ## Ce qu'on ne lit pas
//!
//! `prefs.js` contient aussi des jetons OAuth, des identifiants clients et des réglages de
//! filtres. Ce module ne retient **que** les clés énumérées ci-dessus, et ne journalise jamais
//! une valeur : un fichier de préférences est une pièce sensible, et il n'y a aucune raison
//! d'en faire transiter plus que le strict nécessaire.
//!
//! Lecture seule, comme tout ce qui touche au profil (règle 1 du `CLAUDE.md`).

use std::collections::HashMap;
use std::path::Path;

/// Taille maximale lue de `prefs.js`.
///
/// 8 Mio : un `prefs.js` réel fait quelques dizaines de kilo-octets. Le plafond existe pour
/// qu'un fichier aberrant — corrompu, ou pas un `prefs.js` du tout — ne fasse pas allouer sans
/// borne.
const MAX_PREFS: u64 = 8 * 1024 * 1024;

/// Ce que `prefs.js` dit d'un compte.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Account {
    /// L'adresse, quand on a su la trouver.
    pub label: Option<String>,
    /// Le type de serveur déclaré : `imap`, `pop3`, `rss`, `none`…
    ///
    /// C'est l'information **autoritative** pour reconnaître un compte de flux RSS. Le nom
    /// du répertoire — `Mail/Feeds` — ne serait qu'une devinette, et une devinette sur le nom
    /// d'un répertoire est exactement ce que `docs/PHASE-1.md` interdit.
    pub kind: Option<String>,
}

impl Account {
    /// Vrai si ce compte est un agrégateur de flux et non une boîte mail.
    #[must_use]
    pub fn is_feed(&self) -> bool {
        self.kind.as_deref() == Some("rss")
    }
}

/// Les comptes, indexés par **nom de répertoire** sous `ImapMail/` ou `Mail/`.
///
/// Vide si `prefs.js` est absent ou illisible : ce n'est pas une erreur. Un profil sans
/// préférences lisibles s'importe très bien, avec des comptes nommés d'après leur répertoire —
/// ce qui était le comportement avant ce module.
#[must_use]
pub fn accounts(profile: &Path) -> HashMap<String, Account> {
    let path = profile.join("prefs.js");
    let Ok(meta) = std::fs::metadata(&path) else {
        tracing::debug!("prefs.js absent : les comptes garderont leur nom de répertoire");
        return HashMap::new();
    };
    if meta.len() > MAX_PREFS {
        tracing::warn!(octets = meta.len(), "prefs.js anormalement gros, ignoré");
        return HashMap::new();
    }
    let Ok(text) = std::fs::read_to_string(&path) else {
        // Encodage inattendu, verrou, permissions : on continue sans.
        tracing::debug!("prefs.js illisible : les comptes garderont leur nom de répertoire");
        return HashMap::new();
    };

    accounts_from(&text)
}

/// Extrait les comptes d'un contenu de `prefs.js`.
///
/// Séparé de la lecture du fichier pour être testable sur des cas tordus sans toucher au
/// disque — et il y en a, voir les tests.
#[must_use]
pub fn accounts_from(text: &str) -> HashMap<String, Account> {
    let prefs = parse(text);

    // `mail.accountmanager.accounts` est l'ordre d'affichage de Thunderbird. On le suit quand
    // il est là, mais on ne s'y fie pas : un compte absent de cette liste existe quand même
    // sur le disque, et il vaut mieux le nommer correctement que pas du tout.
    let declared: Vec<String> = prefs
        .get("mail.accountmanager.accounts")
        .map(|list| {
            list.split(',')
                .map(str::trim)
                .filter(|it| !it.is_empty())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();

    let discovered: Vec<String> = prefs
        .keys()
        .filter_map(|key| {
            key.strip_prefix("mail.account.")
                .and_then(|rest| rest.strip_suffix(".server"))
                .map(str::to_owned)
        })
        .collect();

    let mut out = HashMap::new();
    for account in declared.iter().chain(discovered.iter()) {
        let Some(server) = prefs.get(&format!("mail.account.{account}.server")) else {
            continue;
        };
        let Some(directory) = prefs.get(&format!("mail.server.{server}.directory-rel")) else {
            continue;
        };
        let Some(dirname) = last_segment(directory) else {
            continue;
        };

        // La première source qui répond gagne, et on ne réécrit pas une entrée déjà posée :
        // `declared` passe avant `discovered`, donc l'ordre de Thunderbird est respecté.
        if out.contains_key(&dirname) {
            continue;
        }

        let identity = prefs
            .get(&format!("mail.account.{account}.identities"))
            .and_then(|list| list.split(',').map(str::trim).find(|it| !it.is_empty()));

        let kind = prefs
            .get(&format!("mail.server.{server}.type"))
            .map(String::as_str)
            .map(str::trim)
            .filter(|it| !it.is_empty())
            .map(str::to_lowercase);

        // Les replis sur le serveur ne valent que pour un compte qui parle à un serveur de
        // courrier. Découvert sur le profil réel : `Local Folders` porte
        // `userName = "nobody"` — la valeur factice de Thunderbird — et le compte s'est
        // affiché « nobody » pendant un temps. Une adresse n'existe que là où il y a une
        // boîte distante ; ailleurs, le nom de répertoire est plus honnête.
        let talks_to_a_server = matches!(kind.as_deref(), Some("imap" | "pop3"));

        let label = identity
            .and_then(|id| prefs.get(&format!("mail.identity.{id}.useremail")))
            .or_else(|| {
                talks_to_a_server.then(|| {
                    prefs
                        .get(&format!("mail.server.{server}.userName"))
                        .or_else(|| prefs.get(&format!("mail.server.{server}.name")))
                })?
            })
            .map(String::as_str)
            .map(str::trim)
            .filter(|it| !it.is_empty())
            .map(str::to_owned);

        // Une entrée est posée même sans adresse : le **type** vaut à lui seul le détour,
        // puisque c'est lui qui dit qu'un compte est un agrégateur de flux.
        if label.is_some() || kind.is_some() {
            out.insert(dirname, Account { label, kind });
        }
    }

    tracing::debug!(comptes = out.len(), "comptes lus depuis prefs.js");
    out
}

/// Le dernier segment d'un chemin, quel que soit le séparateur.
///
/// `directory-rel` utilise `/` même sur Windows, mais `directory` — qu'on pourrait vouloir
/// lire en repli un jour — utilise `\`. Accepter les deux coûte un caractère.
fn last_segment(path: &str) -> Option<String> {
    let trimmed = path.trim_end_matches(['/', '\\']);
    let segment = trimmed.rsplit(['/', '\\']).next()?.trim();
    (!segment.is_empty()).then(|| segment.to_owned())
}

/// Les paires `user_pref("clé", "valeur")` d'un `prefs.js`, valeurs de chaîne seulement.
///
/// Les nombres et les booléens sont ignorés : rien de ce qu'on cherche n'en est un, et les
/// accepter demanderait de décider quoi en faire.
fn parse(text: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();

    for line in text.lines() {
        let line = line.trim();
        let Some(rest) = line.strip_prefix("user_pref(") else {
            continue;
        };
        let Some((key, rest)) = quoted(rest) else {
            continue;
        };
        // Entre la clé et la valeur : `, `. Tolérer l'absence d'espace et les espaces en trop.
        let rest = rest.trim_start();
        let Some(rest) = rest.strip_prefix(',') else {
            continue;
        };
        let Some((value, _)) = quoted(rest.trim_start()) else {
            continue;
        };
        out.insert(key, value);
    }

    out
}

/// Lit une chaîne entre guillemets doubles au début de `input`, et rend le reste.
///
/// Gère les échappements `\\` et `\"` que Thunderbird écrit dans les chemins Windows. Une
/// chaîne non terminée rend `None` plutôt que d'avaler la fin du fichier.
fn quoted(input: &str) -> Option<(String, &str)> {
    let mut chars = input.char_indices();
    if chars.next()?.1 != '"' {
        return None;
    }

    let mut value = String::new();
    let mut escaped = false;
    for (offset, character) in chars {
        if escaped {
            // Seuls les échappements qu'on rencontre réellement sont traduits ; les autres
            // sont rendus tels quels, antislash compris, plutôt que perdus en silence.
            match character {
                '\\' => value.push('\\'),
                '"' => value.push('"'),
                'n' => value.push('\n'),
                't' => value.push('\t'),
                other => {
                    value.push('\\');
                    value.push(other);
                }
            }
            escaped = false;
            continue;
        }
        match character {
            '\\' => escaped = true,
            '"' => return Some((value, &input[offset + 1..])),
            other => value.push(other),
        }
    }
    None
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// Un extrait fidèle au profil réel, adresses changées.
    const REAL: &str = r#"
user_pref("mail.accountmanager.accounts", "account1,account2,account3");
user_pref("mail.account.account1.server", "server1");
user_pref("mail.account.account1.identities", "id1");
user_pref("mail.server.server1.directory", "C:\\Users\\x\\Thunderbird\\ImapMail\\outlook.office365.com");
user_pref("mail.server.server1.directory-rel", "[ProfD]ImapMail/outlook.office365.com");
user_pref("mail.server.server1.hostname", "outlook.office365.com");
user_pref("mail.server.server1.userName", "etudiant@ecole.invalid");
user_pref("mail.server.server1.type", "imap");
user_pref("mail.identity.id1.useremail", "etudiant@ecole.invalid");
user_pref("mail.account.account2.server", "server2");
user_pref("mail.account.account2.identities", "id2");
user_pref("mail.server.server2.directory-rel", "[ProfD]ImapMail/imap.gmail-1.com");
user_pref("mail.server.server2.userName", "autre@gmail.com");
user_pref("mail.identity.id2.useremail", "autre@gmail.com");
user_pref("mail.account.account3.server", "server3");
user_pref("mail.server.server3.directory-rel", "[ProfD]Mail/Local Folders");
user_pref("mail.server.server3.type", "none");
user_pref("mail.server.server1.port", 993);
user_pref("mail.server.server1.login_at_startup", true);
"#;

    #[test]
    fn the_address_replaces_the_server_directory_name() {
        // Le point de tout le module : `imap.gmail-1.com` devient une adresse.
        let accounts = accounts_from(REAL);
        assert_eq!(
            accounts
                .get("outlook.office365.com")
                .and_then(|it| it.label.as_deref()),
            Some("etudiant@ecole.invalid")
        );
        assert_eq!(
            accounts
                .get("imap.gmail-1.com")
                .and_then(|it| it.label.as_deref()),
            Some("autre@gmail.com")
        );
    }

    #[test]
    fn an_account_without_an_address_gets_no_label_but_keeps_its_type() {
        // `Local Folders` n'a pas d'adresse. Lui en inventer une serait pire que de laisser
        // l'appelant garder le nom de répertoire — mais son **type** vaut le détour, puisque
        // c'est ce champ qui distingue un agrégateur de flux d'une boîte mail.
        let accounts = accounts_from(REAL);
        let local = accounts.get("Local Folders").expect("le type est connu");
        assert_eq!(local.label, None);
        assert_eq!(local.kind.as_deref(), Some("none"));
        assert!(!local.is_feed());
    }

    #[test]
    fn a_feed_account_is_recognised_by_its_declared_type() {
        // Jamais par le nom du répertoire : `docs/PHASE-1.md` interdit de supposer quoi que ce
        // soit sur un nom de fichier, et rien n'oblige Thunderbird à l'appeler `Feeds`.
        let accounts = accounts_from(
            r#"
user_pref("mail.accountmanager.accounts", "account9");
user_pref("mail.account.account9.server", "server9");
user_pref("mail.server.server9.directory-rel", "[ProfD]Mail/un nom quelconque");
user_pref("mail.server.server9.type", "rss");
"#,
        );
        let feeds = accounts.get("un nom quelconque").expect("compte trouvé");
        assert!(feeds.is_feed());
        assert_eq!(feeds.label, None);
    }

    #[test]
    fn a_directory_named_feeds_is_not_a_feed_account_by_itself() {
        // L'inverse du test précédent : le nom ne décide de rien. Un compte IMAP que
        // quelqu'un aurait appelé « Feeds » reste du courrier.
        let accounts = accounts_from(
            r#"
user_pref("mail.account.account1.server", "server1");
user_pref("mail.server.server1.directory-rel", "[ProfD]ImapMail/Feeds");
user_pref("mail.server.server1.type", "imap");
user_pref("mail.server.server1.userName", "boite@exemple.fr");
"#,
        );
        let account = accounts.get("Feeds").expect("compte trouvé");
        assert!(!account.is_feed());
        assert_eq!(account.label.as_deref(), Some("boite@exemple.fr"));
    }

    #[test]
    fn the_identity_wins_over_the_login_name() {
        // Sur un serveur personnel, l'identifiant peut être un simple `tom` : c'est l'adresse
        // de l'identité qui est la bonne réponse.
        let accounts = accounts_from(
            r#"
user_pref("mail.accountmanager.accounts", "account1");
user_pref("mail.account.account1.server", "server1");
user_pref("mail.account.account1.identities", "id1");
user_pref("mail.server.server1.directory-rel", "[ProfD]ImapMail/mail.exemple.fr");
user_pref("mail.server.server1.userName", "tom");
user_pref("mail.identity.id1.useremail", "tom@exemple.fr");
"#,
        );
        assert_eq!(
            accounts
                .get("mail.exemple.fr")
                .and_then(|it| it.label.as_deref()),
            Some("tom@exemple.fr")
        );
    }

    #[test]
    fn the_placeholder_login_of_local_folders_is_not_taken_for_an_address() {
        // Vu sur le profil réel : `Local Folders` porte `userName = "nobody"`, la valeur
        // factice de Thunderbird, et le compte s'affichait « nobody ». Une adresse n'existe
        // que là où il y a une boîte distante — ailleurs, le nom de répertoire est plus
        // honnête que le premier champ qui traîne.
        let accounts = accounts_from(
            r#"
user_pref("mail.accountmanager.accounts", "account1");
user_pref("mail.account.account1.server", "server1");
user_pref("mail.server.server1.directory-rel", "[ProfD]Mail/Local Folders");
user_pref("mail.server.server1.userName", "nobody");
user_pref("mail.server.server1.name", "Local Folders");
user_pref("mail.server.server1.type", "none");
"#,
        );
        let local = accounts.get("Local Folders").expect("compte trouvé");
        assert_eq!(local.label, None, "un libellé factice a été retenu");
        assert_eq!(local.kind.as_deref(), Some("none"));
    }

    #[test]
    fn an_identity_address_is_accepted_even_without_a_mail_server() {
        // L'inverse : une adresse **déclarée par une identité** est une vraie adresse, quel
        // que soit le type de serveur. C'est un choix explicite de l'utilisateur, pas un
        // champ technique qu'on interprète.
        let accounts = accounts_from(
            r#"
user_pref("mail.account.account1.server", "server1");
user_pref("mail.account.account1.identities", "id1");
user_pref("mail.server.server1.directory-rel", "[ProfD]Mail/Sauvegardes");
user_pref("mail.server.server1.type", "none");
user_pref("mail.identity.id1.useremail", "archives@exemple.fr");
"#,
        );
        assert_eq!(
            accounts
                .get("Sauvegardes")
                .and_then(|it| it.label.as_deref()),
            Some("archives@exemple.fr")
        );
    }

    #[test]
    fn the_login_name_is_used_when_there_is_no_identity() {
        let accounts = accounts_from(
            r#"
user_pref("mail.account.account1.server", "server1");
user_pref("mail.server.server1.directory-rel", "[ProfD]ImapMail/vieux.serveur.fr");
user_pref("mail.server.server1.type", "imap");
user_pref("mail.server.server1.userName", "boite@vieux.serveur.fr");
"#,
        );
        assert_eq!(
            accounts
                .get("vieux.serveur.fr")
                .and_then(|it| it.label.as_deref()),
            Some("boite@vieux.serveur.fr")
        );
    }

    #[test]
    fn a_server_without_a_declared_type_gets_no_fallback_label() {
        // Choix conservateur, et il mérite d'être écrit : Thunderbird écrit toujours le type
        // d'un compte réel. Son absence veut dire qu'on regarde une déclaration incomplète,
        // et adopter un `userName` qu'on ne sait pas interpréter est ce qui a produit
        // « nobody ».
        let accounts = accounts_from(
            r#"
user_pref("mail.account.account1.server", "server1");
user_pref("mail.server.server1.directory-rel", "[ProfD]Mail/quelque-chose");
user_pref("mail.server.server1.userName", "peut-etre-pas-une-adresse");
"#,
        );
        // Le compte n'apparaît pas du tout : ni libellé, ni type à retenir.
        assert!(!accounts.contains_key("quelque-chose"));
    }

    #[test]
    fn an_account_missing_from_the_manager_list_is_still_named() {
        // Un compte présent sur le disque mais absent de `mail.accountmanager.accounts` — ça
        // arrive après une suppression incomplète. Mieux vaut le nommer que l'ignorer.
        let accounts = accounts_from(
            r#"
user_pref("mail.accountmanager.accounts", "account1");
user_pref("mail.account.account2.server", "server2");
user_pref("mail.server.server2.directory-rel", "[ProfD]ImapMail/orphelin.fr");
user_pref("mail.identity.id2.useremail", "vieux@orphelin.fr");
user_pref("mail.account.account2.identities", "id2");
"#,
        );
        assert_eq!(
            accounts
                .get("orphelin.fr")
                .and_then(|it| it.label.as_deref()),
            Some("vieux@orphelin.fr")
        );
    }

    #[test]
    fn windows_path_escapes_are_decoded() {
        let (value, rest) = quoted(r#""C:\\Users\\x\\Mail", true);"#).unwrap();
        assert_eq!(value, r"C:\Users\x\Mail");
        assert!(rest.starts_with(','));
    }

    #[test]
    fn numbers_and_booleans_are_ignored_rather_than_guessed() {
        let prefs = parse(REAL);
        assert!(!prefs.contains_key("mail.server.server1.port"));
        assert!(!prefs.contains_key("mail.server.server1.login_at_startup"));
    }

    // --- Entrée hostile et malformée : la règle du CLAUDE.md ---

    #[test]
    fn malformed_input_yields_no_labels_rather_than_panicking() {
        for broken in [
            "",
            "user_pref(",
            "user_pref(\"",
            "user_pref(\"clé\"",
            "user_pref(\"clé\",",
            "user_pref(\"clé\", \"valeur non terminée",
            "user_pref(\"clé\\\", \"valeur\");",
            "pas du tout du javascript",
            "user_pref(\"mail.account.a.server\", \"s\");", // sans directory-rel
            "user_pref(\"mail.accountmanager.accounts\", \",,,\");",
            "\u{feff}user_pref(\"a\", \"b\");",
        ] {
            let accounts = accounts_from(broken);
            // Ce qui compte : ça termine, ça ne panique pas, et ça ne fabrique pas de libellé.
            assert!(accounts.is_empty(), "libellé inventé depuis {broken:?}");
        }
    }

    #[test]
    fn a_directory_ending_in_a_separator_still_yields_its_name() {
        assert_eq!(
            last_segment("[ProfD]ImapMail/compte/").as_deref(),
            Some("compte")
        );
        assert_eq!(
            last_segment("[ProfD]ImapMail\\compte").as_deref(),
            Some("compte")
        );
        assert_eq!(last_segment("/").as_deref(), None);
        assert_eq!(last_segment("").as_deref(), None);
    }

    #[test]
    fn a_pathological_file_terminates() {
        // Un `prefs.js` d'un million de lignes tronquées : le lecteur n'avance que vers
        // l'avant, ligne par ligne, donc il termine.
        let hostile = "user_pref(\"a\", \"b".repeat(50_000);
        let _ = accounts_from(&hostile);
        let deep = "\\".repeat(100_000);
        let _ = accounts_from(&format!("user_pref(\"a\", \"{deep}\");"));
    }
}
