//! SMTP → le monde. **Le seul crate du projet qui écrive vers l'extérieur.**
//!
//! ## Ce que ça change par rapport à tout le reste
//!
//! Partout ailleurs, une erreur affiche faux et le passage suivant corrige. Ici, un message
//! part chez quelqu'un et rien ne le rattrape. Trois conséquences, et elles décident de la
//! forme du crate :
//!
//! **Un message part une fois.** Le protocole ne permet pas de demander à un serveur « as-tu
//! déjà reçu ceci ? ». La seule défense est locale, et elle est dans la file d'envoi — pas
//! ici. Ce crate remet un message et **dit précisément où il en était** quand ça a échoué,
//! pour que l'appelant puisse décider sans deviner. C'est à ça que sert [`Stage`].
//!
//! **Un refus doit être actionnable.** Un `552` n'est pas un `550` : l'un dit « trop gros »,
//! l'autre « destinataire inconnu ». Le critère 8 de `docs/PHASE-3.md` demande que
//! l'utilisateur voie quoi faire, donc [`Error`] distingue les familles au lieu de recopier un
//! code.
//!
//! **Rien ne s'ajoute au message en secret.** Pas d'en-tête `X-Mailer`, pas d'identifiant de
//! suivi. Ce que ce crate écrit sur le fil est exactement ce que l'appelant lui a donné, plus
//! ce que la RFC 5321 impose à l'enveloppe.
//!
//! ## Pourquoi le flux est un paramètre de type
//!
//! La même raison que `mailsync` : les tests parlent à un serveur de test sur le bouclage, en
//! clair, et le code de production parlera en TLS. Ce n'est **pas** une porte vers le SMTP en
//! clair : `mailcore::Security` n'a pas de variante en clair, donc aucun compte ne peut en
//! demander un — un `SUBMIT` non chiffré transporte le mot de passe.
//!
//! ## Ce que ce crate ne fait pas
//!
//! Il ne met pas le message en file et il ne décide pas de réessayer. Il parle SMTP et rend
//! compte.
//!
//! La connexion chiffrée est dans [`tls::connect`], et c'est le **seul** constructeur de flux
//! que le code de production appelle. Le paramètre de type sert aux tests, pas à ouvrir une
//! porte : les scripts de `tests/dialogue.rs` parlent en clair sur le bouclage, jamais au
//! réseau.

#![forbid(unsafe_code)]

pub mod client;
pub mod compose;
pub mod error;
pub mod queue;
pub mod reply;
pub mod submit;
pub mod tls;

pub use client::{Client, Stage};
pub use error::{Error, Refusal, Result};
pub use queue::{Outcome, Transport, deliver_one};
pub use reply::Reply;
pub use submit::Submitter;
pub use tls::connect;
