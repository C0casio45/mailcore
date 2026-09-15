//! Génère le contexte Tauri — icônes, capacités, schémas — à la compilation.

fn main() {
    tauri_build::build();

    // **Le paquet web est embarqué dans le binaire à la compilation**, par
    // `tauri::generate_context!`. Sans cette ligne, reconstruire le front ne provoque aucune
    // recompilation : `cargo build` répond « Finished » en une seconde et l'exécutable garde
    // l'ancienne page.
    //
    // Écrit après y avoir perdu une exécution de mesure : la CSP corrigée était bien dans
    // `dist-tauri/index.html`, et le binaire servait toujours celle d'avant. Un cache muet qui
    // sert une politique de sécurité périmée est exactement ce qu'on ne veut pas déboguer deux
    // fois.
    //
    // Cargo parcourt un répertoire récursivement pour ce contrôle, donc un seul fichier
    // modifié dans le paquet suffit à déclencher la recompilation.
    println!("cargo:rerun-if-changed=../web/dist-tauri");
}
