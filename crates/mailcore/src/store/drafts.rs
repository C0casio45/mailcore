//! Les brouillons : ce qu'on a commencé à écrire et pas envoyé.
//!
//! ## Pourquoi ce n'est pas la file d'envoi
//!
//! Voir la migration `SCHEMA_V9`. En un mot : la file décide ce qui **part chez quelqu'un**, et
//! son énumération d'états porte la règle la plus sérieuse du dépôt — un message en
//! `committing` ne se remet jamais automatiquement. Un brouillon n'a pas de blob RFC 5322, ses
//! adresses ne sont pas validées, son sujet peut être vide, et il n'a aucune chance de partir
//! tant que personne n'a cliqué. Deux cycles de vie, deux tables.
//!
//! ## Ce module écrit ce que le formulaire avait à l'écran
//!
//! Les champs d'adresses sont des chaînes **telles que tapées**. Un brouillon rouvert doit
//! montrer exactement ce qu'on avait sous les yeux, y compris « jean@, mar » au milieu d'une
//! saisie : découper à l'enregistrement et recoller à la relecture perdrait le fragment.

use rusqlite::{OptionalExtension, params};

use crate::error::Result;
use crate::model::{AccountId, BlobHash};
use crate::store::Store;

/// L'identifiant interne d'un brouillon.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DraftId(pub i64);

/// Une pièce jointe d'un brouillon, désignée par son **contenu**.
///
/// Un `blob_hash`, jamais un chemin : c'est la règle du critère 3 de `docs/PHASE-3.md`. Un
/// chemin dans une demande de client donnerait la lecture de n'importe quel fichier à quiconque
/// détient le jeton.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DraftAttachment {
    /// Le contenu, déjà dans le magasin de blobs.
    pub blob: BlobHash,
    /// Le nom de fichier à annoncer au destinataire.
    pub filename: String,
    /// Le type MIME déclaré.
    pub mime: String,
    /// La taille en octets, avant encodage.
    pub size: u64,
}

/// Un brouillon, tel que le formulaire l'avait à l'écran.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Draft {
    /// L'identifiant, `None` pour un brouillon jamais enregistré.
    pub id: Option<DraftId>,
    /// Le compte qui enverra.
    pub account: AccountId,
    /// Le champ « À », tel que tapé.
    pub to: String,
    /// Le champ « Copie », tel que tapé.
    pub cc: String,
    /// Le champ « Copie cachée », tel que tapé.
    pub bcc: String,
    /// Le sujet.
    pub subject: String,
    /// Le corps en texte.
    pub body: String,
    /// Le `Message-ID` auquel ce brouillon répond, s'il répond.
    pub in_reply_to: Option<String>,
    /// La chaîne `References` du fil, du plus ancien au plus récent.
    pub references: Vec<String>,
    /// Vrai si la signature du compte doit être ajoutée à l'envoi.
    pub sign: bool,
    /// Les pièces jointes, dans l'ordre d'ajout.
    pub attachments: Vec<DraftAttachment>,
    /// Quand il a été touché pour la dernière fois, en secondes Unix.
    pub updated_at: i64,
}

impl Draft {
    /// Vrai si ce brouillon ne contient rien qui vaille d'être gardé.
    ///
    /// ## Ce qui compte comme « rien »
    ///
    /// Tous les champs de texte vides ou blancs, **et** aucune pièce jointe. Une fenêtre
    /// ouverte puis fermée sans rien taper ne doit pas laisser un brouillon vide dans la liste
    /// — c'est le geste le plus fréquent après « ouvrir la fenêtre par erreur ».
    ///
    /// Un brouillon qui n'a qu'une pièce jointe n'est **pas** vide : quelqu'un a déposé un
    /// fichier, et le jeter perdrait le geste.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.to.trim().is_empty()
            && self.cc.trim().is_empty()
            && self.bcc.trim().is_empty()
            && self.subject.trim().is_empty()
            && self.body.trim().is_empty()
            && self.attachments.is_empty()
    }

    /// De quoi se repérer dans une liste : le sujet, à défaut le destinataire, à défaut le
    /// début du corps.
    ///
    /// Un brouillon sans sujet est le cas ordinaire — c'est souvent la dernière chose qu'on
    /// écrit — donc une liste qui n'afficherait que le sujet montrerait des lignes vides.
    #[must_use]
    pub fn label(&self) -> String {
        for candidate in [&self.subject, &self.to, &self.body] {
            let trimmed = candidate.trim();
            if !trimmed.is_empty() {
                let short: String = trimmed.chars().take(80).collect();
                return short;
            }
        }
        "(brouillon vide)".to_owned()
    }
}

impl Store {
    /// Enregistre un brouillon, ou met à jour celui dont l'identifiant est donné.
    ///
    /// ## Une seule fonction pour créer et mettre à jour
    ///
    /// La coquille enregistre le même brouillon plusieurs fois — à la fermeture, avant un
    /// envoi, à chaque relève automatique le jour où il y en aura une. Deux fonctions
    /// obligeraient l'appelant à savoir laquelle appeler, et un appelant qui se trompe crée un
    /// deuxième brouillon à chaque enregistrement.
    ///
    /// Les pièces jointes sont réécrites en entier : elles sont peu nombreuses, et un
    /// rapprochement ligne à ligne coûterait plus à écrire — et à relire — que la réécriture.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Sqlite`] si une écriture échoue.
    pub fn save_draft(&self, draft: &Draft, now: i64) -> Result<DraftId> {
        let tx = self.connection().unchecked_transaction()?;
        let references = draft.references.join(" ");
        let id = match draft.id {
            Some(id) => {
                let touched = tx.execute(
                    "UPDATE drafts
                        SET account_id = ?2, to_field = ?3, cc_field = ?4, bcc_field = ?5,
                            subject = ?6, body = ?7, in_reply_to = ?8, refs = ?9, sign = ?10,
                            updated_at = ?11
                      WHERE id = ?1",
                    params![
                        id.0,
                        draft.account.0,
                        draft.to,
                        draft.cc,
                        draft.bcc,
                        draft.subject,
                        draft.body,
                        draft.in_reply_to,
                        references,
                        draft.sign,
                        now,
                    ],
                )?;
                if touched == 0 {
                    // Le brouillon a été supprimé entre-temps — par un autre client, ou parce
                    // que son compte a disparu. Le recréer serait ressusciter ce que quelqu'un
                    // a jeté ; ne rien faire perdrait la frappe en cours. On insère donc, avec
                    // un identifiant neuf : c'est ce que l'utilisateur a sous les yeux qui
                    // gagne.
                    insert_draft(&tx, draft, &references, now)?
                } else {
                    tx.execute(
                        "DELETE FROM draft_attachments WHERE draft_id = ?1",
                        params![id.0],
                    )?;
                    id
                }
            }
            None => insert_draft(&tx, draft, &references, now)?,
        };

        for (rank, attachment) in draft.attachments.iter().enumerate() {
            tx.execute(
                "INSERT INTO draft_attachments (draft_id, rank, blob_hash, filename, mime, size)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    id.0,
                    i64::try_from(rank).unwrap_or(i64::MAX),
                    attachment.blob.as_bytes(),
                    attachment.filename,
                    attachment.mime,
                    // SQLite ne stocke que des entiers signés : la taille d'une pièce jointe
                    // ne les dépasse pas — 25 Mo au plus — et un `MAX` vaudrait mieux qu'une
                    // panique si un jour elle le faisait.
                    i64::try_from(attachment.size).unwrap_or(i64::MAX),
                ],
            )?;
        }
        tx.commit()?;
        Ok(id)
    }

    /// Les brouillons, du plus récemment touché au plus ancien.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Sqlite`].
    pub fn drafts(&self) -> Result<Vec<Draft>> {
        let mut statement = self.connection().prepare_cached(
            "SELECT id, account_id, to_field, cc_field, bcc_field, subject, body,
                    in_reply_to, refs, sign, updated_at
               FROM drafts ORDER BY updated_at DESC, id DESC",
        )?;
        let rows: Vec<Draft> = statement
            .query_map([], |row| {
                Ok(Draft {
                    id: Some(DraftId(row.get(0)?)),
                    account: AccountId(row.get(1)?),
                    to: row.get(2)?,
                    cc: row.get(3)?,
                    bcc: row.get(4)?,
                    subject: row.get(5)?,
                    body: row.get(6)?,
                    in_reply_to: row.get(7)?,
                    references: split_references(&row.get::<_, String>(8)?),
                    sign: row.get(9)?,
                    attachments: Vec::new(),
                    updated_at: row.get(10)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        // Les pièces jointes en une seule requête plutôt qu'une par brouillon : la liste est
        // affichée en entier, et un aller-retour par ligne est le motif qui a coûté cher à
        // l'affichage des dossiers.
        let mut out = rows;
        let mut statement = self.connection().prepare_cached(
            "SELECT draft_id, blob_hash, filename, mime, size
               FROM draft_attachments ORDER BY draft_id, rank",
        )?;
        let pieces = statement
            .query_map([], |row| {
                let hash: Vec<u8> = row.get(1)?;
                Ok((
                    DraftId(row.get(0)?),
                    DraftAttachment {
                        blob: crate::store::read::blob_hash(&hash),
                        filename: row.get(2)?,
                        mime: row.get(3)?,
                        size: row.get::<_, i64>(4)?.unsigned_abs(),
                    },
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        for (draft, attachment) in pieces {
            if let Some(found) = out.iter_mut().find(|it| it.id == Some(draft)) {
                found.attachments.push(attachment);
            }
        }
        Ok(out)
    }

    /// Un brouillon par son identifiant.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Sqlite`].
    pub fn draft(&self, id: DraftId) -> Result<Option<Draft>> {
        // Par la liste : elle est courte — un humain n'a pas mille brouillons — et la
        // recherche évite une seconde requête de pièces jointes à écrire et à tester.
        Ok(self.drafts()?.into_iter().find(|it| it.id == Some(id)))
    }

    /// Jette un brouillon. Sans effet s'il n'existe pas.
    ///
    /// Les pièces jointes partent avec, par la cascade du schéma. **Les blobs restent** : ils
    /// peuvent être partagés avec un message déjà envoyé, et `mail doctor` compte les orphelins.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Sqlite`].
    pub fn delete_draft(&self, id: DraftId) -> Result<bool> {
        let removed = self
            .connection()
            .prepare_cached("DELETE FROM drafts WHERE id = ?1")?
            .execute(params![id.0])?;
        Ok(removed > 0)
    }
}

/// Insère une ligne de brouillon et rend son identifiant.
fn insert_draft(
    tx: &rusqlite::Transaction<'_>,
    draft: &Draft,
    references: &str,
    now: i64,
) -> Result<DraftId> {
    let id = tx
        .prepare_cached(
            "INSERT INTO drafts (account_id, to_field, cc_field, bcc_field, subject, body,
                                 in_reply_to, refs, sign, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             RETURNING id",
        )?
        .query_row(
            params![
                draft.account.0,
                draft.to,
                draft.cc,
                draft.bcc,
                draft.subject,
                draft.body,
                draft.in_reply_to,
                references,
                draft.sign,
                now,
            ],
            |row| row.get::<_, i64>(0),
        )
        .optional()?;
    Ok(DraftId(id.unwrap_or_default()))
}

/// Découpe une chaîne `References` en identifiants.
///
/// Sur les blancs, comme l'en-tête lui-même : RFC 5322 §3.6.4 sépare les `msg-id` par des
/// espaces, et un `Message-ID` n'en contient pas.
fn split_references(raw: &str) -> Vec<String> {
    raw.split_whitespace().map(ToOwned::to_owned).collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::{Draft, DraftAttachment, DraftId};
    use crate::model::{AccountId, BlobHash};
    use crate::store::Store;
    use camino::Utf8Path;

    fn store() -> (tempfile::TempDir, Store, AccountId) {
        let dir = tempfile::tempdir().unwrap();
        let root = Utf8Path::from_path(dir.path()).unwrap().to_owned();
        let store = Store::open(&root).unwrap();
        let writer = store.writer().unwrap();
        let account = writer.upsert_account("imap", "compte").unwrap();
        writer.commit().unwrap();
        (dir, store, account)
    }

    fn draft(account: AccountId) -> Draft {
        Draft {
            id: None,
            account,
            to: "jean@ailleurs.fr, mar".to_owned(),
            cc: String::new(),
            bcc: String::new(),
            subject: "Sujet à moitié".to_owned(),
            body: "Bonjour,\n\nJe voulais".to_owned(),
            in_reply_to: Some("<parent@exemple.fr>".to_owned()),
            references: vec!["<a@x.fr>".to_owned(), "<parent@exemple.fr>".to_owned()],
            sign: true,
            attachments: Vec::new(),
            updated_at: 0,
        }
    }

    #[test]
    fn a_draft_comes_back_exactly_as_it_was_typed() {
        // **Ce qui compte pour un brouillon** : rouvrir doit montrer ce qu'on avait à l'écran,
        // y compris « mar » au milieu d'une adresse en cours de frappe. Découper les adresses à
        // l'enregistrement perdrait le fragment.
        let (_dir, store, account) = store();
        let id = store.save_draft(&draft(account), 1_000).unwrap();

        let back = store.draft(id).unwrap().unwrap();
        assert_eq!(back.to, "jean@ailleurs.fr, mar");
        assert_eq!(back.subject, "Sujet à moitié");
        assert_eq!(back.body, "Bonjour,\n\nJe voulais");
        assert_eq!(back.in_reply_to.as_deref(), Some("<parent@exemple.fr>"));
        assert_eq!(back.references.len(), 2);
        assert!(back.sign);
        assert_eq!(back.updated_at, 1_000);
        assert_eq!(back.account, account);
    }

    #[test]
    fn saving_the_same_draft_twice_updates_it_instead_of_making_a_second() {
        // La coquille enregistre le même brouillon plusieurs fois — à la fermeture, avant un
        // envoi. Sans mise à jour, la liste se remplirait d'une copie par enregistrement.
        let (_dir, store, account) = store();
        let id = store.save_draft(&draft(account), 1_000).unwrap();

        let mut second = draft(account);
        second.id = Some(id);
        second.subject = "Sujet fini".to_owned();
        let again = store.save_draft(&second, 2_000).unwrap();

        assert_eq!(again, id, "un deuxième brouillon a été créé");
        assert_eq!(store.drafts().unwrap().len(), 1);
        assert_eq!(store.draft(id).unwrap().unwrap().subject, "Sujet fini");
        assert_eq!(store.draft(id).unwrap().unwrap().updated_at, 2_000);
    }

    #[test]
    fn attachments_are_kept_in_order_and_replaced_wholesale() {
        let (_dir, store, account) = store();
        let mut it = draft(account);
        it.attachments = vec![
            DraftAttachment {
                blob: BlobHash::of(b"un"),
                filename: "un.pdf".to_owned(),
                mime: "application/pdf".to_owned(),
                size: 2,
            },
            DraftAttachment {
                blob: BlobHash::of(b"deux"),
                filename: "deux.png".to_owned(),
                mime: "image/png".to_owned(),
                size: 4,
            },
        ];
        let id = store.save_draft(&it, 1_000).unwrap();
        let back = store.draft(id).unwrap().unwrap();
        assert_eq!(back.attachments.len(), 2);
        assert_eq!(back.attachments[0].filename, "un.pdf");
        assert_eq!(back.attachments[1].filename, "deux.png");
        assert_eq!(back.attachments[1].blob, BlobHash::of(b"deux"));

        // Retirer une pièce et réenregistrer : la table fille est réécrite, pas complétée.
        let mut fewer = back;
        fewer.attachments.truncate(1);
        store.save_draft(&fewer, 2_000).unwrap();
        assert_eq!(store.draft(id).unwrap().unwrap().attachments.len(), 1);
    }

    #[test]
    fn the_list_shows_the_most_recently_touched_first() {
        // C'est celui qu'on rouvre.
        let (_dir, store, account) = store();
        let first = store.save_draft(&draft(account), 1_000).unwrap();
        let mut other = draft(account);
        other.subject = "Le second".to_owned();
        let second = store.save_draft(&other, 2_000).unwrap();

        let listed = store.drafts().unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].id, Some(second));
        assert_eq!(listed[1].id, Some(first));
    }

    #[test]
    fn a_deleted_draft_takes_its_attachments_and_leaves_the_blobs() {
        // Les blobs peuvent être partagés avec un message déjà envoyé : les supprimer ici
        // effacerait une pièce jointe d'un message parti.
        let (_dir, store, account) = store();
        let mut it = draft(account);
        it.attachments = vec![DraftAttachment {
            blob: BlobHash::of(b"un"),
            filename: "un.pdf".to_owned(),
            mime: "application/pdf".to_owned(),
            size: 2,
        }];
        let id = store.save_draft(&it, 1_000).unwrap();

        assert!(store.delete_draft(id).unwrap());
        assert!(store.drafts().unwrap().is_empty());
        let orphans: i64 = store
            .connection()
            .query_row("SELECT COUNT(*) FROM draft_attachments", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(orphans, 0, "la cascade n'a pas joué");

        // Idempotent : supprimer deux fois n'est pas une erreur.
        assert!(!store.delete_draft(id).unwrap());
        assert!(!store.delete_draft(DraftId(999)).unwrap());
    }

    #[test]
    fn an_empty_draft_is_recognised_as_empty_but_one_with_only_an_attachment_is_not() {
        // Ouvrir la fenêtre par erreur puis la fermer ne doit pas laisser un brouillon vide.
        // Mais quelqu'un qui a déposé un fichier a fait un geste, et le jeter le perdrait.
        let (_dir, _store, account) = store();
        let empty = Draft {
            id: None,
            account,
            to: "   ".to_owned(),
            cc: String::new(),
            bcc: "\t".to_owned(),
            subject: String::new(),
            body: "\n\n".to_owned(),
            in_reply_to: None,
            references: Vec::new(),
            sign: true,
            attachments: Vec::new(),
            updated_at: 0,
        };
        assert!(empty.is_empty());

        let with_file = Draft {
            attachments: vec![DraftAttachment {
                blob: BlobHash::of(b"un"),
                filename: "un.pdf".to_owned(),
                mime: "application/pdf".to_owned(),
                size: 2,
            }],
            ..empty
        };
        assert!(!with_file.is_empty());
    }

    #[test]
    fn a_draft_finds_a_label_even_without_a_subject() {
        // Le sujet est souvent la dernière chose qu'on écrit : une liste qui ne montrerait que
        // lui afficherait des lignes vides.
        let (_dir, _store, account) = store();
        let mut it = draft(account);
        assert_eq!(it.label(), "Sujet à moitié");
        it.subject = "  ".to_owned();
        assert_eq!(it.label(), "jean@ailleurs.fr, mar");
        it.to = String::new();
        assert!(it.label().starts_with("Bonjour,"));
        it.body = String::new();
        assert_eq!(it.label(), "(brouillon vide)");
    }

    #[test]
    fn deleting_the_account_takes_its_drafts_with_it() {
        // Sans la cascade, un brouillon resterait attaché à un compte disparu — donc
        // impossible à envoyer et impossible à comprendre.
        let (_dir, store, account) = store();
        store.save_draft(&draft(account), 1_000).unwrap();
        store
            .connection()
            .execute("DELETE FROM accounts WHERE id = ?1", [account.0])
            .unwrap();
        assert!(store.drafts().unwrap().is_empty());
    }

    #[test]
    fn saving_a_draft_that_someone_else_deleted_keeps_what_is_on_screen() {
        // Un autre client a jeté le brouillon pendant qu'on écrivait. Ne rien faire perdrait la
        // frappe ; ressusciter la ligne effacée irait contre le geste de l'autre. On écrit une
        // ligne neuve : ce qui est à l'écran gagne.
        let (_dir, store, account) = store();
        let id = store.save_draft(&draft(account), 1_000).unwrap();
        assert!(store.delete_draft(id).unwrap());

        let mut mine = draft(account);
        mine.id = Some(id);
        mine.subject = "Écrit pendant ce temps".to_owned();
        let again = store.save_draft(&mine, 2_000).unwrap();

        // Une **ligne neuve**, avec ce qui était à l'écran. Son identifiant peut être celui qui
        // vient d'être libéré — SQLite réattribue le plus petit `rowid` disponible sur une table
        // vidée, et c'est exactement ce qui se passe ici. Un appelant ne doit donc pas croire
        // qu'un identifiant de brouillon ne revient jamais : ce qui compte est qu'il y ait une
        // ligne, et qu'elle porte le texte en cours.
        let listed = store.drafts().unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, Some(again));
        assert_eq!(listed[0].subject, "Écrit pendant ce temps");
        assert_eq!(listed[0].updated_at, 2_000);
    }
}
