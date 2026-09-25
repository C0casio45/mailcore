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

/// Un transformeur minuscule, écrit dans `dir` : `config.json`, `tokenizer.json`,
/// `model.safetensors`, au format exact de `multilingual-e5-small`.
///
/// Minuscule — dimension 8, une couche, deux têtes — pour qu'un test le charge en quelques
/// millisecondes, et **vrai** : c'est le `BertModel` de `candle` qui le lit, par le même
/// `Bert::load` que le modèle livré. C'est ce qui a manqué à `bert.rs` jusqu'au 2026-09-25 :
/// aucun test, parce que le seul modèle était celui de 470 Mo qui n'est pas dans le dépôt.
///
/// `pieces` est le vocabulaire, jetons spéciaux compris ; les poids sont déterministes, et chaque
/// ligne de la table est distincte, pour qu'une ligne mal recopiée se voie.
pub fn tiny_bert(dir: &camino::Utf8Path, pieces: &[(&str, f64)]) {
    tiny_bert_with(dir, pieces, &serde_json::Value::Null);
}

/// [`tiny_bert`] avec un normaliseur, écrit comme dans un `tokenizer.json` — `{"type": "NFKC"}`.
pub fn tiny_bert_with(
    dir: &camino::Utf8Path,
    pieces: &[(&str, f64)],
    normalizer: &serde_json::Value,
) {
    use candle_core::{Device, Tensor};
    use std::collections::HashMap;

    const HIDDEN: usize = 8;
    const INTERMEDIATE: usize = 16;
    const POSITIONS: usize = 64;

    std::fs::create_dir_all(dir.as_std_path()).unwrap();
    let vocab = pieces.len();
    let config = serde_json::json!({
        "architectures": ["BertModel"],
        "attention_probs_dropout_prob": 0.1,
        "classifier_dropout": null,
        "hidden_act": "gelu",
        "hidden_dropout_prob": 0.1,
        "hidden_size": HIDDEN,
        "initializer_range": 0.02,
        "intermediate_size": INTERMEDIATE,
        "layer_norm_eps": 1e-12,
        "max_position_embeddings": POSITIONS,
        "model_type": "bert",
        "num_attention_heads": 2,
        "num_hidden_layers": 1,
        "pad_token_id": 0,
        "position_embedding_type": "absolute",
        "type_vocab_size": 2,
        "vocab_size": vocab,
    });
    std::fs::write(
        dir.join("config.json").as_std_path(),
        serde_json::to_string_pretty(&config).unwrap(),
    )
    .unwrap();

    let special = |id: usize, content: &str| {
        serde_json::json!({
            "id": id, "content": content, "single_word": false, "lstrip": false,
            "rstrip": false, "normalized": false, "special": true,
        })
    };
    let mask = pieces
        .iter()
        .position(|(piece, _)| *piece == "<mask>")
        .unwrap();
    let tokenizer = serde_json::json!({
        "version": "1.0",
        "truncation": null,
        "padding": null,
        "added_tokens": [
            special(0, "<s>"), special(1, "<pad>"), special(2, "</s>"), special(3, "<unk>"),
            special(mask, "<mask>"),
        ],
        "normalizer": normalizer,
        "pre_tokenizer": {"type": "Metaspace", "replacement": "▁", "add_prefix_space": true},
        "post_processor": {
            "type": "TemplateProcessing",
            "single": [
                {"SpecialToken": {"id": "<s>", "type_id": 0}},
                {"Sequence": {"id": "A", "type_id": 0}},
                {"SpecialToken": {"id": "</s>", "type_id": 0}},
            ],
            "pair": [
                {"SpecialToken": {"id": "<s>", "type_id": 0}},
                {"Sequence": {"id": "A", "type_id": 0}},
                {"SpecialToken": {"id": "</s>", "type_id": 0}},
                {"SpecialToken": {"id": "</s>", "type_id": 0}},
                {"Sequence": {"id": "B", "type_id": 0}},
                {"SpecialToken": {"id": "</s>", "type_id": 0}},
            ],
            "special_tokens": {
                "</s>": {"id": "</s>", "ids": [2], "tokens": ["</s>"]},
                "<s>": {"id": "<s>", "ids": [0], "tokens": ["<s>"]},
            },
        },
        "decoder": {"type": "Metaspace", "replacement": "▁", "add_prefix_space": true},
        "model": {
            "type": "Unigram",
            "unk_id": 3,
            "vocab": pieces.iter().map(|(piece, score)| serde_json::json!([piece, score])).collect::<Vec<_>>(),
        },
    });
    std::fs::write(
        dir.join("tokenizer.json").as_std_path(),
        serde_json::to_string(&tokenizer).unwrap(),
    )
    .unwrap();

    // Des poids déterministes et tous distincts : une fonction du nom et de la position.
    let device = Device::Cpu;
    let mut seed = 0.0f32;
    let mut tensor = |shape: &[usize]| {
        seed += 1.0;
        let count: usize = shape.iter().product();
        let base = seed;
        let values: Vec<f32> = (0..count)
            .map(|k| ((k as f32).mul_add(0.37, base * 1.3)).sin() * 0.1)
            .collect();
        Tensor::from_vec(values, shape, &device).unwrap()
    };
    let mut tensors: HashMap<String, Tensor> = HashMap::new();
    tensors.insert(
        "embeddings.word_embeddings.weight".to_owned(),
        tensor(&[vocab, HIDDEN]),
    );
    tensors.insert(
        "embeddings.position_embeddings.weight".to_owned(),
        tensor(&[POSITIONS, HIDDEN]),
    );
    tensors.insert(
        "embeddings.token_type_embeddings.weight".to_owned(),
        tensor(&[2, HIDDEN]),
    );
    for norm in [
        "embeddings.LayerNorm",
        "encoder.layer.0.attention.output.LayerNorm",
        "encoder.layer.0.output.LayerNorm",
    ] {
        tensors.insert(
            format!("{norm}.weight"),
            Tensor::ones(HIDDEN, candle_core::DType::F32, &device).unwrap(),
        );
        tensors.insert(format!("{norm}.bias"), tensor(&[HIDDEN]));
    }
    for (name, out, input) in [
        ("encoder.layer.0.attention.self.query", HIDDEN, HIDDEN),
        ("encoder.layer.0.attention.self.key", HIDDEN, HIDDEN),
        ("encoder.layer.0.attention.self.value", HIDDEN, HIDDEN),
        ("encoder.layer.0.attention.output.dense", HIDDEN, HIDDEN),
        ("encoder.layer.0.intermediate.dense", INTERMEDIATE, HIDDEN),
        ("encoder.layer.0.output.dense", HIDDEN, INTERMEDIATE),
    ] {
        tensors.insert(format!("{name}.weight"), tensor(&[out, input]));
        tensors.insert(format!("{name}.bias"), tensor(&[out]));
    }
    candle_core::safetensors::save(&tensors, dir.join("model.safetensors").as_std_path()).unwrap();
}

/// Le vocabulaire du transformeur minuscule : du français, un émoji, et du russe.
///
/// Le russe est là pour la taille — c'est ce qu'elle doit retirer —, et chaque caractère a sa
/// pièce d'une lettre, pour qu'aucun texte de test ne tombe sur `<unk>` par accident.
pub const TINY_PIECES: &[(&str, f64)] = &[
    ("<s>", 0.0),
    ("<pad>", 0.0),
    ("</s>", 0.0),
    ("<unk>", 0.0),
    ("▁", -2.0),
    ("▁bon", -3.0),
    ("jour", -3.0),
    ("▁Мос", -3.0),
    ("▁été", -3.0),
    ("ква", -3.0),
    ("▁query", -3.0),
    ("▁passage", -3.0),
    (":", -4.0),
    ("😀", -5.0),
    ("b", -6.0),
    ("o", -6.0),
    ("n", -6.0),
    ("j", -6.0),
    ("u", -6.0),
    ("r", -6.0),
    ("é", -6.0),
    ("t", -6.0),
    ("q", -6.0),
    ("e", -6.0),
    ("y", -6.0),
    ("p", -6.0),
    ("a", -6.0),
    ("s", -6.0),
    ("g", -6.0),
    ("М", -7.0),
    ("о", -7.0),
    ("с", -7.0),
    ("к", -7.0),
    ("в", -7.0),
    ("а", -7.0),
    ("<mask>", 0.0),
];
