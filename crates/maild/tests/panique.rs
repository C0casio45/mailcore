//! Ce qu'une panique laisse dans le journal : son emplacement, jamais un message formaté.
//!
//! Un exécutable de test à part, parce que le crochet de panique est global au processus : posé
//! dans les tests unitaires du crate, il ferait taire le message de tout test qui échoue à côté.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::Write;
use std::sync::{Arc, Mutex};

#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl Write for Captured {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Captured {
    type Writer = Self;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

#[test]
fn a_panic_leaves_its_place_and_a_written_message_but_never_a_formatted_one() {
    let captured = Captured::default();
    tracing_subscriber::fmt()
        .with_ansi(false)
        .with_writer(captured.clone())
        .init();
    maild::panics::install();

    // Un message formaté à partir d'une donnée — ce que n'importe quelle dépendance peut faire
    // d'une entrée qu'elle refuse. Ici, une chaîne qui vient d'un message.
    let body = String::from("Objet : rendez-vous chez le notaire");
    let formatted = std::panic::catch_unwind(|| panic!("en-tête illisible : {body}"));
    assert!(formatted.is_err());

    // Un message écrit dans le source, lui, vient du code.
    let written = std::panic::catch_unwind(|| panic!("état impossible de la file"));
    assert!(written.is_err());

    let log = String::from_utf8(captured.0.lock().unwrap().clone()).unwrap();
    assert!(!log.contains("notaire"), "{log}");
    assert!(log.contains("non recopié"), "{log}");
    assert!(log.contains("état impossible de la file"), "{log}");
    // L'emplacement, qui suffit à retrouver la ligne : ce fichier, en chemin relatif.
    assert!(log.contains("tests/panique.rs:"), "{log}");
}
