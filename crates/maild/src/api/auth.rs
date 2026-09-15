//! Authentification des clients distants.
//!
//! Un jeton porteur, **généré par le démon** et jamais choisi par l'utilisateur : un
//! secret que quelqu'un a tapé est un secret devinable. Stocké côté client dans le
//! trousseau du système, jamais dans un fichier de configuration ni dans une variable
//! d'environnement (`docs/PRIVACY.md`, section 7).
//!
//! ## Fail closed — critère 10 de `docs/PHASE-1.md`
//!
//! Un démon configuré pour écouter sur une interface non locale **sans jeton refuse de
//! démarrer**. Pas un avertissement dans un journal que personne ne lit : une mauvaise
//! configuration doit empêcher le service de tourner, pas exposer une boîte mail en clair
//! sur un réseau. C'est un test, pas une intention.
//!
//! ## Détails qui comptent au moment de l'écrire
//!
//! - Comparaison du jeton en temps constant. Un `==` sur des `String` fuit la longueur du
//!   préfixe commun.
//! - Aucun jeton, complet ou tronqué, dans les logs — quel que soit le niveau de
//!   `tracing`.
//! - Un échec d'authentification se journalise sans le jeton présenté et sans le corps de
//!   la requête.
