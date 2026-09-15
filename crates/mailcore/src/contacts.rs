//! Le carnet d'adresses, dérivé du corpus. **Aucun protocole, aucune requête réseau.**
//!
//! ## Le problème n'est pas de trouver, c'est de classer
//!
//! Sur le corpus réel — 48 551 messages, 105 502 références — taper « c » correspond à des
//! milliers d'adresses. Un carnet qui les rend toutes par ordre alphabétique est inutilisable :
//! la personne cherchée est page huit.
//!
//! Le travail est donc dans [`score`], et la question qu'il répond est « à qui l'utilisateur
//! est-il en train d'écrire ? ». Deux signaux, et ils ne valent pas la même chose :
//!
//! **Avoir écrit à quelqu'un est une intention.** Si l'utilisateur a envoyé douze messages à
//! une adresse, c'est celle-là qu'il veut.
//!
//! **Avoir reçu de quelqu'un est une statistique.** Une lettre d'information monte à deux mille
//! réceptions sans qu'on lui ait jamais répondu. Compter les deux ensemble ferait proposer le
//! service client d'un marchand avant le collègue de tous les jours — c'est le défaut de
//! beaucoup de clients, et il est mesurable.
//!
//! D'où un plafond sur les réceptions : au-delà, un correspondant de plus n'ajoute rien. Le
//! plafond est [`FROM_CAP`], et c'est le paramètre le plus important du module.
//!
//! ## Ce que le carnet n'est pas
//!
//! Ni fiches, ni photos, ni numéros. `docs/PHASE-3.md` met CardDAV dehors, et une table de
//! fiches sans protocole pour les remplir serait une table vide avec un formulaire. Et rien
//! n'en sort : aucune requête ne part pour enrichir une adresse — règle 5 du `CLAUDE.md`
//! appliquée aux contacts.

use rusqlite::params;

use crate::error::Result;
use crate::store::Store;

/// Le plafond des réceptions dans le classement.
///
/// ## Le paramètre qui décide si l'autocomplétion sert à quelque chose
///
/// Trois. Autrement dit : à partir du quatrième message reçu d'une adresse, en recevoir plus ne
/// la fait pas monter. C'est bas, et c'est délibéré — la distribution des réceptions sur le
/// corpus réel est très étalée, et sans plafond serré les quelques adresses à quatre chiffres
/// écrasent tout le reste quel que soit le poids qu'on leur donne.
///
/// **Il doit rester strictement sous [`TO_WEIGHT`]**, sinon la propriété du module tombe : avoir
/// écrit une fois ne passerait plus devant avoir reçu mille fois. Au premier jet, le plafond
/// valait cinq pour un poids de quatre, et le test
/// `writing_once_beats_receiving_a_thousand_times` a échoué en le disant — la documentation
/// affirmait la propriété que les constantes contredisaient.
///
/// Ce que le plafond préserve, en revanche : une adresse qui a écrit **une** fois reste
/// distinguable d'une adresse jamais vue, ce qui est le cas d'usage « répondre à un inconnu qui
/// vient de m'écrire ».
const FROM_CAP: i64 = 3;

/// Le poids d'un message envoyé, relativement à un message reçu.
///
/// Quatre, donc strictement au-dessus de [`FROM_CAP`]. Une adresse à qui on a écrit **une seule**
/// fois passe donc devant n'importe quelle adresse qu'on n'a fait que recevoir, aussi souvent
/// qu'elle ait écrit. C'est la propriété qu'on veut, et le test
/// `writing_once_beats_receiving_a_thousand_times` la fige.
const TO_WEIGHT: i64 = 4;

/// Une adresse connue du corpus.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Contact {
    /// L'adresse, en minuscules.
    pub address: String,
    /// Le nom affiché le plus récemment vu, avec sa casse. `None` si aucun message n'en portait.
    pub name: Option<String>,
    /// Combien de fois l'utilisateur a écrit à cette adresse.
    pub seen_to: u32,
    /// Combien de fois elle lui a écrit.
    pub seen_from: u32,
    /// Secondes Unix du message le plus récent qui la mentionne.
    pub last_seen: i64,
}

impl Contact {
    /// Ce qu'il faut afficher dans une liste de propositions.
    ///
    /// `Marie Dupont <marie@exemple.fr>` quand on a un nom, l'adresse seule sinon. C'est aussi
    /// la forme qu'un champ de destinataire accepte : la proposition est **insérable telle
    /// quelle**, et `mailsmtp::compose::Address` la relit.
    #[must_use]
    pub fn label(&self) -> String {
        match &self.name {
            Some(name) if !name.is_empty() && name != &self.address => {
                format!("{name} <{}>", self.address)
            }
            _ => self.address.clone(),
        }
    }

    /// Le rang de cette adresse. Plus grand est meilleur.
    #[must_use]
    pub fn score(&self) -> i64 {
        score(i64::from(self.seen_to), i64::from(self.seen_from))
    }
}

/// Le classement, isolé pour être testable sans store.
///
/// Voir la documentation du module : les envois pèsent [`TO_WEIGHT`], les réceptions sont
/// plafonnées à [`FROM_CAP`].
#[must_use]
pub const fn score(seen_to: i64, seen_from: i64) -> i64 {
    let sent = seen_to.saturating_mul(TO_WEIGHT);
    let received = if seen_from > FROM_CAP {
        FROM_CAP
    } else {
        seen_from
    };
    sent.saturating_add(received)
}

/// Une ligne de message, réduite à ce que le carnet lit.
///
/// Distincte de [`crate::ListItem`] et de `IndexRow` : le carnet n'a besoin ni du sujet, ni des
/// pièces jointes, ni des dossiers. `all_for_indexing` joint `refs` et `folders` puis groupe —
/// du travail inutile ici, et qui serait payé à chaque moisson depuis que la passe est
/// incrémentale.
#[derive(Debug, Clone)]
pub struct ContactRow {
    /// L'identifiant, pour marquer la ligne comptée ensuite.
    pub id: crate::MessageId,
    /// L'identité du contenu, pour aller lire les en-têtes.
    pub blob: crate::BlobHash,
    /// L'adresse de l'expéditeur, telle que le store la connaît.
    pub from_addr: String,
    /// Le nom affiché de l'expéditeur.
    pub from_name: Option<String>,
    /// La date, en secondes Unix.
    pub date: i64,
}

/// Une adresse et son nom, tels qu'un en-tête les portait.
#[derive(Debug, Clone)]
pub struct Seen {
    /// L'adresse, en minuscules.
    pub address: String,
    /// Le nom affiché, s'il y en avait un.
    pub name: Option<String>,
    /// Vrai si c'est l'utilisateur qui écrivait — `To`, `Cc`.
    pub outgoing: bool,
    /// La date du message.
    pub date: i64,
}

impl Store {
    /// Ajoute — ou met à jour — ce qu'un message apprend sur des adresses.
    ///
    /// ## L'`UPSERT` ne recule pas
    ///
    /// Le nom n'est écrasé que par un message **plus récent** : sans ça, l'ordre dans lequel la
    /// reconstruction parcourt le corpus déciderait du nom affiché, et deux reconstructions
    /// donneraient deux carnets. Un nom absent n'écrase jamais un nom présent, pour la même
    /// raison — beaucoup de messages ne portent que l'adresse.
    ///
    /// Les compteurs, eux, s'ajoutent : ils comptent des occurrences, pas un état.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Sqlite`] si l'écriture échoue.
    pub fn record_seen(&self, seen: &[Seen]) -> Result<()> {
        let tx = self.connection().unchecked_transaction()?;
        {
            let mut statement = tx.prepare_cached(
                "INSERT INTO contacts (address, name, name_fold, seen_to, seen_from, last_seen)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(address) DO UPDATE SET
                     -- Le nom ne recule pas : plus récent, et non vide.
                     name      = CASE
                                     WHEN excluded.name IS NOT NULL
                                      AND excluded.last_seen >= contacts.last_seen
                                     THEN excluded.name
                                     ELSE contacts.name
                                 END,
                     -- **La même condition, mot pour mot.** Deux conditions différentes
                     -- feraient un nom affiché qui ne correspond pas au nom cherché.
                     name_fold = CASE
                                     WHEN excluded.name IS NOT NULL
                                      AND excluded.last_seen >= contacts.last_seen
                                     THEN excluded.name_fold
                                     ELSE contacts.name_fold
                                 END,
                     seen_to   = contacts.seen_to   + excluded.seen_to,
                     seen_from = contacts.seen_from + excluded.seen_from,
                     last_seen = MAX(contacts.last_seen, excluded.last_seen)",
            )?;
            for it in seen {
                let (to, from) = if it.outgoing { (1, 0) } else { (0, 1) };
                let name = it.name.as_deref().filter(|name| !name.trim().is_empty());
                statement.execute(params![
                    it.address,
                    name,
                    // `to_lowercase` de Rust et non `lower()` de SQLite : voir la migration.
                    name.map(str::to_lowercase),
                    to,
                    from,
                    it.date,
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Vide le carnet **et remet tous les messages à « pas encore compté »**.
    ///
    /// Le premier pas d'une reconstruction, et les deux gestes ne se séparent pas : vider le
    /// carnet sans remettre les drapeaux laisserait un carnet vide que plus rien ne remplirait,
    /// et remettre les drapeaux sans vider ferait tout compter deux fois. Les faire ensemble,
    /// dans une transaction, est ce qui rend `rebuild` idempotent.
    ///
    /// ## Il a existé une raison de ne pas avoir de drapeau, et elle ne tenait pas
    ///
    /// Ce commentaire disait : *« une mise à jour incrémentale demanderait de savoir quels
    /// messages ont déjà été vus — une colonne de plus, et une source de dérive silencieuse »*.
    /// La colonne est arrivée (`SCHEMA_V11`), parce que l'alternative était pire : sans elle, le
    /// carnet ne suivait **aucune** moisson, et une personne à qui on venait d'écrire
    /// n'apparaissait qu'à la prochaine reconstruction lancée à la main. Une dérive certaine
    /// vaut moins qu'une dérive possible.
    ///
    /// La dérive possible, elle, existe et se nomme : un message dont toutes les références
    /// disparaissent garde sa contribution au carnet. Elle ne va que dans un sens — le carnet
    /// connaît quelqu'un d'un peu trop — et [`crate::contacts::rebuild`] la corrige.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Sqlite`].
    pub fn clear_contacts(&self) -> Result<()> {
        let tx = self.connection().unchecked_transaction()?;
        tx.execute("DELETE FROM contacts", [])?;
        tx.execute("UPDATE messages SET contacts_counted = 0", [])?;
        tx.commit()?;
        Ok(())
    }

    /// Les messages que le carnet n'a pas encore comptés, au plus `limit`.
    ///
    /// Sert la passe incrémentale. L'ordre est celui des identifiants : il n'a pas d'importance
    /// pour le résultat — les compteurs s'ajoutent, l'addition est commutative — mais un ordre
    /// stable rend une exécution interrompue reproductible.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Sqlite`].
    pub fn uncounted_for_contacts(&self, limit: usize) -> Result<Vec<ContactRow>> {
        let mut statement = self.connection().prepare_cached(
            "SELECT id, blob_hash, from_addr, from_name, date
             FROM messages
             WHERE contacts_counted = 0
             ORDER BY id
             LIMIT ?1",
        )?;
        let rows = statement.query_map([i64::try_from(limit).unwrap_or(i64::MAX)], |row| {
            let hash: Vec<u8> = row.get(1)?;
            Ok(ContactRow {
                id: crate::MessageId(row.get(0)?),
                blob: crate::store::read::blob_hash(&hash),
                from_addr: row.get(2)?,
                from_name: row.get(3)?,
                date: row.get(4)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Combien de messages restent à compter.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Sqlite`].
    pub fn uncounted_for_contacts_total(&self) -> Result<u64> {
        let count: i64 = self.connection().query_row(
            "SELECT COUNT(*) FROM messages WHERE contacts_counted = 0",
            [],
            |row| row.get(0),
        )?;
        Ok(u64::try_from(count).unwrap_or(0))
    }

    /// Marque des messages comme comptés.
    ///
    /// **À n'appeler qu'après que leur contribution est écrite.** L'ordre est celui qui rend une
    /// interruption sans dommage : un lot compté puis non marqué sera recompté — le carnet
    /// connaîtra quelqu'un un peu trop — alors qu'un lot marqué puis non compté serait perdu
    /// pour toujours, en silence. Des deux dérives possibles, on choisit celle qui se voit et
    /// que `rebuild` corrige.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Sqlite`].
    pub fn mark_contacts_counted(&self, ids: &[crate::MessageId]) -> Result<()> {
        let tx = self.connection().unchecked_transaction()?;
        {
            let mut statement =
                tx.prepare_cached("UPDATE messages SET contacts_counted = 1 WHERE id = ?1")?;
            for id in ids {
                statement.execute([id.0])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Retire une adresse du carnet.
    ///
    /// Sert à en sortir les adresses des comptes de l'utilisateur : se proposer soi-même en
    /// complétion n'aide personne, et elles y arrivent forcément — elles sont dans le `To` de
    /// tout ce qu'il reçoit.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Sqlite`].
    pub fn forget_contact(&self, address: &str) -> Result<()> {
        self.connection().execute(
            "DELETE FROM contacts WHERE address = ?1",
            params![address.to_lowercase()],
        )?;
        Ok(())
    }

    /// Le nombre d'adresses connues.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Sqlite`].
    pub fn contact_count(&self) -> Result<u64> {
        Ok(self
            .connection()
            .query_row("SELECT count(*) FROM contacts", [], |row| {
                let count: i64 = row.get(0)?;
                Ok(u64::try_from(count).unwrap_or(0))
            })?)
    }

    /// Complète un début de saisie.
    ///
    /// ## Deux requêtes, et une seule est un parcours de plage
    ///
    /// Le préfixe d'**adresse** passe par la clé primaire : `address >= 'ma' AND address < 'mb'`
    /// est un parcours de plage, ce que le critère 4 demande — et ce qu'un `LIKE '%ma%'` ne
    /// serait jamais.
    ///
    /// Le nom est un parcours de table sur `name_fold`, et c'est assumé jusqu'à ce que la mesure
    /// dise le contraire. Un index n'aiderait de toute façon que sur le premier mot : « Dupont »
    /// ne se trouverait pas en tapant « dup » si le nom est « Marie Dupont », et c'est
    /// précisément la recherche qu'un utilisateur fait.
    ///
    /// ## La borne haute du préfixe
    ///
    /// Incrémenter le dernier **octet** et non le dernier caractère. Les adresses sont de
    /// l'ASCII en pratique, et un préfixe qui finirait sur un octet de continuation UTF-8
    /// donnerait une borne invalide — d'où le repli sur un parcours quand ça arrive, plutôt
    /// qu'une plage fausse qui raterait des résultats en silence.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Sqlite`].
    pub fn complete(&self, prefix: &str, limit: usize) -> Result<Vec<Contact>> {
        let needle = prefix.trim().to_lowercase();
        if needle.is_empty() {
            return self.top_contacts(limit);
        }

        let mut found: Vec<Contact> = Vec::new();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

        // 1. Le préfixe d'adresse, par la clé primaire.
        if let Some(upper) = upper_bound(&needle) {
            let mut statement = self.connection().prepare_cached(
                "SELECT address, name, seen_to, seen_from, last_seen
                   FROM contacts
                  WHERE address >= ?1 AND address < ?2",
            )?;
            let rows = statement.query_map(params![needle, upper], row_to_contact)?;
            for row in rows {
                let contact = row?;
                if seen.insert(contact.address.clone()) {
                    found.push(contact);
                }
            }
        }

        // 2. Le nom, n'importe quel mot. `instr` sur un nom déjà en minuscules côté SQL : la
        //    colonne garde sa casse pour l'affichage, donc la comparaison la baisse.
        let mut statement = self.connection().prepare_cached(
            "SELECT address, name, seen_to, seen_from, last_seen
               FROM contacts
              WHERE name_fold IS NOT NULL AND instr(name_fold, ?1) > 0",
        )?;
        let rows = statement.query_map(params![needle], row_to_contact)?;
        for row in rows {
            let contact = row?;
            if seen.insert(contact.address.clone()) {
                found.push(contact);
            }
        }

        // Le classement a lieu **après** l'union : trier chaque requête séparément donnerait un
        // ordre qui dépend de laquelle a répondu, ce qui n'est pas un ordre.
        found.sort_by(|a, b| {
            b.score()
                .cmp(&a.score())
                .then(b.last_seen.cmp(&a.last_seen))
                .then(a.address.cmp(&b.address))
        });
        found.truncate(limit);
        Ok(found)
    }

    /// Les adresses les mieux classées, sans préfixe.
    ///
    /// Ce qu'un champ de destinataire vide propose. Le tri est fait en Rust et non en SQL,
    /// parce que [`score`] n'est pas exprimable en SQL sans y recopier ses constantes — et deux
    /// copies d'un classement finissent par ne plus classer pareil.
    ///
    /// L'index `contacts_rank` sert quand même : il borne le nombre de lignes à trier à celles
    /// qui ont déjà reçu un envoi, plus un rattrapage.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Sqlite`].
    pub fn top_contacts(&self, limit: usize) -> Result<Vec<Contact>> {
        let mut statement = self.connection().prepare_cached(
            "SELECT address, name, seen_to, seen_from, last_seen
               FROM contacts
              ORDER BY seen_to DESC, last_seen DESC
              LIMIT ?1",
        )?;
        // Trois fois la limite demandée : le tri SQL n'est pas le classement final, donc il
        // faut de la marge pour que le vrai classement puisse remonter quelqu'un.
        let budget = i64::try_from(limit.saturating_mul(3)).unwrap_or(i64::MAX);
        let rows = statement.query_map(params![budget], row_to_contact)?;
        let mut found: Vec<Contact> = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        found.sort_by(|a, b| {
            b.score()
                .cmp(&a.score())
                .then(b.last_seen.cmp(&a.last_seen))
                .then(a.address.cmp(&b.address))
        });
        found.truncate(limit);
        Ok(found)
    }
}

/// La borne haute d'un préfixe, pour un parcours de plage.
///
/// `None` quand le préfixe finit sur un octet qui ne peut pas être incrémenté — `0xFF`, ou un
/// octet de continuation UTF-8. Rendre `None` fait retomber l'appelant sur le parcours par nom
/// plutôt que sur une plage fausse qui raterait des résultats sans le dire.
fn upper_bound(prefix: &str) -> Option<String> {
    let mut bytes = prefix.as_bytes().to_vec();
    let last = bytes.last_mut()?;
    if *last >= 0x7F {
        return None;
    }
    *last += 1;
    String::from_utf8(bytes).ok()
}

/// Une ligne SQL vers un [`Contact`].
fn row_to_contact(row: &rusqlite::Row<'_>) -> rusqlite::Result<Contact> {
    let seen_to: i64 = row.get(2)?;
    let seen_from: i64 = row.get(3)?;
    Ok(Contact {
        address: row.get(0)?,
        name: row.get(1)?,
        seen_to: u32::try_from(seen_to).unwrap_or(u32::MAX),
        seen_from: u32::try_from(seen_from).unwrap_or(u32::MAX),
        last_seen: row.get(4)?,
    })
}
/// Ce qu'une reconstruction a trouvé.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ContactStats {
    /// Messages parcourus.
    pub scanned: u64,
    /// Adresses distinctes retenues.
    pub addresses: u64,
    /// Messages reconnus comme **envoyés par l'utilisateur**.
    ///
    /// Le contrôle de la reconstruction : un zéro veut dire que `seen_to` est vide partout,
    /// donc que le classement n'a plus qu'un signal sur deux — et l'autocomplétion redevient
    /// « par ordre de réception », ce que le module existe pour éviter.
    pub outgoing: u64,
    /// Messages dont le blob a disparu. Comptés, jamais propagés.
    pub missing: u64,
}

/// Reconstruit le carnet depuis le corpus.
///
/// ## Le tri « envoyé » / « reçu » se fait sur les adresses des comptes
///
/// Un message est **envoyé par l'utilisateur** si son `From` est l'une des adresses de ses
/// comptes. Ses `To` et `Cc` sont alors des adresses à qui il a écrit, donc du `seen_to`.
///
/// Sans cette règle, `seen_to` serait vide partout : le seul en-tête qu'on lirait avec certitude
/// serait `From`, et le carnet ne saurait que « qui m'écrit ». C'est exactement le classement que
/// [`score`] existe pour éviter.
///
/// Les co-destinataires d'un message **reçu** — les autres personnes en copie — comptent comme
/// du `seen_from` : on ne leur a pas écrit, mais on a été dans la même conversation, et c'est
/// plus qu'une adresse jamais vue.
///
/// ## Seuls les en-têtes sont lus
///
/// `mail-parser` a besoin du message entier pour rendre un `Message`, mais on ne touche que ses
/// en-têtes : aucun corps n'est aplati, aucun HTML n'est assaini. Sur 6,3 Gio de corpus, c'est
/// la différence entre quelques minutes et une heure.
///
/// ## Elle vide d'abord
///
/// Les compteurs s'ajoutent, donc repasser sur un message déjà compté le compterait deux fois.
/// Voir [`Store::clear_contacts`].
///
/// # Errors
///
/// [`crate::Error::Sqlite`] si le store est illisible. Un message dont le blob a disparu est
/// **compté**, jamais propagé : un carnet incomplet vaut mieux qu'aucun carnet.
pub fn rebuild(store: &Store, progress: &crate::Progress) -> Result<ContactStats> {
    // Vider **et** remettre tous les drapeaux : les deux gestes sont dans la même transaction,
    // voir `Store::clear_contacts`. C'est ce qui rend la reconstruction idempotente.
    store.clear_contacts()?;
    advance(store, progress)
}

/// Compte ce que le carnet n'a pas encore vu, et s'arrête là.
///
/// ## Pourquoi elle existe
///
/// Le carnet ne suivait aucune moisson : il fallait relancer une reconstruction complète à la
/// main pour qu'une personne à qui on venait d'écrire apparaisse en complétion. Et enchaîner une
/// reconstruction après chaque moisson était exclu — `IDLE` met un `Kind::Sync` en file **à
/// chaque arrivée de courrier**, et une passe complète sur 73 000 messages se compte en
/// minutes. Un mail reçu aurait coûté des minutes de processeur.
///
/// ## Elle compte exactement comme [`rebuild`], parce que c'est le même code
///
/// Les deux appellent [`advance`]. La seule différence est ce que `rebuild` fait **avant** :
/// vider. Deux boucles séparées auraient divergé — et un carnet reconstruit qui ne donne pas le
/// même classement qu'un carnet tenu à jour serait le pire des deux mondes, parce que la
/// différence ne se verrait qu'à l'usage.
///
/// # Errors
///
/// Les mêmes que [`rebuild`].
pub fn update(store: &Store, progress: &crate::Progress) -> Result<ContactStats> {
    advance(store, progress)
}

/// La boucle partagée : compte les messages non comptés, les marque, et rend le bilan.
///
/// ## L'ordre des deux écritures, et pourquoi il n'est pas l'inverse
///
/// Un lot est **d'abord** enregistré dans le carnet, **ensuite** marqué compté. Une coupure
/// entre les deux fait recompter le lot au prochain passage : le carnet connaît alors quelqu'un
/// un peu trop, ce qui décale un classement et se corrige par `rebuild`. L'ordre inverse
/// perdrait le lot pour toujours, en silence, sans que rien ne puisse le retrouver. Des deux
/// dérives, on choisit celle qui se voit.
///
/// C'est le même raisonnement que `mailsmtp::queue::deliver_one` sur l'envoi, à enjeu moindre :
/// on préfère le doublon visible à la perte muette.
fn advance(store: &Store, progress: &crate::Progress) -> Result<ContactStats> {
    let own = own_addresses(store)?;

    let mut stats = ContactStats::default();
    let parser = mail_parser::MessageParser::default();

    // **Une passe traite ce qui était en attente quand elle a commencé, et pas davantage.**
    //
    // Le relevé initial sert à deux choses. La première est la progression. La seconde est de
    // **borner la boucle** : sans borne, elle ne s'arrête que parce que `mark_contacts_counted`
    // retire les lignes de la requête — et un jour où ce marquage ne ferait plus son travail,
    // le job de fond tournerait pour toujours en consommant un cœur. C'est ce qu'a montré le
    // contrôle négatif du 2026-09-11 : en retirant le marquage, les tests ne tombent pas, ils
    // **pendent**. Une boucle de fond qui ne finit jamais est pire qu'un résultat faux, parce
    // que rien ne la signale.
    //
    // Ce que la borne coûte : un message arrivé pendant la passe attend la suivante. Sans
    // importance ici, puisqu'une passe suit chaque moisson.
    let pending = store.uncounted_for_contacts_total()?;
    progress.set_total(pending);
    let mut remaining = pending;

    // Les écritures sont groupées : une transaction par message coûterait un `fsync` par
    // message sur 48 000 messages.
    let mut batch: Vec<Seen> = Vec::with_capacity(BATCH * 8);
    let mut counted: Vec<crate::MessageId> = Vec::with_capacity(ROWS);

    while remaining > 0 {
        // Les lignes sont relues par paquets plutôt que toutes d'un coup : la passe tourne
        // maintenant après **chaque** moisson, et charger 73 000 lignes pour en traiter trois
        // serait payer le prix de la reconstruction à chaque mail reçu.
        let rows = store.uncounted_for_contacts(ROWS.min(remaining as usize))?;
        if rows.is_empty() {
            break;
        }
        remaining = remaining.saturating_sub(rows.len() as u64);

        for row in rows {
            if progress.is_cancelled() {
                tracing::info!(scanned = stats.scanned, "carnet interrompu");
                // Ce qui est en main est écrit avant de partir : une annulation ne doit pas
                // jeter le travail déjà fait, elle doit l'arrêter.
                flush(store, &mut batch, &mut counted)?;
                return finish(store, &own, stats);
            }
            progress.advance(1);
            stats.scanned += 1;
            counted.push(row.id);

            let raw = match store.blobs().read(row.blob) {
                Ok(bytes) => bytes,
                Err(_) => {
                    stats.missing += 1;
                    continue;
                }
            };
            let Some(parsed) = parser.parse(&raw) else {
                // Un message que l'analyseur refuse en entier : son `From` est quand même connu
                // du store, donc on le retient plutôt que de le perdre.
                batch.push(Seen {
                    address: row.from_addr.to_lowercase(),
                    name: row.from_name.clone(),
                    outgoing: false,
                    date: row.date,
                });
                continue;
            };

            let sender = parsed
                .from()
                .and_then(|list| list.first())
                .and_then(|it| it.address())
                .map(str::to_lowercase)
                .unwrap_or_else(|| row.from_addr.to_lowercase());
            let outgoing = own.contains(&sender);
            if outgoing {
                stats.outgoing += 1;
            }

            if outgoing {
                // L'utilisateur écrivait : ses destinataires sont ce qui compte.
                harvest(&parsed, &mut batch, row.date, true);
            } else {
                batch.push(Seen {
                    address: sender,
                    name: parsed
                        .from()
                        .and_then(|list| list.first())
                        .and_then(|it| it.name())
                        .map(str::to_owned)
                        .or_else(|| row.from_name.clone()),
                    outgoing: false,
                    date: row.date,
                });
                // Les co-destinataires. Les adresses des comptes de l'utilisateur y sont, et on
                // les retire : se proposer soi-même en complétion n'aide personne.
                harvest(&parsed, &mut batch, row.date, false);
            }

            if batch.len() >= BATCH {
                flush(store, &mut batch, &mut counted)?;
            }
        }
        // Fin du paquet : ce qui reste est écrit avant d'en relire un autre. Sans ça, la
        // requête suivante rendrait les **mêmes** lignes — elles ne sont marquées qu'ici — et
        // la boucle ne s'arrêterait jamais.
        flush(store, &mut batch, &mut counted)?;
    }

    finish(store, &own, stats)
}

/// Écrit un lot dans le carnet, **puis** marque ses messages comptés.
///
/// L'ordre est la propriété de `advance` : voir sa documentation.
fn flush(store: &Store, batch: &mut Vec<Seen>, counted: &mut Vec<crate::MessageId>) -> Result<()> {
    if !batch.is_empty() {
        store.record_seen(batch)?;
        batch.clear();
    }
    if !counted.is_empty() {
        store.mark_contacts_counted(counted)?;
        counted.clear();
    }
    Ok(())
}

/// Retire les adresses de l'utilisateur et rend le bilan.
fn finish(
    store: &Store,
    own: &std::collections::HashSet<String>,
    mut stats: ContactStats,
) -> Result<ContactStats> {
    // Les adresses des comptes sont retirées à la fin : les retirer au fil de l'eau
    // demanderait de filtrer chaque lot, et une adresse peut arriver par plusieurs chemins.
    for address in own {
        store.forget_contact(address)?;
    }

    stats.addresses = store.contact_count()?;
    tracing::info!(
        scanned = stats.scanned,
        addresses = stats.addresses,
        outgoing = stats.outgoing,
        missing = stats.missing,
        "carnet avancé"
    );
    Ok(stats)
}

/// Combien d'adresses accumuler avant d'écrire.
///
/// Mille. Le coût dominant est le `fsync` de la transaction, pas la mémoire : mille `Seen`
/// tiennent dans quelques dizaines de kilooctets.
const BATCH: usize = 1_000;

/// Combien de lignes de message relire à la fois.
///
/// Deux mille : assez pour que le coût par requête disparaisse sur une reconstruction complète,
/// assez peu pour qu'une moisson qui apporte trois messages ne charge pas le corpus entier.
///
/// Le plafond n'existe que depuis que la passe est incrémentale. Avant, elle lisait tout d'un
/// coup — ce qui était correct pour une commande lancée à la main, et ne l'est plus pour une
/// passe qui tourne après chaque arrivée de courrier.
const ROWS: usize = 2_000;

/// Ajoute les `To` et `Cc` d'un message au lot.
fn harvest(parsed: &mail_parser::Message<'_>, batch: &mut Vec<Seen>, date: i64, outgoing: bool) {
    for header in [parsed.to(), parsed.cc()].into_iter().flatten() {
        for it in header.iter() {
            let Some(address) = it.address() else {
                continue;
            };
            batch.push(Seen {
                address: address.to_lowercase(),
                name: it.name().map(str::to_owned),
                outgoing,
                date,
            });
        }
    }
}

/// Les adresses des comptes de l'utilisateur, en minuscules.
///
/// ## Un compte incohérent ne fait pas échouer le carnet
///
/// `full_accounts` refuse un compte imap sans serveur, et cette lecture-là ne doit pas empêcher
/// de reconstruire un carnet : elle retombe alors sur la liste des comptes sans leurs serveurs.
/// Le carnet est moins bon — `seen_to` sera vide pour le compte fautif — et il existe.
fn own_addresses(store: &Store) -> Result<std::collections::HashSet<String>> {
    match store.full_accounts() {
        Ok(accounts) => Ok(accounts
            .iter()
            .filter_map(|it| it.server.as_ref())
            .map(|it| it.username.to_lowercase())
            .collect()),
        Err(source) => {
            tracing::warn!(%source, "comptes illisibles : le carnet ne saura pas qui a écrit");
            Ok(std::collections::HashSet::new())
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::{Contact, FROM_CAP, Seen, TO_WEIGHT, score, upper_bound};
    use crate::store::Store;

    fn store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let root = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        let store = Store::open(&root).unwrap();
        (dir, store)
    }

    fn seen(address: &str, name: Option<&str>, outgoing: bool, date: i64) -> Seen {
        Seen {
            address: address.to_owned(),
            name: name.map(ToOwned::to_owned),
            outgoing,
            date,
        }
    }

    #[test]
    fn writing_once_beats_receiving_a_thousand_times() {
        // **La propriété qui décide si l'autocomplétion sert à quelque chose.** Sans le
        // plafond sur les réceptions, taper « c » proposerait le service client d'un marchand
        // avant le collègue à qui on écrit chaque semaine.
        assert!(
            score(1, 0) > score(0, 1_000),
            "une lettre d'information passe devant quelqu'un à qui on a écrit"
        );
        assert!(score(2, 0) > score(0, i64::MAX / 2));
    }

    #[test]
    fn receiving_once_still_beats_never_having_been_seen() {
        // Le contrôle négatif du plafond : il ne doit pas écraser les réceptions à zéro, sinon
        // « répondre à un inconnu qui vient de m'écrire » ne marcherait pas.
        assert!(score(0, 1) > score(0, 0));
    }

    #[test]
    fn the_cap_stops_counting_but_never_subtracts() {
        assert_eq!(score(0, FROM_CAP), FROM_CAP);
        assert_eq!(score(0, FROM_CAP + 1), FROM_CAP);
        assert_eq!(score(0, 10_000), FROM_CAP);
        assert_eq!(score(1, 0), TO_WEIGHT);
    }

    #[test]
    fn a_huge_count_never_overflows_into_a_negative_score() {
        // Un score négatif classerait l'adresse **dernière**, ce qui est le contraire du sens.
        for (to, from) in [(i64::MAX, i64::MAX), (i64::MAX, 0), (0, i64::MAX)] {
            assert!(score(to, from) > 0, "score({to}, {from}) a débordé");
        }
    }

    #[test]
    fn a_name_never_goes_backwards() {
        // Sans cette règle, l'ordre de parcours de la reconstruction déciderait du nom affiché,
        // et deux reconstructions donneraient deux carnets.
        let (_dir, store) = store();
        store
            .record_seen(&[seen("marie@x.fr", Some("Marie Dupont"), false, 2_000)])
            .unwrap();
        store
            .record_seen(&[seen("marie@x.fr", Some("M. D."), false, 1_000)])
            .unwrap();

        let found = store.complete("marie", 5).unwrap();
        assert_eq!(found[0].name.as_deref(), Some("Marie Dupont"));
    }

    #[test]
    fn an_absent_name_never_erases_a_known_one() {
        // Beaucoup de messages ne portent que l'adresse. Les laisser effacer le nom viderait le
        // carnet de ce qui le rend lisible.
        let (_dir, store) = store();
        store
            .record_seen(&[seen("marie@x.fr", Some("Marie Dupont"), false, 1_000)])
            .unwrap();
        store
            .record_seen(&[seen("marie@x.fr", None, false, 9_000)])
            .unwrap();
        store
            .record_seen(&[seen("marie@x.fr", Some("   "), false, 9_999)])
            .unwrap();

        let found = store.complete("marie", 5).unwrap();
        assert_eq!(found[0].name.as_deref(), Some("Marie Dupont"));
        assert_eq!(found[0].last_seen, 9_999, "la date doit avancer, elle");
    }

    #[test]
    fn the_counters_add_up_and_stay_separate() {
        let (_dir, store) = store();
        store
            .record_seen(&[
                seen("marie@x.fr", None, true, 1_000),
                seen("marie@x.fr", None, true, 2_000),
                seen("marie@x.fr", None, false, 3_000),
            ])
            .unwrap();

        let found = store.complete("marie", 5).unwrap();
        assert_eq!(found[0].seen_to, 2);
        assert_eq!(found[0].seen_from, 1);
    }

    #[test]
    fn completion_finds_a_name_by_any_of_its_words() {
        // **La recherche qu'un utilisateur fait vraiment.** Taper « dup » pour trouver
        // « Marie Dupont » : un index sur un préfixe de nom ne le permettrait pas, et c'est la
        // raison documentée de ne pas en avoir mis un.
        let (_dir, store) = store();
        store
            .record_seen(&[seen("m.d@exemple.fr", Some("Marie Dupont"), true, 1_000)])
            .unwrap();

        assert_eq!(store.complete("dup", 5).unwrap().len(), 1);
        assert_eq!(store.complete("Dup", 5).unwrap().len(), 1, "la casse");
        assert_eq!(store.complete("marie", 5).unwrap().len(), 1);
        assert_eq!(store.complete("m.d", 5).unwrap().len(), 1, "par adresse");
        assert!(store.complete("zzz", 5).unwrap().is_empty());
    }

    #[test]
    fn a_contact_is_never_proposed_twice() {
        // Une adresse qui correspond **et** par l'adresse **et** par le nom sort des deux
        // requêtes. La proposer deux fois ferait une liste où l'on croit avoir deux personnes.
        let (_dir, store) = store();
        store
            .record_seen(&[seen("marie@x.fr", Some("marie dupont"), true, 1_000)])
            .unwrap();

        let found = store.complete("marie", 5).unwrap();
        assert_eq!(found.len(), 1, "{found:?}");
    }

    #[test]
    fn the_ranking_puts_the_person_written_to_first() {
        let (_dir, store) = store();
        let mut bulk = Vec::new();
        // Une lettre d'information : mille réceptions, aucun envoi.
        for date in 0..1_000 {
            bulk.push(seen("contact@marchand.fr", Some("Marchand"), false, date));
        }
        // Un collègue : deux envois.
        bulk.push(seen("collegue@bureau.fr", Some("Collègue"), true, 10));
        bulk.push(seen("collegue@bureau.fr", Some("Collègue"), true, 20));
        store.record_seen(&bulk).unwrap();

        let found = store.complete("c", 5).unwrap();
        assert_eq!(
            found[0].address, "collegue@bureau.fr",
            "le classement met la lettre d'information devant : {found:?}"
        );
    }

    #[test]
    fn an_empty_prefix_proposes_the_best_ranked() {
        let (_dir, store) = store();
        store
            .record_seen(&[
                seen("rare@x.fr", None, false, 1),
                seen("souvent@x.fr", None, true, 2),
            ])
            .unwrap();

        let found = store.complete("   ", 5).unwrap();
        assert_eq!(found[0].address, "souvent@x.fr");
    }

    #[test]
    fn the_label_is_insertable_in_a_recipient_field() {
        // La proposition doit pouvoir être insérée telle quelle : c'est
        // `mailsmtp::compose::Address` qui la relira.
        let with_name = Contact {
            address: "marie@x.fr".to_owned(),
            name: Some("Marie Dupont".to_owned()),
            seen_to: 1,
            seen_from: 0,
            last_seen: 0,
        };
        assert_eq!(with_name.label(), "Marie Dupont <marie@x.fr>");

        let bare = Contact {
            name: None,
            ..with_name.clone()
        };
        assert_eq!(bare.label(), "marie@x.fr");

        // Un nom qui répète l'adresse ne l'écrit pas deux fois.
        let echoed = Contact {
            name: Some("marie@x.fr".to_owned()),
            ..with_name
        };
        assert_eq!(echoed.label(), "marie@x.fr");
    }

    #[test]
    fn the_upper_bound_refuses_rather_than_lying() {
        assert_eq!(upper_bound("ma").as_deref(), Some("mb"));
        assert_eq!(upper_bound("m").as_deref(), Some("n"));
        // Un octet non ASCII en fin de préfixe : pas de plage, plutôt qu'une plage fausse.
        assert!(upper_bound("é").is_none());
        assert!(upper_bound("").is_none());
    }

    #[test]
    fn a_non_ascii_prefix_still_finds_a_name() {
        // Le repli du test ci-dessus, vu de l'appelant : la plage d'adresses est abandonnée, et
        // le parcours par nom répond quand même. Sans lui, taper « é » ne trouverait rien.
        let (_dir, store) = store();
        store
            .record_seen(&[seen("e.dupont@x.fr", Some("Éloïse Dupont"), true, 1_000)])
            .unwrap();

        let found = store.complete("éloïse", 5).unwrap();
        assert_eq!(found.len(), 1, "{found:?}");
    }

    #[test]
    fn clearing_makes_a_rebuild_idempotent() {
        // Les compteurs s'ajoutent, donc repasser sur le corpus sans vider compterait tout deux
        // fois. C'est la raison pour laquelle la reconstruction est complète et non
        // incrémentale.
        let (_dir, store) = store();
        let corpus = vec![seen("marie@x.fr", Some("Marie"), true, 1_000)];

        store.record_seen(&corpus).unwrap();
        store.clear_contacts().unwrap();
        store.record_seen(&corpus).unwrap();

        assert_eq!(store.contact_count().unwrap(), 1);
        assert_eq!(store.complete("marie", 5).unwrap()[0].seen_to, 1);
    }

    /// Un store avec un compte dont l'adresse est `moi@exemple.fr`, et des messages.
    fn corpus(messages: &[(&str, i64)]) -> (tempfile::TempDir, Store) {
        use crate::model::{AuthKind, Security, Server};

        let (dir, store) = store();
        let account = {
            let writer = store.writer().unwrap();
            let id = writer
                .upsert_imap_account(
                    "moi",
                    &Server {
                        host: "imap.exemple.fr".to_owned(),
                        port: 993,
                        username: "moi@exemple.fr".to_owned(),
                        auth: AuthKind::Password,
                        security: Security::Tls,
                    },
                )
                .unwrap();
            writer.commit().unwrap();
            id
        };
        {
            let writer = store.writer().unwrap();
            writer
                .upsert_folder(account, "INBOX", crate::model::FolderKind::Inbox)
                .unwrap();
            writer.commit().unwrap();
        }

        for (raw, date) in messages {
            add_message(&store, raw, *date);
        }
        (dir, store)
    }

    /// Ajoute un message au premier dossier du store, comme une moisson le ferait.
    ///
    /// Extrait de `corpus` pour que les tests de la passe incrémentale puissent en ajouter
    /// **après** — c'est tout leur sujet : ce qui arrive entre deux passes.
    fn add_message(store: &Store, raw: &str, date: i64) {
        use crate::store::write::NewMessage;

        let folder = store.folders().unwrap()[0].id;
        let blob = store.blobs().put(raw.as_bytes()).unwrap().hash;
        let from = raw
            .lines()
            .find_map(|line| line.strip_prefix("From: "))
            .unwrap_or("inconnu@nulle-part.fr");
        let writer = store.writer().unwrap();
        let (id, _) = writer
            .insert_message(&NewMessage {
                blob,
                rfc822_id: None,
                date,
                from_addr: from,
                from_name: None,
                subject: "sujet",
                size: raw.len() as u64,
                has_attachments: false,
            })
            .unwrap();
        writer
            .insert_ref(id, folder, date, crate::model::MessageFlags::empty())
            .unwrap();
        writer.commit().unwrap();
    }

    #[test]
    fn a_rebuild_tells_who_the_user_wrote_to() {
        // **Le tri qui fait tout le classement.** Sans lui, `seen_to` est vide partout et
        // l'autocomplétion redevient « par ordre de réception ».
        let (_dir, store) = corpus(&[
            (
                "From: moi@exemple.fr\r\n\
                 To: Collègue <collegue@bureau.fr>\r\n\
                 Cc: chef@bureau.fr\r\n\
                 Subject: envoyé\r\n\r\ncorps\r\n",
                2_000,
            ),
            (
                "From: Marchand <contact@marchand.fr>\r\n\
                 To: moi@exemple.fr\r\n\
                 Subject: reçu\r\n\r\ncorps\r\n",
                1_000,
            ),
        ]);

        let stats = super::rebuild(&store, &crate::Progress::default()).unwrap();
        assert_eq!(stats.scanned, 2);
        assert_eq!(stats.outgoing, 1, "le message envoyé n'a pas été reconnu");
        assert_eq!(stats.missing, 0);

        let collegue = &store.complete("collegue", 5).unwrap()[0];
        assert_eq!(collegue.seen_to, 1, "un destinataire compté comme reçu");
        assert_eq!(collegue.seen_from, 0);
        assert_eq!(collegue.name.as_deref(), Some("Collègue"));

        let marchand = &store.complete("contact@", 5).unwrap()[0];
        assert_eq!(marchand.seen_to, 0, "un expéditeur compté comme écrit");
        assert_eq!(marchand.seen_from, 1);
        assert_eq!(marchand.name.as_deref(), Some("Marchand"));
    }

    /// Le carnet entier, trié, pour comparer deux chemins qui devraient donner la même chose.
    fn book(store: &Store) -> Vec<(String, u32, u32)> {
        let mut all: Vec<(String, u32, u32)> = store
            .complete("", 500)
            .unwrap()
            .into_iter()
            .map(|it| (it.address, it.seen_to, it.seen_from))
            .collect();
        all.sort();
        all
    }

    #[test]
    fn an_incremental_pass_gives_exactly_what_a_rebuild_gives() {
        // **La propriété qui justifie la colonne.** Deux chemins mènent maintenant au carnet, et
        // s'ils divergeaient la différence ne se verrait qu'à l'usage — un classement un peu
        // faux, sans rien pour le signaler. Les deux appellent `advance`, et ce test le fige.
        let messages: &[(&str, i64)] = &[
            (
                "From: moi@exemple.fr\r\nTo: Collègue <collegue@bureau.fr>\r\n\
                 Cc: chef@bureau.fr\r\nSubject: envoyé\r\n\r\ncorps\r\n",
                2_000,
            ),
            (
                "From: Marchand <contact@marchand.fr>\r\nTo: moi@exemple.fr\r\n\
                 Subject: reçu\r\n\r\ncorps\r\n",
                1_000,
            ),
            (
                "From: moi@exemple.fr\r\nTo: collegue@bureau.fr\r\n\
                 Subject: encore\r\n\r\ncorps\r\n",
                3_000,
            ),
        ];

        // Chemin A : tout d'un coup.
        let (_dir_a, full) = corpus(messages);
        crate::contacts::rebuild(&full, &crate::Progress::new()).unwrap();

        // Chemin B : un message à la fois, comme une moisson les apporterait.
        let (_dir_b, step) = corpus(&[]);
        for message in messages {
            add_message(&step, message.0, message.1);
            crate::contacts::update(&step, &crate::Progress::new()).unwrap();
        }

        assert_eq!(book(&full), book(&step));
        assert!(!book(&full).is_empty(), "les deux carnets sont vides");
    }

    #[test]
    fn a_second_pass_over_the_same_messages_counts_nothing_twice() {
        // Le défaut que la colonne existe pour empêcher. Sans elle, relancer la passe doublait
        // tous les compteurs — et c'est la raison pour laquelle le carnet ne suivait aucune
        // moisson.
        let (_dir, store) = corpus(&[(
            "From: Marchand <contact@marchand.fr>\r\nTo: moi@exemple.fr\r\n\
             Subject: reçu\r\n\r\ncorps\r\n",
            1_000,
        )]);
        crate::contacts::update(&store, &crate::Progress::new()).unwrap();
        let after_one = book(&store);

        let stats = crate::contacts::update(&store, &crate::Progress::new()).unwrap();
        assert_eq!(stats.scanned, 0, "un message déjà compté a été relu");
        assert_eq!(book(&store), after_one);
    }

    #[test]
    fn a_rebuild_after_incremental_passes_does_not_double_anything_either() {
        // Le contrôle croisé : `rebuild` doit remettre les drapeaux **et** vider. N'en faire
        // qu'un des deux donnerait soit un carnet vide que rien ne remplit, soit des compteurs
        // doublés — et les deux moitiés sont dans la même transaction pour cette raison.
        let (_dir, store) = corpus(&[(
            "From: moi@exemple.fr\r\nTo: collegue@bureau.fr\r\n\
             Subject: envoyé\r\n\r\ncorps\r\n",
            2_000,
        )]);
        crate::contacts::update(&store, &crate::Progress::new()).unwrap();
        let after_update = book(&store);

        let stats = crate::contacts::rebuild(&store, &crate::Progress::new()).unwrap();
        assert_eq!(stats.scanned, 1, "la reconstruction n'a pas tout relu");
        assert_eq!(book(&store), after_update);
    }

    #[test]
    fn an_empty_pass_is_cheap_and_says_so() {
        // Le cas de très loin le plus fréquent : une moisson qui n'apporte rien. Elle doit ne
        // rien parcourir, sinon la passe après chaque `IDLE` coûterait le prix du corpus.
        let (_dir, store) = corpus(&[(
            "From: contact@marchand.fr\r\nTo: moi@exemple.fr\r\n\
             Subject: reçu\r\n\r\ncorps\r\n",
            1_000,
        )]);
        crate::contacts::update(&store, &crate::Progress::new()).unwrap();
        let stats = crate::contacts::update(&store, &crate::Progress::new()).unwrap();
        assert_eq!(stats.scanned, 0);
        assert_eq!(stats.missing, 0);
    }

    #[test]
    fn the_user_is_never_proposed_to_himself() {
        // Son adresse est dans le `To` de tout ce qu'il reçoit, donc elle arrive forcément au
        // carnet. La proposer n'aide personne.
        let (_dir, store) = corpus(&[(
            "From: Marchand <contact@marchand.fr>\r\n\
             To: moi@exemple.fr\r\n\
             Subject: reçu\r\n\r\ncorps\r\n",
            1_000,
        )]);

        super::rebuild(&store, &crate::Progress::default()).unwrap();
        assert!(
            store.complete("moi@", 5).unwrap().is_empty(),
            "l'utilisateur se propose lui-même"
        );
    }

    #[test]
    fn a_co_recipient_is_known_without_having_been_written_to() {
        // Quelqu'un en copie d'un message reçu : on ne lui a pas écrit, mais on a été dans la
        // même conversation. Plus qu'une adresse jamais vue, moins qu'un destinataire.
        let (_dir, store) = corpus(&[(
            "From: Marchand <contact@marchand.fr>\r\n\
             To: moi@exemple.fr\r\n\
             Cc: Autre <autre@ailleurs.fr>\r\n\
             Subject: reçu\r\n\r\ncorps\r\n",
            1_000,
        )]);

        super::rebuild(&store, &crate::Progress::default()).unwrap();
        let autre = &store.complete("autre", 5).unwrap()[0];
        assert_eq!(autre.seen_to, 0);
        assert_eq!(autre.seen_from, 1);
    }

    #[test]
    fn two_rebuilds_give_the_same_book() {
        // L'idempotence, mesurée et non raisonnée : les compteurs s'ajoutent, donc c'est le
        // `clear_contacts` de `rebuild` qui la tient.
        let (_dir, store) = corpus(&[(
            "From: moi@exemple.fr\r\n\
             To: collegue@bureau.fr\r\n\
             Subject: envoyé\r\n\r\ncorps\r\n",
            2_000,
        )]);

        super::rebuild(&store, &crate::Progress::default()).unwrap();
        let first = store.complete("collegue", 5).unwrap();
        super::rebuild(&store, &crate::Progress::default()).unwrap();
        let second = store.complete("collegue", 5).unwrap();

        assert_eq!(first, second, "deux reconstructions, deux carnets");
        assert_eq!(second[0].seen_to, 1, "compté deux fois");
    }

    #[test]
    fn a_missing_blob_is_counted_not_propagated() {
        // Un carnet incomplet vaut mieux qu'aucun carnet.
        let (_dir, store) = corpus(&[(
            "From: moi@exemple.fr\r\n\
             To: collegue@bureau.fr\r\n\r\ncorps\r\n",
            2_000,
        )]);
        // Le blob disparaît, la ligne reste.
        let rows = store.all_for_indexing().unwrap();
        store.blobs().delete(rows[0].blob).unwrap();

        let stats = super::rebuild(&store, &crate::Progress::default()).unwrap();
        assert_eq!(stats.missing, 1);
        assert_eq!(stats.scanned, 1);
        assert_eq!(stats.addresses, 0);
    }

    #[test]
    fn a_cancelled_rebuild_stops_without_failing() {
        let (_dir, store) = corpus(&[(
            "From: moi@exemple.fr\r\nTo: collegue@bureau.fr\r\n\r\ncorps\r\n",
            2_000,
        )]);
        let progress = crate::Progress::default();
        progress.cancel();

        let stats = super::rebuild(&store, &progress).unwrap();
        assert_eq!(stats.scanned, 0, "un carnet annulé a quand même travaillé");
    }
}
