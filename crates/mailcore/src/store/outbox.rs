//! La file d'envoi : le seul journal du projet dont la perte soit **irréparable**.
//!
//! ## Pourquoi ce module est à part de `write`
//!
//! [`crate::store::write::Writer`] est fait pour les gros lots : une transaction, des dizaines
//! de milliers de lignes, un `fsync` pour tout le monde. C'est ce qui rend un import
//! réalisable, et c'est exactement le mauvais compromis ici.
//!
//! Une transition de la file est une ligne, et sa **durabilité est le mécanisme de correction**
//! du critère 2 : `committing` doit être sur le disque avant que le point final ne parte. Une
//! transition regroupée avec la suivante, ou validée en `synchronous = NORMAL`, ne garantit
//! plus l'ordre — et l'ordre est tout ce qu'on a.
//!
//! D'où un module qui écrit une ligne à la fois, chacune dans sa transaction, et qui monte
//! `synchronous` à `FULL` pour la durée de l'écriture. Voir [`Store::commit_outgoing`].
//!
//! ## Ce que la durabilité couvre, et ce qu'elle ne couvre pas
//!
//! `FULL` en WAL demande à SQLite un `fsync` du journal à chaque validation. Ça couvre la
//! coupure d'alimentation et le redémarrage brutal, en plus du processus tué — que `NORMAL`
//! couvrait déjà, puisque le cache de pages du système survit à la mort d'un processus.
//!
//! Ce qu'aucun réglage ne couvre : un disque qui ment sur ses `fsync`. C'est hors de portée du
//! programme, et le dire vaut mieux que de l'ignorer.

use rusqlite::{OptionalExtension, params};

use crate::error::Result;
use crate::model::{AccountId, BlobHash, OutboxId, Outgoing, SendState};
use crate::store::Store;

impl Store {
    /// Met un message dans la file, à l'état [`SendState::Queued`].
    ///
    /// Les octets du message doivent déjà être dans le magasin de blobs : l'appelant les y met
    /// **avant**, parce qu'une ligne de file qui pointe vers un blob absent serait un message
    /// perdu que rien ne signale.
    ///
    /// ## `not_before` est la fenêtre de rétractation, et elle réutilise la porte qui existe
    ///
    /// [`Store::deliverable`] refuse déjà ce dont `retry_after` est dans le futur : c'est le
    /// recul entre deux tentatives. Un maintien à la mise en file est exactement la même
    /// question posée plus tôt — « pas avant cet instant » — donc il s'écrit dans la même
    /// colonne et passe par la même porte.
    ///
    /// Ce qu'il ne touche pas, et c'est ce qui rend le procédé sûr : **`state`**. La règle du
    /// critère 2 — `committing` ne se remet jamais — vit sur l'état, pas sur le temps. Un
    /// maintien ne peut donc pas la relâcher, quelle que soit sa durée.
    ///
    /// `None` veut dire « tout de suite », et c'est ce que passe un appelant qui remet
    /// lui-même dans la foulée.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Sqlite`] si l'écriture échoue.
    // Huit paramètres pour huit colonnes, et l'appelant est unique — `mailsmtp::queue::stage`.
    // Les grouper dans une structure ne retirerait rien à ce qu'il faut savoir pour appeler :
    // ça déplacerait les huit champs d'un cran, et ajouterait un type dont la seule raison
    // d'être serait de contenter un compteur.
    #[allow(clippy::too_many_arguments)]
    pub fn enqueue(
        &self,
        account: AccountId,
        blob: BlobHash,
        sender: &str,
        recipients: &[String],
        size: u64,
        now: i64,
        not_before: Option<i64>,
    ) -> Result<OutboxId> {
        let joined = recipients.join("\n");
        // La taille est celle du message assemblé, connue par l'appelant qui vient de l'écrire.
        // La relire du blob demanderait de le décompresser en entier pour le compter — voir la
        // migration v7.
        let size = i64::try_from(size).unwrap_or(i64::MAX);
        self.durably(|conn| {
            conn.execute(
                "INSERT INTO outbox
                    (account_id, blob_hash, sender, recipients, state, queued_at, size,
                     retry_after)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    account.0,
                    blob.as_bytes().as_slice(),
                    sender,
                    joined,
                    SendState::Queued.as_str(),
                    now,
                    size,
                    not_before
                ],
            )?;
            Ok(OutboxId(conn.last_insert_rowid()))
        })
    }

    /// Écrit un état, **et le rend durable avant de rendre la main**.
    ///
    /// ## C'est la fonction dont l'ordre décide du critère 2
    ///
    /// Appelée avec [`SendState::Committing`], elle doit revenir seulement quand la transition
    /// est sur le disque : le point final part après. Appelée avec [`SendState::Sending`], elle
    /// pose la borne inverse — tant qu'elle n'est pas revenue, aucune enveloppe n'est ouverte.
    ///
    /// Un appelant qui inverserait l'ordre — écrire l'état après l'action — perdrait la
    /// garantie sans que rien ne le signale. C'est la raison de `mailsmtp::queue::deliver_one` :
    /// elle est le seul endroit du projet où cet ordre est écrit.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Sqlite`] si l'écriture ou la validation échoue. **Une erreur ici doit
    /// arrêter l'envoi** : sans la transition sur le disque, la suite serait indéfendable.
    ///
    /// ## Elle n'est plus sur le chemin de la remise
    ///
    /// C'est un écrivain **inconditionnel** : il pose l'état demandé quel que soit l'état
    /// présent. Les tests et les outils s'en servent pour placer une ligne dans un état donné.
    /// La remise, elle, passe depuis le 2026-10-19 par [`Store::claim_outgoing`] et
    /// [`Store::cross_frontier`], qui n'écrivent que si la ligne est encore dans l'état
    /// attendu — voir [`DeliveryLock`].
    pub fn commit_outgoing(&self, id: OutboxId, state: SendState) -> Result<()> {
        self.durably(|conn| {
            conn.execute(
                "UPDATE outbox SET state = ?2 WHERE id = ?1",
                params![id.0, state.as_str()],
            )?;
            Ok(())
        })
    }

    /// Prend le verrou de remise du store, **en attendant** qu'il se libère.
    ///
    /// Voir [`DeliveryLock`] : un seul remettant à la fois, tous processus confondus. Attendre
    /// plutôt que renoncer, parce que les deux appelants ont de quoi attendre — le facteur est
    /// un fil de fond, et `mail send` est une commande qu'on a lancée pour remettre.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Io`] si le fichier de verrou n'est pas ouvrable ou verrouillable.
    pub fn lock_delivery(&self) -> Result<DeliveryLock> {
        let path = self.root.join(DELIVERY_LOCK);
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path.as_std_path())
            .map_err(|source| crate::Error::Io {
                path: path.clone(),
                source,
            })?;
        match file.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => {
                tracing::info!("verrou de remise occupé : attente de son tour");
                file.lock()
                    .map_err(|source| crate::Error::Io { path, source })?;
            }
            Err(std::fs::TryLockError::Error(source)) => {
                return Err(crate::Error::Io { path, source });
            }
        }
        let lock = DeliveryLock {
            _file: file,
            marker: self.root.join(IN_FLIGHT),
        };
        // **Ce qu'un remettant mort a laissé s'efface ici**, avant toute remise : sous le verrou,
        // la ligne que nomme le fichier n'est plus celle de personne. Voir [`Store::in_flight`].
        lock.write_marker("");
        Ok(lock)
    }

    /// La ligne qu'un remettant **vivant** est en train de remettre, s'il y en a une.
    ///
    /// ## Pourquoi, depuis le 2026-10-19
    ///
    /// La frontière du doute est franchie **avant** le corps — c'est le critère 2 : une coupure
    /// pendant le transfert doit laisser un doute, pas une ligne remettable. Mais pendant tout ce
    /// transfert, des dizaines de secondes pour une pièce jointe, la ligne était donc
    /// `committing`, et l'interface l'affichait comme **douteuse** : « état incertain », et les
    /// deux boutons du doute, sur un envoi qui se passait normalement. Un avertissement qui
    /// s'affiche à chaque envoi finit par ne plus être lu — y compris le jour où il compte.
    ///
    /// L'état ne peut pas porter la différence : un remettant qui meurt ne réécrit rien, et sa
    /// ligne doit **alors** devenir douteuse sans que personne n'y touche. Ce qui le peut est le
    /// verrou de remise, que le système rend à la mort du processus. Le remettant écrit le
    /// numéro de sa ligne dans `outbox.sending` ([`DeliveryLock::announce`]) ; la ligne est en
    /// cours si ce fichier la nomme **et** que le verrou est tenu.
    ///
    /// ## Les deux lectures du fichier
    ///
    /// Le fichier est lu avant et après le contrôle du verrou, et les deux lectures doivent
    /// s'accorder. Sans la seconde, un remettant qui finit la ligne lue pendant qu'un autre prend
    /// le verrou pour la suivante ferait passer la première pour en cours alors qu'elle vient
    /// peut-être de tomber en doute.
    ///
    /// Ce qui reste, et qui est borné : un remettant **tué** laisse le fichier derrière lui ; le
    /// suivant l'efface sitôt le verrou pris ([`Store::lock_delivery`]). Entre les deux appels
    /// système, une lecture peut voir la ligne du mort comme en cours — le temps d'un
    /// rafraîchissement d'écran, et seulement après une mort.
    ///
    /// ## Ce que la réponse décide
    ///
    /// L'affichage, et un refus : [`Store::resolve_doubt`] ne tranche pas une ligne en cours,
    /// et [`Store::doubtful`] ne la compte pas. Elle ne décide **jamais** d'une remise — la règle
    /// du critère 2 reste sur l'état, dans `Store::deliverable`.
    ///
    /// Le contrôle prend le verrou **en partage** un instant s'il est libre : un remettant qui
    /// arrive à ce moment attend quelques microsecondes, et deux lecteurs ne se gênent pas.
    ///
    /// **Ce n'est pas une notification.** Un remettant qui meurt ne réécrit rien dans SQLite ;
    /// c'est `Mailbox::revision` qui porte ce terme, pour que les clients relisent la file.
    ///
    /// Sur un système de fichiers où `flock` est émulé par `fcntl` — certains montages réseau —,
    /// fermer la poignée de lecture rendrait aussi le verrou d'un remettant **du même processus**.
    /// Le verrou de remise avait déjà cette limite : un store ne vit pas sur un montage réseau.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Io`] si le fichier ou le verrou ne se lisent pas. Un appelant qui affiche
    /// doit alors pencher du côté du doute : le montrer à tort coûte un regard, le cacher à tort
    /// peut coûter un message.
    pub fn in_flight(&self) -> Result<Option<OutboxId>> {
        let marker = self.root.join(IN_FLIGHT);
        named_while_held(|| read_marker(&marker), || self.delivery_held())
    }

    /// Vrai si un remettant tient le verrou de remise, sans l'attendre.
    ///
    /// **Un verrou partagé, rendu aussitôt.** Exclusif, deux lecteurs simultanés — la coquille
    /// et `mail doctor` — se voyaient l'un l'autre comme un remettant, et le nom d'un mort passait
    /// pour en cours. Partagé, il ne heurte que le verrou exclusif d'un remettant. Et il est
    /// rendu par `unlock` avant la fermeture : sous Windows, un verrou rendu par la seule
    /// fermeture de la poignée l'est « selon les ressources disponibles ».
    fn delivery_held(&self) -> Result<bool> {
        let path = self.root.join(DELIVERY_LOCK);
        let file = match std::fs::OpenOptions::new()
            .read(true)
            .open(path.as_std_path())
        {
            Ok(file) => file,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(source) => return Err(crate::Error::Io { path, source }),
        };
        match file.try_lock_shared() {
            Ok(()) => {
                file.unlock()
                    .map_err(|source| crate::Error::Io { path, source })?;
                Ok(false)
            }
            Err(std::fs::TryLockError::WouldBlock) => Ok(true),
            Err(std::fs::TryLockError::Error(source)) => Err(crate::Error::Io { path, source }),
        }
    }

    /// Prend une ligne pour la remettre : `sending`, **si elle est encore dans l'état `seen`**.
    ///
    /// Rend `false` — sans rien écrire — si la ligne a disparu ou changé d'état depuis qu'on
    /// l'a lue : annulée, remise, reportée, tranchée. La condition est dans le `WHERE`, pour la
    /// raison de [`Store::cancel_outgoing`] : lire puis écrire laisserait une fenêtre.
    ///
    /// Un état `seen` qui n'est pas remettable rend `false` aussi : cette fonction ne doit pas
    /// pouvoir servir à reprendre un `committing`.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Sqlite`] si l'écriture échoue.
    pub fn claim_outgoing(&self, id: OutboxId, seen: SendState) -> Result<bool> {
        if !seen.is_deliverable() {
            return Ok(false);
        }
        self.durably(|conn| {
            let changed = conn.execute(
                "UPDATE outbox SET state = ?3 WHERE id = ?1 AND state = ?2",
                params![id.0, seen.as_str(), SendState::Sending.as_str()],
            )?;
            Ok(changed == 1)
        })
    }

    /// La frontière du doute : `committing`, **si la ligne est encore `sending`**.
    ///
    /// Rend `false` si ce n'est plus le cas, et l'appelant doit alors renoncer **avant** le
    /// point final — c'est le contrat de `mailsmtp::queue::Transport`. Sous le verrou de remise,
    /// aucun geste de l'utilisateur ne peut sortir une ligne de `sending` ; la condition est une
    /// seconde barrière, au cas où une évolution en ouvrirait un.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Sqlite`] si l'écriture échoue.
    pub fn cross_frontier(&self, id: OutboxId) -> Result<bool> {
        self.durably(|conn| {
            let changed = conn.execute(
                "UPDATE outbox SET state = ?3 WHERE id = ?1 AND state = ?2",
                params![
                    id.0,
                    SendState::Sending.as_str(),
                    SendState::Committing.as_str()
                ],
            )?;
            Ok(changed == 1)
        })
    }

    /// Note une tentative : l'horodatage, le compteur, et le recul avant la suivante.
    ///
    /// Le texte d'erreur est celui que l'utilisateur lira — critère 8. **L'appelant est
    /// responsable de n'y mettre aucun secret** ; ce module ne peut pas le vérifier, et le
    /// critère 7 le mesure sur le store entier.
    ///
    /// `resendable` accompagne ce texte : la phrase dit quoi faire, le bit dit si l'interface
    /// peut le faire d'un clic. Voir [`Outgoing::resendable`]. Il est écrit **dans la même
    /// transaction** que l'état et le texte, pour qu'aucun lecteur ne voie une phrase et un bit
    /// qui se contredisent.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Sqlite`] si l'écriture échoue.
    pub fn record_attempt(
        &self,
        id: OutboxId,
        state: SendState,
        now: i64,
        error: Option<&str>,
        retry_after: Option<i64>,
        resendable: bool,
    ) -> Result<()> {
        self.durably(|conn| {
            conn.execute(
                "UPDATE outbox
                    SET state = ?2,
                        attempts = attempts + 1,
                        tried_at = ?3,
                        last_error = ?4,
                        retry_after = ?5,
                        resendable = ?6
                  WHERE id = ?1",
                params![id.0, state.as_str(), now, error, retry_after, resendable],
            )?;
            Ok(())
        })
    }

    /// Remet en file une ligne **échouée**, sur décision de l'utilisateur.
    ///
    /// ## Pourquoi ce n'est pas `resolve_doubt`, et pourquoi ce n'est pas dangereux
    ///
    /// [`Store::resolve_doubt`] existe pour l'état `committing`, où le serveur a **peut-être**
    /// le message : renvoyer y est un pari, et c'est pour ça qu'il demande d'avoir vérifié chez
    /// le fournisseur.
    ///
    /// `failed` est l'inverse : le serveur a refusé, donc il n'a rien pris, donc **un renvoi ne
    /// peut pas faire de doublon**. Ce qu'il peut faire, c'est échouer à nouveau — et c'est à
    /// quoi sert [`Outgoing::resendable`], que l'interface consulte avant de proposer le geste.
    /// Cette fonction ne le consulte pas elle-même : un utilisateur qui demande explicitement un
    /// renvoi après avoir corrigé son compte chez son fournisseur a raison, et le bit ne sait
    /// pas ce qu'il a corrigé.
    ///
    /// ## Le compteur de tentatives repart de zéro
    ///
    /// À l'inverse de `resolve_doubt`, qui le conserve comme historique. Ici il **doit** repartir
    /// : une ligne `failed` par épuisement porte six tentatives, et les garder ferait échouer le
    /// renvoi au premier refus passager. Un renvoi demandé par un humain est un envoi neuf, pas
    /// la septième tentative d'un ancien.
    ///
    /// Ce qui est perdu est le compte des tentatives passées. `tried_at` le rattrape en partie,
    /// et le journal `info` ci-dessous garde la trace de la décision.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Sqlite`] si l'écriture échoue. Rend `false` — sans rien écrire — si la
    /// ligne n'existe pas ou n'est pas à l'état `failed`. Le refus des autres états n'est pas de
    /// la prudence : sans lui, cette fonction renverrait un message déjà `sent`.
    pub fn retry_outgoing(&self, id: OutboxId) -> Result<bool> {
        let Some(line) = self.outgoing(id)? else {
            return Ok(false);
        };
        if line.state != SendState::Failed {
            tracing::warn!(
                job = id.0,
                state = line.state.as_str(),
                "renvoi refusé : cette ligne n'a pas échoué"
            );
            return Ok(false);
        }
        self.durably(|conn| {
            conn.execute(
                "UPDATE outbox
                    SET state = ?2,
                        attempts = 0,
                        last_error = ?3,
                        retry_after = NULL,
                        resendable = 0
                  WHERE id = ?1",
                params![
                    id.0,
                    SendState::Queued.as_str(),
                    "remis en file sur décision de l'utilisateur"
                ],
            )?;
            Ok(())
        })?;
        tracing::info!(job = id.0, "ligne échouée remise en file");
        Ok(true)
    }

    /// Retire une ligne **finie** de la file.
    ///
    /// ## Elle refuse tout ce qui n'est pas fini
    ///
    /// `queued`, `sending` et surtout `committing` sont refusés. Retirer une ligne en cours
    /// perdrait un message que le facteur allait remettre ; retirer une ligne douteuse
    /// effacerait la trace d'un message peut-être parti, ce qui est exactement l'information
    /// que le critère 2 existe pour conserver.
    ///
    /// Seuls `sent` et `failed` partent, et ils partent sur décision de l'utilisateur : rien
    /// n'appelle cette fonction tout seul. L'historique d'envoi est un journal, et un journal
    /// qui se purge de lui-même ne sert à rien le jour où on le consulte.
    ///
    /// ## Le blob n'est pas supprimé ici
    ///
    /// Il devient orphelin, et `Store::orphan_blobs` le compte — `mail doctor` le dit. Le
    /// supprimer ici demanderait de savoir qu'aucune autre ligne ne le partage, ce qui arrive :
    /// deux envois du même contenu ont le même blob, par construction.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Sqlite`]. Rend `false` — sans rien écrire — si la ligne n'existe pas ou
    /// n'est pas finie.
    pub fn forget_outgoing(&self, id: OutboxId) -> Result<bool> {
        let Some(line) = self.outgoing(id)? else {
            return Ok(false);
        };
        if !matches!(line.state, SendState::Sent | SendState::Failed) {
            tracing::warn!(
                job = id.0,
                state = line.state.as_str(),
                "retrait refusé : cette ligne n'est pas finie"
            );
            return Ok(false);
        }
        self.durably(|conn| {
            conn.execute("DELETE FROM outbox WHERE id = ?1", params![id.0])?;
            Ok(())
        })?;
        tracing::info!(job = id.0, "ligne de file retirée");
        Ok(true)
    }

    /// Retire une ligne **qui n'a pas commencé**, sur décision de l'utilisateur.
    ///
    /// C'est la symétrique de [`Store::forget_outgoing`], qui ne retire que des lignes finies :
    /// celle-ci ne retire que des lignes pas encore parties. Entre les deux, il n'y a aucune
    /// sortie, et c'est voulu — `sending` et `committing` sont exactement les états où personne
    /// ne peut dire ce que le serveur a vu.
    ///
    /// ## Le refus est dans le `WHERE`, et c'est ce qui ferme la course
    ///
    /// Le facteur peut prendre la ligne entre le moment où on la lit et celui où on l'efface :
    /// [`mailsmtp::queue::deliver_one`] écrit `sending` **avant** d'ouvrir l'enveloppe. Lire
    /// l'état puis effacer laisserait donc une fenêtre où l'annulation retirerait un message
    /// déjà sur le fil. La condition est dans la requête, donc SQLite tranche : ou bien la
    /// ligne est encore `queued` et elle part, ou bien elle ne l'est plus et rien n'est écrit.
    ///
    /// Même raisonnement que la clause de [`Store::deliverable`] : la règle qui protège un
    /// message vit dans le `WHERE`, pas dans la boucle appelante.
    ///
    /// ## Une tentative déjà comptée n'empêche pas d'annuler
    ///
    /// `queued` avec `attempts > 0` est une ligne qu'un refus passager a fait reculer. Le
    /// serveur a refusé **avant** le corps — sinon l'état serait `committing` — donc il n'a
    /// rien pris, donc il n'y a rien à annuler chez lui. Refuser ici obligerait l'utilisateur à
    /// attendre l'épuisement des six tentatives pour se débarrasser d'un message qu'il ne veut
    /// plus envoyer.
    ///
    /// ## Le blob n'est pas supprimé ici
    ///
    /// Pour la même raison que [`Store::forget_outgoing`] : deux envois du même contenu
    /// partagent leur blob, par construction. `Store::orphan_blobs` le compte.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Sqlite`]. Rend `false` — sans rien écrire — si la ligne n'existe pas ou
    /// n'est plus `queued`.
    pub fn cancel_outgoing(&self, id: OutboxId) -> Result<bool> {
        let removed = self.durably(|conn| {
            let changed = conn.execute(
                "DELETE FROM outbox WHERE id = ?1 AND state = ?2",
                params![id.0, SendState::Queued.as_str()],
            )?;
            Ok(changed == 1)
        })?;
        if removed {
            tracing::info!(job = id.0, "envoi annulé avant tout départ");
        } else {
            tracing::warn!(
                job = id.0,
                "annulation refusée : la ligne n'est plus en attente"
            );
        }
        Ok(removed)
    }

    /// Dit pourquoi une ligne **en attente** ne part pas, et la met de côté jusqu'à `retry_after`.
    ///
    /// ## Pourquoi, depuis le 2026-10-19
    ///
    /// Quand le facteur ne pouvait même pas tenter la remise — compte sans serveur d'envoi,
    /// secret absent, jeton OAuth2 mort —, il le disait à son journal et à personne d'autre. La
    /// ligne restait `queued`, sans phrase, et la coquille affichait « EN ATTENTE » pour
    /// toujours, après avoir promis « il partira dans 10 s ». Et comme la file se lit par lots de
    /// dix, dix lignes ainsi bloquées suffisaient à ce que plus rien ne parte, d'aucun compte.
    ///
    /// La phrase rejoint donc la ligne, et `retry_after` la sort des lots pour un temps : les
    /// lignes saines passent devant, et un jeton mort n'est pas redemandé au fournisseur toutes
    /// les trente secondes.
    ///
    /// ## Ce qu'elle ne touche pas
    ///
    /// Ni `state`, ni `attempts` : rien n'a été tenté, donc rien n'est compté. Et seulement une
    /// ligne `queued`, la condition dans le `WHERE` — la règle de [`Store::cancel_outgoing`].
    /// Une ligne prise entre-temps, `committing` surtout, n'est pas touchée.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Sqlite`]. Rend `false` — sans rien écrire — si la ligne n'existe pas ou
    /// n'est plus en attente.
    pub fn hold_queued(&self, id: OutboxId, note: &str, retry_after: i64) -> Result<bool> {
        self.durably(|conn| {
            let changed = conn.execute(
                "UPDATE outbox SET last_error = ?2, retry_after = ?3
                  WHERE id = ?1 AND state = ?4",
                params![id.0, note, retry_after, SendState::Queued.as_str()],
            )?;
            Ok(changed == 1)
        })
    }

    /// Les messages que la boucle de remise peut prendre, du plus ancien au plus récent.
    ///
    /// ## `committing` n'en fait jamais partie
    ///
    /// C'est la règle du critère 2, et elle est dans la clause `WHERE` plutôt que dans la
    /// boucle appelante : un état douteux ne doit pas pouvoir être remis par oubli d'un
    /// filtre. [`SendState::is_deliverable`] dit la même chose côté Rust, et les deux doivent
    /// rester d'accord — d'où le test `the_sql_filter_and_the_rust_predicate_agree`.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Sqlite`] si la lecture échoue, [`crate::Error::UnknownSendState`] si une
    /// ligne porte un état que ce binaire ne connaît pas.
    pub fn deliverable(&self, now: i64, limit: u32) -> Result<Vec<Outgoing>> {
        let mut statement = self.conn.prepare_cached(
            "SELECT id, account_id, blob_hash, sender, recipients, state, attempts,
                    size,
                    queued_at, tried_at, last_error, retry_after, resendable
               FROM outbox
              WHERE state IN ('queued', 'sending')
                AND (retry_after IS NULL OR retry_after <= ?1)
              ORDER BY id
              LIMIT ?2",
        )?;
        let rows = statement.query_map(params![now, limit], row_to_outgoing)?;
        collect(rows)
    }

    /// Les messages dont personne ne sait s'ils sont partis.
    ///
    /// C'est ce que l'interface doit montrer, et ce que la boucle de remise doit ignorer. Deux
    /// lecteurs, une seule vérité.
    ///
    /// La ligne qu'un remettant vivant est en train de remettre n'en fait pas partie : elle est
    /// `committing` pendant tout le transfert de son corps, et quelqu'un sait où elle en est —
    /// voir [`Store::in_flight`]. Si cette question échoue, la ligne est gardée : on penche du
    /// côté du doute.
    ///
    /// # Errors
    ///
    /// Comme [`Store::deliverable`].
    pub fn doubtful(&self) -> Result<Vec<Outgoing>> {
        let mut statement = self.conn.prepare_cached(
            "SELECT id, account_id, blob_hash, sender, recipients, state, attempts,
                    size,
                    queued_at, tried_at, last_error, retry_after, resendable
               FROM outbox
              WHERE state = 'committing'
              ORDER BY id",
        )?;
        let rows = statement.query_map([], row_to_outgoing)?;
        let mut lines = collect(rows)?;
        if let Some(moving) = self.in_flight_or_none() {
            lines.retain(|line| line.id != moving);
        }
        Ok(lines)
    }

    /// [`Store::in_flight`], une erreur valant « aucune » — donc le doute. Pour les appelants
    /// qui affichent ou refusent, jamais pour ceux qui remettent.
    pub fn in_flight_or_none(&self) -> Option<OutboxId> {
        self.in_flight().unwrap_or_else(|source| {
            tracing::warn!(%source, "remise en cours illisible : la ligne est montrée douteuse");
            None
        })
    }

    /// Une ligne de la file, par identifiant.
    ///
    /// # Errors
    ///
    /// Comme [`Store::deliverable`].
    pub fn outgoing(&self, id: OutboxId) -> Result<Option<Outgoing>> {
        let mut statement = self.conn.prepare_cached(
            "SELECT id, account_id, blob_hash, sender, recipients, state, attempts,
                    size,
                    queued_at, tried_at, last_error, retry_after, resendable
               FROM outbox
              WHERE id = ?1",
        )?;
        statement
            .query_row(params![id.0], row_to_outgoing)
            .optional()?
            .transpose()
    }

    /// Toute la file, du plus ancien au plus récent. Pour l'interface et pour `mail doctor`.
    ///
    /// # Errors
    ///
    /// Comme [`Store::deliverable`].
    pub fn outbox(&self) -> Result<Vec<Outgoing>> {
        let mut statement = self.conn.prepare_cached(
            "SELECT id, account_id, blob_hash, sender, recipients, state, attempts,
                    size,
                    queued_at, tried_at, last_error, retry_after, resendable
               FROM outbox
              ORDER BY id",
        )?;
        let rows = statement.query_map([], row_to_outgoing)?;
        collect(rows)
    }

    /// Exécute une écriture et la rend durable avant de rendre la main.
    ///
    /// `synchronous` est remonté à `FULL` le temps de la transaction, puis remis à `NORMAL`.
    /// Le pragma est **par connexion**, pas par transaction : il n'y a pas d'autre façon de
    /// demander la durabilité à SQLite pour une écriture donnée.
    ///
    /// Le retour à `NORMAL` a lieu même si l'écriture échoue. Le laisser à `FULL` ne serait pas
    /// faux, seulement lent : un import ultérieur sur la même connexion paierait un `fsync` par
    /// transaction.
    fn durably<T>(&self, write: impl FnOnce(&rusqlite::Connection) -> Result<T>) -> Result<T> {
        self.conn.pragma_update(None, "synchronous", "FULL")?;
        let outcome = (|| {
            let tx = self.conn.unchecked_transaction()?;
            let value = write(&tx)?;
            tx.commit()?;
            Ok(value)
        })();
        // Le résultat de la remise en place est ignoré exprès : une erreur ici n'annule pas
        // l'écriture, et la masquer derrière le succès de l'écriture serait mentir sur ce qui
        // s'est passé. Le pire cas est une connexion restée en `FULL`, donc lente.
        let _ = self.conn.pragma_update(None, "synchronous", "NORMAL");
        outcome
    }
}

/// Le droit de remettre les messages d'un store : **un seul remettant à la fois**, tous
/// processus confondus.
///
/// ## Pourquoi, depuis le 2026-10-19
///
/// La remise lisait sa ligne une fois, puis écrivait `sending` sans regarder ce qu'elle était
/// devenue. Deux défauts en sortaient, trouvés par une relecture et vérifiés par deux
/// contradicteurs chacun :
///
/// - **un message annulé partait quand même.** Le facteur lit dix lignes d'un coup ; pendant
///   qu'il remet la première, l'utilisateur annule la deuxième — l'annulation réussit, la
///   coquille dit « le message ne partira pas » — et le facteur l'envoie avec ce qu'il en avait
///   lu ;
/// - **deux remettants envoyaient deux fois.** La coquille ouverte deux fois, ou la coquille et
///   `mail send` sur le même store : chacun prenait la même ligne, ou reprenait comme
///   « interrompue avant le corps » une ligne `sending` que l'autre était en train d'envoyer.
///
/// ## Ce que le verrou rend vrai
///
/// Sous lui, une ligne `sending` est **vraiment** celle d'un remettant mort avant le corps — le
/// système rend le verrou à la mort du processus — donc la reprendre reste sans risque, comme
/// le dit `SendState::is_deliverable`. Sans lui, c'était faux dès qu'il y avait deux
/// remettants. Et c'est sous lui que la remise relit sa ligne : ce qui a été annulé, remis ou
/// reporté entre-temps n'est plus pris.
///
/// ## Pourquoi un fichier à côté
///
/// La raison de `vectors::VectorLock` : sous Windows un verrou de fichier est obligatoire, et
/// verrouiller la base empêcherait de la lire. `outbox.lock` ne sert qu'à ça et ne se supprime
/// jamais — supprimer un fichier verrouillé réussit sous Windows, et le suivant en créerait un
/// neuf que personne ne tient.
#[derive(Debug)]
pub struct DeliveryLock {
    _file: std::fs::File,
    /// `outbox.sending` : la ligne que ce remettant remet. Voir [`Store::in_flight`].
    marker: camino::Utf8PathBuf,
}

/// Le fichier du verrou de remise, à la racine du store.
const DELIVERY_LOCK: &str = "outbox.lock";

/// Le fichier qui nomme la ligne en cours de remise, à la racine du store.
const IN_FLIGHT: &str = "outbox.sending";

impl DeliveryLock {
    /// Dit quelle ligne ce remettant est en train de remettre, jusqu'à ce qu'il lâche le
    /// verrou. Voir [`Store::in_flight`].
    ///
    /// Une écriture ratée n'arrête pas la remise : elle ne sert qu'à l'affichage. D'ordinaire la
    /// ligne est alors montrée douteuse pendant son transfert — ce qu'elle était avant le
    /// 2026-10-19. Le cas qui échappe : si l'effacement à la prise **et** l'annonce ratent tous
    /// deux, le nom d'un remettant mort survit sous un verrou vivant, et la ligne du mort passe
    /// pour en cours le temps de cette remise. Le journal le dit, en avertissement.
    pub fn announce(&self, id: OutboxId) {
        self.write_marker(&id.0.to_string());
    }

    fn write_marker(&self, text: &str) {
        if let Err(source) = std::fs::write(self.marker.as_std_path(), text) {
            tracing::warn!(
                %source,
                path = %self.marker,
                "ligne en cours de remise non signalée : elle sera montrée douteuse"
            );
        }
    }
}

impl Drop for DeliveryLock {
    /// Le fichier est vidé **avant** que le verrou ne soit rendu — les champs tombent après ce
    /// corps : aucun instant où le verrou passerait à un autre avec ce nom encore écrit.
    fn drop(&mut self) {
        self.write_marker("");
    }
}

/// La règle de [`Store::in_flight`], séparée des fichiers pour que sa course se teste : le nom,
/// le verrou, puis le nom encore — les deux lectures doivent s'accorder.
fn named_while_held(
    mut named: impl FnMut() -> Result<Option<OutboxId>>,
    held: impl FnOnce() -> Result<bool>,
) -> Result<Option<OutboxId>> {
    let Some(first) = named()? else {
        return Ok(None);
    };
    // Libre : personne ne remet, et le nom est le reste d'un mort.
    if !held()? {
        return Ok(None);
    }
    Ok((named()? == Some(first)).then_some(first))
}

/// La ligne que nomme `outbox.sending` : `None` s'il est absent, vide ou illisible comme
/// nombre — un fichier coupé au milieu d'une écriture se lit vide.
fn read_marker(path: &camino::Utf8Path) -> Result<Option<OutboxId>> {
    match std::fs::read_to_string(path.as_std_path()) {
        Ok(text) => Ok(text.trim().parse().ok().map(OutboxId)),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(crate::Error::Io {
            path: path.to_owned(),
            source,
        }),
    }
}

/// Ce que l'utilisateur décide d'un message douteux.
///
/// ## Pourquoi ces deux-là et pas une seule
///
/// Le protocole ne peut pas lever le doute ; l'utilisateur, lui, peut aller regarder ses
/// messages envoyés chez son fournisseur. Il revient alors avec l'une de deux réponses, et elles
/// demandent l'inverse l'une de l'autre :
///
/// - **il n'est pas arrivé** → le remettre en file. Sans cette issue, un message perdu le reste,
///   et l'utilisateur doit le réécrire ;
/// - **il est arrivé** → l'accepter comme envoyé. Sans cette issue, l'avertissement reste
///   affiché pour toujours, et un avertissement qu'on ne peut pas faire disparaître finit par
///   ne plus être lu — y compris le jour où il compte.
///
/// La troisième réponse — « je ne sais pas » — est l'état actuel : ne rien faire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Le destinataire ne l'a pas reçu : le remettre en file.
    Resend,
    /// Le destinataire l'a reçu : le marquer envoyé.
    Accept,
}

impl Decision {
    /// L'étiquette reçue d'un client.
    ///
    /// # Errors
    ///
    /// [`crate::Error::UnknownSendState`] sur une valeur inconnue. Pas de repli : un repli
    /// choisirait entre « renvoyer » et « accepter » à la place de l'utilisateur, et les deux
    /// sont irréversibles dans un sens différent.
    pub fn parse(label: &str) -> crate::Result<Self> {
        match label {
            "resend" => Ok(Self::Resend),
            "accept" => Ok(Self::Accept),
            other => Err(crate::Error::UnknownSendState {
                found: other.to_owned(),
            }),
        }
    }
}

impl Store {
    /// Tranche le doute sur un message, **sur décision de l'utilisateur**.
    ///
    /// ## C'est la seule sortie de `committing`, et elle ne s'automatise pas
    ///
    /// Rien dans le programme n'appelle cette fonction tout seul : ni le facteur, ni
    /// `mail send --flush`, ni un redémarrage. C'est le critère 2 de `docs/PHASE-3.md` — un
    /// message peut-être parti ne se remet pas sans qu'un humain l'ait demandé.
    ///
    /// ## Elle refuse tout état qui n'est pas `committing`
    ///
    /// Et le refus n'est pas de la prudence : sans lui, cette fonction serait un contournement
    /// général de la file. Un appelant qui passerait un identifiant quelconque avec
    /// [`Decision::Resend`] renverrait un message déjà envoyé — exactement le doublon que tout
    /// le reste du chemin existe pour empêcher.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Sqlite`] si l'écriture échoue. Rend `false` — et n'écrit rien — si la
    /// ligne n'existe pas, n'est pas douteuse, ou est en train de partir.
    ///
    /// ## Pas d'horodatage de la décision
    ///
    /// `tried_at` reste celui de la tentative qui a échoué, et c'est exact : une décision n'est
    /// pas une tentative. Y écrire l'instant de la décision ferait croire à un envoi qui n'a pas
    /// eu lieu — sur un `Accept`, il n'y en a précisément pas eu.
    pub fn resolve_doubt(&self, id: OutboxId, decision: Decision) -> Result<bool> {
        let Some(line) = self.outgoing(id)? else {
            return Ok(false);
        };
        if !line.state.is_doubtful() {
            tracing::warn!(
                job = id.0,
                state = line.state.as_str(),
                "décision refusée : ce message n'est pas douteux"
            );
            return Ok(false);
        }
        // **Une ligne en cours de remise n'est pas douteuse** : son remettant est vivant, et il
        // saura s'il est parti. Trancher à sa place, sur un bouton resté affiché d'un écran
        // ancien, serait parier là où il suffit d'attendre. Voir [`Store::in_flight`].
        if self.in_flight_or_none() == Some(id) {
            tracing::warn!(
                job = id.0,
                "décision refusée : ce message est en train de partir"
            );
            return Ok(false);
        }

        let (state, note) = match decision {
            // `retry_after` est remis à NULL : la décision vient d'être prise, l'attente n'a
            // plus de sens. Le compteur de tentatives, lui, est conservé — c'est l'historique.
            Decision::Resend => (
                SendState::Queued,
                "remis en file sur décision de l'utilisateur",
            ),
            Decision::Accept => (
                SendState::Sent,
                "marqué envoyé sur décision de l'utilisateur",
            ),
        };
        self.durably(|conn| {
            conn.execute(
                "UPDATE outbox
                    SET state = ?2, last_error = ?3, retry_after = NULL
                  WHERE id = ?1",
                params![id.0, state.as_str(), note],
            )?;
            Ok(())
        })?;
        // Journalisé en `info` : c'est une décision humaine sur un message, et savoir qu'elle a
        // été prise vaut mieux que de la déduire d'un état.
        tracing::info!(job = id.0, decision = ?decision, "doute tranché");
        Ok(true)
    }
}

/// Une ligne SQL vers un [`Outgoing`].
///
/// L'état est relu par [`SendState::parse`], donc une valeur inconnue est une erreur et non un
/// repli. Le `Result` imbriqué sort du fait que `query_map` ne sait rendre qu'une
/// `rusqlite::Error` : l'erreur de domaine remonte dans la valeur.
fn row_to_outgoing(row: &rusqlite::Row<'_>) -> rusqlite::Result<Result<Outgoing>> {
    let raw: Vec<u8> = row.get(2)?;
    let label: String = row.get(5)?;
    let recipients: String = row.get(4)?;
    let id = OutboxId(row.get(0)?);
    let account = AccountId(row.get(1)?);
    let sender: String = row.get(3)?;
    let attempts: i64 = row.get(6)?;
    // **Les rangs suivent l'ordre du `SELECT`, pas celui de la table.** La colonne `size` a été
    // insérée au milieu de la liste par la migration v7 : tout ce qui suit a décalé d'un rang.
    // Les quatre requêtes de ce module partagent la même liste de colonnes, mot pour mot, et
    // c'est ce qui rend cette fonction utilisable par les quatre.
    let size: i64 = row.get(7)?;
    let queued_at: i64 = row.get(8)?;
    let tried_at: Option<i64> = row.get(9)?;
    let last_error: Option<String> = row.get(10)?;
    let retry_after: Option<i64> = row.get(11)?;
    // Ajoutée **en fin de liste** par la migration v10, précisément pour ne décaler aucun des
    // rangs ci-dessus — la leçon de `size`, insérée au milieu en v7.
    let resendable: bool = row.get(12)?;

    Ok((|| {
        let bytes: [u8; 32] = raw
            .try_into()
            .map_err(|_| crate::Error::CorruptRow { what: "blob_hash" })?;
        Ok(Outgoing {
            id,
            account,
            blob: BlobHash::from_bytes(bytes),
            size: u64::try_from(size).unwrap_or(0),
            sender,
            // `split` et non `lines` : une chaîne vide donnerait une adresse vide avec `split`,
            // et aucune avec `lines` sur une chaîne vraiment vide — mais `filter` tranche pour
            // les deux. Une adresse ne peut pas être vide, et une file sans destinataire est
            // refusée à l'entrée par `compose`.
            recipients: recipients
                .split('\n')
                .filter(|it| !it.is_empty())
                .map(ToOwned::to_owned)
                .collect(),
            state: SendState::parse(&label)?,
            attempts: u32::try_from(attempts).unwrap_or(u32::MAX),
            queued_at,
            tried_at,
            last_error,
            retry_after,
            resendable,
        })
    })())
}

/// Aplatit les deux niveaux d'erreur de `query_map`.
fn collect(
    rows: impl Iterator<Item = rusqlite::Result<Result<Outgoing>>>,
) -> Result<Vec<Outgoing>> {
    let mut out = Vec::new();
    for row in rows {
        out.push(row??);
    }
    Ok(out)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::model::{AccountKind, SendState};

    fn store() -> (tempfile::TempDir, Store, AccountId) {
        let dir = tempfile::tempdir().unwrap();
        let root = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        let store = Store::open(&root).unwrap();
        let account = {
            let writer = store.writer().unwrap();
            let id = writer
                .upsert_account(AccountKind::Imap.as_str(), "compte")
                .unwrap();
            writer.commit().unwrap();
            id
        };
        (dir, store, account)
    }

    fn queued(store: &Store, account: AccountId) -> OutboxId {
        store
            .enqueue(
                account,
                BlobHash::from_bytes([7_u8; 32]),
                "marie@exemple.fr",
                &[
                    "jean@ailleurs.fr".to_owned(),
                    "cache@ailleurs.fr".to_owned(),
                ],
                // La taille du message assemblé. Les tests de ce module ne la lisent pas ;
                // `SIZE` en dépend, et un zéro veut dire « inconnue ».
                42,
                1_000,
                None,
            )
            .unwrap()
    }

    #[test]
    fn a_queued_message_comes_back_whole() {
        let (_dir, store, account) = store();
        let id = queued(&store, account);

        let it = store.outgoing(id).unwrap().expect("la ligne doit exister");
        assert_eq!(it.state, SendState::Queued);
        assert_eq!(it.sender, "marie@exemple.fr");
        assert_eq!(it.recipients, vec!["jean@ailleurs.fr", "cache@ailleurs.fr"]);
        assert_eq!(it.blob.as_bytes(), &[7_u8; 32]);
        assert_eq!(it.attempts, 0);
        assert_eq!(it.queued_at, 1_000);
        assert!(it.tried_at.is_none());
    }

    #[test]
    fn a_doubtful_message_is_never_deliverable() {
        // **La règle du critère 2, au niveau du store.** Un message en `committing` est peut-être
        // parti ; le rendre à la boucle de remise serait un doublon chez le destinataire.
        let (_dir, store, account) = store();
        let id = queued(&store, account);
        store.commit_outgoing(id, SendState::Committing).unwrap();

        assert!(
            store.deliverable(9_999, 10).unwrap().is_empty(),
            "un message douteux a été rendu à la remise"
        );
        let doubtful = store.doubtful().unwrap();
        assert_eq!(doubtful.len(), 1);
        assert_eq!(doubtful[0].id, id);
    }

    #[test]
    fn a_message_interrupted_before_its_body_is_deliverable_again() {
        // Le contrôle négatif du test ci-dessus. `sending` veut dire « l'enveloppe est ouverte,
        // aucun octet du corps n'est parti » : le remettre est sans risque, et **ne pas** le
        // remettre serait une perte.
        let (_dir, store, account) = store();
        let id = queued(&store, account);
        store.commit_outgoing(id, SendState::Sending).unwrap();

        let pending = store.deliverable(9_999, 10).unwrap();
        assert_eq!(
            pending.len(),
            1,
            "un envoi interrompu avant le corps est perdu"
        );
        assert_eq!(pending[0].id, id);
        assert_eq!(pending[0].state, SendState::Sending);
    }

    #[test]
    fn the_sql_filter_and_the_rust_predicate_agree() {
        // Deux endroits disent quels états sont remettables : la clause `WHERE` de
        // `deliverable` et `SendState::is_deliverable`. S'ils divergent, l'un des deux devient
        // un mensonge — et selon le sens, c'est un doublon ou une perte.
        let (_dir, store, account) = store();
        for state in [
            SendState::Queued,
            SendState::Sending,
            SendState::Committing,
            SendState::Sent,
            SendState::Failed,
        ] {
            let id = queued(&store, account);
            store.commit_outgoing(id, state).unwrap();
            let seen = store
                .deliverable(9_999, 100)
                .unwrap()
                .iter()
                .any(|it| it.id == id);
            assert_eq!(
                seen,
                state.is_deliverable(),
                "{state:?} : le SQL et le prédicat Rust ne disent pas la même chose"
            );
        }
    }

    #[test]
    fn a_retry_deadline_in_the_future_holds_the_message_back() {
        let (_dir, store, account) = store();
        let id = queued(&store, account);
        store
            .record_attempt(
                id,
                SendState::Queued,
                1_100,
                Some("serveur occupé"),
                Some(1_400),
                false,
            )
            .unwrap();

        assert!(store.deliverable(1_399, 10).unwrap().is_empty());
        assert_eq!(store.deliverable(1_400, 10).unwrap().len(), 1);

        let it = store.outgoing(id).unwrap().unwrap();
        assert_eq!(it.attempts, 1);
        assert_eq!(it.tried_at, Some(1_100));
        assert_eq!(it.last_error.as_deref(), Some("serveur occupé"));
    }

    #[test]
    fn the_connection_goes_back_to_normal_after_a_durable_write() {
        // `FULL` laissé en place ne serait pas faux, seulement lent : un import sur la même
        // connexion paierait un `fsync` par transaction. Ce test le voit, la mesure non.
        let (_dir, store, account) = store();
        queued(&store, account);
        let level: i64 = store
            .conn
            .query_row("PRAGMA synchronous", [], |row| row.get(0))
            .unwrap();
        // 1 = NORMAL, 2 = FULL.
        assert_eq!(level, 1, "la connexion est restée en synchronous FULL");
    }

    #[test]
    fn a_claim_takes_a_line_only_in_the_state_it_was_read_in() {
        let (_dir, store, account) = store();
        let id = queued(&store, account);

        assert!(store.claim_outgoing(id, SendState::Queued).unwrap());
        assert_eq!(
            store.outgoing(id).unwrap().unwrap().state,
            SendState::Sending
        );
        // Lue `queued`, elle ne l'est plus : la prise échoue, rien n'est écrit.
        assert!(!store.claim_outgoing(id, SendState::Queued).unwrap());
        // Lue `sending` — un remettant mort avant le corps, sous le verrou : elle se reprend.
        assert!(store.claim_outgoing(id, SendState::Sending).unwrap());

        // **Jamais un `committing`**, ni lu tel quel, ni lu comme `sending` d'avant la frontière.
        store.commit_outgoing(id, SendState::Committing).unwrap();
        assert!(!store.claim_outgoing(id, SendState::Committing).unwrap());
        assert!(!store.claim_outgoing(id, SendState::Sending).unwrap());
        assert_eq!(
            store.outgoing(id).unwrap().unwrap().state,
            SendState::Committing
        );

        // Et une ligne annulée ne se prend pas.
        let gone = queued(&store, account);
        assert!(store.cancel_outgoing(gone).unwrap());
        assert!(!store.claim_outgoing(gone, SendState::Queued).unwrap());
    }

    #[test]
    fn a_waiting_line_says_why_and_steps_aside() {
        let (_dir, store, account) = store();
        let stuck = queued(&store, account);
        let healthy = queued(&store, account);

        assert!(
            store
                .hold_queued(stuck, "jeton expiré : refaire le consentement", 1_300)
                .unwrap()
        );
        let line = store.outgoing(stuck).unwrap().unwrap();
        assert_eq!(line.state, SendState::Queued, "l'état a bougé");
        assert_eq!(
            line.attempts, 0,
            "une tentative a été comptée sans avoir eu lieu"
        );
        assert_eq!(
            line.last_error.as_deref(),
            Some("jeton expiré : refaire le consentement")
        );
        // Mise de côté : la ligne saine passe devant, la bloquée revient à son heure.
        let ready: Vec<OutboxId> = store
            .deliverable(1_200, 10)
            .unwrap()
            .iter()
            .map(|it| it.id)
            .collect();
        assert_eq!(ready, vec![healthy]);
        assert_eq!(store.deliverable(1_300, 10).unwrap().len(), 2);
    }

    #[test]
    fn only_a_waiting_line_can_be_set_aside() {
        // Le contrôle inverse : une ligne prise — surtout douteuse — n'est jamais touchée. Une
        // phrase « attend » sur un message peut-être parti mentirait, et un `retry_after` n'y a
        // aucun sens.
        for state in [
            SendState::Sending,
            SendState::Committing,
            SendState::Sent,
            SendState::Failed,
        ] {
            let (_dir, store, account) = store();
            let id = queued(&store, account);
            store.commit_outgoing(id, state).unwrap();
            assert!(
                !store.hold_queued(id, "attente", 9_999).unwrap(),
                "{state:?}"
            );
            let line = store.outgoing(id).unwrap().unwrap();
            assert_eq!(line.state, state);
            assert!(line.last_error.is_none(), "{state:?} a reçu une phrase");
            assert!(line.retry_after.is_none(), "{state:?} a reçu un recul");
        }
    }

    #[test]
    fn a_cancelled_number_is_never_given_to_another_message() {
        // Un facteur qui tenait ce numéro dans son lot relirait l'autre message et l'enverrait
        // avec le compte du premier ; une annulation sur une liste affichée un instant trop tôt
        // atteindrait le mauvais message. Voir `SCHEMA_V16`.
        let (_dir, store, account) = store();
        queued(&store, account);
        let last = queued(&store, account);
        assert!(store.cancel_outgoing(last).unwrap());
        let next = queued(&store, account);
        assert!(next.0 > last.0, "#{} a été redonné", last.0);
    }

    #[test]
    fn the_frontier_is_crossed_only_from_sending() {
        let (_dir, store, account) = store();
        let id = queued(&store, account);
        assert!(
            !store.cross_frontier(id).unwrap(),
            "une ligne pas encore prise a franchi la frontière"
        );
        assert!(store.claim_outgoing(id, SendState::Queued).unwrap());
        assert!(store.cross_frontier(id).unwrap());
        assert_eq!(
            store.outgoing(id).unwrap().unwrap().state,
            SendState::Committing
        );
        assert!(!store.cross_frontier(id).unwrap());
    }

    #[test]
    fn the_delivery_lock_is_held_by_one_connection_at_a_time() {
        let (dir, store, _account) = store();
        let root = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        let held = store.lock_delivery().unwrap();

        let (taken, was_taken) = std::sync::mpsc::channel();
        let waiter = std::thread::spawn(move || {
            let other = Store::open(&root).unwrap();
            let lock = other.lock_delivery().unwrap();
            taken.send(()).unwrap();
            drop(lock);
        });
        assert!(
            was_taken
                .recv_timeout(std::time::Duration::from_millis(300))
                .is_err(),
            "une seconde connexion a pris le verrou pendant que la première le tenait"
        );
        // Le contrôle inverse : relâché, il passe à l'autre.
        drop(held);
        assert!(
            was_taken
                .recv_timeout(std::time::Duration::from_secs(10))
                .is_ok(),
            "le verrou relâché n'est jamais passé à l'autre connexion"
        );
        waiter.join().unwrap();
    }

    /// Une ligne prise et passée la frontière, comme la remise le fait.
    fn in_transfer(store: &Store, id: OutboxId) {
        assert!(store.claim_outgoing(id, SendState::Queued).unwrap());
        assert!(store.cross_frontier(id).unwrap());
    }

    #[test]
    fn a_line_whose_body_is_being_sent_is_in_flight_not_doubtful() {
        // **Le défaut du 2026-10-19.** Pendant le transfert du corps, la ligne est `committing`
        // et l'interface la montrait douteuse, boutons du doute compris.
        let (dir, store, account) = store();
        let id = queued(&store, account);
        let lock = store.lock_delivery().unwrap();
        in_transfer(&store, id);
        lock.announce(id);

        assert_eq!(store.in_flight().unwrap(), Some(id));
        assert!(
            store.doubtful().unwrap().is_empty(),
            "une ligne en cours de transfert est comptée douteuse"
        );
        assert!(
            !store.resolve_doubt(id, Decision::Resend).unwrap(),
            "un doute a été tranché sur une ligne en train de partir"
        );
        assert_eq!(
            store.outgoing(id).unwrap().unwrap().state,
            SendState::Committing
        );

        // Le verrou lâché proprement ne laisse pas de nom derrière lui : sans ça, la seconde
        // lecture de `in_flight` ne protégerait de rien.
        drop(lock);
        assert_eq!(
            std::fs::read_to_string(dir.path().join(IN_FLIGHT)).unwrap(),
            ""
        );

        // Le contrôle inverse, celui du critère 2 : un remettant **mort** ne vide pas le nom —
        // on le remet comme il l'aurait laissé —, mais le système rend son verrou. La ligne
        // devient douteuse sans que personne n'y touche.
        std::fs::write(dir.path().join(IN_FLIGHT), id.0.to_string()).unwrap();
        assert_eq!(store.in_flight().unwrap(), None);
        assert_eq!(store.doubtful().unwrap().len(), 1);
    }

    #[test]
    fn a_name_without_the_lock_is_not_in_flight() {
        // Le reste d'un remettant tué : il ne vide pas `outbox.sending`, mais le système rend
        // son verrou. C'est le verrou qui décide, pas le fichier.
        let (dir, store, account) = store();
        let id = queued(&store, account);
        in_transfer(&store, id);
        // Le fichier de verrou existe, comme après toute remise passée.
        drop(store.lock_delivery().unwrap());
        std::fs::write(dir.path().join(IN_FLIGHT), id.0.to_string()).unwrap();

        assert_eq!(store.in_flight().unwrap(), None);
        assert_eq!(
            store.doubtful().unwrap().len(),
            1,
            "le nom laissé par un mort a caché un doute"
        );
        assert!(store.resolve_doubt(id, Decision::Accept).unwrap());
    }

    #[test]
    fn another_reader_checking_the_lock_is_not_taken_for_a_deliverer() {
        // **Trouvé par la relecture du 2026-10-19.** Le contrôle prenait le verrou en exclusif :
        // un second lecteur au même instant — la coquille et `mail doctor` — voyait le premier
        // comme un remettant, et le nom d'un mort passait pour en cours.
        let (dir, store, account) = store();
        let id = queued(&store, account);
        in_transfer(&store, id);
        drop(store.lock_delivery().unwrap());
        std::fs::write(dir.path().join(IN_FLIGHT), id.0.to_string()).unwrap();

        // L'autre lecteur, au milieu de son contrôle.
        let other = std::fs::File::open(dir.path().join(DELIVERY_LOCK)).unwrap();
        other.lock_shared().unwrap();
        assert_eq!(
            store.in_flight().unwrap(),
            None,
            "un lecteur a été pris pour un remettant"
        );
        assert_eq!(store.doubtful().unwrap().len(), 1);
        other.unlock().unwrap();
    }

    #[test]
    fn the_lock_held_for_another_line_does_not_hide_an_older_doubt() {
        // Un vieux doute, et le facteur qui remet une autre ligne : le verrou est tenu, mais
        // pas pour celle-là.
        let (_dir, store, account) = store();
        let old = queued(&store, account);
        in_transfer(&store, old);
        let new = queued(&store, account);
        let lock = store.lock_delivery().unwrap();
        in_transfer(&store, new);
        lock.announce(new);

        let doubtful: Vec<_> = store
            .doubtful()
            .unwrap()
            .into_iter()
            .map(|line| line.id)
            .collect();
        assert_eq!(doubtful, vec![old]);
        drop(lock);
    }

    #[test]
    fn a_line_finished_while_the_next_one_takes_the_lock_is_not_in_flight() {
        // La course de la seconde lecture : le nom lu est celui d'une remise qui finit, et le
        // verrou, contrôlé juste après, est déjà celui de la suivante. Sans la seconde lecture,
        // la première ligne — tombée en doute, peut-être — passerait pour en cours.
        let (finished, next) = (OutboxId(1), OutboxId(2));
        let mut reads = [Some(finished), Some(next)].into_iter();
        let seen = named_while_held(|| Ok(reads.next().flatten()), || Ok(true)).unwrap();
        assert_eq!(seen, None);

        // Les contrôles : un nom stable sous un verrou tenu est en cours ; sans verrou, non.
        let seen = named_while_held(|| Ok(Some(next)), || Ok(true)).unwrap();
        assert_eq!(seen, Some(next));
        let seen = named_while_held(|| Ok(Some(next)), || Ok(false)).unwrap();
        assert_eq!(seen, None);
    }

    #[test]
    fn a_new_holder_wipes_the_name_a_dead_one_left() {
        // Sans l'effacement à la prise, la ligne du mort passerait pour en cours pendant toute
        // la remise suivante — le verrou serait tenu, et le nom toujours écrit.
        let (dir, store, account) = store();
        let id = queued(&store, account);
        in_transfer(&store, id);
        std::fs::write(dir.path().join(IN_FLIGHT), id.0.to_string()).unwrap();

        let lock = store.lock_delivery().unwrap();
        assert_eq!(store.in_flight().unwrap(), None);
        assert_eq!(store.doubtful().unwrap().len(), 1);
        drop(lock);
    }

    #[test]
    fn an_unknown_state_is_refused_rather_than_treated_as_queued() {
        // **Le refus le plus important du module.** Un état illisible traité comme « à
        // envoyer » renverrait un message peut-être déjà parti.
        let (_dir, store, account) = store();
        let id = queued(&store, account);
        store
            .conn
            .execute(
                "UPDATE outbox SET state = 'peut-etre' WHERE id = ?1",
                params![id.0],
            )
            .unwrap();

        let outcome = store.outgoing(id);
        assert!(
            matches!(outcome, Err(crate::Error::UnknownSendState { .. })),
            "un état inconnu n'a pas été refusé"
        );
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod decision_tests {
    use super::*;
    use crate::model::AccountKind;

    fn fixture() -> (tempfile::TempDir, Store, OutboxId) {
        let dir = tempfile::tempdir().unwrap();
        let root = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        let store = Store::open(&root).unwrap();
        let account = {
            let writer = store.writer().unwrap();
            let id = writer
                .upsert_account(AccountKind::Imap.as_str(), "compte")
                .unwrap();
            writer.commit().unwrap();
            id
        };
        let id = store
            .enqueue(
                account,
                BlobHash::from_bytes([7_u8; 32]),
                "marie@exemple.fr",
                &["jean@ailleurs.fr".to_owned()],
                // La taille : les tests de la file ne la lisent pas, mais `SIZE` en dépend.
                42,
                1_000,
                None,
            )
            .unwrap();
        (dir, store, id)
    }

    #[test]
    fn resending_a_doubtful_message_puts_it_back_in_the_queue() {
        // La sortie qui évite de perdre un message : l'utilisateur a vérifié, il n'est pas
        // arrivé. Sans cette issue, il faut le réécrire.
        let (_dir, store, id) = fixture();
        store.commit_outgoing(id, SendState::Committing).unwrap();

        assert!(store.resolve_doubt(id, Decision::Resend).unwrap());
        let after = store.outgoing(id).unwrap().unwrap();
        assert_eq!(after.state, SendState::Queued);
        assert!(after.retry_after.is_none(), "un recul survit à la décision");
        assert_eq!(store.deliverable(2_000, 10).unwrap().len(), 1);
        assert!(store.doubtful().unwrap().is_empty());
    }

    #[test]
    fn accepting_a_doubtful_message_closes_it_without_sending_anything() {
        // L'autre sortie : l'utilisateur a vérifié, il est arrivé. Sans elle, l'avertissement
        // reste affiché pour toujours — et un avertissement indélébile finit par ne plus être lu.
        let (_dir, store, id) = fixture();
        store.commit_outgoing(id, SendState::Committing).unwrap();

        assert!(store.resolve_doubt(id, Decision::Accept).unwrap());
        let after = store.outgoing(id).unwrap().unwrap();
        assert_eq!(after.state, SendState::Sent);
        assert!(
            store.deliverable(9_999_999, 10).unwrap().is_empty(),
            "un message accepté est reparti"
        );
        assert!(store.doubtful().unwrap().is_empty());
    }

    #[test]
    fn a_state_that_is_not_doubtful_is_refused_and_nothing_is_written() {
        // **Le refus qui empêche cette fonction d'être un contournement de la file.** Sans lui,
        // un `Resend` sur un identifiant quelconque renverrait un message déjà envoyé, ce que
        // tout le reste du chemin existe pour empêcher.
        let (_dir, store, id) = fixture();
        for state in [
            SendState::Queued,
            SendState::Sending,
            SendState::Sent,
            SendState::Failed,
        ] {
            store.commit_outgoing(id, state).unwrap();
            for decision in [Decision::Resend, Decision::Accept] {
                assert!(
                    !store.resolve_doubt(id, decision).unwrap(),
                    "{state:?} + {decision:?} a été accepté"
                );
                assert_eq!(
                    store.outgoing(id).unwrap().unwrap().state,
                    state,
                    "l'état a bougé alors que la décision était refusée"
                );
            }
        }
    }

    #[test]
    fn retrying_a_failed_message_puts_it_back_with_a_fresh_credit() {
        // Le geste que la phrase du critère 8 demande — « renvoyez le message plus tard » — et
        // qui n'existait pas : la seule autre sortie de `failed` était `forget_outgoing`, qui
        // jette le message.
        let (_dir, store, id) = fixture();
        store
            .record_attempt(id, SendState::Failed, 1_200, Some("occupé"), None, true)
            .unwrap();
        let dead = store.outgoing(id).unwrap().unwrap();
        assert_eq!(dead.attempts, 1);
        assert!(dead.resendable);

        assert!(store.retry_outgoing(id).unwrap());
        let back = store.outgoing(id).unwrap().unwrap();
        assert_eq!(back.state, SendState::Queued);
        // **Le crédit repart de zéro**, sinon une ligne épuisée échouerait au premier refus
        // passager du renvoi. Voir la documentation de `retry_outgoing`.
        assert_eq!(back.attempts, 0);
        assert!(back.retry_after.is_none(), "un recul survit au renvoi");
        // Et le bit s'éteint : il ne décrit plus rien, la ligne n'a pas encore échoué.
        assert!(!back.resendable);
        assert_eq!(store.deliverable(1_300, 10).unwrap().len(), 1);
    }

    #[test]
    fn only_a_failed_message_can_be_retried() {
        // Le même refus que `resolve_doubt`, pour la même raison : sans lui, cette fonction
        // renverrait un message déjà `sent`, ou remettrait en file un `committing` — dont le
        // serveur a peut-être la copie. Le doublon que le critère 2 existe pour empêcher.
        let (_dir, store, id) = fixture();
        for state in [
            SendState::Queued,
            SendState::Sending,
            SendState::Committing,
            SendState::Sent,
        ] {
            store.commit_outgoing(id, state).unwrap();
            assert!(
                !store.retry_outgoing(id).unwrap(),
                "{state:?} a été accepté au renvoi"
            );
            assert_eq!(
                store.outgoing(id).unwrap().unwrap().state,
                state,
                "l'état a bougé alors que le renvoi était refusé"
            );
        }
    }

    #[test]
    fn retrying_a_line_that_does_not_exist_is_not_a_panic() {
        let (_dir, store, _id) = fixture();
        assert!(!store.retry_outgoing(OutboxId(9_999)).unwrap());
    }

    #[test]
    fn a_queued_line_is_never_resendable() {
        // Le bit n'a de sens que sur une ligne finie. Une ligne neuve le porterait à faux, et
        // un client qui offrirait « Renvoyer » sur un message encore en file ferait renvoyer
        // ce qui n'est pas encore parti.
        let (_dir, store, id) = fixture();
        assert!(!store.outgoing(id).unwrap().unwrap().resendable);
    }

    #[test]
    fn an_unknown_id_is_refused_rather_than_created() {
        let (_dir, store, _id) = fixture();
        assert!(
            !store
                .resolve_doubt(OutboxId(4_242), Decision::Resend)
                .unwrap()
        );
    }

    #[test]
    fn the_decision_is_readable_by_the_user_afterwards() {
        // Le texte remplace l'erreur d'origine : ce qui compte désormais est que la décision a
        // été prise, pas la coupure qui l'a provoquée — critère 8.
        let (_dir, store, id) = fixture();
        store.commit_outgoing(id, SendState::Committing).unwrap();
        store.resolve_doubt(id, Decision::Accept).unwrap();

        let note = store.outgoing(id).unwrap().unwrap().last_error.unwrap();
        assert!(note.contains("utilisateur"), "{note}");
    }

    #[test]
    fn a_label_that_is_neither_is_refused_rather_than_guessed() {
        // Les deux décisions sont irréversibles dans un sens différent : un repli choisirait à
        // la place de l'utilisateur.
        assert_eq!(Decision::parse("resend").unwrap(), Decision::Resend);
        assert_eq!(Decision::parse("accept").unwrap(), Decision::Accept);
        assert!(Decision::parse("peut-etre").is_err());
        assert!(Decision::parse("").is_err());
        assert!(
            Decision::parse("RESEND").is_err(),
            "la casse n'est pas un repli"
        );
    }

    // ------------------------------------------------------------------------------------
    // L'annulation, et la fenêtre de rétractation
    // ------------------------------------------------------------------------------------

    #[test]
    fn a_line_that_has_not_started_can_be_cancelled() {
        let (_dir, store, id) = fixture();
        assert!(store.cancel_outgoing(id).unwrap());
        assert!(
            store.outgoing(id).unwrap().is_none(),
            "la ligne annulée doit disparaître, sinon elle repartirait"
        );
    }

    #[test]
    fn cancelling_refuses_every_state_but_queued() {
        // **Le test qui porte le risque.** `sending` et `committing` sont les états où personne
        // ne sait ce que le serveur a vu : les retirer perdrait un message, ou effacerait la
        // trace d'un message peut-être parti — l'information même que le critère 2 conserve.
        // `sent` et `failed` ne sortent que par `forget_outgoing`, qui est l'autre bout.
        for state in [
            SendState::Sending,
            SendState::Committing,
            SendState::Sent,
            SendState::Failed,
        ] {
            let (_dir, store, id) = fixture();
            store.commit_outgoing(id, state).unwrap();

            assert!(
                !store.cancel_outgoing(id).unwrap(),
                "l'état « {} » ne doit pas s'annuler",
                state.as_str()
            );
            assert!(
                store.outgoing(id).unwrap().is_some(),
                "la ligne « {} » a été retirée alors que l'annulation était refusée",
                state.as_str()
            );
        }
    }

    #[test]
    fn cancelling_a_line_that_does_not_exist_is_not_a_panic() {
        let (_dir, store, _id) = fixture();
        assert!(!store.cancel_outgoing(OutboxId(9_999)).unwrap());
    }

    #[test]
    fn a_transient_refusal_does_not_lock_the_message_in() {
        // `queued` avec des tentatives est une ligne qu'un refus passager a fait reculer : le
        // serveur a refusé **avant** le corps, donc il n'a rien pris. Refuser l'annulation ici
        // obligerait à attendre l'épuisement des six tentatives pour se débarrasser d'un
        // message dont on ne veut plus.
        let (_dir, store, id) = fixture();
        store
            .record_attempt(
                id,
                SendState::Queued,
                2_000,
                Some("quota"),
                Some(3_000),
                false,
            )
            .unwrap();

        assert_eq!(store.outgoing(id).unwrap().unwrap().attempts, 1);
        assert!(store.cancel_outgoing(id).unwrap());
    }

    #[test]
    fn a_held_line_is_not_deliverable_before_its_time() {
        // La fenêtre de rétractation passe par la porte qui existe déjà — `retry_after` — et
        // c'est ce qui la rend sûre : elle ne touche pas `state`.
        let dir = tempfile::tempdir().unwrap();
        let root = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        let store = Store::open(&root).unwrap();
        let account = {
            let writer = store.writer().unwrap();
            let id = writer
                .upsert_account(AccountKind::Imap.as_str(), "compte")
                .unwrap();
            writer.commit().unwrap();
            id
        };
        let id = store
            .enqueue(
                account,
                BlobHash::from_bytes([9_u8; 32]),
                "marie@exemple.fr",
                &["jean@ailleurs.fr".to_owned()],
                42,
                1_000,
                Some(1_010),
            )
            .unwrap();

        assert!(
            store.deliverable(1_005, 10).unwrap().is_empty(),
            "le facteur ne doit pas prendre une ligne encore tenue"
        );
        // L'état, lui, n'a pas bougé : c'est bien `queued`, donc annulable.
        assert_eq!(
            store.outgoing(id).unwrap().unwrap().state,
            SendState::Queued
        );
        assert!(store.cancel_outgoing(id).unwrap());
    }

    #[test]
    fn a_line_without_a_hold_is_deliverable_at_once() {
        // Le contrôle négatif du précédent : sans maintien, rien ne retarde la remise. Sans
        // lui, un `retry_after` écrit par erreur sur toutes les lignes passerait inaperçu.
        let (_dir, store, id) = fixture();
        let ready = store.deliverable(1_000, 10).unwrap();
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].id, id);
    }
}
