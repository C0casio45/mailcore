//! Le schéma tantivy. Une seule définition, partagée par l'écrivain et le chercheur.
//!
//! Un désaccord entre les deux se solde par un index qui répond faux sans jamais échouer —
//! le pire mode de défaillance qui soit. D'où la définition unique et les accesseurs typés
//! plutôt que des `schema.get_field("subject")?` dispersés.
//!
//! ## Ce qui est stocké, et ce qui ne l'est pas
//!
//! Seul `id` est `STORED`. Tout le reste n'existe que sous forme de postings :
//! `docs/ARCHITECTURE.md` le dit, « on stocke les postings, pas le contenu ». Le corps se
//! relit depuis le blob, qui est la source de vérité. Dupliquer le texte dans l'index
//! doublerait l'espace disque et créerait une seconde copie à garder cohérente.

use tantivy::schema::{FAST, INDEXED, STORED, Schema, TEXT};

/// Les champs de l'index, résolus une fois.
#[derive(Debug, Clone, Copy)]
pub struct Fields {
    /// L'identifiant interne du message, pour retrouver ses métadonnées dans SQLite.
    pub id: tantivy::schema::Field,
    /// Le sujet, décodé.
    pub subject: tantivy::schema::Field,
    /// Le corps aplati en texte.
    pub body: tantivy::schema::Field,
    /// L'expéditeur : adresse et nom affiché.
    pub from: tantivy::schema::Field,
    /// Les destinataires : `To` et `Cc`.
    pub to: tantivy::schema::Field,
    /// Les chemins des dossiers qui référencent ce message.
    pub folder: tantivy::schema::Field,
    /// La date, en secondes Unix. `FAST` pour pouvoir trier et filtrer sans lire le document.
    pub date: tantivy::schema::Field,
}

/// Construit le schéma et ses accesseurs.
#[must_use]
pub fn build() -> (Schema, Fields) {
    let mut builder = Schema::builder();

    // `id` est le seul champ stocké : c'est la clé de retour vers SQLite. `FAST` en plus de
    // `STORED` pour que la collecte n'ait pas à désérialiser un document par résultat.
    let id = builder.add_i64_field("id", INDEXED | STORED | FAST);
    let subject = builder.add_text_field("subject", TEXT);
    let body = builder.add_text_field("body", TEXT);
    let from = builder.add_text_field("from", TEXT);
    let to = builder.add_text_field("to", TEXT);
    let folder = builder.add_text_field("folder", TEXT);
    let date = builder.add_i64_field("date", INDEXED | FAST);

    let schema = builder.build();
    (
        schema,
        Fields {
            id,
            subject,
            body,
            from,
            to,
            folder,
            date,
        },
    )
}

/// Retrouve les accesseurs dans un schéma déjà ouvert sur le disque.
///
/// # Errors
///
/// [`crate::Error::Tantivy`] si l'index sur le disque a été écrit par une version dont le
/// schéma diffère. Refuser plutôt que deviner : un champ manquant rendrait des résultats
/// silencieusement incomplets.
pub fn resolve(schema: &Schema) -> crate::Result<Fields> {
    Ok(Fields {
        id: schema.get_field("id")?,
        subject: schema.get_field("subject")?,
        body: schema.get_field("body")?,
        from: schema.get_field("from")?,
        to: schema.get_field("to")?,
        folder: schema.get_field("folder")?,
        date: schema.get_field("date")?,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn the_built_schema_resolves_to_the_same_fields() {
        // Le test qui empêche l'écrivain et le chercheur de diverger.
        let (schema, built) = build();
        let resolved = resolve(&schema).expect("le schéma qu'on vient de construire");

        assert_eq!(built.id, resolved.id);
        assert_eq!(built.subject, resolved.subject);
        assert_eq!(built.body, resolved.body);
        assert_eq!(built.from, resolved.from);
        assert_eq!(built.to, resolved.to);
        assert_eq!(built.folder, resolved.folder);
        assert_eq!(built.date, resolved.date);
    }

    #[test]
    fn only_the_identifier_is_stored() {
        // Si un champ texte devenait `STORED`, l'index doublerait de taille sans raison.
        let (schema, _) = build();
        for (field, entry) in schema.fields() {
            let stored = entry.is_stored();
            let is_id = entry.name() == "id";
            assert_eq!(
                stored,
                is_id,
                "champ {} ({field:?}) : stocké = {stored}",
                entry.name()
            );
        }
    }

    #[test]
    fn a_schema_missing_a_field_is_refused() {
        let mut builder = Schema::builder();
        builder.add_i64_field("id", INDEXED | STORED | FAST);
        assert!(resolve(&builder.build()).is_err());
    }
}
