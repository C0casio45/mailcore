//! `mail model install` : dire quoi télécharger, puis vérifier et installer.
//!
//! La commande ne télécharge rien — voir `mailvec::install`. Sans `--from`, elle dit les trois
//! fichiers, leur adresse à une révision épinglée et leur taille ; avec, elle vérifie chacun contre
//! son empreinte, taille le vocabulaire et range le modèle là où le démon le lit.

use anyhow::{Context, Result};
use camino::Utf8PathBuf;

/// Point d'entrée de `mail model install`.
///
/// # Errors
///
/// Si le store est introuvable, si un fichier ne correspond pas à son empreinte, ou si un modèle
/// est déjà installé.
pub fn install(store_root: Option<&Utf8PathBuf>, from: Option<&Utf8PathBuf>) -> Result<()> {
    let root = crate::store_root(store_root)?;
    let target = mailvec::bert::default_dir(&root);

    let Some(source) = from else {
        let total: u64 = mailvec::install::FILES.iter().map(|file| file.size).sum();
        println!("Modèle      {}", mailvec::install::REPOSITORY);
        println!("Révision    {}", mailvec::install::REVISION);
        println!(
            "Poids       {:.0} Mo à télécharger, {:.0} Mo une fois taillé\n",
            mb(total),
            mb(mailvec::install::TRIMMED_BYTES)
        );
        println!("Téléchargez ces trois fichiers dans un même répertoire :");
        for file in mailvec::install::FILES {
            println!(
                "  {:<18} {:>9.1} Mo  {}",
                file.name,
                mb(file.size),
                mailvec::install::url(file.name)
            );
        }
        println!("\npuis : mail model install --from <ce répertoire>");
        println!(
            "\nChaque fichier est vérifié contre une empreinte écrite dans le code avant d'être \
             installé.\nmailcore ne télécharge rien lui-même, et aucun octet de courrier ne quitte \
             la machine."
        );
        println!("Installé dans : {target}");
        return Ok(());
    };

    println!("Vérification de {source}…");
    let installed = mailvec::install::install(source, &root).context("installation du modèle")?;
    println!("Installé     {}", installed.target);
    println!(
        "Vocabulaire  {} pièces gardées sur {}",
        installed.trim.kept, installed.trim.pieces
    );
    println!("Empreinte    {}", installed.trim.digest);
    println!(
        "\nLe démon vectorise de lui-même ce qui arrive. Pour le courrier déjà là : \
         mail vectors --bert"
    );
    Ok(())
}

/// Des octets en mégaoctets décimaux, l'unité d'un téléchargement.
fn mb(bytes: u64) -> f64 {
    #[allow(clippy::cast_precision_loss)]
    {
        bytes as f64 / 1_000_000.0
    }
}
