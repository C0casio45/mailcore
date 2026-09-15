//! Le binaire de bureau : rien d'autre que l'appel à la bibliothèque.
//!
//! `windows_subsystem = "windows"` en release supprime la console qui s'ouvrirait derrière la
//! fenêtre. En debug elle reste, parce que c'est là que les traces sortent.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
#![forbid(unsafe_code)]

fn main() -> anyhow::Result<()> {
    mail_ui_lib::run()
}
