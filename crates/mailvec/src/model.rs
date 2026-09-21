//! Le modèle local, chargé depuis un répertoire.
//!
//! Une enveloppe mince autour de `model2vec-rs`, et elle existe pour trois raisons qui sont
//! toutes des refus.
//!
//! **Refus de résoudre un nom.** `StaticModel::from_pretrained` accepte un chemin *ou* un
//! identifiant de dépôt. La variante `local-only` n'a pas de client HTTP pour aller le chercher,
//! mais l'ambiguïté resterait dans la signature — et une signature qui accepte les deux finit par
//! recevoir les deux. Ici, le paramètre est un répertoire, et son absence est une erreur qui se
//! lit.
//!
//! **Refus de charger avant de savoir s'il y a du travail.** Le chargement coûte une cinquantaine
//! de millisecondes et une centaine de mébioctets. La passe demande d'abord au store s'il reste
//! un message à vectoriser — un `COUNT` sur colonne indexée — et ne construit ce type qu'ensuite.
//! C'est la leçon du 2026-09-11, appliquée avant d'avoir le défaut.
//!
//! **Refus de rendre des vecteurs non normalisés.** Le magasin les range normalisés pour que le
//! balayage soit un simple produit scalaire ; c'est `VectorWriter::append` qui s'en charge, et
//! ce module ne fait rien qui pourrait entrer en concurrence avec lui.

use camino::{Utf8Path, Utf8PathBuf};

use crate::{Error, Result};

/// Le modèle chargé, prêt à encoder.
///
/// À garder vivant sur toute la durée d'une passe : le construire par lot paierait le chargement
/// autant de fois.
pub struct Model {
    inner: model2vec_rs::model::StaticModel,
    dim: usize,
}

// `StaticModel` ne porte pas `Debug`, et le dériver ici afficherait de toute façon une table de
// trente-quatre mébioctets. On rend ce qui identifie le modèle et rien d'autre — même
// raisonnement que le `Debug` écrit à la main de `Credential`, pour une raison de volume plutôt
// que de secret.
impl std::fmt::Debug for Model {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Model").field("dim", &self.dim).finish()
    }
}

impl Model {
    /// Charge le modèle depuis un répertoire.
    ///
    /// Le répertoire est celui que `cargo xtask model-trim` a produit : le modèle taillé au
    /// vocabulaire du corpus, 34 Mio au lieu de 488.
    ///
    /// # Errors
    ///
    /// [`Error::Model`] si le répertoire n'existe pas, s'il ne porte pas les fichiers attendus,
    /// ou si le modèle ne rend aucune dimension.
    pub fn load(dir: impl AsRef<Utf8Path>) -> Result<Self> {
        let dir = dir.as_ref().to_owned();

        // Vérifier l'existence avant d'appeler la bibliothèque : son erreur pour un répertoire
        // absent est la même que pour un fichier corrompu, et les deux appellent des gestes
        // opposés — installer, ou réinstaller.
        if !dir.is_dir() {
            return Err(Error::Model {
                path: dir,
                reason: "le répertoire n'existe pas".to_owned(),
            });
        }

        let inner =
            model2vec_rs::model::StaticModel::from_pretrained(dir.as_str(), None, None, None)
                .map_err(|source| Error::Model {
                    path: dir.clone(),
                    reason: source.to_string(),
                })?;

        // La dimension se lit sur un encodage à blanc plutôt que sur un champ : la bibliothèque
        // n'en expose pas, et la deviner depuis le fichier `safetensors` ferait de ce module un
        // second lecteur de format. Un mot suffit, et le coût est celui d'un encodage.
        let dim = inner.encode(&["a".to_owned()]).first().map_or(0, Vec::len);
        if dim == 0 {
            return Err(Error::Model {
                path: dir,
                reason: "le modèle rend des vecteurs vides".to_owned(),
            });
        }

        tracing::info!(dir = %dir, dim, "modèle chargé");
        Ok(Self { inner, dim })
    }

    /// La dimension des vecteurs de ce modèle.
    #[must_use]
    pub fn dim(&self) -> usize {
        self.dim
    }

    /// Encode un lot de textes.
    ///
    /// Un lot et non un texte : c'est la forme que la bibliothèque sait faire, et la passe en a
    /// besoin pour amortir. Les vecteurs sortent dans l'ordre des textes.
    ///
    /// # Errors
    ///
    /// [`Error::Encode`] si le nombre de vecteurs rendus n'est pas celui des textes fournis, ou
    /// si l'un d'eux n'a pas la dimension annoncée. Les deux seraient des défauts de la
    /// bibliothèque plutôt que des nôtres — et c'est exactement pourquoi ils sont vérifiés :
    /// un décalage d'un cran donnerait des vecteurs rangés sous l'identifiant du voisin, ce qui
    /// ne se verrait que sur la qualité des résultats.
    pub fn encode(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let vectors = self.inner.encode(texts);
        if vectors.len() != texts.len() {
            return Err(Error::Encode(format!(
                "{} vecteurs pour {} textes",
                vectors.len(),
                texts.len()
            )));
        }
        if let Some(found) = vectors.iter().map(Vec::len).find(|len| *len != self.dim) {
            return Err(Error::Encode(format!(
                "vecteur de dimension {found}, attendu {}",
                self.dim
            )));
        }
        Ok(vectors)
    }
}

/// Le répertoire où le modèle est installé, sous la racine du store.
///
/// À côté du magasin de vecteurs et de l'index tantivy, pour la raison inverse de la leur : eux
/// sont **reconstructibles** depuis les blobs, lui ne l'est pas — le perdre demande de le
/// réinstaller. Le ranger ailleurs serait plus juste et compliquerait la vie de tout le monde
/// pour une distinction que seul un nettoyage de disque rendrait visible.
#[must_use]
pub fn default_dir(store_root: &Utf8Path) -> Utf8PathBuf {
    store_root.join("model")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_directory_is_an_error_and_not_a_download() {
        let dir = tempfile::tempdir().expect("répertoire jetable");
        let absent = Utf8PathBuf::from_path_buf(dir.path().join("pas-la")).expect("chemin UTF-8");
        let found = Model::load(&absent);
        assert!(
            matches!(&found, Err(Error::Model { reason, .. }) if reason.contains("n'existe pas")),
            "{found:?}"
        );
    }

    /// Un répertoire qui existe mais ne porte pas de modèle : l'erreur doit dire autre chose que
    /// la précédente, parce que le geste n'est pas le même — réinstaller plutôt qu'installer.
    #[test]
    fn a_directory_without_a_model_says_something_else() {
        let dir = tempfile::tempdir().expect("répertoire jetable");
        let path = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("chemin UTF-8");
        let found = Model::load(&path);
        assert!(
            matches!(&found, Err(Error::Model { reason, .. }) if !reason.contains("n'existe pas")),
            "{found:?}"
        );
    }

    #[test]
    fn the_default_directory_sits_beside_the_other_derived_stores() {
        let root = Utf8Path::new("/store");
        assert_eq!(default_dir(root), Utf8PathBuf::from("/store/model"));
    }
}
