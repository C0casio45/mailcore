//! Parcours de l'arborescence d'un profil Thunderbird.
//!
//! ## La disposition
//!
//! ```text
//! <profil>/
//! ├── Mail/                      comptes locaux et POP
//! │   └── Local Folders/
//! │       ├── Inbox              ← le dossier EST le fichier
//! │       ├── Inbox.msf          ← l'index de Thunderbird, on ne le lit pas
//! │       └── Archives.sbd/      ← les sous-dossiers sont dans <nom>.sbd/
//! │           └── 2024
//! └── ImapMail/                  comptes IMAP
//!     └── imap.exemple.fr/
//!         ├── INBOX
//!         └── [Gmail].sbd/
//!             └── Tous les messages
//! ```
//!
//! Les `.msf` sont les index de Thunderbird. On ne les lit pas et on ne s'y fie pas : ils
//! encodent l'état interne d'un autre programme, qui tourne en parallèle et les réécrit.
//!
//! ## Lecture seule, et rien d'autre
//!
//! Ce module n'ouvre aucun fichier et ne lit que des métadonnées de répertoire. Il n'écrit
//! nulle part, pas même un temporaire. Le profil est en production — critère 7 de
//! `docs/PHASE-1.md` : exactement zéro écriture.
//!
//! ## Encodage des noms
//!
//! Ce module travaille sur [`std::path::Path`] et non sur `camino`, parce qu'un nom de
//! fichier n'est pas garanti UTF-8. Un nom indécodable ne fait pas échouer le parcours : il
//! est converti avec remplacement, marqué [`MboxFile::name_is_lossy`], et on continue. Le
//! chemin réel, lui, reste intact — c'est celui qui sert à ouvrir le fichier.

use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

/// Les répertoires racines d'un profil qui contiennent des comptes.
const ACCOUNT_ROOTS: &[&str] = &["Mail", "ImapMail"];

/// Pseudo-comptes de Thunderbird : des répertoires qui ressemblent à des comptes mais ne
/// stockent rien.
///
/// `smart mailboxes` porte les dossiers virtuels — « toutes les boîtes de réception »
/// et compagnie. Sur un profil réel, ses fichiers font tous zéro octet : le contenu vit dans
/// les vrais comptes. Les importer créerait des dossiers vides et, le jour où Thunderbird y
/// écrirait, des références en double vers des messages déjà importés ailleurs.
const PSEUDO_ACCOUNTS: &[&str] = &["smart mailboxes"];

/// Extensions qui ne sont jamais un mbox.
///
/// Liste blanche inversée : on écarte ce qu'on reconnaît, on garde le reste. Un dossier
/// Thunderbird n'a pas d'extension, donc tout fichier sans extension connue est un candidat.
const NON_MBOX_EXTENSIONS: &[&str] = &[
    "msf",     // index de Thunderbird
    "dat",     // popstate.dat, msgFilterRules.dat
    "html",    // filterlog.html
    "log",     // journaux de filtres
    "json",    // configurations diverses
    "sqlite",  // gloda et compagnie
    "sbd",     // ne devrait pas être un fichier, mais on ne présume pas
    "bak",     //
    "tmp",     //
    "sqlite3", //
    "backup",  // feeds.json.backup, vu dans un profil réel
];

/// Le type de compte, déduit du répertoire racine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountKind {
    /// `Mail/` — dossiers locaux et comptes POP.
    Local,
    /// `ImapMail/` — comptes IMAP.
    Imap,
    /// Agrégateur de flux RSS.
    ///
    /// Reconnu par `mail.server.serverN.type = "rss"` dans `prefs.js`, jamais par le nom du
    /// répertoire : `docs/PHASE-1.md` interdit de supposer quoi que ce soit sur un nom de
    /// fichier, et Thunderbird n'a aucune obligation d'appeler ce répertoire `Feeds`.
    ///
    /// Ce n'est pas du courrier. Sur le profil réel, un unique dossier de flux porte
    /// **15 881 références** — 15,5 % de tout le store — d'articles supprimés. L'import les
    /// ignore par défaut ; voir `ImportOptions::include_feeds`.
    Feeds,
}

impl AccountKind {
    /// L'étiquette persistée dans `accounts.kind`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Imap => "imap",
            Self::Feeds => "rss",
        }
    }
}

/// Un fichier mbox trouvé dans le profil.
#[derive(Debug, Clone)]
pub struct MboxFile {
    /// Le chemin réel, tel que le système de fichiers le donne. Le seul à utiliser pour
    /// ouvrir le fichier.
    pub path: PathBuf,
    /// Le compte, tel que nommé par son répertoire.
    pub account: String,
    /// Le type de compte.
    pub account_kind: AccountKind,
    /// Le chemin logique du dossier, séparé par `/`, sans les suffixes `.sbd`.
    ///
    /// Par exemple `[Gmail]/Tous les messages`.
    pub folder: String,
    /// Le rôle déduit du nom.
    pub kind: mailcore::FolderKind,
    /// Taille en octets.
    pub size: u64,
    /// Vrai si un segment du nom a dû être décodé avec remplacement.
    pub name_is_lossy: bool,
    /// Vrai si `prefs.js` déclare le compte auquel ce dossier appartient.
    ///
    /// **Faux veut dire orphelin.** Thunderbird ne nettoie pas les répertoires d'un compte
    /// supprimé de sa configuration : les mbox restent sur le disque, indéfiniment. Sur le
    /// profil réel, deux répertoires sont dans ce cas — dont un qui porte **15 881 messages**,
    /// soit 15,5 % de tout le volume, entièrement dans une corbeille.
    ///
    /// Ce n'est pas du courrier vivant : ce sont les restes de comptes que l'utilisateur a
    /// retirés. L'import les écarte par défaut ; voir `ImportOptions::include_orphans`.
    ///
    /// Toujours vrai quand `prefs.js` n'a rien donné : sans déclarations, on ne peut pas
    /// distinguer un orphelin d'un compte vivant, et tout écarter serait catastrophique.
    pub declared: bool,
}

/// Le résultat d'un parcours.
#[derive(Debug, Clone, Default)]
pub struct Scan {
    /// Les fichiers mbox trouvés, triés par taille décroissante.
    ///
    /// Décroissante parce que c'est l'ordre dans lequel on veut les regarder : le plus gros
    /// fichier est celui qui casse les implémentations naïves.
    pub files: Vec<MboxFile>,
    /// Fichiers écartés parce que reconnus comme non-mbox.
    pub skipped: u64,
    /// Somme des tailles des mbox retenus.
    pub total_bytes: u64,
    /// Répertoires qu'on n'a pas pu lire — permissions, verrou, chemin trop long.
    ///
    /// Non vide ne fait pas échouer le parcours : un profil partiellement lisible vaut
    /// mieux qu'une erreur. Mais ça se journalise.
    pub unreadable: Vec<PathBuf>,
}

impl Scan {
    /// Le plus gros fichier trouvé.
    #[must_use]
    pub fn largest(&self) -> Option<&MboxFile> {
        self.files.first()
    }
}

/// Parcourt un profil et rend les mbox qu'il contient.
///
/// N'ouvre aucun fichier. Ne lit que des métadonnées de répertoire.
///
/// # Errors
///
/// [`Error::Io`] si le profil lui-même est introuvable ou illisible. Un sous-répertoire
/// illisible est signalé dans [`Scan::unreadable`] sans interrompre le parcours.
pub fn scan_profile(profile: &Path) -> Result<Scan> {
    if !profile.is_dir() {
        return Err(Error::Io {
            path: lossy_utf8(profile),
            source: std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "profil introuvable ou pas un répertoire",
            ),
        });
    }

    let mut scan = Scan::default();

    // Les comptes déclarés, lus une fois. Vide si `prefs.js` est absent ou illisible : les
    // comptes gardent alors leur nom de répertoire et leur type deviné du répertoire racine,
    // ce qui était le comportement avant.
    let declared = crate::prefs::accounts(profile);

    for root_name in ACCOUNT_ROOTS {
        let root = profile.join(root_name);
        if !root.is_dir() {
            continue;
        }
        let root_kind = if *root_name == "ImapMail" {
            AccountKind::Imap
        } else {
            AccountKind::Local
        };

        let entries = match std::fs::read_dir(&root) {
            Ok(entries) => entries,
            Err(_) => {
                scan.unreadable.push(root);
                continue;
            }
        };

        for entry in entries.flatten() {
            let account_dir = entry.path();
            if !account_dir.is_dir() {
                continue;
            }
            let (directory, lossy) = file_name_of(&account_dir);
            if PSEUDO_ACCOUNTS
                .iter()
                .any(|p| directory.eq_ignore_ascii_case(p))
            {
                continue;
            }

            let entry = declared.get(&directory);

            // Le nom de répertoire est celui du serveur, dédupliqué par Thunderbird :
            // `imap.gmail.com`, `imap.gmail-1.com`… Quatre comptes s'y ressemblent sur le
            // profil réel, chacun avec son `INBOX`, et rien ne dit lequel est lequel.
            // L'adresse, elle, est dans `prefs.js`. À défaut, on garde le répertoire.
            let account = entry
                .and_then(|it| it.label.clone())
                .unwrap_or_else(|| directory.clone());

            // Le type déclaré prime sur le répertoire racine : un compte de flux vit sous
            // `Mail/` et ressemblerait sinon à un compte local.
            let kind = if entry.is_some_and(crate::prefs::Account::is_feed) {
                AccountKind::Feeds
            } else {
                root_kind
            };

            // Sans aucune déclaration lisible, on ne sait rien : tout est réputé vivant.
            // Avec des déclarations, un répertoire absent de la liste est un orphelin.
            let is_declared = declared.is_empty() || entry.is_some();

            walk(
                &account_dir,
                &account,
                kind,
                lossy,
                is_declared,
                &[],
                &mut scan,
            );
        }
    }

    // Décroissant : le plus gros fichier est celui qui casse les implémentations naïves.
    scan.files.sort_by_key(|f| std::cmp::Reverse(f.size));
    Ok(scan)
}

/// Descend un répertoire de dossiers, en accumulant le chemin logique.
///
/// `logical` est le chemin logique des dossiers parents, un segment par niveau de `.sbd`.
fn walk(
    dir: &Path,
    account: &str,
    kind: AccountKind,
    account_lossy: bool,
    declared: bool,
    logical: &[String],
    scan: &mut Scan,
) {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => {
            scan.unreadable.push(dir.to_path_buf());
            return;
        }
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let (name, name_lossy) = file_name_of(&path);

        // Un nom qui commence par un point est une convention d'outil, pas un dossier mail.
        if name.starts_with('.') {
            continue;
        }

        let Ok(meta) = entry.metadata() else {
            scan.unreadable.push(path);
            continue;
        };

        if meta.is_dir() {
            // Seuls les répertoires `.sbd` portent des sous-dossiers. Tout autre répertoire
            // dans un compte n'est pas de la hiérarchie de dossiers.
            if let Some(parent) = name.strip_suffix(".sbd") {
                let mut deeper = logical.to_vec();
                deeper.push(parent.to_owned());
                walk(&path, account, kind, account_lossy, declared, &deeper, scan);
            }
            continue;
        }

        if !meta.is_file() {
            // Lien symbolique cassé, socket, ce qu'on veut : pas un mbox.
            continue;
        }

        if is_non_mbox(&name) {
            scan.skipped += 1;
            continue;
        }

        let mut segments = logical.to_vec();
        segments.push(name);
        // Les noms de boîtes IMAP sont en UTF-7 modifié, et Thunderbird nomme ses fichiers
        // avec cet encodage tel quel : `P&AOk-pite` sur le disque est `Pépite` pour
        // l'utilisateur. Réservé aux comptes IMAP — un dossier local nommé « Trucs & Machins »
        // n'a rien à décoder, et le décodeur le laisserait intact mais autant ne pas
        // l'appeler du tout.
        let folder = if kind == AccountKind::Imap {
            segments
                .iter()
                .map(|segment| crate::utf7::decode(segment))
                .collect::<Vec<_>>()
                .join("/")
        } else {
            segments.join("/")
        };

        scan.total_bytes += meta.len();
        scan.files.push(MboxFile {
            path,
            account: account.to_owned(),
            account_kind: kind,
            kind: folder_kind(&folder),
            folder,
            size: meta.len(),
            name_is_lossy: account_lossy || name_lossy,
            declared,
        });
    }
}

/// Vrai si l'extension du nom le disqualifie comme mbox.
fn is_non_mbox(name: &str) -> bool {
    let Some((_, ext)) = name.rsplit_once('.') else {
        return false;
    };
    NON_MBOX_EXTENSIONS
        .iter()
        .any(|known| ext.eq_ignore_ascii_case(known))
}

/// Le nom d'un chemin, décodé avec remplacement, plus un drapeau disant s'il a fallu.
fn file_name_of(path: &Path) -> (String, bool) {
    let Some(name) = path.file_name() else {
        return (String::new(), false);
    };
    match name.to_str() {
        Some(clean) => (clean.to_owned(), false),
        None => (name.to_string_lossy().into_owned(), true),
    }
}

fn lossy_utf8(path: &Path) -> camino::Utf8PathBuf {
    camino::Utf8PathBuf::from(path.to_string_lossy().into_owned())
}
/// Devine le rôle d'un dossier à partir de son nom.
///
/// **Délègue à [`mailcore::FolderKind::guess`].** L'heuristique a déménagé dans `mailcore` le
/// 2026-09-03, parce que la synchronisation IMAP en a besoin aussi et que deux heuristiques
/// divergentes feraient de la même `Corbeille` deux rôles différents selon la porte d'entrée.
/// La fonction reste ici parce que les tests qui la couvrent sont écrits sur des noms de
/// dossiers Thunderbird réels, et qu'ils appartiennent à ce crate.
#[must_use]
pub fn folder_kind(folder: &str) -> mailcore::FolderKind {
    mailcore::FolderKind::guess(folder)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// Construit un faux profil dans un répertoire temporaire.
    ///
    /// Jamais le vrai profil : les tests ne doivent pas dépendre d'une machine, et surtout
    /// pas d'un profil en production.
    fn fake_profile() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        let imap = root.join("ImapMail").join("imap.exemple.fr");
        std::fs::create_dir_all(&imap).unwrap();
        std::fs::write(imap.join("INBOX"), b"From a@b d\ncorps\n").unwrap();
        std::fs::write(imap.join("INBOX.msf"), b"index a ignorer").unwrap();
        std::fs::write(imap.join("Corbeille"), b"From a@b d\n").unwrap();

        let gmail = imap.join("[Gmail].sbd");
        std::fs::create_dir_all(&gmail).unwrap();
        // Volontairement le plus gros du faux profil : `largest()` doit être sans ambiguïté.
        std::fs::write(
            gmail.join("Tous les messages"),
            b"From a@b d\nx\ny\nz\nle plus gros fichier du faux profil\n",
        )
        .unwrap();
        std::fs::write(gmail.join("Tous les messages.msf"), b"index").unwrap();

        let nested = gmail.join("Sous dossier.sbd");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("Encore plus loin"), b"From a@b d\n").unwrap();

        let local = root.join("Mail").join("Local Folders");
        std::fs::create_dir_all(&local).unwrap();
        std::fs::write(local.join("Inbox"), b"From a@b d\n").unwrap();
        std::fs::write(local.join("popstate.dat"), b"etat pop").unwrap();
        std::fs::write(local.join("msgFilterRules.dat"), b"filtres").unwrap();

        dir
    }

    fn folders(scan: &Scan) -> Vec<String> {
        let mut names: Vec<String> = scan.files.iter().map(|f| f.folder.clone()).collect();
        names.sort();
        names
    }

    // ---------------------------------------------------------------- parcours

    #[test]
    fn finds_every_mbox_and_no_index() {
        let dir = fake_profile();
        let scan = scan_profile(dir.path()).unwrap();

        assert_eq!(
            folders(&scan),
            vec![
                "Corbeille",
                "INBOX",
                "Inbox",
                "[Gmail]/Sous dossier/Encore plus loin",
                "[Gmail]/Tous les messages",
            ]
        );
    }

    #[test]
    fn counts_the_files_it_skipped() {
        let dir = fake_profile();
        let scan = scan_profile(dir.path()).unwrap();
        // Deux .msf et deux .dat.
        assert_eq!(scan.skipped, 4);
    }

    #[test]
    fn builds_the_logical_path_from_sbd_nesting() {
        let dir = fake_profile();
        let scan = scan_profile(dir.path()).unwrap();

        let deep = scan
            .files
            .iter()
            .find(|f| f.folder.ends_with("Encore plus loin"))
            .unwrap();
        assert_eq!(deep.folder, "[Gmail]/Sous dossier/Encore plus loin");
    }

    #[test]
    fn separates_imap_from_local_accounts() {
        let dir = fake_profile();
        let scan = scan_profile(dir.path()).unwrap();

        let imap = scan.files.iter().find(|f| f.folder == "INBOX").unwrap();
        let local = scan.files.iter().find(|f| f.folder == "Inbox").unwrap();

        assert_eq!(imap.account_kind, AccountKind::Imap);
        assert_eq!(imap.account, "imap.exemple.fr");
        assert_eq!(local.account_kind, AccountKind::Local);
        assert_eq!(local.account, "Local Folders");
    }

    #[test]
    fn sorts_by_size_descending() {
        let dir = fake_profile();
        let scan = scan_profile(dir.path()).unwrap();

        let sizes: Vec<u64> = scan.files.iter().map(|f| f.size).collect();
        let mut sorted = sizes.clone();
        sorted.sort_unstable_by(|a, b| b.cmp(a));
        assert_eq!(sizes, sorted);
        assert_eq!(scan.largest().unwrap().folder, "[Gmail]/Tous les messages");
    }

    #[test]
    fn sums_only_the_mbox_bytes() {
        let dir = fake_profile();
        let scan = scan_profile(dir.path()).unwrap();

        let expected: u64 = scan.files.iter().map(|f| f.size).sum();
        assert_eq!(scan.total_bytes, expected);
    }

    #[test]
    fn a_directory_without_sbd_suffix_is_not_folder_hierarchy() {
        let dir = tempfile::tempdir().unwrap();
        let account = dir.path().join("ImapMail").join("compte");
        std::fs::create_dir_all(account.join("pas un dossier mail")).unwrap();
        std::fs::write(account.join("pas un dossier mail").join("truc"), b"x").unwrap();
        std::fs::write(account.join("INBOX"), b"From a@b d\n").unwrap();

        let scan = scan_profile(dir.path()).unwrap();
        assert_eq!(folders(&scan), vec!["INBOX"]);
    }

    #[test]
    fn an_empty_folder_file_is_still_a_folder() {
        let dir = tempfile::tempdir().unwrap();
        let account = dir.path().join("ImapMail").join("compte");
        std::fs::create_dir_all(&account).unwrap();
        std::fs::write(account.join("Vide"), b"").unwrap();

        let scan = scan_profile(dir.path()).unwrap();
        assert_eq!(folders(&scan), vec!["Vide"]);
        assert_eq!(scan.files[0].size, 0);
    }

    #[test]
    fn skips_dotfiles() {
        let dir = tempfile::tempdir().unwrap();
        let account = dir.path().join("ImapMail").join("compte");
        std::fs::create_dir_all(&account).unwrap();
        std::fs::write(account.join(".directory"), b"x").unwrap();
        std::fs::write(account.join("INBOX"), b"From a@b d\n").unwrap();

        let scan = scan_profile(dir.path()).unwrap();
        assert_eq!(folders(&scan), vec!["INBOX"]);
    }

    #[test]
    fn a_directory_absent_from_prefs_is_marked_as_an_orphan() {
        // Thunderbird ne supprime pas les mbox d'un compte retiré de sa configuration. Sur le
        // profil réel, deux répertoires sont dans ce cas et l'un porte 15 881 messages.
        let dir = tempfile::tempdir().unwrap();

        let vivant = dir.path().join("ImapMail").join("imap.exemple.fr");
        std::fs::create_dir_all(&vivant).unwrap();
        std::fs::write(vivant.join("INBOX"), b"From a@b d\n").unwrap();

        let orphelin = dir.path().join("Mail").join("ancien-compte");
        std::fs::create_dir_all(&orphelin).unwrap();
        std::fs::write(orphelin.join("Inbox"), b"From a@b d\n").unwrap();

        std::fs::write(
            dir.path().join("prefs.js"),
            concat!(
                "user_pref(\"mail.account.account1.server\", \"server1\");\n",
                "user_pref(\"mail.server.server1.directory-rel\", ",
                "\"[ProfD]ImapMail/imap.exemple.fr\");\n",
                "user_pref(\"mail.server.server1.type\", \"imap\");\n",
            ),
        )
        .unwrap();

        let scan = scan_profile(dir.path()).unwrap();
        let vivant = scan.files.iter().find(|f| f.folder == "INBOX").unwrap();
        assert!(vivant.declared, "un compte déclaré est passé pour orphelin");

        let orphelin = scan.files.iter().find(|f| f.folder == "Inbox").unwrap();
        assert!(!orphelin.declared, "l'orphelin n'a pas été repéré");
    }

    #[test]
    fn without_prefs_nothing_is_an_orphan() {
        // La garantie qui empêche la catastrophe : sans déclarations, on ne peut pas
        // distinguer un orphelin d'un compte vivant, donc tout est réputé vivant. Un profil
        // dont `prefs.js` est illisible s'importe entièrement.
        let dir = tempfile::tempdir().unwrap();
        let compte = dir.path().join("Mail").join("Local Folders");
        std::fs::create_dir_all(&compte).unwrap();
        std::fs::write(compte.join("Inbox"), b"From a@b d\n").unwrap();

        let scan = scan_profile(dir.path()).unwrap();
        assert_eq!(scan.files.len(), 1);
        assert!(
            scan.files[0].declared,
            "tout serait écarté sur un profil sans prefs.js"
        );
    }

    #[test]
    fn a_subfolder_inherits_the_declaration_of_its_account() {
        // Le drapeau est porté par le compte, pas par le dossier : un `.sbd` profond doit
        // hériter, sinon la moitié d'un orphelin serait importée.
        let dir = tempfile::tempdir().unwrap();
        let orphelin = dir.path().join("Mail").join("ancien");
        let profond = orphelin.join("Archives.sbd").join("2024.sbd");
        std::fs::create_dir_all(&profond).unwrap();
        std::fs::write(orphelin.join("Inbox"), b"From a@b d\n").unwrap();
        std::fs::write(profond.join("Janvier"), b"From a@b d\n").unwrap();

        std::fs::write(
            dir.path().join("prefs.js"),
            concat!(
                "user_pref(\"mail.account.account1.server\", \"server1\");\n",
                "user_pref(\"mail.server.server1.directory-rel\", \"[ProfD]Mail/autre\");\n",
                "user_pref(\"mail.server.server1.type\", \"none\");\n",
            ),
        )
        .unwrap();

        let scan = scan_profile(dir.path()).unwrap();
        assert!(!scan.files.is_empty());
        assert!(
            scan.files.iter().all(|f| !f.declared),
            "un sous-dossier d'orphelin a été réputé déclaré"
        );
    }

    #[test]
    fn a_feed_account_is_marked_from_prefs_not_from_its_name() {
        // Le compte de flux du profil réel s'appelle `Feeds`, mais c'est `prefs.js` qui le dit
        // — et c'est lui qu'on écoute. Un répertoire nommé autrement doit être reconnu quand
        // même.
        let dir = tempfile::tempdir().unwrap();
        let flux = dir.path().join("Mail").join("agregateur");
        std::fs::create_dir_all(&flux).unwrap();
        std::fs::write(flux.join("Articles"), b"From a@b d\n").unwrap();

        let boite = dir.path().join("Mail").join("Local Folders");
        std::fs::create_dir_all(&boite).unwrap();
        std::fs::write(boite.join("Inbox"), b"From a@b d\n").unwrap();

        std::fs::write(
            dir.path().join("prefs.js"),
            concat!(
                "user_pref(\"mail.account.account1.server\", \"server1\");\n",
                "user_pref(\"mail.server.server1.directory-rel\", \"[ProfD]Mail/agregateur\");\n",
                "user_pref(\"mail.server.server1.type\", \"rss\");\n",
            ),
        )
        .unwrap();

        let scan = scan_profile(dir.path()).unwrap();
        let flux = scan
            .files
            .iter()
            .find(|f| f.folder == "Articles")
            .expect("le dossier de flux est trouvé");
        assert_eq!(flux.account_kind, AccountKind::Feeds);

        let boite = scan
            .files
            .iter()
            .find(|f| f.folder == "Inbox")
            .expect("le dossier de courrier est trouvé");
        assert_eq!(boite.account_kind, AccountKind::Local);
    }

    #[test]
    fn without_prefs_a_feed_account_looks_like_any_local_account() {
        // Honnêteté sur la limite : sans `prefs.js`, rien ne distingue un agrégateur. On ne
        // devine pas sur le nom, donc le compte est importé comme du courrier. C'est le
        // comportement voulu — mieux vaut importer ce qu'on ne sait pas classer que le perdre.
        let dir = tempfile::tempdir().unwrap();
        let flux = dir.path().join("Mail").join("Feeds");
        std::fs::create_dir_all(&flux).unwrap();
        std::fs::write(flux.join("Articles"), b"From a@b d\n").unwrap();

        let scan = scan_profile(dir.path()).unwrap();
        assert_eq!(scan.files.len(), 1);
        assert_eq!(scan.files[0].account_kind, AccountKind::Local);
    }

    #[test]
    fn skips_thunderbird_virtual_folders() {
        // `smart mailboxes` porte les dossiers virtuels : des fichiers vides dont le contenu
        // réel vit dans les autres comptes. Vu sur un profil réel.
        let dir = tempfile::tempdir().unwrap();
        let smart = dir.path().join("Mail").join("smart mailboxes");
        std::fs::create_dir_all(&smart).unwrap();
        std::fs::write(smart.join("Inbox"), b"").unwrap();
        std::fs::write(smart.join("Sent"), b"").unwrap();

        let vrai = dir.path().join("Mail").join("Local Folders");
        std::fs::create_dir_all(&vrai).unwrap();
        std::fs::write(vrai.join("Inbox"), b"From a@b d\n").unwrap();

        let scan = scan_profile(dir.path()).unwrap();
        assert_eq!(scan.files.len(), 1, "un dossier virtuel a été importé");
        assert_eq!(scan.files[0].account, "Local Folders");
    }

    #[test]
    fn skips_backup_files() {
        // `feeds.json.backup` était pris pour un mbox : 135 Kio de préambule dans la sonde.
        let dir = tempfile::tempdir().unwrap();
        let account = dir.path().join("Mail").join("Feeds");
        std::fs::create_dir_all(&account).unwrap();
        std::fs::write(account.join("feeds.json.backup"), b"{\"pas\":\"un mbox\"}").unwrap();
        std::fs::write(account.join("Trash"), b"From a@b d\n").unwrap();

        let scan = scan_profile(dir.path()).unwrap();
        assert_eq!(scan.files.len(), 1);
        assert_eq!(scan.files[0].folder, "Trash");
    }

    #[test]
    fn a_folder_named_after_an_email_address_is_kept() {
        // Vu sur un profil réel : « Brouillons-tom@exemple.com ». L'extension apparente est
        // `com`, qui n'est pas dans la liste des non-mbox — d'où le choix d'une liste
        // d'exclusion plutôt que d'une règle « sans extension ».
        let dir = tempfile::tempdir().unwrap();
        let account = dir.path().join("Mail").join("Local Folders");
        std::fs::create_dir_all(&account).unwrap();
        std::fs::write(account.join("Brouillons-tom@exemple.com"), b"From a@b d\n").unwrap();

        let scan = scan_profile(dir.path()).unwrap();
        assert_eq!(scan.files.len(), 1);
        assert_eq!(scan.files[0].folder, "Brouillons-tom@exemple.com");
    }
    #[test]
    fn a_profile_with_neither_root_yields_an_empty_scan() {
        let dir = tempfile::tempdir().unwrap();
        let scan = scan_profile(dir.path()).unwrap();
        assert!(scan.files.is_empty());
        assert_eq!(scan.total_bytes, 0);
    }

    #[test]
    fn a_missing_profile_is_an_error_not_a_panic() {
        let dir = tempfile::tempdir().unwrap();
        let absent = dir.path().join("pas la");
        assert!(matches!(scan_profile(&absent), Err(Error::Io { .. })));
    }

    #[test]
    fn a_file_where_a_profile_is_expected_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("un fichier");
        std::fs::write(&file, b"x").unwrap();
        assert!(scan_profile(&file).is_err());
    }

    #[test]
    fn scanning_writes_nothing_into_the_profile() {
        // Critère 7, en miniature. Le vrai relevé porte sur le profil complet, mais si ce
        // test-ci tombe, inutile d'aller plus loin.
        let dir = fake_profile();
        let before = snapshot(dir.path());

        let scan = scan_profile(dir.path()).unwrap();
        assert!(!scan.files.is_empty());

        assert_eq!(
            before,
            snapshot(dir.path()),
            "le parcours a modifié le profil"
        );
    }

    /// Relevé (chemin, taille, mtime) de tout un répertoire, trié.
    fn snapshot(root: &Path) -> Vec<(String, u64, std::time::SystemTime)> {
        let mut out = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).unwrap().flatten() {
                let meta = entry.metadata().unwrap();
                if meta.is_dir() {
                    stack.push(entry.path());
                } else {
                    out.push((
                        entry.path().to_string_lossy().into_owned(),
                        meta.len(),
                        meta.modified().unwrap(),
                    ));
                }
            }
        }
        out.sort();
        out
    }

    // ---------------------------------------------------------------- rôles

    #[test]
    fn recognises_english_folder_names() {
        use mailcore::FolderKind as K;
        assert_eq!(folder_kind("INBOX"), K::Inbox);
        assert_eq!(folder_kind("Sent"), K::Sent);
        assert_eq!(folder_kind("Drafts"), K::Drafts);
        assert_eq!(folder_kind("Trash"), K::Trash);
        assert_eq!(folder_kind("Junk"), K::Junk);
        assert_eq!(folder_kind("Archives"), K::Archive);
    }

    #[test]
    fn recognises_french_folder_names() {
        use mailcore::FolderKind as K;
        assert_eq!(folder_kind("Corbeille"), K::Trash);
        assert_eq!(folder_kind("Brouillons"), K::Drafts);
        assert_eq!(folder_kind("Éléments envoyés"), K::Sent);
        assert_eq!(folder_kind("Indésirables"), K::Junk);
        assert_eq!(folder_kind("Boîte de réception"), K::Inbox);
    }

    #[test]
    fn recognises_gmail_folder_names() {
        use mailcore::FolderKind as K;
        assert_eq!(folder_kind("[Gmail]/Tous les messages"), K::Archive);
        assert_eq!(folder_kind("[Gmail]/Messages envoyés"), K::Sent);
        assert_eq!(folder_kind("[Gmail]/Corbeille"), K::Trash);
        assert_eq!(folder_kind("[Gmail]/Brouillons"), K::Drafts);
        assert_eq!(folder_kind("[Gmail]/Spam"), K::Junk);
    }

    #[test]
    fn folder_roles_ignore_case_and_surrounding_space() {
        use mailcore::FolderKind as K;
        assert_eq!(folder_kind("inbox"), K::Inbox);
        assert_eq!(folder_kind("  CORBEILLE  "), K::Trash);
    }

    #[test]
    fn an_unknown_name_is_other_not_a_guess() {
        use mailcore::FolderKind as K;
        assert_eq!(folder_kind("Factures 2024"), K::Other);
        assert_eq!(folder_kind("[Gmail]/Important"), K::Other);
        assert_eq!(folder_kind(""), K::Other);
        // Piège : contient « sent » mais n'est pas le dossier envoyés.
        assert_eq!(folder_kind("Consentements"), K::Other);
    }

    #[test]
    fn only_the_last_segment_decides_the_role() {
        use mailcore::FolderKind as K;
        // Un sous-dossier d'INBOX n'est pas la boîte de réception.
        assert_eq!(folder_kind("INBOX/Projets"), K::Other);
        assert_eq!(folder_kind("Archives/Trash"), K::Trash);
    }

    // ---------------------------------------------------------------- extensions

    #[test]
    fn recognises_non_mbox_extensions_case_insensitively() {
        assert!(is_non_mbox("INBOX.msf"));
        assert!(is_non_mbox("INBOX.MSF"));
        assert!(is_non_mbox("popstate.dat"));
        assert!(!is_non_mbox("INBOX"));
        assert!(!is_non_mbox("Factures 2024"));
    }

    #[test]
    fn a_folder_name_containing_a_dot_is_still_a_folder() {
        // « Factures 2024.old » n'est pas une extension connue : c'est un nom de dossier.
        assert!(!is_non_mbox("Factures 2024.old"));
        assert!(!is_non_mbox("v1.2"));
    }
}
