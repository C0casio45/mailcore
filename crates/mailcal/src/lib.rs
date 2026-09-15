//! # mailcal
//!
//! La lecture d'une invitation `text/calendar` — RFC 5545 — et **rien d'autre**.
//!
//! ## Ce que ce crate fait
//!
//! Il lit. Une pièce `text/calendar` reçue entre ici comme des octets écrits par un inconnu, et
//! en sort comme un rendez-vous : un titre, un début, une fin, un organisateur, des
//! participants — ou un **refus qui nomme ce qui manque**. C'est le critère 6 de
//! `docs/PHASE-3.md`, et le « ou » en est la moitié importante : une invitation qu'on affiche
//! de travers est pire qu'une invitation qu'on refuse d'afficher, parce que personne ne sait
//! qu'elle est fausse.
//!
//! ## Ce qu'il ne fait pas, et ne fera pas ici
//!
//! **Aucune requête réseau, et il n'a rien pour en faire.** Ce crate ne dépend d'aucun client
//! HTTP, d'aucune base de fuseaux téléchargée, de rien qui parle à quoi que ce soit. Une
//! invitation porte souvent une URL — de conférence, d'organisateur, de « voir dans mon
//! agenda » — et [`Invitation::urls`] les rend **pour être montrées**. Les suivre est une
//! action de l'utilisateur, jamais un effet de l'affichage : `docs/PRIVACY.md`, règle 5.
//!
//! **Il ne répond pas à une invitation.** Lire est un problème d'analyse ; répondre est un
//! problème de protocole, et écrire dans un agenda distant un troisième.
//! `docs/PHASE-3.md` les met dehors explicitement.
//!
//! **Il n'interprète pas les répétitions.** Une `RRULE` d'événement est rendue telle qu'elle est
//! écrite, pour être affichée — « tous les lundis » n'est pas calculé, et surtout aucune
//! occurrence n'est déduite. Une occurrence déduite de travers déplacerait un rendez-vous.
//!
//! ## Les fuseaux : ce qui est résolu, et comment
//!
//! Une heure d'événement s'écrit de quatre façons, et trois seulement sont résolubles sans
//! rien demander à personne :
//!
//! - `DTSTART:20260910T140000Z` — de l'UTC. Rien à résoudre ;
//! - `DTSTART;VALUE=DATE:20260910` — une journée entière. Pas d'heure, donc pas de fuseau ;
//! - `DTSTART;TZID=Europe/Paris:20260910T140000` **avec** une `VTIMEZONE` dans le même fichier —
//!   le décalage est écrit dans le fichier, par celui qui a écrit l'heure. C'est la source la
//!   plus fiable qui existe : elle vient du même producteur que l'heure elle-même ;
//! - `DTSTART;TZID=Europe/Paris:20260910T140000` **sans** `VTIMEZONE` — non résoluble ici. Il
//!   faudrait une base de fuseaux, donc une dépendance qui affirmerait un décalage que le
//!   fichier ne dit pas. L'heure murale et le nom du fuseau sont rendus, l'instant est absent,
//!   et [`Gap::UnknownZone`] le dit.
//!
//! Ce dernier choix est délibéré : afficher « 14:00 » en affirmant un instant faux d'une heure
//! est le mode de panne le plus coûteux d'un lecteur d'invitations.

#![forbid(unsafe_code)]

pub mod ics;
pub mod invitation;
pub mod time;

pub use ics::{Property, unfold};
pub use invitation::{Answer, Gap, Invitation, Method, Person, read};
pub use time::{Civil, Moment, Observance, Timezone, Zone};
