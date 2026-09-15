//! La conformité d'une **réponse**, vérifiée sur les vrais fils du corpus — critère 1.
//!
//! ## Ce que ce banc mesure, et ce qu'il ne remplace pas
//!
//! Le critère 1 demande qu'un message envoyé arrive **et soit conforme** : `Message-ID` présent,
//! `In-Reply-To` et `References` corrects sur une réponse, en-têtes non ASCII lisibles chez le
//! destinataire. Deux aller-retours réels l'ont vérifié le 2026-09-09 pour un premier envoi ; la
//! conformité d'une *réponse* restait ouverte.
//!
//! Ce banc la vérifie **mécaniquement, sur des centaines de vrais fils** : pour chaque message
//! du corpus qui porte un `Message-ID`, il construit la réponse comme la fenêtre de rédaction
//! le fait — la même fonction, `mailsmtp::compose::reply_threading` — l'assemble en octets RFC
//! 5322, **relit ces octets** avec `mail-parser`, et compare ce qui en sort à ce qui devait y
//! entrer.
//!
//! Ce qu'il ne remplace pas : la remise. Un serveur peut réécrire un en-tête, une passerelle
//! peut recoder un sujet. Le dernier mot du critère reste un envoi réel d'un compte à un autre —
//! mais il portera sur un message dont la conformité est déjà établie sur mille cas au lieu
//! d'un.
//!
//! ## Rien n'est envoyé, et rien n'est écrit
//!
//! Aucune connexion n'est ouverte : `Draft::assemble` produit des octets, et c'est tout ce qu'on
//! regarde. Le store est ouvert en lecture, et aucune ligne n'est écrite — pas même une ligne de
//! file d'envoi.

use anyhow::{Context, Result};
use camino::Utf8PathBuf;
use mail_parser::MessageParser;
use mailcore::Store;
use mailsmtp::compose::{Address, Draft};
use std::collections::BTreeMap;

/// Ce que le banc a constaté.
#[derive(Debug, Default)]
struct Verdict {
    /// Messages examinés — ceux qui portent un `Message-ID`.
    examined: usize,
    /// Réponses dont **tous** les contrôles passent.
    conform: usize,
    /// Messages sans `Message-ID`, comptés à part : il n'y a pas de fil à reprendre.
    without_id: usize,
    /// Blobs illisibles, comptés et non inspectés.
    unreadable: usize,
    /// Les manquements, par nature.
    failures: BTreeMap<&'static str, usize>,
    /// Quelques exemples, pour qu'un chiffre devienne vérifiable à la main.
    examples: Vec<String>,
    /// Réponses dont le sujet portait des caractères non ASCII : le cas que la RFC 2047 borne.
    non_ascii_subjects: usize,
    /// Réponses dont le parent avait déjà une chaîne `References`.
    with_chain: usize,
    /// La plus longue chaîne `References` produite.
    longest_chain: usize,
    /// Parents auxquels on ne peut pas répondre : `From:` cassé, vide, ou absent.
    ///
    /// Comptés à part des manquements : ce n'est pas la réponse qui est fautive.
    unanswerable: usize,
    /// La forme des adresses inexploitables, pour rendre leur nombre interprétable.
    shapes: BTreeMap<&'static str, usize>,
    /// Combien de **valeurs distinctes** prennent les adresses sans arobase.
    ///
    /// C'est le chiffre qui distingue une anomalie systématique d'un courrier réel biscornu :
    /// quinze mille valeurs distinctes sont quinze mille expéditeurs, une poignée répétée
    /// quinze mille fois est un défaut d'import. Les valeurs elles-mêmes ne sortent pas.
    distinct_broken: std::collections::HashSet<String>,
    /// Identifiants de la chaîne du parent que la RFC ne permet pas d'écrire.
    ///
    /// Un `msg-id` avec un blanc, non ASCII, ou vide : `canonical_msg_id` les refuse, et c'est
    /// **correct** — les écrire corromprait l'en-tête. Comptés pour que la chaîne attendue
    /// soit la bonne.
    dropped_ids: usize,
}

/// Vérifie la conformité d'une réponse sur tous les fils du corpus. N'écrit rien, n'envoie rien.
///
/// # Errors
///
/// Si le store est illisible.
pub fn measure(store_root: &Utf8PathBuf, limit: usize) -> Result<()> {
    let store = Store::open(store_root).with_context(|| format!("ouverture de {store_root}"))?;
    let rows = store.all_for_indexing()?;
    anyhow::ensure!(!rows.is_empty(), "store vide");

    // L'expéditeur de la réponse : une adresse qui n'existe pas, parce que rien ne part. Le
    // domaine sert au `Message-ID`, et c'est tout ce qu'il fait.
    let from = Address::parse("moi@exemple.invalid", Some("Moi")).context("adresse d'essai")?;

    let mut verdict = Verdict::default();
    let parser = MessageParser::default();

    for row in &rows {
        if limit > 0 && verdict.examined >= limit {
            break;
        }
        let Ok(raw) = store.blobs().read(row.blob) else {
            verdict.unreadable += 1;
            continue;
        };
        let Some(parent) = parser.parse(&raw) else {
            verdict.unreadable += 1;
            continue;
        };
        let Some(parent_id) = parent.message_id().map(str::to_owned) else {
            verdict.without_id += 1;
            continue;
        };
        // **`References` est un `TextList`, pas un texte.** Le premier jet lisait `as_text()`,
        // qui ne répond que pour `HeaderValue::Text` : la chaîne du parent sortait donc
        // toujours vide, et le banc annonçait « 0 chaîne à prolonger » sur deux mille messages
        // réels — un chiffre invraisemblable, qui a dénoncé le défaut. Les identifiants sortent
        // en plus **sans leurs chevrons**, que l'écriture remet.
        let parent_references: Vec<String> = parent
            .references()
            .as_text_list()
            .map(|list| list.iter().map(|it| bracketed(it)).collect())
            .unwrap_or_default();

        verdict.examined += 1;
        if !parent_references.is_empty() {
            verdict.with_chain += 1;
        }

        // La réponse, construite **par la même fonction que la fenêtre de rédaction**.
        let threading = mailsmtp::compose::reply_threading(Some(&parent_id), &parent_references);
        let subject = mailsmtp::compose::reply_subject(&row.subject);
        if !subject.is_ascii() {
            verdict.non_ascii_subjects += 1;
        }
        verdict.longest_chain = verdict.longest_chain.max(threading.references.len());

        let to = match Address::parse(&row.from_addr, None) {
            Ok(to) => to,
            Err(_) => {
                // **Compté à part, et pas comme un manquement.** Le corpus contient des `From:`
                // cassés, vides, ou des articles de flux RSS importés qui n'ont pas d'adresse
                // du tout. Ce n'est pas la réponse qui est fautive : c'est un message auquel on
                // ne peut pas répondre, et le mêler aux manquements ferait échouer un verdict
                // sur l'état du courrier reçu plutôt que sur le nôtre.
                verdict.unanswerable += 1;
                verdict.examined -= 1;
                // **La forme, pour que le chiffre soit interprétable.** 21 % du corpus sans
                // adresse exploitable est un nombre qui invite à une conclusion fausse — « notre
                // import perd les `From:` » — s'il n'est pas expliqué. La forme le dit.
                let shape = if row.from_addr.trim().is_empty() {
                    "adresse vide"
                } else if !row.from_addr.contains('@') {
                    "adresse sans arobase"
                } else if row.from_addr.contains(char::is_whitespace) {
                    "adresse avec un blanc"
                } else if !row.from_addr.is_ascii() {
                    "adresse non ASCII"
                } else {
                    "adresse d'apparence ordinaire"
                };
                *verdict.shapes.entry(shape).or_default() += 1;
                if !row.from_addr.contains('@') {
                    verdict
                        .distinct_broken
                        .insert(row.from_addr.trim().to_lowercase());
                }
                continue;
            }
        };

        let mut draft = Draft::new(from.clone(), vec![to], &subject, "Réponse d'essai.\r\n");
        draft.in_reply_to = threading.in_reply_to.clone();
        draft.references = threading.references.clone();

        let Ok(bytes) = draft.assemble(1_789_041_600) else {
            *verdict.failures.entry("assemblage refusé").or_default() += 1;
            continue;
        };

        // **Et on relit ce qu'on vient d'écrire.** C'est le seul contrôle qui vaut : un
        // assemblage qui se relit mal chez `mail-parser` se relira mal chez le destinataire.
        let Some(reply) = parser.parse(&bytes) else {
            *verdict.failures.entry("réponse illisible").or_default() += 1;
            continue;
        };

        let mut faults = Vec::new();
        if reply.message_id().is_none() {
            faults.push("Message-ID absent");
        }
        // Le `Message-ID` sans ses chevrons : `mail-parser` les retire d'un côté et pas
        // toujours de l'autre, et comparer des formes différentes ferait échouer un contrôle
        // qui n'a rien à dire.
        let bare = parent_id.trim_matches(['<', '>']);
        let points_at_parent = reply
            .in_reply_to()
            .as_text_list()
            .is_some_and(|list| list.iter().any(|it| it.contains(bare)));
        if !points_at_parent {
            faults.push("In-Reply-To ne pointe pas le parent");
        }
        let chain: Vec<String> = reply
            .references()
            .as_text_list()
            .map(|list| list.iter().map(|it| bracketed(it)).collect())
            .unwrap_or_default();
        // La chaîne **attendue** est celle des identifiants que la RFC permet d'écrire : un
        // identifiant avec un blanc est refusé à l'écriture, et c'est correct. Comparer à la
        // liste brute ferait échouer le banc sur un refus qui protège l'en-tête.
        let writable: Vec<String> = threading
            .references
            .iter()
            .filter_map(|it| mailsmtp::compose::canonical_msg_id(it))
            .collect();
        verdict.dropped_ids += threading.references.len() - writable.len();
        if chain.len() != writable.len() {
            faults.push("References : longueur changée par l'aller-retour");
        }
        if !chain.iter().any(|it| it.contains(bare)) {
            faults.push("References ne contient pas le parent");
            // **La forme du `Message-ID` fautif, jamais sa valeur.** Un identifiant est écrit
            // par un inconnu : le corpus en a de tordus, et savoir *lequel* des travers casse
            // l'aller-retour est la seule question utile. Un identifiant n'est pas du contenu
            // personnel, mais il désigne un message : le compter par forme suffit à enquêter.
            let shape = if bare.contains(char::is_whitespace) {
                "identifiant avec un blanc"
            } else if !bare.is_ascii() {
                "identifiant non ASCII"
            } else if bare.contains(['<', '>']) {
                "identifiant avec un chevron interne"
            } else if bare.len() > 78 {
                "identifiant de plus de 78 octets"
            } else if !bare.contains('@') {
                "identifiant sans arobase"
            } else if bare.is_empty() {
                "identifiant vide"
            } else {
                "identifiant d'apparence ordinaire"
            };
            *verdict.failures.entry(shape).or_default() += 1;
        }
        // Le sujet, décodé par le lecteur, doit être celui qu'on a voulu : c'est le contrôle
        // des en-têtes non ASCII, et le corpus a des sujets en arabe, en hébreu et en
        // devanagari.
        //
        // La comparaison se fait **blancs repliés**, et il faut le justifier : la RFC 5322
        // §2.2.3 autorise à couper un en-tête long sur un blanc, et RFC 2047 §8 dit qu'un
        // lecteur peut restituer un blanc de repliage comme une espace. Un sujet qui revient
        // avec une espace là où il y avait un saut de ligne n'est donc pas abîmé — il est
        // replié, ce que la spécification permet. Ce qui compterait comme abîmé est un
        // caractère perdu ou changé, et c'est ce que cette comparaison attrape encore.
        let wanted = collapsed(&subject);
        let seen = reply.subject().map(collapsed).unwrap_or_default();
        if seen != wanted {
            faults.push("sujet abîmé par l'encodage");
        }
        // **Ce qu'on ajoute, pas ce que le corpus portait déjà.** Le premier jet signalait tout
        // sujet commençant par « Re: Re: » — donc les 29 messages dont l'expéditeur avait
        // lui-même empilé les préfixes. Ce qui nous concerne est de n'en ajouter qu'un.
        if prefixes(&subject) > prefixes(&row.subject) + 1 {
            faults.push("« Re: » ajouté plus d'une fois");
        }

        if faults.is_empty() {
            verdict.conform += 1;
        } else {
            for fault in &faults {
                *verdict.failures.entry(fault).or_default() += 1;
            }
            if verdict.examples.len() < 12 {
                // L'identifiant du message et la nature du manquement — **jamais** le sujet ni
                // l'adresse, qui disent avec qui l'utilisateur correspond.
                verdict
                    .examples
                    .push(format!("#{} : {}", row.id.0, faults.join(", ")));
            }
        }
    }

    report(store_root, &verdict, rows.len());
    Ok(())
}

/// Écrit le bilan.
fn report(store_root: &Utf8PathBuf, verdict: &Verdict, total: usize) {
    println!("Store                        {store_root}");
    println!("Messages du corpus           {total}");
    if verdict.unreadable > 0 {
        println!(
            "Blobs illisibles             {} — non inspectés",
            verdict.unreadable
        );
    }
    println!(
        "Sans Message-ID              {} — aucun fil à reprendre",
        verdict.without_id
    );
    if verdict.unanswerable > 0 {
        println!(
            "Sans adresse exploitable     {} — `From:` cassé ou absent, pas une faute de la réponse",
            verdict.unanswerable
        );
    }
    println!("Réponses construites         {}", verdict.examined);
    if verdict.examined == 0 {
        println!();
        println!("Aucun fil exploitable : le critère 1 n'est pas mesurable sur ce store.");
        return;
    }

    #[allow(clippy::cast_precision_loss)]
    let share = verdict.conform as f64 * 100.0 / verdict.examined as f64;
    println!();
    println!("Critère 1 — conformité d'une réponse, sur les vrais fils");
    println!(
        "  Conformes                  {:>7}   {share:>6.2} %",
        verdict.conform
    );
    println!("  Avec une chaîne à prolonger {:>6}", verdict.with_chain);
    println!(
        "  Sujet non ASCII            {:>7}",
        verdict.non_ascii_subjects
    );
    println!(
        "  Plus longue chaîne         {:>7} identifiants",
        verdict.longest_chain
    );
    if verdict.dropped_ids > 0 {
        println!(
            "  Identifiants refusés       {:>7}   hors RFC — les écrire corromprait l'en-tête",
            verdict.dropped_ids
        );
    }

    if !verdict.shapes.is_empty() {
        println!();
        println!("Adresses inexploitables, par forme :");
        let mut sorted: Vec<(&&str, &usize)> = verdict.shapes.iter().collect();
        sorted.sort_by(|a, b| b.1.cmp(a.1));
        for (shape, count) in sorted {
            println!("  {shape:<44} {count:>6}");
        }
        if !verdict.distinct_broken.is_empty() {
            println!(
                "  dont valeurs distinctes                     {:>6}",
                verdict.distinct_broken.len()
            );
            // **La forme, jamais la valeur.** Une adresse désigne une personne. Ce qui rend le
            // chiffre exploitable est de savoir à quoi ces valeurs ressemblent : leur longueur,
            // et ce qu'elles contiennent comme séparateurs. Le propriétaire du corpus les
            // reconnaîtra à ça.
            let mut shapes: Vec<String> = verdict
                .distinct_broken
                .iter()
                .take(8)
                .map(|it| {
                    format!(
                        "{} octets, {} point(s), {} blanc(s), commence par {}",
                        it.len(),
                        it.matches('.').count(),
                        it.matches(char::is_whitespace).count(),
                        it.chars()
                            .next()
                            .map_or("rien", |c| if c.is_ascii_alphabetic() {
                                "une lettre"
                            } else if c.is_ascii_digit() {
                                "un chiffre"
                            } else {
                                "autre chose"
                            })
                    )
                })
                .collect();
            shapes.sort();
            for shape in shapes {
                println!("    · {shape}");
            }
        }
    }

    if verdict.failures.is_empty() {
        println!();
        println!("Aucun manquement.");
    } else {
        println!();
        println!("Manquements, par nature :");
        let mut sorted: Vec<(&&str, &usize)> = verdict.failures.iter().collect();
        sorted.sort_by(|a, b| b.1.cmp(a.1));
        for (fault, count) in sorted {
            println!("  {fault:<44} {count:>6}");
        }
        if !verdict.examples.is_empty() {
            println!();
            println!("Exemples — identifiant et nature, jamais le contenu :");
            for example in &verdict.examples {
                println!("  {example}");
            }
        }
    }

    let clean = verdict.failures.is_empty();
    println!();
    println!(
        "Verdict                      {}",
        if clean {
            "passé — chaque réponse se relit conforme"
        } else {
            "ÉCHOUÉ — des réponses ne se relisent pas conformes"
        }
    );
    println!("Envois                       0 — rien n'est sorti, rien n'a été écrit");
}

/// Remet les chevrons d'un `Message-ID` que `mail-parser` a retirés.
///
/// L'en-tête les porte — RFC 5322 §3.6.4 — et le lecteur les enlève. Comparer une forme à
/// l'autre ferait échouer un contrôle qui n'a rien à dire, donc les deux côtés sont ramenés à
/// la forme de l'en-tête.
fn bracketed(id: &str) -> String {
    let bare = id.trim().trim_matches(['<', '>']);
    format!("<{bare}>")
}

/// Un en-tête avec ses blancs repliés en une espace, et rogné.
///
/// C'est la forme dans laquelle deux sujets se comparent : voir la justification à l'appel.
fn collapsed(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Combien de préfixes de réponse un sujet empile en tête.
///
/// Sert à vérifier qu'on n'en ajoute **qu'un** : le corpus contient des sujets qui en portent
/// déjà trois, et les compter comme des manquements accuserait le lecteur de ce que
/// l'expéditeur a écrit.
fn prefixes(subject: &str) -> usize {
    let mut rest = subject.trim();
    let mut count = 0;
    loop {
        let lowered = rest.to_lowercase();
        let cut = if lowered.starts_with("re:") {
            3
        } else if lowered.starts_with("re :") {
            4
        } else {
            return count;
        };
        rest = rest[cut..].trim_start();
        count += 1;
    }
}
