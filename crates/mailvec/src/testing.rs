//! Le corpus jetable que les tests de ce crate partagent.
//!
//! Partagé plutôt que recopié dans chaque module : deux fabriques de corpus divergeraient, et un
//! test qui passe sur un corpus légèrement différent de celui du voisin ne dit plus la même
//! chose. C'est la règle appliquée au carnet et à l'indexation — deux chemins vers la même donnée
//! partagent leur code.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use mailcore::Store;

/// Un store avec un compte, un dossier, et `count` messages en texte brut.
///
/// Les messages sont délibérément **banals et distincts** : un sujet et un corps qui portent leur
/// numéro. Un test de recherche a besoin que les messages se distinguent ; un test de passe a
/// besoin qu'ils existent. Les deux s'accommodent de ça.
pub fn corpus(count: usize) -> (tempfile::TempDir, Store) {
    corpus_with(
        &(0..count)
            .map(|index| {
                (
                    format!("sujet {index}"),
                    format!("Le corps du message numero {index}."),
                )
            })
            .collect::<Vec<_>>(),
    )
}

/// Un store portant exactement ces messages, sujet et corps.
pub fn corpus_with(messages: &[(String, String)]) -> (tempfile::TempDir, Store) {
    use mailcore::model::{AuthKind, FolderKind, MessageFlags, Security, Server};
    use mailcore::store::write::NewMessage;

    let dir = tempfile::tempdir().unwrap();
    let root = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    let store = Store::open(&root).unwrap();

    let account = {
        let writer = store.writer().unwrap();
        let id = writer
            .upsert_imap_account(
                "moi",
                &Server {
                    host: "imap.exemple.invalid".to_owned(),
                    port: 993,
                    username: "moi@exemple.invalid".to_owned(),
                    auth: AuthKind::Password,
                    security: Security::Tls,
                },
            )
            .unwrap();
        writer.commit().unwrap();
        id
    };
    let folder = {
        let writer = store.writer().unwrap();
        let id = writer
            .upsert_folder(account, "INBOX", FolderKind::Inbox)
            .unwrap();
        writer.commit().unwrap();
        id
    };

    for (index, (subject, body)) in messages.iter().enumerate() {
        let raw = format!(
            "From: Expediteur {index} <e{index}@exemple.invalid>\r\n\
             Subject: {subject}\r\n\
             Content-Type: text/plain; charset=utf-8\r\n\
             \r\n\
             {body}\r\n"
        );
        let blob = store.blobs().put(raw.as_bytes()).unwrap().hash;
        let writer = store.writer().unwrap();
        let (id, _) = writer
            .insert_message(&NewMessage {
                blob,
                rfc822_id: None,
                date: 1_700_000_000 + index as i64,
                from_addr: "e@exemple.invalid",
                from_name: Some("Expediteur"),
                subject,
                size: raw.len() as u64,
                has_attachments: false,
            })
            .unwrap();
        writer
            .insert_ref(id, folder, 1_700_000_000, MessageFlags::empty())
            .unwrap();
        writer.commit().unwrap();
    }
    (dir, store)
}
