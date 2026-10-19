//! Le journal de la coquille, sur disque.
//!
//! **Pourquoi il existe.** En release, la coquille est du sous-système Windows : pas de console,
//! donc une sortie d'erreur que personne ne lit. Le 2026-10-13, un arriéré de vecteurs refusé a
//! dû être reconstitué depuis le store, parce que la phrase de l'écran — « le journal du démon dit
//! pourquoi » — désignait un journal qui n'existait nulle part. Une panique, une erreur
//! d'arguments, un pilote OpenGL absent restaient muets de la même façon (voir `failure`).
//!
//! **Ce qu'il ne doit jamais devenir** : un deuxième exemplaire du courrier. Un journal est fait
//! pour être collé dans un rapport, donc pour sortir de la machine ; `docs/PRIVACY.md` §8 n'y
//! admet ni sujet, ni adresse, ni corps, quel que soit le niveau. D'où cinq règles :
//!
//! - **`info` au plus bas, quoi que dise `RUST_LOG`.** La sortie d'erreur suit la variable — c'est
//!   l'outil de qui développe —, le fichier non : un `debug` d'une dépendance ne se relit pas ;
//! - **une panique n'y laisse que son emplacement** (`maild::panics`), et les enfants du démon
//!   tiennent la même règle, puisque leurs lignes passent par ici ;
//! - **dans `%LOCALAPPDATA%`**, jamais dans le profil itinérant — la règle du cache de lecture ;
//! - **un fichier par lancement**, nommé par sa date : celui de la panne se trouve sans lire les
//!   autres, et deux coquilles ouvertes en même temps n'écrivent jamais le même fichier ;
//! - **borné** : [`LIMIT`] par fichier, un nouveau au-delà, et seuls les [`KEEP`] plus récents
//!   restent. Quatre mégaoctets au plus, sans rien à faire pour personne ;
//! - **le répertoire personnel y devient `~`**, au point d'écriture. Un chemin de store, de
//!   modèle ou de pièce jointe porte le nom du compte Windows, qui est souvent celui de la
//!   personne ; trier les appelants un par un en oublierait un, une barrière ne l'oublie pas.
//!
//! Chaque ligne est écrite **sans tampon** : la ligne qui compte est la dernière avant une
//! panne, et un tampon la perdrait avec le processus.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::sync::{Arc, Mutex};

use camino::{Utf8Path, Utf8PathBuf};

/// Le filtre du fichier, et celui de la sortie d'erreur quand `RUST_LOG` ne dit rien.
///
/// - `html5ever` avertit à chaque tableau mal formé, ce qui est le cas normal du courrier réel ;
/// - `tantivy` écrit en `info` le chemin de son `meta.json` à chaque validation de l'index —
///   donc à chaque moisson —, par le pont `log` que `tracing-subscriber` installe. Ses
///   avertissements restent : une fusion jetée se dit là, et nulle part ailleurs.
pub const FILTER: &str = "info,html5ever=error,tantivy=warn";

/// La taille au-delà de laquelle un lancement passe à un nouveau fichier.
pub const LIMIT: u64 = 1024 * 1024;

/// Combien de fichiers restent, tous lancements confondus.
pub const KEEP: usize = 4;

/// Le début du nom de chaque fichier. Rien d'autre dans le répertoire n'est effacé.
const PREFIX: &str = "mail-shell-";

/// La fin du nom de chaque fichier.
const SUFFIX: &str = ".log";

/// Le répertoire du journal : `%LOCALAPPDATA%\mailcore\journal` sous Windows.
#[must_use]
pub fn directory() -> Option<Utf8PathBuf> {
    let dirs = directories::BaseDirs::new()?;
    // `data_local_dir` et non `data_dir` : la raison est écrite dans `cache::path`.
    let mut root = Utf8PathBuf::from_path_buf(dirs.data_local_dir().to_path_buf()).ok()?;
    root.push("mailcore");
    root.push("journal");
    Some(root)
}

/// Un journal ouvert : ce que `tracing_subscriber` appelle pour écrire une ligne.
#[derive(Clone)]
pub struct Journal {
    inner: Arc<Mutex<Inner>>,
}

struct Inner {
    dir: Utf8PathBuf,
    /// La date du lancement et le numéro du processus, communs à tous les fichiers de ce
    /// lancement.
    stem: String,
    /// Le rang du fichier en cours dans ce lancement, à partir de 1.
    part: u32,
    file: Option<File>,
    written: u64,
    limit: u64,
    keep: usize,
    /// Les graphies du répertoire personnel à remplacer par `~`, la plus longue d'abord.
    home: Vec<String>,
}

/// Les façons dont un chemin sous le répertoire personnel arrive dans une ligne : tel quel
/// (`%chemin`), avec des barres obliques (`camino`, un chemin écrit à la main), et échappé
/// (`?chemin`, où `Debug` double chaque barre inverse). La plus longue d'abord, pour qu'une
/// graphie ne soit pas remplacée à moitié par une autre qu'elle contient.
fn spellings(home: &str) -> Vec<String> {
    // Un répertoire personnel de un ou deux caractères — `/`, `C:` — remplacerait n'importe
    // quoi : rien ne vaut mieux qu'un journal illisible.
    if home.trim_end_matches(['/', '\\']).len() < 3 {
        return Vec::new();
    }
    let mut all = vec![
        home.to_owned(),
        home.replace('\\', "/"),
        home.replace('\\', "\\\\"),
    ];
    all.sort_by_key(|spelling| std::cmp::Reverse(spelling.len()));
    all.dedup();
    all
}

impl Journal {
    /// Ouvre le premier fichier de ce lancement, et fait la place.
    ///
    /// # Errors
    ///
    /// Si le répertoire ne se crée pas ou si le fichier ne s'ouvre pas : la coquille démarre
    /// alors sans journal sur disque, ce qui vaut mieux que ne pas démarrer.
    pub fn open(dir: &Utf8Path) -> std::io::Result<(Self, Utf8PathBuf)> {
        let stamp = jiff::Zoned::now().strftime("%Y-%m-%d_%H-%M-%S").to_string();
        let home = directories::BaseDirs::new()
            .and_then(|dirs| dirs.home_dir().to_str().map(str::to_owned));
        Self::open_with(dir, &stamp, LIMIT, KEEP, home.as_deref())
    }

    fn open_with(
        dir: &Utf8Path,
        stamp: &str,
        limit: u64,
        keep: usize,
        home: Option<&str>,
    ) -> std::io::Result<(Self, Utf8PathBuf)> {
        std::fs::create_dir_all(dir)?;
        let mut inner = Inner {
            dir: dir.to_owned(),
            stem: format!("{PREFIX}{stamp}_{}", std::process::id()),
            part: 0,
            file: None,
            written: 0,
            limit,
            keep: keep.max(1),
            home: home.map(spellings).unwrap_or_default(),
        };
        let path = inner.next_file()?;
        Ok((
            Self {
                inner: Arc::new(Mutex::new(inner)),
            },
            path,
        ))
    }
}

impl Inner {
    /// Ferme le fichier en cours, ouvre le suivant, et efface les plus anciens.
    fn next_file(&mut self) -> std::io::Result<Utf8PathBuf> {
        self.file = None;
        self.part += 1;
        // Le rang sur trois chiffres : l'ordre alphabétique reste l'ordre d'écriture.
        let path = self
            .dir
            .join(format!("{}_{:03}{SUFFIX}", self.stem, self.part));
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        self.file = Some(file);
        self.written = 0;
        prune(&self.dir, self.keep);
        Ok(path)
    }

    fn write_line(&mut self, bytes: &[u8]) {
        let redacted = self.redact(bytes);
        let bytes = redacted.as_slice();
        if self.written > 0 && self.written + bytes.len() as u64 > self.limit {
            // Un échec ici laisse `file` à `None` : les lignes suivantes sont perdues, la
            // coquille continue. Rien d'un journal ne doit pouvoir l'arrêter.
            let _ = self.next_file();
        }
        if let Some(file) = &mut self.file
            && file.write_all(bytes).is_ok()
        {
            self.written += bytes.len() as u64;
        }
    }

    /// La ligne, le répertoire personnel remplacé par `~`.
    ///
    /// Sans distinction de casse : Windows ne la fait pas, et `c:\users\…` désigne le même
    /// répertoire que `C:\Users\…`. Une ligne qui n'est pas de l'UTF-8 — `tracing` n'en écrit
    /// pas — passe telle quelle plutôt que d'être perdue.
    fn redact(&self, bytes: &[u8]) -> Vec<u8> {
        let Ok(line) = std::str::from_utf8(bytes) else {
            return bytes.to_vec();
        };
        let mut line = line.to_owned();
        for spelling in &self.home {
            line = replace_ignoring_ascii_case(&line, spelling, "~");
        }
        line.into_bytes()
    }
}

/// Remplace `needle` dans `haystack`, sans distinction de casse ASCII.
fn replace_ignoring_ascii_case(haystack: &str, needle: &str, with: &str) -> String {
    let lower_haystack = haystack.to_ascii_lowercase();
    let lower_needle = needle.to_ascii_lowercase();
    // `to_ascii_lowercase` garde les longueurs et les frontières de caractères : un indice dans
    // l'une est un indice dans l'autre.
    let mut out = String::with_capacity(haystack.len());
    let mut from = 0;
    while let Some(found) = lower_haystack[from..].find(&lower_needle) {
        let at = from + found;
        out.push_str(&haystack[from..at]);
        out.push_str(with);
        from = at + needle.len();
    }
    out.push_str(&haystack[from..]);
    out
}

/// Efface les fichiers du journal au-delà des `keep` plus récents.
///
/// Le plus récent se juge **au nom**, qui commence par la date du lancement, et non à la date
/// de modification : une coquille restée ouverte une semaine écrit encore dans un fichier daté
/// d'il y a une semaine, et c'est un fichier en cours, pas un vieux.
fn prune(dir: &Utf8Path, keep: usize) {
    let Ok(entries) = dir.read_dir_utf8() else {
        return;
    };
    let mut names: Vec<String> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_owned())
        .filter(|name| name.starts_with(PREFIX) && name.ends_with(SUFFIX))
        .collect();
    if names.len() <= keep {
        return;
    }
    names.sort_unstable();
    for name in &names[..names.len() - keep] {
        // Un fichier qu'une autre coquille tient encore ouvert s'efface quand elle le ferme.
        let _ = std::fs::remove_file(dir.join(name));
    }
}

/// Ce que `tracing_subscriber` écrit pour une ligne : il la formate entière, puis l'écrit.
pub struct Line {
    inner: Arc<Mutex<Inner>>,
    buffer: Vec<u8>,
}

impl Write for Line {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.buffer.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Drop for Line {
    /// Une ligne s'écrit d'un seul tenant, à la fin : deux fils qui journalisent ensemble ne
    /// s'entremêlent pas, et le passage au fichier suivant ne coupe jamais une ligne en deux.
    fn drop(&mut self) {
        if self.buffer.is_empty() {
            return;
        }
        if let Ok(mut inner) = self.inner.lock() {
            inner.write_line(&self.buffer);
        }
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Journal {
    type Writer = Line;

    fn make_writer(&'a self) -> Self::Writer {
        Line {
            inner: Arc::clone(&self.inner),
            buffer: Vec::new(),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use tracing_subscriber::fmt::MakeWriter;

    fn files(dir: &Utf8Path) -> Vec<String> {
        let mut names: Vec<String> = dir
            .read_dir_utf8()
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_owned())
            .collect();
        names.sort();
        names
    }

    fn line(journal: &Journal, text: &str) {
        let mut writer = journal.make_writer();
        writer.write_all(text.as_bytes()).unwrap();
    }

    #[test]
    fn a_launch_writes_its_own_dated_file() {
        let dir = tempfile::tempdir().unwrap();
        let dir = Utf8Path::from_path(dir.path()).unwrap();
        let (journal, path) =
            Journal::open_with(dir, "2026-10-19_20-00-00", 1024, 4, None).unwrap();
        line(&journal, "une ligne\n");
        assert!(
            path.file_name()
                .unwrap()
                .starts_with("mail-shell-2026-10-19_20-00-00_")
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "une ligne\n");
    }

    #[test]
    fn a_file_past_the_limit_gives_way_to_the_next_without_cutting_a_line() {
        let dir = tempfile::tempdir().unwrap();
        let dir = Utf8Path::from_path(dir.path()).unwrap();
        let (journal, first) = Journal::open_with(dir, "2026-10-19_20-00-00", 20, 4, None).unwrap();
        line(&journal, "0123456789\n");
        line(&journal, "abcdefghij\n");
        let names = files(dir);
        assert_eq!(names.len(), 2, "{names:?}");
        assert_eq!(std::fs::read_to_string(&first).unwrap(), "0123456789\n");
        assert_eq!(
            std::fs::read_to_string(dir.join(&names[1])).unwrap(),
            "abcdefghij\n"
        );
    }

    #[test]
    fn a_line_longer_than_the_limit_is_still_written() {
        // Le contrôle inverse : sans la garde `written > 0`, une ligne trop longue ouvrirait un
        // fichier vide à chaque fois, et ne serait jamais écrite nulle part.
        let dir = tempfile::tempdir().unwrap();
        let dir = Utf8Path::from_path(dir.path()).unwrap();
        let (journal, first) = Journal::open_with(dir, "2026-10-19_20-00-00", 4, 4, None).unwrap();
        line(&journal, "bien plus long que la limite\n");
        assert_eq!(
            std::fs::read_to_string(&first).unwrap(),
            "bien plus long que la limite\n"
        );
        assert_eq!(files(dir).len(), 1);
    }

    #[test]
    fn only_the_most_recent_files_are_kept_and_nothing_else_is_touched() {
        let dir = tempfile::tempdir().unwrap();
        let dir = Utf8Path::from_path(dir.path()).unwrap();
        std::fs::write(dir.join("notes.txt"), "à moi").unwrap();
        for stamp in [
            "2026-10-01_08-00-00",
            "2026-10-02_08-00-00",
            "2026-10-03_08-00-00",
        ] {
            let (journal, _) = Journal::open_with(dir, stamp, 1024, 2, None).unwrap();
            line(&journal, "x\n");
        }
        let names = files(dir);
        assert_eq!(names.len(), 3, "{names:?}");
        assert!(names.contains(&"notes.txt".to_owned()));
        assert!(names.iter().any(|name| name.contains("2026-10-02")));
        assert!(names.iter().any(|name| name.contains("2026-10-03")));
        assert!(!names.iter().any(|name| name.contains("2026-10-01")));
    }

    #[test]
    fn the_parts_of_one_launch_are_pruned_in_writing_order() {
        // Le rang sur trois chiffres garde l'ordre alphabétique égal à l'ordre d'écriture
        // au-delà de neuf fichiers : sur un chiffre, `_10` passerait avant `_2`, et le
        // nettoyage effacerait le fichier en cours.
        let dir = tempfile::tempdir().unwrap();
        let dir = Utf8Path::from_path(dir.path()).unwrap();
        let (journal, _) = Journal::open_with(dir, "2026-10-01_08-00-00", 2, 3, None).unwrap();
        for _ in 0..12 {
            line(&journal, "xx\n");
        }
        let names = files(dir);
        assert_eq!(names.len(), 3, "{names:?}");
        assert!(names[2].ends_with("_012.log"), "{names:?}");
        assert!(names[0].ends_with("_010.log"), "{names:?}");
    }

    fn written_with_home(home: &str, text: &str) -> String {
        let dir = tempfile::tempdir().unwrap();
        let dir = Utf8Path::from_path(dir.path()).unwrap();
        let (journal, path) =
            Journal::open_with(dir, "2026-10-19_20-00-00", 1024, 4, Some(home)).unwrap();
        line(&journal, text);
        std::fs::read_to_string(path).unwrap()
    }

    #[test]
    fn the_home_directory_becomes_a_tilde_in_every_spelling() {
        let home = r"C:\Users\Camille Durand";
        // Tel quel, en barres obliques, échappé par `Debug`, et dans une autre casse.
        let written = written_with_home(
            home,
            "store=C:\\Users\\Camille Durand\\AppData\\Local\\mailcore \
             modèle=C:/Users/Camille Durand/m \
             erreur=\"C:\\\\Users\\\\Camille Durand\\\\x\" \
             autre=c:\\users\\camille durand\\y\n",
        );
        assert!(!written.to_lowercase().contains("camille"), "{written}");
        assert_eq!(
            written,
            "store=~\\AppData\\Local\\mailcore modèle=~/m erreur=\"~\\\\x\" autre=~\\y\n"
        );
    }

    #[test]
    fn a_line_without_the_home_directory_is_left_alone() {
        // Le contrôle inverse : la barrière ne touche qu'au répertoire personnel. Un chemin
        // voisin — même début, autre compte — reste tel quel, accents compris.
        let text = "store=F:\\courrier élu ; voisin=C:\\Users\\Camille Duranda\\z\n";
        let written = written_with_home(r"C:\Users\Camille Durand", text);
        // `Camille Duranda` commence comme le répertoire personnel : il est remplacé jusqu'à
        // la frontière du nom, ce qui retire le compte voisin au lieu de le garder — l'erreur
        // va du côté de moins écrire.
        assert_eq!(written, "store=F:\\courrier élu ; voisin=~a\\z\n");
    }

    #[test]
    fn a_home_too_short_to_mean_anything_is_not_replaced() {
        assert!(spellings("/").is_empty());
        assert!(spellings("C:\\").is_empty());
        let written = written_with_home("/", "chemin=/var/log/x\n");
        assert_eq!(written, "chemin=/var/log/x\n");
    }
}
