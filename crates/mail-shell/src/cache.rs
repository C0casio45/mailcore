//! Le cache de lecture du **mode distant** : de quoi dessiner avant le premier aller-retour.
//!
//! ## Pourquoi il n'existe que pour le mode distant
//!
//! En mode embarqué, le store *est* local : il s'ouvre en 31 ms, mesuré, et un cache serait une
//! deuxième copie des mêmes octets, à invalider, pour rien.
//!
//! En mode distant, la note du critère 1 de `docs/PHASE-1.md` est explicite : « avec un démon
//! distant, il impose en plus qu'elle n'attende pas le premier aller-retour réseau — donc
//! qu'elle ouvre sur son cache de lecture ». Sans cache, la coquille ouvre sur une liste vide
//! et attend le réseau ; et après un redémarrage hors ligne elle n'a **rien** à montrer, ce qui
//! vide le critère 9 de la moitié de son sens.
//!
//! ## Ce qu'il contient, et ce qu'il ne contient pas
//!
//! Les dossiers, et les lignes du dernier dossier ouvert : de quoi remplir le premier écran.
//! **Jamais un corps de message.** Un corps est gros, il se relit vite, et il n'apporte rien au
//! premier pixel.
//!
//! ## C'est une donnée sensible, et ça change ce qu'on en fait
//!
//! `docs/PRIVACY.md` §8 : les sujets et les expéditeurs d'une boîte sont « presque aussi
//! révélateurs que le courrier lui-même ». Trois conséquences, toutes appliquées ici :
//!
//! - le fichier vit dans le répertoire de données de l'utilisateur, **un par démon**, et jamais
//!   dans le store ni dans le profil Thunderbird ;
//! - il est **borné** — [`MAX_ROWS`] lignes — pour qu'un cache ne devienne pas une copie de la
//!   boîte ;
//! - il se **purge**, et le bouton est dans l'interface. Un cache qu'on ne peut pas effacer est
//!   une trace qu'on ne peut pas retirer.
//!
//! ## L'écriture ne bloque jamais l'interface
//!
//! Règle 3 du `CLAUDE.md`. L'écriture part sur un fil, et elle est **atomique** : fichier
//! temporaire puis renommage. Un cache tronqué par un arrêt brutal serait un cache illisible au
//! prochain démarrage — donc un premier écran vide, exactement ce qu'il devait éviter.

use std::io::Write as _;

use camino::{Utf8Path, Utf8PathBuf};
use mailapi::dto;
use serde::{Deserialize, Serialize};

/// Lignes gardées au plus. Un écran large, pas une boîte.
///
/// 500 : de quoi défiler un moment sans réseau, et ~100 Ko de JSON. Le front web en gardait
/// beaucoup plus dans IndexedDB ; ici on préfère un fichier qu'on relit d'un bloc en quelques
/// millisecondes, parce que ces millisecondes sont sur le chemin du critère 1.
const MAX_ROWS: usize = 500;

/// Ce que le cache retient d'une boîte.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Snapshot {
    /// La version du contrat au moment de l'écriture.
    ///
    /// Un cache écrit par une version antérieure du contrat n'est pas relisible : les types de
    /// `mailapi::dto` auraient changé de forme. Le numéro le dit avant que `serde` échoue, et
    /// c'est plus lisible qu'une erreur de désérialisation.
    pub protocol: u32,
    /// La révision du store au moment de l'écriture. Informative : la réponse du démon gagne
    /// toujours, il n'y a donc rien à arbitrer.
    pub revision: String,
    pub folders: Vec<dto::Folder>,
    /// Le dossier dont les lignes sont gardées.
    pub folder: Option<i64>,
    pub rows: Vec<dto::Row>,
}

impl Snapshot {
    /// Construit un instantané borné.
    #[must_use]
    pub fn new(
        revision: String,
        folders: Vec<dto::Folder>,
        folder: Option<i64>,
        rows: &[dto::Row],
    ) -> Self {
        Self {
            protocol: mailapi::PROTOCOL,
            revision,
            folders,
            folder,
            rows: rows.iter().take(MAX_ROWS).cloned().collect(),
        }
    }

    /// Vrai si l'instantané vaut la peine d'être écrit.
    #[must_use]
    pub fn worth_writing(&self) -> bool {
        !self.folders.is_empty()
    }
}

/// Le fichier de cache d'un démon.
///
/// Un fichier par hôte : deux démons n'ont pas la même boîte, et mélanger leurs listes
/// afficherait le courrier de l'un sous le nom de l'autre.
///
/// L'hôte est **assaini** avant d'entrer dans un nom de fichier. Il vient d'une ligne de
/// commande, donc de l'utilisateur, mais un nom de fichier construit sans y regarder est
/// exactement le genre de ligne qui laisse passer un `../`.
fn path(host: &str) -> Option<Utf8PathBuf> {
    let dirs = directories::BaseDirs::new()?;
    // **`data_local_dir` et non `data_dir`.** Sur Windows, `data_dir` est `%APPDATA%`, le profil
    // **itinérant** : sur un poste en domaine, il est synchronisé vers un serveur de profils. Le
    // cache contient les sujets et les expéditeurs d'une boîte — `docs/PRIVACY.md` §8 exige
    // qu'il ne vive « jamais dans un emplacement partagé ou synchronisé ». `data_local_dir`
    // (`%LOCALAPPDATA%`) ne quitte pas la machine.
    //
    // Sur macOS et Linux, les deux pointent au même endroit ; le choix ne coûte donc rien
    // ailleurs et ferme une fuite sur la plate-forme de référence.
    let mut root = Utf8PathBuf::from_path_buf(dirs.data_local_dir().to_path_buf()).ok()?;
    root.push("mailcore");

    let safe: String = host
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '.' || character == '-' {
                character
            } else {
                '_'
            }
        })
        .collect();
    // Un hôte vide ou entièrement réécrit reste un nom valide : la borne évite un nom
    // interminable si quelqu'un passe une chaîne absurde.
    let safe: String = safe.chars().take(64).collect();
    root.push(format!("cache-{safe}.json"));
    Some(root)
}

/// Relit le cache d'un démon, s'il y en a un d'exploitable.
///
/// Ne rend jamais d'erreur : un cache absent, illisible, tronqué ou écrit par une autre version
/// du contrat est un cache qu'on ignore. La coquille attend alors le réseau, ce qui est plus
/// lent et parfaitement correct.
#[must_use]
pub fn load(host: &str) -> Option<Snapshot> {
    let path = path(host)?;
    let text = std::fs::read_to_string(&path).ok()?;
    let snapshot: Snapshot = serde_json::from_str(&text).ok()?;

    if snapshot.protocol != mailapi::PROTOCOL {
        tracing::debug!(
            attendu = mailapi::PROTOCOL,
            trouve = snapshot.protocol,
            "cache écrit par une autre version du contrat, ignoré"
        );
        return None;
    }
    tracing::debug!(
        fichier = %path,
        dossiers = snapshot.folders.len(),
        lignes = snapshot.rows.len(),
        "cache de lecture relu"
    );
    Some(snapshot)
}

/// Écrit le cache d'un démon, sur un fil, sans jamais bloquer l'interface.
pub fn save(host: &str, snapshot: Snapshot) {
    let Some(path) = path(host) else {
        return;
    };
    let spawned = std::thread::Builder::new()
        .name("mailcore-shell-cache".to_owned())
        .spawn(move || {
            if let Err(source) = write(&path, &snapshot) {
                // Un cache non écrit coûte un démarrage plus lent, rien d'autre. Le dire en
                // `debug` et continuer : ce n'est pas une panne de l'application.
                tracing::debug!(%source, fichier = %path, "cache non écrit");
            }
        });
    if let Err(source) = spawned {
        tracing::debug!(%source, "fil d'écriture du cache non démarré");
    }
}

/// L'écriture atomique : fichier temporaire, puis renommage.
fn write(path: &Utf8Path, snapshot: &Snapshot) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension("json.partiel");
    {
        let mut file = std::fs::File::create(&temporary)?;
        // `to_writer` et non `to_string` : le cache peut faire cent kilo-octets, et il n'y a
        // aucune raison de les tenir deux fois en mémoire.
        serde_json::to_writer(&mut file, snapshot).map_err(std::io::Error::other)?;
        file.flush()?;
        // Le `sync_all` avant le renommage est ce qui rend l'atomicité vraie : sans lui, le
        // renommage peut atteindre le disque avant le contenu.
        file.sync_all()?;
    }
    std::fs::rename(&temporary, path)
}

/// Efface le cache d'un démon. **Appelée par le bouton de purge.**
///
/// Rend le chemin effacé, pour que l'interface puisse dire ce qui a disparu.
pub fn purge(host: &str) -> std::io::Result<Utf8PathBuf> {
    let Some(path) = path(host) else {
        return Err(std::io::Error::other("répertoire de données introuvable"));
    };
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(path),
        // Un cache déjà absent est un cache purgé : c'est le résultat demandé.
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(path),
        Err(source) => Err(source),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn a_host_never_escapes_its_directory() {
        // Un hôte est une donnée de l'utilisateur ; le chemin ne doit pas s'en remettre à sa
        // bonne volonté.
        //
        // **Ce qui compte est le répertoire d'arrivée, pas la présence d'un `..` dans le nom.**
        // La première version de ce test refusait `..` en sous-chaîne, et `cache-.._.._x.json`
        // le faisait tomber — alors que ce nom est un fichier unique, sans séparateur, donc
        // incapable de sortir de son répertoire. C'est le séparateur qui traverse, pas le
        // point.
        let reference = path("temoin").unwrap();
        let expected = reference.parent().unwrap().to_path_buf();

        for hostile in [
            "../../ailleurs",
            "..\\..\\ailleurs",
            "hôte:7847",
            "/etc/passwd",
            "a/b/c",
            "..",
            "",
        ] {
            let path = path(hostile).unwrap();
            assert_eq!(
                path.parent().unwrap(),
                expected,
                "l'hôte {hostile:?} a changé de répertoire : {path}"
            );

            let name = path.file_name().unwrap();
            assert!(
                name.starts_with("cache-") && name.ends_with(".json"),
                "nom inattendu : {name}"
            );
            assert!(
                !name.contains('/') && !name.contains('\\'),
                "séparateur dans le nom : {name}"
            );
        }
    }

    #[test]
    fn the_name_stays_bounded() {
        let long = "a".repeat(4_000);
        let path = path(&long).unwrap();
        let name = path.file_name().unwrap();
        // 64 caractères d'hôte, plus `cache-` et `.json`.
        assert!(name.len() <= 64 + 12, "nom de {} caractères", name.len());
    }

    #[test]
    fn a_snapshot_is_bounded_in_rows() {
        let row = dto::Row {
            id: 1,
            date: 0,
            from: "a@b.c".to_owned(),
            from_name: None,
            subject: "s".to_owned(),
            has_attachments: false,
            unread: false,
            flagged: false,
            score: None,
        };
        let rows = vec![row; MAX_ROWS * 3];
        let snapshot = Snapshot::new(String::new(), Vec::new(), Some(1), &rows);
        assert_eq!(snapshot.rows.len(), MAX_ROWS);
    }

    #[test]
    fn an_empty_snapshot_is_not_written() {
        let snapshot = Snapshot::new(String::new(), Vec::new(), None, &[]);
        assert!(!snapshot.worth_writing());
    }

    #[test]
    fn a_round_trip_keeps_what_matters() {
        let dir = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(dir.path().join("cache.json")).unwrap();
        let snapshot = Snapshot {
            protocol: mailapi::PROTOCOL,
            revision: "r1".to_owned(),
            folders: Vec::new(),
            folder: Some(7),
            rows: Vec::new(),
        };
        write(&path, &snapshot).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        let read: Snapshot = serde_json::from_str(&text).unwrap();
        assert_eq!(read.folder, Some(7));
        assert_eq!(read.revision, "r1");

        // Le fichier temporaire ne survit pas à l'écriture.
        assert!(!path.with_extension("json.partiel").exists());
    }
}
