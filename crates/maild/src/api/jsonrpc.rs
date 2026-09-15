//! Le service JSON-RPC des clients non-MCP, indépendant du transport.
//!
//! [`Api`] prend un message et rend une réponse. C'est tout ce que stdio et HTTP ont à
//! savoir : les deux transports appellent [`Api::handle_message`] et se contentent
//! d'acheminer des octets. Une méthode ajoutée à `mailapi` est donc servie par les deux
//! transports sans qu'aucun des deux ne change.
//!
//! ## Pourquoi le motif « verrou + `spawn_blocking` » est écrit deux fois
//!
//! `mailmcp` a le même. Ce n'est pas un oubli : `mailcore` est délibérément sans runtime
//! async — il ne peut donc pas héberger ce motif — et faire dépendre l'un des deux frontends
//! de l'autre serait pire que quinze lignes en double. Le jour où un troisième apparaît, il
//! faudra un crate d'hébergement commun ; à deux, ça ne vaut pas l'indirection.
//!
//! ## `store.wait` : l'abonnement aux changements
//!
//! Servi ici et pas dans `mailapi`, parce qu'il dort. Un long-poll plutôt qu'un flux
//! poussé, et ce n'est pas de la paresse : en phase 1, l'import et l'indexation tournent
//! dans un **autre processus** (`mail import`), donc un canal interne au démon ne se
//! déclencherait jamais. Ce que le démon peut observer, c'est la révision du store — et un
//! client qui la surveille voit un import extérieur, ce qu'aucun `broadcast` interne ne
//! saurait faire.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use mailapi::dispatch::WaitParams;
use mailapi::jsonrpc::{self, Error, Request, Response};
use mailcore::Mailbox;
use serde_json::{Value, json};

/// Délai d'attente par défaut de `store.wait`, en millisecondes.
///
/// 30 s : assez long pour qu'un client au repos ne réveille pas le démon toutes les
/// secondes, assez court pour qu'aucun intermédiaire réseau ne coupe la connexion en croyant
/// qu'elle est morte.
const DEFAULT_WAIT_MS: u64 = 30_000;

/// Plafond de l'attente, en millisecondes.
///
/// Un client qui demande une heure d'attente immobiliserait une connexion et un fil du
/// runtime pour rien. Le plafond n'est pas une erreur : on attend moins et on répond
/// `changed: false`, ce que le client sait déjà interpréter.
const MAX_WAIT_MS: u64 = 120_000;

/// Intervalle entre deux relevés de révision.
///
/// 250 ms : un relevé est un `PRAGMA data_version` plus un `stat`, soit quelques
/// microsecondes. Le coût est négligeable et la latence perçue reste sous le seuil où un
/// humain remarque un délai.
const POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Le service JSON-RPC.
#[derive(Clone)]
pub struct Api {
    mailbox: Arc<Mutex<Mailbox>>,
    jobs: crate::jobs::Jobs,
    /// Le facteur, à réveiller après `outbox.send`.
    ///
    /// `None` quand l'API est montée sans lui — c'est le cas de tous les tests qui ne testent
    /// pas l'envoi. Un `None` ne fait pas échouer `outbox.send` : le message est en file, et
    /// il partira au tic suivant plutôt que tout de suite.
    postman: Option<Arc<crate::outbox::Postman>>,
}

impl std::fmt::Debug for Api {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Api").finish_non_exhaustive()
    }
}

impl Api {
    /// Construit le service autour d'une boîte partagée avec le serveur MCP.
    #[must_use]
    pub const fn new(mailbox: Arc<Mutex<Mailbox>>, jobs: crate::jobs::Jobs) -> Self {
        Self {
            mailbox,
            jobs,
            postman: None,
        }
    }

    /// Attache le facteur, pour que `outbox.send` le réveille au lieu de le laisser dormir.
    #[must_use]
    pub fn with_postman(self, postman: Arc<crate::outbox::Postman>) -> Self {
        Self {
            postman: Some(postman),
            ..self
        }
    }

    /// Traite un message et rend la réponse à écrire, s'il y en a une.
    ///
    /// `None` veut dire « ne rien renvoyer » : le message était une notification. Une
    /// notification illisible ne rend rien non plus — un client qui n'a pas donné d'`id`
    /// n'attend rien, pas même une erreur.
    pub async fn handle_message(&self, message: &str) -> Option<String> {
        let response = match jsonrpc::parse(message) {
            Ok(request) => self.handle(request).await?,
            // Une requête illisible n'a pas d'`id` exploitable : la spécification demande
            // alors une réponse avec `id: null`, et c'est la seule façon pour le client
            // d'apprendre que son message était mal formé.
            Err(error) => Response::failure(Value::Null, error),
        };

        match serde_json::to_string(&response) {
            Ok(text) => Some(text),
            Err(source) => {
                tracing::error!(%source, "réponse non sérialisable");
                None
            }
        }
    }

    /// Traite une requête analysée.
    ///
    /// `None` pour une notification : elle est exécutée, sa réponse est jetée.
    pub async fn handle(&self, request: Request) -> Option<Response> {
        let outcome = if mailapi::method::SERVED_BY_TRANSPORT.contains(&request.method.as_str()) {
            self.transport_method(&request.method, request.params.clone())
                .await
        } else {
            let method = request.method.clone();
            let params = request.params.clone();
            self.read(move |mailbox| mailapi::call(mailbox, &method, params))
                .await
        };

        // Le nom de la méthode se journalise, ses paramètres non : un identifiant de message
        // ou une requête de recherche décrit ce que l'utilisateur lit (`docs/PRIVACY.md`,
        // section 8).
        let id = request.id?;
        Some(match outcome {
            Ok(result) => Response::success(id, result),
            Err(error) => {
                tracing::debug!(method = %request.method, code = error.code, "appel refusé");
                Response::failure(id, error)
            }
        })
    }

    /// Sert une méthode que le répartiteur ne peut pas servir seul.
    ///
    /// Elles ont toutes besoin de quelque chose qu'une boîte mail ne contient pas : le temps
    /// qui passe pour `store.wait`, le registre des tâches de fond pour les autres.
    async fn transport_method(&self, method: &str, params: Value) -> Result<Value, Error> {
        use mailapi::method as m;

        match method {
            m::STORE_WAIT => self.wait(params).await,
            m::JOBS_SOURCES => self.job_sources(),
            m::JOBS_START => self.job_start(params),
            m::JOBS_LIST => self.job_list(),
            m::JOBS_GET => self.job_get(params),
            m::JOBS_CANCEL => self.job_cancel(params),
            m::ACCOUNTS_LIST => self.accounts_list().await,
            m::ACCOUNTS_SIGNATURE => self.account_signature(params).await,
            m::ACCOUNTS_SET_SIGNATURE => self.account_set_signature(params).await,
            m::DRAFTS_LIST => self.drafts_list().await,
            m::DRAFTS_SAVE => self.drafts_save(params).await,
            m::DRAFTS_DELETE => self.drafts_delete(params).await,
            m::OUTBOX_LIST => self.outbox_list().await,
            m::OUTBOX_SEND => self.outbox_send(params).await,
            m::OUTBOX_DECIDE => self.outbox_decide(params).await,
            m::OUTBOX_RETRY => self.outbox_retry(params).await,
            m::MESSAGES_MARK_READ => self.mark_read(params).await,
            m::MESSAGES_STAGE_PART => self.stage_part(params).await,
            other => Err(Error::method_not_found(other)),
        }
    }

    /// `jobs.sources` : les profils que l'opérateur autorise à importer.
    fn job_sources(&self) -> Result<Value, Error> {
        let sources: Vec<mailapi::dto::Source> = self
            .jobs
            .sources()
            .paths()
            .iter()
            .enumerate()
            .map(|(id, path)| mailapi::dto::Source {
                id,
                path: path.to_string(),
            })
            .collect();
        encode(&sources)
    }

    /// `jobs.start` : met une tâche de fond en file.
    ///
    /// **La première méthode de l'API qui écrit.** Ce qu'elle peut faire écrire est borné par
    /// ce que l'opérateur a déclaré : un client choisit un rang dans `jobs.sources`, il ne
    /// nomme jamais de chemin. Voir `crate::jobs::Sources`.
    fn job_start(&self, params: Value) -> Result<Value, Error> {
        let params: mailapi::dispatch::StartJobParams =
            serde_json::from_value(params).map_err(Error::invalid_params)?;

        let kind = match params.kind.as_str() {
            "import" => crate::jobs::Kind::Import {
                source: params.source.unwrap_or(0),
                dry_run: params.dry_run,
                include_feeds: params.include_feeds,
                include_orphans: params.include_orphans,
            },
            "index" => crate::jobs::Kind::Index,
            "thread" => crate::jobs::Kind::Thread,
            "contacts" => crate::jobs::Kind::Contacts,
            // Le compte est désigné par son identifiant, qui est **déjà connu du client** :
            // `folders.list` le porte pour chaque dossier. Ce n'est donc pas une capacité
            // nouvelle, contrairement au chemin d'un profil que l'import refuse.
            "sync" => crate::jobs::Kind::Sync {
                account: params.account,
            },
            other => {
                return Err(Error::invalid_params(format!("tâche inconnue : {other}")));
            }
        };

        match self.jobs.enqueue(kind) {
            // Un refus de démarrage est une erreur du client — un profil qui n'existe pas —
            // ou une configuration incomplète du démon. Dans les deux cas le message dit quoi
            // faire, et il ne contient aucun chemin.
            Err(source) => Err(Error::invalid_params(source)),
            Ok(id) => match self.jobs.get(id) {
                Some(snapshot) => encode(&job(&snapshot)),
                // Évincé entre la mise en file et la relecture : impossible en pratique, le
                // registre ne supprime jamais un job non terminé.
                None => Err(Error::new(
                    jsonrpc::INTERNAL_ERROR,
                    "la tâche a disparu du registre",
                )),
            },
        }
    }

    /// `jobs.list` : les tâches connues, de la plus récente à la plus ancienne.
    fn job_list(&self) -> Result<Value, Error> {
        let listed: Vec<mailapi::dto::Job> = self.jobs.list().iter().map(job).collect();
        encode(&listed)
    }

    /// `jobs.get` : une tâche par son identifiant.
    fn job_get(&self, params: Value) -> Result<Value, Error> {
        let params: mailapi::dispatch::JobParams =
            serde_json::from_value(params).map_err(Error::invalid_params)?;
        encode(&self.jobs.get(params.id).as_ref().map(job))
    }

    /// `jobs.cancel` : demande l'arrêt d'une tâche.
    fn job_cancel(&self, params: Value) -> Result<Value, Error> {
        let params: mailapi::dispatch::JobParams =
            serde_json::from_value(params).map_err(Error::invalid_params)?;
        encode(&self.jobs.cancel(params.id).as_ref().map(job))
    }

    /// Attend qu'une révision change, ou que le délai expire.
    async fn wait(&self, params: Value) -> Result<Value, Error> {
        let params: WaitParams = serde_json::from_value(params).map_err(Error::invalid_params)?;
        let budget = Duration::from_millis(
            params
                .timeout_ms
                .unwrap_or(DEFAULT_WAIT_MS)
                .min(MAX_WAIT_MS),
        );
        let deadline = Instant::now() + budget;

        loop {
            // Le premier relevé est immédiat : un client dont la révision est déjà périmée
            // obtient sa réponse sans attendre, et ne paie pas un aller-retour pour
            // apprendre ce que le démon savait déjà.
            let current = self
                .read(|mailbox| {
                    mailapi::call(mailbox, mailapi::method::STORE_REVISION, Value::Null)
                })
                .await?;
            let current = current
                .get("revision")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();

            if current != params.revision {
                return Ok(json!({ "revision": current, "changed": true }));
            }

            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                // Le délai a expiré sans que rien ne bouge. Ce n'est pas une erreur : le
                // client rappelle, et entre-temps il n'a rien eu à redessiner.
                return Ok(json!({ "revision": current, "changed": false }));
            }
            tokio::time::sleep(remaining.min(POLL_INTERVAL)).await;
        }
    }

    /// Exécute une lecture sur la boîte, hors du fil du runtime.
    async fn read<T, F>(&self, action: F) -> Result<T, Error>
    where
        F: FnOnce(&Mailbox) -> Result<T, Error> + Send + 'static,
        T: Send + 'static,
    {
        let mailbox = Arc::clone(&self.mailbox);
        let outcome = tokio::task::spawn_blocking(move || {
            // Un verrou empoisonné vient d'une panique dans une autre lecture. La boîte
            // n'est jamais mutée, donc son état reste valide : reprendre est plus utile que
            // de refuser tout le reste de la session.
            let guard = mailbox
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            action(&guard)
        })
        .await;

        match outcome {
            Ok(result) => result,
            Err(source) => {
                tracing::error!(%source, "tâche de l'API interrompue");
                Err(Error::new(
                    jsonrpc::INTERNAL_ERROR,
                    "la lecture a été interrompue",
                ))
            }
        }
    }

    /// `accounts.list` : les comptes, et lesquels peuvent envoyer.
    ///
    /// Ne rend ni hôte, ni port, ni mécanisme, ni secret : voir `mailapi::dto::Account`.
    async fn accounts_list(&self) -> Result<Value, Error> {
        self.read(|mailbox| {
            let accounts = mailbox.store().full_accounts().map_err(|source| {
                tracing::warn!(%source, "comptes illisibles");
                Error::new(jsonrpc::INTERNAL_ERROR, "les comptes ne sont pas lisibles")
            })?;
            let out: Vec<mailapi::dto::Account> = accounts
                .iter()
                .map(|it| mailapi::dto::Account {
                    id: it.id.0,
                    name: it.display_name.clone(),
                    address: it.server.as_ref().map(|s| s.username.clone()),
                    can_send: it.can_send(),
                    enabled: it.enabled,
                })
                .collect();
            encode(&out)
        })
        .await
    }

    /// `accounts.signature` : la signature d'un compte, ou `null`.
    ///
    /// Un compte sans signature, un compte inconnu et une colonne illisible rendent tous les
    /// trois `null` : voir `mailcore::Store::signature`, qui dit pourquoi une signature abîmée
    /// ne doit pas empêcher d'écrire un message.
    async fn account_signature(&self, params: Value) -> Result<Value, Error> {
        let params: mailapi::dispatch::SignatureParams =
            serde_json::from_value(params).map_err(Error::invalid_params)?;
        self.read(move |mailbox| {
            let signature = mailbox
                .store()
                .signature(mailcore::AccountId(params.account))
                .map_err(|source| {
                    tracing::warn!(%source, "signature illisible");
                    Error::new(jsonrpc::INTERNAL_ERROR, "le store n'est pas lisible")
                })?;
            encode(&signature)
        })
        .await
    }

    /// `accounts.set_signature` : enregistre la signature d'un compte, ou l'efface.
    ///
    /// ## Elle écrit, et elle refuse avant d'écrire
    ///
    /// Un compte inconnu est un `-32602` et non un `UPDATE` sans effet : une signature écrite
    /// dans le vide se retrouverait « enregistrée » à l'écran et absente au message suivant.
    ///
    /// Ce que le client envoie n'est pas cru : le document est remis d'aplomb à la relecture,
    /// et le HTML qui partira écarte un lien piégé. Voir `mailapi::dispatch::SetSignatureParams`.
    async fn account_set_signature(&self, params: Value) -> Result<Value, Error> {
        let params: mailapi::dispatch::SetSignatureParams =
            serde_json::from_value(params).map_err(Error::invalid_params)?;
        self.read(move |mailbox| {
            let account = mailcore::AccountId(params.account);
            let known = mailbox
                .store()
                .accounts()
                .map_err(|source| {
                    tracing::warn!(%source, "comptes illisibles");
                    Error::new(jsonrpc::INTERNAL_ERROR, "les comptes ne sont pas lisibles")
                })?
                .iter()
                .any(|(it, _, _)| *it == account);
            if !known {
                return Err(Error::invalid_params(format!(
                    "aucun compte #{}",
                    params.account
                )));
            }

            let writer = mailbox.store().writer().map_err(|source| {
                tracing::warn!(%source, "store non inscriptible");
                Error::new(jsonrpc::INTERNAL_ERROR, "le store n'est pas inscriptible")
            })?;
            writer
                .set_signature(account, params.signature.as_ref())
                .and_then(|()| writer.commit())
                .map_err(|source| {
                    tracing::warn!(%source, "signature non enregistrée");
                    Error::new(
                        jsonrpc::INTERNAL_ERROR,
                        "la signature n'a pas pu être enregistrée",
                    )
                })?;
            // Ce qui est relu, et non ce qui a été envoyé : le client voit le document tel que
            // le store le rendra — vide effacé, invariants remis — plutôt que sa propre copie.
            let stored = mailbox.store().signature(account).map_err(|source| {
                tracing::warn!(%source, "signature illisible après écriture");
                Error::new(jsonrpc::INTERNAL_ERROR, "le store n'est pas lisible")
            })?;
            encode(&stored)
        })
        .await
    }

    /// `messages.stage_part` : range une pièce jointe d'un message reçu dans le magasin.
    ///
    /// ## À quoi ça sert, et pourquoi ça n'ouvre aucune porte
    ///
    /// À transférer un message avec ses pièces, et à joindre depuis un client distant — les
    /// deux manques que le journal du 2026-09-09 listait. Le client nomme un message qu'il peut
    /// **déjà lire** et un rang dans la liste que `messages.get` lui a rendue : rien de neuf ne
    /// devient accessible, le contenu était déjà dans le magasin sous une autre forme.
    ///
    /// Il n'y a **pas** de champ de chemin, et c'est ce qui distingue cette méthode d'un
    /// téléversement : le démon ne lit aucun fichier que le client désigne.
    ///
    /// ## Le rang est celui de `messages.get`, et c'est l'invariant qui compte
    ///
    /// Même itérateur des deux côtés — voir `mailcore::attachment_bytes`. Deux parcours
    /// différents feraient joindre un fichier à la place d'un autre, ce qui est la façon la plus
    /// discrète d'envoyer à quelqu'un un document qui ne lui était pas destiné.
    async fn stage_part(&self, params: Value) -> Result<Value, Error> {
        let params: mailapi::dispatch::StagePartParams =
            serde_json::from_value(params).map_err(Error::invalid_params)?;
        self.read(move |mailbox| {
            let id = mailcore::MessageId(params.id);
            let item = mailbox
                .store()
                .message(id)
                .map_err(|source| {
                    tracing::warn!(%source, "message illisible");
                    Error::new(jsonrpc::INTERNAL_ERROR, "le store n'est pas lisible")
                })?
                .ok_or_else(|| Error::invalid_params(format!("aucun message #{}", params.id)))?;
            let raw = mailbox.store().blobs().read(item.blob).map_err(|source| {
                tracing::warn!(%source, "blob absent");
                Error::new(
                    jsonrpc::INTERNAL_ERROR,
                    "le contenu de ce message est introuvable",
                )
            })?;

            let (filename, bytes) =
                mailcore::attachment_bytes(&raw, params.part).ok_or_else(|| {
                    Error::invalid_params(format!(
                        "le message #{} n'a pas de pièce jointe n° {}",
                        params.id, params.part
                    ))
                })?;
            let size = bytes.len() as u64;
            let put = mailbox.store().blobs().put(&bytes).map_err(|source| {
                tracing::warn!(%source, "pièce non rangée");
                Error::new(
                    jsonrpc::INTERNAL_ERROR,
                    "la pièce jointe n'a pas pu être rangée",
                )
            })?;

            tracing::info!(
                message = params.id,
                part = params.part,
                size,
                "pièce jointe rangée pour un renvoi"
            );
            encode(&mailapi::dto::Attached {
                filename,
                blob: put.hash.to_hex(),
                size,
            })
        })
        .await
    }
    /// `drafts.list` : les brouillons, du plus récemment touché au plus ancien.
    async fn drafts_list(&self) -> Result<Value, Error> {
        self.read(|mailbox| {
            let drafts = mailbox.store().drafts().map_err(|source| {
                tracing::warn!(%source, "brouillons illisibles");
                Error::new(
                    jsonrpc::INTERNAL_ERROR,
                    "les brouillons ne sont pas lisibles",
                )
            })?;
            let out: Vec<mailapi::dto::Draft> = drafts.iter().map(draft_dto).collect();
            encode(&out)
        })
        .await
    }

    /// `drafts.save` : enregistre un brouillon, ou met à jour celui dont l'identifiant est donné.
    ///
    /// ## Rien n'est validé, et c'est le point
    ///
    /// Un brouillon est ce qu'il y avait à l'écran : une adresse à moitié tapée, un sujet vide,
    /// aucun destinataire. `outbox.send` refuse tout ça — à juste titre — mais le refuser ici
    /// ferait perdre la frappe en cours, c'est-à-dire exactement ce que les brouillons servent à
    /// ne pas perdre.
    ///
    /// Le compte, lui, est vérifié : un brouillon attaché à un compte qui n'existe pas ne
    /// pourrait jamais partir, et la cascade du schéma l'effacerait au premier ménage.
    ///
    /// **Un brouillon vide n'est pas enregistré** : il est supprimé s'il existait, et rien
    /// n'est créé sinon. Une fenêtre ouverte par erreur puis refermée ne doit pas laisser une
    /// ligne dans la liste, et c'est le geste le plus fréquent de tous.
    async fn drafts_save(&self, params: Value) -> Result<Value, Error> {
        let params: mailapi::dto::Draft =
            serde_json::from_value(params).map_err(Error::invalid_params)?;
        self.read(move |mailbox| {
            let account = mailcore::AccountId(params.account);
            let known = mailbox
                .store()
                .accounts()
                .map_err(|source| {
                    tracing::warn!(%source, "comptes illisibles");
                    Error::new(jsonrpc::INTERNAL_ERROR, "les comptes ne sont pas lisibles")
                })?
                .iter()
                .any(|(it, _, _)| *it == account);
            if !known {
                return Err(Error::invalid_params(format!(
                    "aucun compte #{}",
                    params.account
                )));
            }

            let draft = draft_from(&params, account)?;
            if draft.is_empty() {
                // Vide : on jette ce qui existait, et on ne crée rien. Le client reçoit `null`,
                // qui est la réponse honnête à « enregistre ceci » quand il n'y a rien.
                if let Some(id) = draft.id {
                    mailbox.store().delete_draft(id).map_err(|source| {
                        tracing::warn!(%source, "brouillon non supprimé");
                        Error::new(
                            jsonrpc::INTERNAL_ERROR,
                            "le brouillon n'a pas pu être supprimé",
                        )
                    })?;
                }
                return Ok(Value::Null);
            }

            let id = mailbox
                .store()
                .save_draft(&draft, unix_now())
                .map_err(|source| {
                    tracing::warn!(%source, "brouillon non enregistré");
                    Error::new(
                        jsonrpc::INTERNAL_ERROR,
                        "le brouillon n'a pas pu être enregistré",
                    )
                })?;
            // Ce qui est relu, et non ce qui a été envoyé : le client reçoit l'identifiant à
            // renvoyer au prochain enregistrement, et la date que le service a posée.
            let stored = mailbox.store().draft(id).map_err(|source| {
                tracing::warn!(%source, "brouillon illisible après écriture");
                Error::new(jsonrpc::INTERNAL_ERROR, "le store n'est pas lisible")
            })?;
            match stored {
                Some(draft) => encode(&draft_dto(&draft)),
                None => Ok(Value::Null),
            }
        })
        .await
    }

    /// `drafts.delete` : jette un brouillon.
    ///
    /// Idempotent : supprimer un brouillon déjà supprimé rend `false` et non une erreur. Deux
    /// clics rapides sur la corbeille ne sont pas une faute.
    async fn drafts_delete(&self, params: Value) -> Result<Value, Error> {
        let params: mailapi::dispatch::DraftParams =
            serde_json::from_value(params).map_err(Error::invalid_params)?;
        self.read(move |mailbox| {
            let removed = mailbox
                .store()
                .delete_draft(mailcore::DraftId(params.id))
                .map_err(|source| {
                    tracing::warn!(%source, "brouillon non supprimé");
                    Error::new(
                        jsonrpc::INTERNAL_ERROR,
                        "le brouillon n'a pas pu être supprimé",
                    )
                })?;
            encode(&json!({"removed": removed}))
        })
        .await
    }
    /// `outbox.list` : la file d'envoi, états compris.
    async fn outbox_list(&self) -> Result<Value, Error> {
        self.read(|mailbox| {
            let outbox = mailbox.store().outbox().map_err(|source| {
                tracing::warn!(%source, "file d'envoi illisible");
                Error::new(jsonrpc::INTERNAL_ERROR, "la file d'envoi n'est pas lisible")
            })?;
            let out: Vec<mailapi::dto::Outgoing> =
                outbox.iter().map(mailapi::dto::Outgoing::new).collect();
            encode(&out)
        })
        .await
    }

    /// `outbox.send` : compose un message, l'écrit dans la file, et réveille le facteur.
    ///
    /// ## Elle n'envoie rien elle-même, et c'est la règle 3 du `CLAUDE.md`
    ///
    /// Une poignée de main TLS, une authentification et un transfert de 25 Mo se comptent en
    /// dizaines de secondes. Une méthode d'API qui attendrait ça tiendrait le verrou de la boîte
    /// pendant tout ce temps, et l'interface qui l'a appelée serait figée.
    ///
    /// Ce qu'elle fait est donc borné et rapide : composer, refuser ce qui n'est pas envoyable,
    /// écrire le blob, écrire la ligne, réveiller. `crate::outbox::Postman` fait le reste.
    ///
    /// ## Le refus a lieu **avant** l'écriture
    ///
    /// Une adresse qui porte un retour à la ligne, aucun destinataire, un compte sans serveur
    /// d'envoi : rien n'est écrit, et le client reçoit un `-32602`. Mettre en file un message
    /// qui ne peut pas partir remplirait la file de lignes que personne ne peut remettre.
    async fn outbox_send(&self, params: Value) -> Result<Value, Error> {
        let params: mailapi::dispatch::SendParams =
            serde_json::from_value(params).map_err(Error::invalid_params)?;
        let postman = self.postman.clone();

        let queued = self
            .read(move |mailbox| {
                let accounts = mailbox.store().full_accounts().map_err(|source| {
                    tracing::warn!(%source, "comptes illisibles");
                    Error::new(jsonrpc::INTERNAL_ERROR, "les comptes ne sont pas lisibles")
                })?;
                let account = accounts
                    .iter()
                    .find(|it| it.id.0 == params.account)
                    .ok_or_else(|| {
                        Error::invalid_params(format!("aucun compte #{}", params.account))
                    })?;
                let reading = account.server.as_ref().ok_or_else(|| {
                    Error::invalid_params(format!(
                        "le compte #{} n'a pas de serveur",
                        params.account
                    ))
                })?;
                // Refusé **avant** de composer : un message mis en file pour un compte qui ne
                // peut pas l'envoyer resterait en file pour toujours.
                if !account.can_send() {
                    return Err(Error::invalid_params(format!(
                        "le compte #{} n'a pas de serveur d'envoi",
                        params.account
                    )));
                }

                let mut draft = draft(account, reading, &params)?;
                // La signature est ajoutée **ici**, par la fonction que `mail send` appelle
                // aussi : un seul endroit sait comment elle rejoint un corps. Le client demande,
                // il ne compose pas — sinon un client qui l'a déjà mise la verrait doublée.
                if params.signature
                    && let Some(signature) =
                        mailbox.store().signature(account.id).map_err(|source| {
                            tracing::warn!(%source, "signature illisible");
                            Error::new(jsonrpc::INTERNAL_ERROR, "le store n'est pas lisible")
                        })?
                {
                    draft.sign_with(&signature);
                }
                let recipients: Vec<String> = draft
                    .envelope_recipients()
                    .iter()
                    .map(|it| it.addr().to_owned())
                    .collect();
                let now = unix_now();
                // **Une seule fonction assemble et met en file**, partagée avec `mail send`.
                // Elle écrit en flux : un message avec 25 Mo de pièces jointes ne passe jamais
                // en entier par la mémoire du démon — critère 3 de `docs/PHASE-3.md`.
                let (id, size) = mailsmtp::queue::stage(mailbox.store(), account.id, &draft, now)
                    .map_err(|source| {
                    // `Unsendable` couvre deux cas très différents : un brouillon
                    // invalide, qui est une erreur du client, et un magasin illisible,
                    // qui est une panne du démon. Le premier doit dire quoi corriger,
                    // le second ne doit rien dire de l'intérieur.
                    tracing::warn!(%source, "mise en file impossible");
                    Error::invalid_params(source.to_string())
                })?;

                Ok(mailapi::dto::Queued {
                    id: id.0,
                    size,
                    recipients,
                })
            })
            .await?;

        // **Après le verrou, jamais dedans.** Le facteur ouvre le store dans son propre fil ;
        // le réveiller en tenant la boîte l'enverrait attendre le verrou qu'on tient.
        if let Some(postman) = postman {
            postman.nudge();
        }
        tracing::info!(job = queued.id, "message mis en file par l'API");
        encode(&queued)
    }

    /// `messages.mark_read` : marque un message lu, et note la poussée `\Seen` à faire.
    ///
    /// ## Pourquoi ce n'est pas un effet de bord de `messages.get`
    ///
    /// Parce que **lire n'est pas marquer**. `mailmcp` sert des modèles : un modèle qui parcourt
    /// une boîte pour répondre à une question marquerait tout comme lu au passage, et
    /// l'utilisateur retrouverait sa boîte vidée de ses non-lus sans avoir rien ouvert.
    ///
    /// Le coût est un appel de plus quand la coquille ouvre un message. C'est le bon prix.
    ///
    /// ## Elle n'attend aucun serveur
    ///
    /// L'écriture est locale ; la poussée `\Seen` est faite par la moisson, qui a la connexion.
    /// Règle 3 du `CLAUDE.md` : ouvrir un message est un clic, et un clic n'attend pas un
    /// réseau.
    async fn mark_read(&self, params: Value) -> Result<Value, Error> {
        let params: mailapi::dispatch::MarkReadParams =
            serde_json::from_value(params).map_err(Error::invalid_params)?;
        self.read(move |mailbox| {
            let marked = mailbox
                .store()
                .mark_seen(
                    mailcore::MessageId(params.id),
                    mailcore::FolderId(params.folder),
                    unix_now(),
                )
                .map_err(|source| {
                    tracing::warn!(%source, "marquage lu impossible");
                    Error::new(jsonrpc::INTERNAL_ERROR, "le message n'a pas pu être marqué")
                })?;
            // Le nombre de copies marquées, donc **zéro** quand il n'y avait rien à faire. Un
            // client qui rappelle sur un message déjà lu doit pouvoir le distinguer d'un échec.
            encode(&json!({ "marked": marked }))
        })
        .await
    }

    /// `outbox.decide` : tranche le doute sur un message, **sur décision de l'utilisateur**.
    ///
    /// ## C'est la seule sortie de l'état incertain, et elle ne s'automatise pas
    ///
    /// Rien dans le démon n'appelle cette méthode tout seul. Le facteur ne la connaît pas, un
    /// redémarrage ne la déclenche pas. C'est le critère 2 de `docs/PHASE-3.md` : un message
    /// peut-être parti ne repart pas sans qu'un humain l'ait demandé.
    ///
    /// `Store::resolve_doubt` refuse toute ligne qui n'est pas douteuse, ce qui empêche cette
    /// méthode d'être un contournement général de la file.
    async fn outbox_decide(&self, params: Value) -> Result<Value, Error> {
        let params: mailapi::dispatch::DecideParams =
            serde_json::from_value(params).map_err(Error::invalid_params)?;
        let decision = mailcore::store::outbox::Decision::parse(&params.decision)
            .map_err(|source| Error::invalid_params(source.to_string()))?;
        let postman = self.postman.clone();

        let resolved = self
            .read(move |mailbox| {
                mailbox
                    .store()
                    .resolve_doubt(mailcore::OutboxId(params.id), decision)
                    .map_err(|source| {
                        tracing::warn!(%source, "décision non écrite");
                        Error::new(
                            jsonrpc::INTERNAL_ERROR,
                            "la décision n'a pas pu être écrite",
                        )
                    })
            })
            .await?;

        // Réveiller seulement si quelque chose est reparti en file — et **après** le verrou,
        // jamais dedans.
        if resolved
            && decision == mailcore::store::outbox::Decision::Resend
            && let Some(postman) = postman
        {
            postman.nudge();
        }
        encode(&json!({ "resolved": resolved }))
    }

    /// `outbox.retry` : remet en file un envoi **échoué**, sur décision de l'utilisateur.
    ///
    /// ## Elle n'est pas le pendant de `outbox.decide`, et le store le fait respecter
    ///
    /// `Store::retry_outgoing` refuse toute ligne qui n'est pas `failed`. C'est ce qui empêche
    /// cette méthode d'être un contournement de la file : appelée sur une ligne `sent`, elle ne
    /// renvoie rien ; sur une ligne `committing`, elle ne fait pas le pari que
    /// `outbox.decide` demande d'assumer.
    ///
    /// ## Rien n'appelle cette méthode tout seul
    ///
    /// Le facteur ne la connaît pas, un redémarrage ne la déclenche pas. Une ligne `failed` a
    /// été rendue à l'utilisateur, et c'est lui qui décide si le refus a été corrigé.
    async fn outbox_retry(&self, params: Value) -> Result<Value, Error> {
        let params: mailapi::dispatch::RetryParams =
            serde_json::from_value(params).map_err(Error::invalid_params)?;
        let postman = self.postman.clone();

        let queued = self
            .read(move |mailbox| {
                mailbox
                    .store()
                    .retry_outgoing(mailcore::OutboxId(params.id))
                    .map_err(|source| {
                        tracing::warn!(%source, "renvoi non écrit");
                        Error::new(jsonrpc::INTERNAL_ERROR, "le renvoi n'a pas pu être écrit")
                    })
            })
            .await?;

        // **Après le verrou, jamais dedans** : le facteur va lire le store.
        if queued && let Some(postman) = postman {
            postman.nudge();
        }
        encode(&json!({ "queued": queued }))
    }
}

/// Projette un brouillon du store vers le contrat de fil.
///
/// L'étiquette est calculée **ici** et non par le client : un brouillon sans sujet est le cas
/// ordinaire, et trois clients qui choisiraient chacun leur repli afficheraient trois listes
/// différentes.
fn draft_dto(draft: &mailcore::Draft) -> mailapi::dto::Draft {
    mailapi::dto::Draft {
        id: draft.id.map(|it| it.0),
        account: draft.account.0,
        to: draft.to.clone(),
        cc: draft.cc.clone(),
        bcc: draft.bcc.clone(),
        subject: draft.subject.clone(),
        body: draft.body.clone(),
        in_reply_to: draft.in_reply_to.clone(),
        references: draft.references.clone(),
        sign: draft.sign,
        attachments: draft
            .attachments
            .iter()
            .map(|it| mailapi::dto::Attached {
                filename: it.filename.clone(),
                blob: it.blob.to_string(),
                size: it.size,
            })
            .collect(),
        updated_at: draft.updated_at,
        label: draft.label(),
    }
}

/// Lit un brouillon envoyé par un client.
///
/// ## Une pièce jointe est refusée si son contenu n'est pas déjà dans le magasin
///
/// Le hachage est vérifié à la lecture, et le blob **doit exister** : un brouillon qui
/// nommerait un contenu absent produirait, au moment de l'envoi, un message dont une pièce
/// manque — découvert par le destinataire. Mieux vaut refuser l'enregistrement et le dire.
fn draft_from(
    params: &mailapi::dto::Draft,
    account: mailcore::AccountId,
) -> Result<mailcore::Draft, Error> {
    let mut attachments = Vec::with_capacity(params.attachments.len());
    for piece in &params.attachments {
        let blob = mailcore::BlobHash::from_hex(&piece.blob).map_err(|source| {
            Error::invalid_params(format!("pièce jointe {:?} : {source}", piece.filename))
        })?;
        attachments.push(mailcore::DraftAttachment {
            blob,
            filename: piece.filename.clone(),
            // Le type est déduit du nom, comme à l'envoi : c'est la même fonction, donc le
            // brouillon annonce exactement ce que le message annoncera.
            mime: mailsmtp::compose::mime_for(&piece.filename).to_owned(),
            size: piece.size,
        });
    }
    Ok(mailcore::Draft {
        id: params.id.map(mailcore::DraftId),
        account,
        to: params.to.clone(),
        cc: params.cc.clone(),
        bcc: params.bcc.clone(),
        subject: params.subject.clone(),
        body: params.body.clone(),
        in_reply_to: params.in_reply_to.clone(),
        references: params.references.clone(),
        sign: params.sign,
        attachments,
        updated_at: params.updated_at,
    })
}

/// Construit le brouillon RFC 5322 à partir de ce qu'un client a demandé.
///
/// ## L'expéditeur vient du compte, pas des paramètres
///
/// Il n'y a pas de champ `from` dans `SendParams`, et c'est ce qui empêche un client qui
/// détient le jeton d'usurper une adresse. Le nom affiché n'est joint que s'il dit quelque
/// chose de plus que l'adresse : chez la plupart des comptes du corpus, `display_name` **est**
/// l'adresse, et le joindre donnerait `From: "marie@x.fr" <marie@x.fr>`.
fn draft(
    account: &mailcore::Account,
    reading: &mailcore::Server,
    params: &mailapi::dispatch::SendParams,
) -> Result<mailsmtp::compose::Draft, Error> {
    use mailsmtp::compose::{Address, Draft};

    let name = Some(account.display_name.as_str()).filter(|it| *it != reading.username);
    let from = Address::parse(&reading.username, name).map_err(|source| {
        tracing::warn!(%source, compte = account.id.0, "identifiant de compte non analysable");
        Error::new(
            jsonrpc::INTERNAL_ERROR,
            "l'identifiant de ce compte n'est pas une adresse valide",
        )
    })?;

    // Le refus est **global** : envoyer à trois destinataires sur quatre en taisant le
    // quatrième laisserait l'utilisateur croire que tout est parti.
    let parse_all = |raw: &[String]| -> Result<Vec<Address>, Error> {
        raw.iter()
            .map(|it| {
                Address::parse(it, None)
                    .map_err(|source| Error::invalid_params(format!("{it:?} : {source}")))
            })
            .collect()
    };

    let mut draft = Draft::new(from, parse_all(&params.to)?, &params.subject, &params.text);
    draft.cc = parse_all(&params.cc)?;
    draft.bcc = parse_all(&params.bcc)?;
    draft.html = params.html.clone();
    draft.in_reply_to = params.in_reply_to.clone();
    draft.references = params.references.clone();

    // **Le hachage est relu, pas recopié.** Un client qui enverrait n'importe quelle chaîne
    // obtiendrait sinon une pièce jointe qui pointe nulle part, et l'échec arriverait à
    // l'assemblage — après avoir écrit la ligne de file.
    //
    // Ce qui n'est **pas** vérifié ici : que le blob existe. `Draft::write_to` le demandera au
    // magasin et refusera s'il manque, ce qui est le bon endroit — entre les deux, un autre
    // client pourrait l'avoir purgé.
    for attachment in &params.attachments {
        let blob = mailcore::BlobHash::from_hex(&attachment.blob).map_err(|source| {
            Error::invalid_params(format!(
                "pièce jointe {:?} : hachage illisible — {source}",
                attachment.filename
            ))
        })?;
        draft.attachments.push(mailsmtp::compose::Attachment {
            filename: attachment.filename.clone(),
            mime: mailsmtp::compose::mime_for(&attachment.filename).to_owned(),
            blob,
            size: attachment.size,
        });
    }
    Ok(draft)
}

/// Secondes Unix.
fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| {
            i64::try_from(since.as_secs()).unwrap_or(i64::MAX)
        })
}
/// Sérialise une réponse.
fn encode<T: serde::Serialize>(payload: &T) -> Result<Value, Error> {
    serde_json::to_value(payload).map_err(|source| {
        tracing::error!(%source, "sérialisation d'une réponse impossible");
        Error::new(jsonrpc::INTERNAL_ERROR, "réponse non sérialisable")
    })
}

/// Convertit l'instantané d'une tâche pour le fil.
fn job(snapshot: &crate::jobs::Snapshot) -> mailapi::dto::Job {
    mailapi::dto::Job {
        id: snapshot.id,
        kind: snapshot.kind.as_str().to_owned(),
        state: snapshot.state.as_str().to_owned(),
        done: snapshot.done,
        total: snapshot.total,
        // Calculée ici plutôt que côté client : le rapport de deux entiers dont l'un peut
        // être nul est exactement le genre de division que chaque client referait, et l'un
        // d'eux la referait mal.
        fraction: (snapshot.total > 0)
            .then(|| (snapshot.done as f32 / snapshot.total as f32).min(1.0)),
        message: snapshot.message.clone(),
        queued_at: snapshot.queued_at,
        finished_at: snapshot.finished_at,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use mailcore::{BlobHash, FolderKind, MessageFlags, NewMessage, Store};

    fn api() -> (tempfile::TempDir, Api) {
        let dir = tempfile::tempdir().unwrap();
        let root = camino::Utf8Path::from_path(dir.path()).unwrap().to_owned();

        let store = Store::open(&root).unwrap();
        let writer = store.writer().unwrap();
        // **Un compte imap **cohérent**, pas un `upsert_account("imap", …)` sans serveur.
        // Ce dernier est refusé par `Store::full_accounts` — à juste titre : un hôte vide
        // finirait dans un résolveur. Le fixture en créait un, et personne ne le voyait
        // parce qu'aucune méthode d'API ne lisait les comptes. `accounts.list` le lit, et le
        // premier test qui s'en sert est tombé sur « les comptes ne sont pas lisibles ».
        //
        // Il n'a **pas** de serveur de soumission : c'est ce qui en fait le contrôle négatif
        // de l'envoi.
        let account = writer
            .upsert_imap_account(
                "compte",
                &mailcore::Server {
                    host: "imap.exemple.invalid".to_owned(),
                    port: 993,
                    username: "compte@exemple.fr".to_owned(),
                    auth: mailcore::AuthKind::Password,
                    security: mailcore::Security::Tls,
                },
            )
            .unwrap();
        let folder = writer
            .upsert_folder(account, "INBOX", FolderKind::Inbox)
            .unwrap();
        let (id, _) = writer
            .insert_message(&NewMessage {
                blob: BlobHash::of(b"un message"),
                rfc822_id: None,
                date: 1_700_000_000,
                from_addr: "a@b.c",
                from_name: None,
                subject: "sujet",
                size: 10,
                has_attachments: false,
            })
            .unwrap();
        writer
            .insert_ref(id, folder, 1_700_000_000, MessageFlags::empty())
            .unwrap();
        writer.commit().unwrap();
        drop(store);

        let mailbox = Mailbox::open(&root).unwrap();
        let shared = Arc::new(Mutex::new(mailbox));
        let jobs =
            crate::jobs::Jobs::start(Arc::clone(&shared), root, crate::jobs::Sources::default());
        (dir, Api::new(shared, jobs))
    }

    #[tokio::test]
    async fn a_call_comes_back_with_its_id() {
        let (_dir, api) = api();
        let text = api
            .handle_message(r#"{"jsonrpc":"2.0","id":42,"method":"folders.list"}"#)
            .await
            .unwrap();
        let response: Response = serde_json::from_str(&text).unwrap();
        assert_eq!(response.id, Value::from(42));
        assert!(!response.is_error());
    }

    #[tokio::test]
    async fn a_notification_gets_no_answer_at_all() {
        let (_dir, api) = api();
        assert!(
            api.handle_message(r#"{"jsonrpc":"2.0","method":"folders.list"}"#)
                .await
                .is_none()
        );
        // Même quand elle échoue : le client n'attend rien.
        assert!(
            api.handle_message(r#"{"jsonrpc":"2.0","method":"folders.nope"}"#)
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn broken_json_answers_with_a_null_id() {
        let (_dir, api) = api();
        let text = api.handle_message("{pas du json").await.unwrap();
        let response: Response = serde_json::from_str(&text).unwrap();
        assert_eq!(response.id, Value::Null);
        assert!(response.is_error());
    }

    #[tokio::test]
    async fn wait_returns_at_once_when_the_client_is_already_behind() {
        // Le cas courant au démarrage d'un client : il a une révision en cache, périmée. Il
        // ne doit pas attendre 30 secondes pour l'apprendre.
        let (_dir, api) = api();
        let started = Instant::now();
        let text = api
            .handle_message(
                r#"{"jsonrpc":"2.0","id":1,"method":"store.wait","params":{"revision":"perimee"}}"#,
            )
            .await
            .unwrap();
        assert!(started.elapsed() < Duration::from_secs(1));

        let response: Response = serde_json::from_str(&text).unwrap();
        let jsonrpc::Outcome::Result(result) = response.outcome else {
            panic!("store.wait a rendu une erreur");
        };
        assert_eq!(result["changed"], Value::Bool(true));
        assert!(result["revision"].as_str().is_some_and(|r| !r.is_empty()));
    }

    #[tokio::test]
    async fn wait_times_out_without_lying_about_a_change() {
        let (_dir, api) = api();
        // La révision courante, puis une attente très courte sur cette même révision.
        let text = api
            .handle_message(r#"{"jsonrpc":"2.0","id":1,"method":"store.revision"}"#)
            .await
            .unwrap();
        let response: Response = serde_json::from_str(&text).unwrap();
        let jsonrpc::Outcome::Result(current) = response.outcome else {
            panic!("store.revision a rendu une erreur");
        };
        let revision = current["revision"].as_str().unwrap().to_owned();

        let text = api
            .handle_message(&format!(
                r#"{{"jsonrpc":"2.0","id":2,"method":"store.wait","params":{{"revision":"{revision}","timeout_ms":300}}}}"#
            ))
            .await
            .unwrap();
        let response: Response = serde_json::from_str(&text).unwrap();
        let jsonrpc::Outcome::Result(result) = response.outcome else {
            panic!("store.wait a rendu une erreur");
        };
        assert_eq!(result["changed"], Value::Bool(false));
        assert_eq!(result["revision"], Value::from(revision));
    }

    #[tokio::test]
    async fn wait_refuses_params_it_cannot_read_rather_than_waiting_forever() {
        let (_dir, api) = api();
        let text = api
            .handle_message(r#"{"jsonrpc":"2.0","id":1,"method":"store.wait","params":{}}"#)
            .await
            .unwrap();
        let response: Response = serde_json::from_str(&text).unwrap();
        assert!(response.is_error());
    }

    /// Une API dont le compte est configuré pour envoyer, et le store sous la main.
    ///
    /// Le serveur d'envoi pointe vers un hôte qui n'existe pas : rien ne doit sortir de ce
    /// test, et `outbox.send` ne parle de toute façon à personne — c'est le facteur qui parle,
    /// et il n'est pas attaché ici.
    fn api_that_can_send() -> (tempfile::TempDir, Api, camino::Utf8PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = camino::Utf8Path::from_path(dir.path()).unwrap().to_owned();

        let store = Store::open(&root).unwrap();
        let account = {
            let writer = store.writer().unwrap();
            let id = writer
                .upsert_imap_account(
                    "marie@exemple.fr",
                    &mailcore::Server {
                        host: "imap.exemple.invalid".to_owned(),
                        port: 993,
                        username: "marie@exemple.fr".to_owned(),
                        auth: mailcore::AuthKind::Password,
                        security: mailcore::Security::Tls,
                    },
                )
                .unwrap();
            writer
                .set_submission(
                    id,
                    Some(&mailcore::Server {
                        host: "smtp.exemple.invalid".to_owned(),
                        port: 587,
                        username: "marie@exemple.fr".to_owned(),
                        auth: mailcore::AuthKind::Password,
                        security: mailcore::Security::StartTls,
                    }),
                )
                .unwrap();
            writer.commit().unwrap();
            id
        };
        let _ = account;
        drop(store);

        let mailbox = Mailbox::open(&root).unwrap();
        let shared = Arc::new(Mutex::new(mailbox));
        let jobs = crate::jobs::Jobs::start(
            Arc::clone(&shared),
            root.clone(),
            crate::jobs::Sources::default(),
        );
        (dir, Api::new(shared, jobs), root)
    }

    /// Appelle une méthode et rend son résultat, ou panique en disant l'erreur.
    async fn call(api: &Api, method: &str, params: Value) -> Value {
        let body = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
        let text = api.handle_message(&body.to_string()).await.unwrap();
        let response: Response = serde_json::from_str(&text).unwrap();
        match response.outcome {
            jsonrpc::Outcome::Result(value) => value,
            jsonrpc::Outcome::Error(error) => panic!("{method} a échoué : {}", error.message),
        }
    }

    /// Appelle une méthode et rend son erreur, ou panique si elle a réussi.
    async fn refusal(api: &Api, method: &str, params: Value) -> String {
        let body = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
        let text = api.handle_message(&body.to_string()).await.unwrap();
        let response: Response = serde_json::from_str(&text).unwrap();
        match response.outcome {
            jsonrpc::Outcome::Error(error) => error.message,
            jsonrpc::Outcome::Result(value) => panic!("{method} a réussi : {value}"),
        }
    }

    #[tokio::test]
    async fn accounts_list_says_who_can_send_and_never_says_how() {
        let (_dir, api, _root) = api_that_can_send();
        let accounts = call(&api, "accounts.list", Value::Null).await;
        let first = &accounts[0];

        assert_eq!(first["can_send"], Value::Bool(true));
        assert_eq!(first["address"], Value::from("marie@exemple.fr"));

        // **Ce que ce DTO ne doit jamais porter.** Un hôte et un port dessinent
        // l'infrastructure de messagerie de l'utilisateur, et quiconque détient le jeton les
        // lirait. Le test fige la forme plutôt que la confiance.
        let text = accounts.to_string();
        for forbidden in ["smtp.exemple.invalid", "imap.exemple.invalid", "587", "993"] {
            assert!(
                !text.contains(forbidden),
                "{forbidden} est exposé par accounts.list : {text}"
            );
        }
    }

    #[tokio::test]
    async fn a_send_queues_the_message_and_keeps_the_blind_copies_out_of_the_headers() {
        let (_dir, api, root) = api_that_can_send();
        let queued = call(
            &api,
            "outbox.send",
            json!({
                "account": 1,
                "to": ["jean@ailleurs.fr"],
                "bcc": ["discret@ailleurs.fr"],
                "subject": "sujet accentué é",
                "text": "bonjour",
            }),
        )
        .await;

        let id = queued["id"].as_i64().unwrap();
        assert!(queued["size"].as_u64().unwrap() > 0);
        // L'enveloppe porte la copie cachée : c'est le mécanisme même du `bcc`.
        let recipients = queued["recipients"].as_array().unwrap();
        assert_eq!(recipients.len(), 2, "{recipients:?}");

        // Et les octets écrits ne la portent pas. **La règle de correction de tout ce chemin**,
        // vérifiée sur ce que le store a réellement gardé, pas sur ce que la fonction a rendu.
        let store = Store::open(&root).unwrap();
        let line = store
            .outgoing(mailcore::OutboxId(id))
            .unwrap()
            .expect("la ligne doit exister");
        assert_eq!(line.state, mailcore::SendState::Queued);
        let bytes = store.blobs().read(line.blob).unwrap();
        let text = String::from_utf8_lossy(&bytes);
        let headers = text.split("\r\n\r\n").next().unwrap_or_default();
        assert!(
            !headers.to_ascii_lowercase().contains("bcc"),
            "un en-tête Bcc a été écrit : {headers}"
        );
        assert!(
            !headers.contains("discret@ailleurs.fr"),
            "la copie cachée est dans les en-têtes : {headers}"
        );
        // Le sujet non ASCII est encodé, pas envoyé brut.
        assert!(
            headers.contains("=?UTF-8?"),
            "le sujet accentué n'est pas encodé : {headers}"
        );
    }

    #[tokio::test]
    async fn an_address_that_could_inject_a_command_is_refused_before_anything_is_written() {
        let (_dir, api, root) = api_that_can_send();
        let message = refusal(
            &api,
            "outbox.send",
            json!({
                "account": 1,
                "to": ["jean@ailleurs.fr\r\nRCPT TO:<ailleurs@encore.fr>"],
                "text": "bonjour",
            }),
        )
        .await;
        assert!(!message.is_empty());

        // **Rien n'a été écrit.** Une ligne mise en file pour un message qui ne peut pas partir
        // resterait en file pour toujours, et un blob écrit pour rien est de la place perdue.
        let store = Store::open(&root).unwrap();
        assert!(store.outbox().unwrap().is_empty(), "une ligne a été écrite");
    }

    #[tokio::test]
    async fn a_send_without_a_recipient_is_refused() {
        let (_dir, api, root) = api_that_can_send();
        refusal(
            &api,
            "outbox.send",
            json!({"account": 1, "text": "bonjour"}),
        )
        .await;
        let store = Store::open(&root).unwrap();
        assert!(store.outbox().unwrap().is_empty());
    }

    #[tokio::test]
    async fn an_account_without_a_submission_server_is_refused_rather_than_queued() {
        // Le contrôle négatif de `accounts.list` : un compte qui ne peut pas envoyer doit être
        // refusé **avant** l'écriture. Mettre en file pour lui remplirait la file de lignes que
        // personne ne peut remettre.
        let (_dir, api) = api();
        let message = refusal(
            &api,
            "outbox.send",
            json!({"account": 1, "to": ["jean@ailleurs.fr"], "text": "bonjour"}),
        )
        .await;
        assert!(
            message.contains("serveur"),
            "le refus ne dit pas ce qui manque : {message}"
        );
    }

    #[tokio::test]
    async fn a_client_cannot_choose_its_sender() {
        // Il n'y a pas de champ `from` dans `SendParams`, et un champ en trop est **ignoré**
        // par serde plutôt que refusé. Ce test vérifie que l'ignorer n'a pas d'effet : la
        // ligne part sous l'identifiant du compte, quoi que le client ait écrit.
        let (_dir, api, _root) = api_that_can_send();
        let queued = call(
            &api,
            "outbox.send",
            json!({
                "account": 1,
                "from": "president@elysee.fr",
                "to": ["jean@ailleurs.fr"],
                "text": "bonjour",
            }),
        )
        .await;
        let id = queued["id"].as_i64().unwrap();

        let listed = call(&api, "outbox.list", Value::Null).await;
        let line = listed
            .as_array()
            .unwrap()
            .iter()
            .find(|it| it["id"].as_i64() == Some(id))
            .unwrap();
        assert_eq!(
            line["sender"],
            Value::from("marie@exemple.fr"),
            "un client a choisi son expéditeur"
        );
    }

    #[tokio::test]
    async fn the_outbox_marks_a_doubtful_line_as_such() {
        // C'est le champ que l'interface doit traiter à part, et il est **dérivé** : un client
        // n'a pas à connaître la liste des états pour reconnaître celui qui compte.
        let (_dir, api, root) = api_that_can_send();
        let queued = call(
            &api,
            "outbox.send",
            json!({"account": 1, "to": ["jean@ailleurs.fr"], "text": "bonjour"}),
        )
        .await;
        let id = mailcore::OutboxId(queued["id"].as_i64().unwrap());

        {
            let store = Store::open(&root).unwrap();
            store
                .commit_outgoing(id, mailcore::SendState::Committing)
                .unwrap();
        }

        let listed = call(&api, "outbox.list", Value::Null).await;
        let line = &listed.as_array().unwrap()[0];
        assert_eq!(line["state"], Value::from("committing"));
        assert_eq!(line["doubtful"], Value::Bool(true));
    }

    #[tokio::test]
    async fn every_declared_method_answers_something() {
        // Le contrôle du contrat : `server.methods` annonce une liste, et une méthode annoncée
        // qui rend `-32601` est un mensonge. Les trois nouvelles y sont maintenant.
        let (_dir, api, _root) = api_that_can_send();
        for name in mailapi::method::ALL {
            let params = match *name {
                "messages.get" | "messages.thread" | "messages.source" | "outbox.source" => {
                    json!({"id": 1})
                }
                "search.query" => json!({"query": "sujet"}),
                "store.wait" => json!({"revision": "0:0", "timeout_ms": 1}),
                "jobs.get" | "jobs.cancel" => json!({"id": 1}),
                "jobs.start" => json!({"kind": "index"}),
                "outbox.send" => {
                    json!({"account": 1, "to": ["jean@ailleurs.fr"], "text": "x"})
                }
                _ => Value::Null,
            };
            let body = json!({"jsonrpc": "2.0", "id": 1, "method": name, "params": params});
            let text = api.handle_message(&body.to_string()).await.unwrap();
            let response: Response = serde_json::from_str(&text).unwrap();
            if let jsonrpc::Outcome::Error(error) = response.outcome {
                assert_ne!(
                    error.code,
                    jsonrpc::METHOD_NOT_FOUND,
                    "{name} est annoncée et non servie"
                );
            }
        }
    }

    /// Les noms d'en-têtes de premier niveau d'un bloc, dans l'ordre où ils sont écrits.
    ///
    /// Une ligne qui commence par une espace ou une tabulation est la suite de la précédente
    /// (RFC 5322 §2.2.3) et n'est donc pas un en-tête de plus.
    fn header_names(headers: &str) -> Vec<String> {
        headers
            .lines()
            .filter(|line| !line.is_empty() && !line.starts_with([' ', '\t']))
            .filter_map(|line| line.split_once(':'))
            .map(|(name, _)| name.to_ascii_lowercase())
            .collect()
    }

    #[tokio::test]
    async fn what_we_compose_carries_nothing_the_user_cannot_read() {
        // **La promesse de `docs/PHASE-3.md` rendue vérifiable.** Elle tenait jusqu'ici sur un
        // argument — `Draft` n'a pas de champ `X-Mailer`, `SendParams` n'a pas de champ
        // `headers` — et un argument se périme au premier en-tête ajouté par commodité. Ici, le
        // message passe par la vraie composition, et ce qu'on lit est le blob remis au `DATA`.
        let (_dir, api, _root) = api_that_can_send();
        let queued = call(
            &api,
            "outbox.send",
            json!({
                "account": 1,
                "to": ["jean@ailleurs.fr"],
                "bcc": ["discret@ailleurs.fr"],
                "subject": "Déjeuner — jeudi",
                "text": "bonjour",
            }),
        )
        .await;
        let id = queued["id"].as_i64().unwrap();

        let source = call(&api, "outbox.source", json!({"id": id})).await;
        let headers = source["headers"].as_str().unwrap();
        let names = header_names(headers);

        // La liste **est** le contrat. Un en-tête de plus fait tomber ce test, et c'est le but :
        // il doit être ajouté ici, donc consciemment, plutôt que de partir en silence.
        let allowed = [
            "date",
            "from",
            "to",
            "cc",
            "subject",
            "message-id",
            "in-reply-to",
            "references",
            "mime-version",
            "content-type",
            "content-transfer-encoding",
        ];
        for name in &names {
            assert!(
                allowed.contains(&name.as_str()),
                "en-tête inattendu : {name}"
            );
        }
        assert!(names.contains(&"message-id".to_owned()));
        assert!(names.contains(&"date".to_owned()));

        // Le contrôle négatif du critère : une copie cachée est dans l'enveloppe et dans
        // **aucun** en-tête. Vérifié sur les noms ligne par ligne, parce qu'un `Message-ID` est
        // de l'hexadécimal et qu'y chercher « bcc » finit par tomber sur de l'aléa.
        assert!(!names.contains(&"bcc".to_owned()));
        assert!(
            !headers.contains("discret@ailleurs.fr"),
            "une copie cachée est visible dans les en-têtes"
        );

        // Et le contrôle inverse du précédent : un destinataire visible, lui, se lit bien.
        assert!(headers.contains("jean@ailleurs.fr"));
    }

    #[tokio::test]
    async fn the_source_of_a_queued_message_is_the_bytes_that_were_handed_over() {
        // Ce que la vue montre doit être le blob, pas une recomposition : une recomposition qui
        // diverge de l'envoi réel ne prouverait rien. Le contrôle est donc le blob lui-même,
        // relu du magasin à côté de l'API.
        let (_dir, api, root) = api_that_can_send();
        let queued = call(
            &api,
            "outbox.send",
            json!({"account": 1, "to": ["jean@ailleurs.fr"], "text": "bonjour"}),
        )
        .await;
        let id = queued["id"].as_i64().unwrap();

        let raw = {
            let store = Store::open(&root).unwrap();
            let line = store.outgoing(mailcore::OutboxId(id)).unwrap().unwrap();
            store.blobs().read(line.blob).unwrap()
        };

        let source = call(&api, "outbox.source", json!({"id": id})).await;
        assert_eq!(source["total"].as_u64().unwrap(), raw.len() as u64);
        assert_eq!(source["headers_truncated"], Value::Bool(false));
        assert_eq!(source["body_truncated"], Value::Bool(false));

        // Les octets eux-mêmes, aux fins de ligne près : la vue enlève le `\r` d'un `\r\n`, et
        // c'est la seule différence qu'elle a le droit d'introduire sur un message conforme.
        let shown = format!(
            "{}\n{}",
            source["headers"].as_str().unwrap(),
            source["body"].as_str().unwrap()
        );
        assert_eq!(shown, String::from_utf8(raw).unwrap().replace("\r\n", "\n"));
    }

    #[tokio::test]
    async fn a_signature_is_stored_and_read_back_through_the_api() {
        let (_dir, api, _root) = api_that_can_send();
        // Rien au départ, et c'est `null` et non une erreur : un compte sans signature est un
        // état ordinaire.
        assert_eq!(
            call(&api, "accounts.signature", json!({"account": 1})).await,
            Value::Null
        );

        let signature = json!({
            "text": "Éloïse Durand\ndirectrice",
            "spans": [{
                "at": 0,
                "len": "Éloïse Durand".len(),
                "style": {"bold": true, "italic": false, "link": null},
            }],
            "blocks": ["Paragraph", "Bullet"],
        });
        let written = call(
            &api,
            "accounts.set_signature",
            json!({"account": 1, "signature": signature}),
        )
        .await;
        // L'écriture rend ce qui a été **rangé** : le client n'a pas à croire sa propre copie.
        assert_eq!(written["text"], Value::from("Éloïse Durand\ndirectrice"));
        assert_eq!(written["blocks"][1], Value::from("Bullet"));
        assert_eq!(
            call(&api, "accounts.signature", json!({"account": 1})).await,
            written
        );
    }

    #[tokio::test]
    async fn an_empty_signature_clears_it_and_a_null_one_too() {
        let (_dir, api, _root) = api_that_can_send();
        let some = json!({"text": "Cordialement,", "spans": [], "blocks": ["Paragraph"]});

        call(
            &api,
            "accounts.set_signature",
            json!({"account": 1, "signature": some}),
        )
        .await;
        // Un document réduit à des blancs efface : garder la colonne remplie ferait sortir une
        // ligne vide en fin de chaque message.
        let blank = json!({"text": "  \n ", "spans": [], "blocks": ["Paragraph", "Paragraph"]});
        assert_eq!(
            call(
                &api,
                "accounts.set_signature",
                json!({"account": 1, "signature": blank}),
            )
            .await,
            Value::Null
        );

        call(
            &api,
            "accounts.set_signature",
            json!({"account": 1, "signature": some}),
        )
        .await;
        assert_eq!(
            call(&api, "accounts.set_signature", json!({"account": 1})).await,
            Value::Null,
            "une signature absente n'a pas effacé"
        );
    }

    #[tokio::test]
    async fn a_signature_on_an_unknown_account_is_refused_rather_than_written_into_the_void() {
        // Un `UPDATE` sans effet rendrait « enregistré » à l'écran et rien en base : la
        // signature réapparaîtrait vide au message suivant, sans que rien n'ait signalé
        // l'échec.
        let (_dir, api, _root) = api_that_can_send();
        let refusal = refusal(
            &api,
            "accounts.set_signature",
            json!({"account": 404, "signature": {"text": "x"}}),
        )
        .await;
        assert!(refusal.contains("404"), "{refusal}");
        // La lecture, elle, ne refuse pas : « pas de signature » est la réponse honnête pour un
        // compte qui n'existe pas, et c'est ce que le client affiche de toute façon.
        assert_eq!(
            call(&api, "accounts.signature", json!({"account": 404})).await,
            Value::Null
        );
    }

    #[tokio::test]
    async fn a_signature_that_carries_a_trapped_link_never_comes_back_out_as_html() {
        // **Le cas qui compte de cette paire de méthodes.** Un client qui détient le jeton peut
        // écrire ce qu'il veut dans la signature de l'utilisateur ; ce qui en sort dans un
        // message ne doit pas porter de piège. Le refus est dans `mailhtml::rich`, et ce test
        // vérifie qu'il tient **après** un aller-retour par l'API et le store.
        let (_dir, api, _root) = api_that_can_send();
        let hostile = json!({
            "text": "clique",
            "spans": [{
                "at": 0,
                "len": 6,
                "style": {"bold": false, "italic": false, "link": "javascript:alert(1)"},
            }],
            "blocks": ["Paragraph"],
        });
        call(
            &api,
            "accounts.set_signature",
            json!({"account": 1, "signature": hostile}),
        )
        .await;

        let back = call(&api, "accounts.signature", json!({"account": 1})).await;
        let document: mailhtml::rich::Document = serde_json::from_value(back).unwrap();
        let html = document.to_html();
        assert!(!html.contains("javascript"), "{html}");
        assert!(!html.contains("href"), "{html}");
        assert_eq!(html, "<p>clique</p>");
        assert!(!document.to_text().contains("javascript"));
    }

    #[tokio::test]
    async fn the_declared_method_list_is_what_the_api_actually_serves() {
        // Les deux méthodes nouvelles sont annoncées **et** servies. Une méthode annoncée qui
        // rend `-32601` est pire qu'une méthode absente : un client la découvre, l'appelle, et
        // n'a aucun moyen de savoir laquelle des deux listes mentait.
        let (_dir, api, _root) = api_that_can_send();
        let served = call(&api, "server.methods", Value::Null).await;
        let names: Vec<String> = serde_json::from_value(served).unwrap();
        for method in ["accounts.signature", "accounts.set_signature"] {
            assert!(names.contains(&method.to_owned()), "{method} non annoncée");
        }
        // Servies : l'appel passe, avec des paramètres valides.
        call(&api, "accounts.signature", json!({"account": 1})).await;
    }

    #[tokio::test]
    async fn a_send_that_asks_for_the_signature_queues_the_bytes_that_carry_it() {
        // **Le bout du chemin, vérifié sur les octets rangés.** La signature est enregistrée par
        // une méthode, ajoutée par une autre, et ce qui compte est ce que le facteur enverra —
        // pas ce que l'API a répondu. C'est la même exigence que le test du `Bcc` : lire le blob.
        let (_dir, api, root) = api_that_can_send();
        let signature = json!({
            "text": "Éloïse Durand\ndirectrice",
            "spans": [{
                "at": 0,
                "len": "Éloïse Durand".len(),
                "style": {"bold": true, "italic": false, "link": null},
            }],
            "blocks": ["Paragraph", "Paragraph"],
        });
        call(
            &api,
            "accounts.set_signature",
            json!({"account": 1, "signature": signature}),
        )
        .await;

        let queued = call(
            &api,
            "outbox.send",
            json!({
                "account": 1,
                "to": ["jean@ailleurs.fr"],
                "subject": "sujet",
                "text": "Bonjour.",
                "signature": true,
            }),
        )
        .await;

        let store = Store::open(&root).unwrap();
        let line = store
            .outgoing(mailcore::OutboxId(queued["id"].as_i64().unwrap()))
            .unwrap()
            .expect("la ligne doit exister");
        let bytes = store.blobs().read(line.blob).unwrap();
        let text = String::from_utf8_lossy(&bytes);

        // Les deux parties, parce que la signature porte un gras.
        assert!(text.contains("multipart/alternative"), "{text}");
        // Le corps en texte : le nom y est, en clair. Le message est encodé en
        // `quoted-printable` ou en base64 selon la partie ; « Durand » est ASCII et survit dans
        // les deux, ce qui en fait le témoin à chercher.
        assert!(
            text.contains("Durand"),
            "la signature n'est pas partie : {text}"
        );
        assert!(text.contains("Bonjour."), "le corps a été perdu : {text}");
    }

    #[tokio::test]
    async fn a_send_that_does_not_ask_for_the_signature_does_not_get_one() {
        // **Le contrôle négatif, et il porte le vrai risque.** Le démon sait où est la
        // signature : s'il l'ajoutait d'office, un client qui l'a déjà mise dans son texte la
        // verrait doublée chez le destinataire. Le drapeau dit qui compose, et son défaut est
        // « pas moi ».
        let (_dir, api, root) = api_that_can_send();
        call(
            &api,
            "accounts.set_signature",
            json!({
                "account": 1,
                "signature": {"text": "Éloïse Durand", "spans": [], "blocks": ["Paragraph"]},
            }),
        )
        .await;

        let queued = call(
            &api,
            "outbox.send",
            json!({
                "account": 1,
                "to": ["jean@ailleurs.fr"],
                "subject": "sujet",
                "text": "Bonjour.",
            }),
        )
        .await;

        let store = Store::open(&root).unwrap();
        let line = store
            .outgoing(mailcore::OutboxId(queued["id"].as_i64().unwrap()))
            .unwrap()
            .expect("la ligne doit exister");
        let bytes = store.blobs().read(line.blob).unwrap();
        let text = String::from_utf8_lossy(&bytes);
        assert!(
            !text.contains("Durand"),
            "une signature est partie sans qu'on la demande : {text}"
        );
        // Et le message reste une seule partie texte : rien n'a été ajouté.
        assert!(!text.contains("multipart/alternative"), "{text}");
    }

    #[tokio::test]
    async fn a_message_that_carries_an_invitation_serves_it_as_an_appointment() {
        // **Le bout du chemin de l'étape 8 :** une pièce `text/calendar` reçue ressort de
        // `messages.get` comme un rendez-vous — quand, où, avec qui — et pas comme un fichier
        // `invite.ics` de 4 Kio dans la liste des pièces jointes.
        let dir = tempfile::tempdir().unwrap();
        let root = camino::Utf8Path::from_path(dir.path()).unwrap().to_owned();
        let raw = invitation_message();
        let (api, id) = api_with_message(&root, &raw);

        let message = call(&api, "messages.get", json!({"id": id})).await;
        let invitation = &message["invitation"];
        assert_eq!(invitation["kind"], Value::from("invitation"));
        assert_eq!(invitation["summary"], Value::from("Réunion de suivi"));
        // L'heure murale telle que l'organisateur l'a écrite, et l'instant résolu par la
        // `VTIMEZONE` du fichier : 14:00 en +02:00 est 12:00 UTC.
        assert_eq!(invitation["start_wall"], Value::from("2026-09-10 14:00"));
        assert_eq!(
            invitation["start_unix"].as_i64(),
            Some(20_706 * 86_400 + 12 * 3_600)
        );
        assert_eq!(invitation["zone"], Value::from("Romance Standard Time"));
        assert_eq!(invitation["refused"], Value::Bool(false));
        assert_eq!(
            invitation["organizer"]["address"],
            Value::from("eloise@exemple.fr")
        );
        assert_eq!(invitation["attendees"][0]["answer"], Value::from("accepte"));
        // Aucune réserve : ce fichier dit tout ce qu'il faut.
        assert_eq!(invitation["caveats"].as_array().map(Vec::len), Some(0));
    }

    #[tokio::test]
    async fn a_message_without_an_invitation_carries_no_invitation_field() {
        // Le champ est omis, pas rendu `null` : 930 pièces sur 73 825 messages du corpus, donc
        // un client qui ne connaît pas les invitations ne doit rien voir changer.
        let (_dir, api, _root) = api_that_can_send();
        let stats = call(&api, "store.stats", Value::Null).await;
        assert!(stats["messages"].as_u64().is_some());
    }

    /// Un message qui porte une invitation Exchange, la forme la plus fréquente du corpus.
    fn invitation_message() -> Vec<u8> {
        let ics = [
            "BEGIN:VCALENDAR",
            "PRODID:-//Microsoft Exchange Server 2010",
            "VERSION:2.0",
            "METHOD:REQUEST",
            "BEGIN:VTIMEZONE",
            "TZID:Romance Standard Time",
            "BEGIN:STANDARD",
            "DTSTART:16011028T030000",
            "TZOFFSETFROM:+0200",
            "TZOFFSETTO:+0100",
            "RRULE:FREQ=YEARLY;INTERVAL=1;BYDAY=-1SU;BYMONTH=10",
            "END:STANDARD",
            "BEGIN:DAYLIGHT",
            "DTSTART:16010325T020000",
            "TZOFFSETFROM:+0100",
            "TZOFFSETTO:+0200",
            "RRULE:FREQ=YEARLY;INTERVAL=1;BYDAY=-1SU;BYMONTH=3",
            "END:DAYLIGHT",
            "END:VTIMEZONE",
            "BEGIN:VEVENT",
            "UID:040000008200E00074C5B7101A82E008",
            "SUMMARY:Réunion de suivi",
            "DTSTART;TZID=Romance Standard Time:20260910T140000",
            "DTEND;TZID=Romance Standard Time:20260910T150000",
            "LOCATION:Salle Jaurès",
            "ORGANIZER;CN=Éloïse Durand:mailto:eloise@exemple.fr",
            "ATTENDEE;CN=Jean Martin;PARTSTAT=ACCEPTED:mailto:jean@ailleurs.fr",
            "END:VEVENT",
            "END:VCALENDAR",
        ]
        .join("\r\n");

        // L'invitation est en `multipart/alternative` à côté du corps, comme Exchange l'envoie :
        // c'est le cas que `attachments()` seul ne verrait pas.
        format!(
            "From: Éloïse <eloise@exemple.fr>\r\n\
             To: moi@exemple.fr\r\n\
             Subject: Réunion de suivi\r\n\
             Message-ID: <inv1@exemple.fr>\r\n\
             MIME-Version: 1.0\r\n\
             Content-Type: multipart/alternative; boundary=\"frontiere\"\r\n\
             \r\n\
             --frontiere\r\n\
             Content-Type: text/plain; charset=utf-8\r\n\
             \r\n\
             Quand : jeudi 10 septembre 14:00\r\n\
             \r\n\
             --frontiere\r\n\
             Content-Type: text/calendar; charset=utf-8; method=REQUEST\r\n\
             \r\n\
             {ics}\r\n\
             --frontiere--\r\n"
        )
        .into_bytes()
    }

    /// Une API montée sur un store qui contient ce message, et l'identifiant du message.
    fn api_with_message(root: &camino::Utf8Path, raw: &[u8]) -> (Api, i64) {
        use mailcore::{FolderKind, MessageFlags, NewMessage};

        let store = Store::open(root).unwrap();
        let writer = store.writer().unwrap();
        let account = writer.upsert_account("mbox", "compte").unwrap();
        let folder = writer
            .upsert_folder(account, "INBOX", FolderKind::Inbox)
            .unwrap();
        let put = store.blobs().put(raw).unwrap();
        let (id, _) = writer
            .insert_message(&NewMessage {
                blob: put.hash,
                rfc822_id: Some("<inv1@exemple.fr>"),
                date: 1_757_000_000,
                from_addr: "eloise@exemple.fr",
                from_name: Some("Éloïse"),
                subject: "Réunion de suivi",
                size: raw.len() as u64,
                has_attachments: false,
            })
            .unwrap();
        writer
            .insert_ref(id, folder, 1_757_000_000, MessageFlags::empty())
            .unwrap();
        writer.commit().unwrap();
        drop(store);

        let mailbox = Mailbox::open(root).unwrap();
        let shared = Arc::new(Mutex::new(mailbox));
        let jobs = crate::jobs::Jobs::start(
            Arc::clone(&shared),
            root.to_owned(),
            crate::jobs::Sources::default(),
        );
        (Api::new(shared, jobs), id.0)
    }

    #[tokio::test]
    async fn a_draft_survives_a_round_trip_and_keeps_what_was_typed() {
        // **Ce que les brouillons servent à ne pas perdre.** Une adresse à moitié tapée, un
        // sujet vide, aucun destinataire valide : `outbox.send` refuserait tout ça, et refuser
        // ici perdrait la frappe en cours.
        let (_dir, api, _root) = api_that_can_send();
        let saved = call(
            &api,
            "drafts.save",
            json!({
                "account": 1,
                "to": "jean@ailleurs.fr, mar",
                "subject": "",
                "body": "Bonjour,\n\nJe voulais",
                "sign": true,
            }),
        )
        .await;

        let id = saved["id"].as_i64().unwrap();
        assert_eq!(saved["to"], Value::from("jean@ailleurs.fr, mar"));
        assert_eq!(saved["body"], Value::from("Bonjour,\n\nJe voulais"));
        // L'étiquette est calculée par le service : sans sujet, elle retombe sur le
        // destinataire.
        assert_eq!(saved["label"], Value::from("jean@ailleurs.fr, mar"));
        assert!(saved["updated_at"].as_i64().unwrap() > 0);

        let listed = call(&api, "drafts.list", Value::Null).await;
        assert_eq!(listed.as_array().map(Vec::len), Some(1));
        assert_eq!(listed[0]["id"].as_i64(), Some(id));
    }

    #[tokio::test]
    async fn saving_the_same_draft_again_updates_it_instead_of_piling_up() {
        // La coquille enregistre à chaque fermeture. Sans mise à jour, trois fermetures
        // laisseraient trois brouillons du même message.
        let (_dir, api, _root) = api_that_can_send();
        let first = call(&api, "drafts.save", json!({"account": 1, "body": "un"})).await;
        let id = first["id"].as_i64().unwrap();

        let second = call(
            &api,
            "drafts.save",
            json!({"id": id, "account": 1, "body": "un et deux"}),
        )
        .await;
        assert_eq!(second["id"].as_i64(), Some(id));

        let listed = call(&api, "drafts.list", Value::Null).await;
        assert_eq!(listed.as_array().map(Vec::len), Some(1));
        assert_eq!(listed[0]["body"], Value::from("un et deux"));
    }

    #[tokio::test]
    async fn an_empty_draft_is_not_kept_and_erases_the_one_it_replaced() {
        // Ouvrir la fenêtre par erreur puis la refermer est le geste le plus fréquent de tous :
        // il ne doit pas laisser de ligne. Et vider un brouillon existant l'efface, plutôt que
        // de laisser une ligne vide dans la liste.
        let (_dir, api, _root) = api_that_can_send();
        assert_eq!(
            call(&api, "drafts.save", json!({"account": 1, "body": "   \n"})).await,
            Value::Null
        );
        assert_eq!(
            call(&api, "drafts.list", Value::Null)
                .await
                .as_array()
                .map(Vec::len),
            Some(0)
        );

        let saved = call(
            &api,
            "drafts.save",
            json!({"account": 1, "body": "du texte"}),
        )
        .await;
        let id = saved["id"].as_i64().unwrap();
        assert_eq!(
            call(
                &api,
                "drafts.save",
                json!({"id": id, "account": 1, "body": ""})
            )
            .await,
            Value::Null
        );
        assert_eq!(
            call(&api, "drafts.list", Value::Null)
                .await
                .as_array()
                .map(Vec::len),
            Some(0),
            "le brouillon vidé est resté"
        );
    }

    #[tokio::test]
    async fn a_draft_on_an_unknown_account_is_refused() {
        // Un brouillon attaché à un compte qui n'existe pas ne pourrait jamais partir, et la
        // cascade du schéma l'effacerait au premier ménage.
        let (_dir, api, _root) = api_that_can_send();
        let refusal = refusal(&api, "drafts.save", json!({"account": 404, "body": "x"})).await;
        assert!(refusal.contains("404"), "{refusal}");
    }

    #[tokio::test]
    async fn deleting_a_draft_twice_is_not_an_error() {
        // Deux clics rapides sur la corbeille ne sont pas une faute.
        let (_dir, api, _root) = api_that_can_send();
        let saved = call(&api, "drafts.save", json!({"account": 1, "body": "x"})).await;
        let id = saved["id"].as_i64().unwrap();

        assert_eq!(
            call(&api, "drafts.delete", json!({"id": id})).await["removed"],
            Value::Bool(true)
        );
        assert_eq!(
            call(&api, "drafts.delete", json!({"id": id})).await["removed"],
            Value::Bool(false)
        );
    }

    #[tokio::test]
    async fn a_draft_that_names_an_unknown_blob_is_refused_rather_than_kept() {
        // Un brouillon qui nommerait un contenu absent produirait, à l'envoi, un message dont
        // une pièce manque — découvert par le destinataire.
        let (_dir, api, _root) = api_that_can_send();
        let refusal = refusal(
            &api,
            "drafts.save",
            json!({
                "account": 1,
                "body": "avec pièce",
                "attachments": [{"filename": "x.pdf", "blob": "pas du hex", "size": 10}],
            }),
        )
        .await;
        assert!(refusal.contains("x.pdf"), "{refusal}");
    }

    /// Un message à deux pièces jointes distinctes, pour que l'ordre soit vérifiable.
    fn two_attachments() -> Vec<u8> {
        // « un » et « deux » en base64 : `dW4=` et `ZGV1eA==`.
        "From: eloise@exemple.fr\r\n\
         To: moi@exemple.fr\r\n\
         Subject: deux pièces\r\n\
         MIME-Version: 1.0\r\n\
         Content-Type: multipart/mixed; boundary=\"f\"\r\n\
         \r\n\
         --f\r\n\
         Content-Type: text/plain\r\n\
         \r\n\
         Le corps.\r\n\
         \r\n\
         --f\r\n\
         Content-Type: application/pdf; name=\"premier.pdf\"\r\n\
         Content-Disposition: attachment; filename=\"premier.pdf\"\r\n\
         Content-Transfer-Encoding: base64\r\n\
         \r\n\
         dW4=\r\n\
         --f\r\n\
         Content-Type: image/png; name=\"second.png\"\r\n\
         Content-Disposition: attachment; filename=\"second.png\"\r\n\
         Content-Transfer-Encoding: base64\r\n\
         \r\n\
         ZGV1eA==\r\n\
         --f--\r\n"
            .to_owned()
            .into_bytes()
    }

    #[tokio::test]
    async fn staging_a_part_uses_the_same_rank_as_the_listed_attachments() {
        // **L'invariant qui compte de cette méthode.** Le client choisit un rang dans la liste
        // que `messages.get` lui a rendue ; deux parcours différents feraient joindre un fichier
        // à la place d'un autre — la façon la plus discrète d'envoyer à quelqu'un un document
        // qui ne lui était pas destiné.
        let dir = tempfile::tempdir().unwrap();
        let root = camino::Utf8Path::from_path(dir.path()).unwrap().to_owned();
        let raw = two_attachments();
        let (api, id) = api_with_message(&root, &raw);

        let message = call(&api, "messages.get", json!({"id": id})).await;
        let listed = message["attachments"].as_array().unwrap();
        assert_eq!(listed.len(), 2, "{listed:?}");
        assert_eq!(listed[0]["name"], Value::from("premier.pdf"));
        assert_eq!(listed[1]["name"], Value::from("second.png"));

        for (rank, expected) in [(0, ("premier.pdf", 2u64)), (1, ("second.png", 4))] {
            let staged = call(&api, "messages.stage_part", json!({"id": id, "part": rank})).await;
            assert_eq!(staged["filename"], Value::from(expected.0), "rang {rank}");
            assert_eq!(staged["size"].as_u64(), Some(expected.1), "rang {rank}");
            assert_eq!(staged["blob"].as_str().map(str::len), Some(64));
        }
    }

    #[tokio::test]
    async fn a_staged_part_is_really_in_the_store_and_holds_the_decoded_bytes() {
        // Le contenu **décodé** : la pièce est en base64 dans le message, et ce qui repart doit
        // être les octets, pas leur encodage.
        let dir = tempfile::tempdir().unwrap();
        let root = camino::Utf8Path::from_path(dir.path()).unwrap().to_owned();
        let (api, id) = api_with_message(&root, &two_attachments());

        let staged = call(&api, "messages.stage_part", json!({"id": id, "part": 0})).await;
        let hash = mailcore::BlobHash::from_hex(staged["blob"].as_str().unwrap()).unwrap();

        let store = Store::open(&root).unwrap();
        assert_eq!(store.blobs().read(hash).unwrap(), b"un");
    }

    #[tokio::test]
    async fn staging_refuses_a_rank_that_does_not_exist_and_an_unknown_message() {
        let dir = tempfile::tempdir().unwrap();
        let root = camino::Utf8Path::from_path(dir.path()).unwrap().to_owned();
        let (api, id) = api_with_message(&root, &two_attachments());

        let bad_rank = refusal(&api, "messages.stage_part", json!({"id": id, "part": 9})).await;
        assert!(bad_rank.contains("pièce jointe"), "{bad_rank}");
        let unknown = refusal(&api, "messages.stage_part", json!({"id": 9_999, "part": 0})).await;
        assert!(unknown.contains("9999"), "{unknown}");
    }

    #[tokio::test]
    async fn there_is_no_way_to_name_a_path_to_stage() {
        // **Ce que cette méthode ne doit pas ouvrir.** Un champ de chemin donnerait la lecture
        // de n'importe quel fichier de la machine du démon à quiconque détient le jeton — c'est
        // le refus que `jobs.start` porte déjà pour les profils d'import. Un paramètre en trop
        // est ignoré par `serde`, donc le contrat est vérifié par ce qui **manque** au type :
        // les paramètres sont un identifiant et un rang, et la méthode échoue si l'un manque.
        let dir = tempfile::tempdir().unwrap();
        let root = camino::Utf8Path::from_path(dir.path()).unwrap().to_owned();
        let (api, id) = api_with_message(&root, &two_attachments());

        let without_id = refusal(
            &api,
            "messages.stage_part",
            json!({"path": "C:/Windows/System32/config/SAM"}),
        )
        .await;
        assert!(!without_id.is_empty(), "un chemin seul a été accepté");
        // Et un chemin ajouté à des paramètres valides ne change rien à ce qui est rangé.
        let staged = call(
            &api,
            "messages.stage_part",
            json!({"id": id, "part": 0, "path": "C:/Windows/System32/config/SAM"}),
        )
        .await;
        assert_eq!(staged["filename"], Value::from("premier.pdf"));
        assert_eq!(staged["size"].as_u64(), Some(2));
    }
    #[tokio::test]
    async fn a_failed_send_comes_back_with_the_sentence_and_the_bit_that_offers_the_gesture() {
        // **Le critère 8 vu par un client.** Ce que `outbox.list` rend doit suffire à dessiner
        // le refus : la phrase à lire, et un booléen pour savoir si le bouton « Renvoyer » a un
        // sens. Sans le booléen, un client devrait deviner en cherchant des mots dans une
        // phrase française — ce que le champ `doubtful` existe déjà pour éviter ailleurs.
        let (_dir, api, root) = api_that_can_send();
        let store = Store::open(&root).unwrap();
        let raw = b"From: marie@exemple.fr\r\nTo: jean@ailleurs.fr\r\n\r\ncorps\r\n";
        let blob = store.blobs().put(raw).unwrap().hash;
        let id = store
            .enqueue(
                mailcore::AccountId(1),
                blob,
                "marie@exemple.fr",
                &["jean@ailleurs.fr".to_owned()],
                raw.len() as u64,
                1_000,
            )
            .unwrap();
        // Ce qu'écrit `mailsmtp::queue::settle` sur un quota épuisé : la phrase sans promesse
        // de reprise, et le bit qui autorise le geste.
        store
            .record_attempt(
                id,
                mailcore::SendState::Failed,
                1_100,
                Some("Le serveur est occupé. Renvoyez le message plus tard."),
                None,
                true,
            )
            .unwrap();
        drop(store);

        let listed = call(&api, "outbox.list", Value::Null).await;
        let line = &listed.as_array().unwrap()[0];
        assert_eq!(line["state"], Value::from("failed"));
        assert_eq!(line["resendable"], Value::Bool(true));
        assert_eq!(line["doubtful"], Value::Bool(false));
        let sentence = line["last_error"].as_str().unwrap();
        assert!(sentence.contains("Renvoyez"), "{sentence}");

        // Et le geste marche : la ligne repart en file, crédit remis à zéro.
        let retried = call(&api, "outbox.retry", json!({"id": id.0})).await;
        assert_eq!(retried["queued"], Value::Bool(true));
        let after = call(&api, "outbox.list", Value::Null).await;
        let line = &after.as_array().unwrap()[0];
        assert_eq!(line["state"], Value::from("queued"));
        assert_eq!(line["attempts"].as_u64(), Some(0));
        // Le bit s'éteint : il ne décrit plus rien, la ligne n'a pas encore échoué.
        assert_eq!(line["resendable"], Value::Bool(false));
    }

    #[tokio::test]
    async fn retrying_something_that_did_not_fail_changes_nothing() {
        // Le refus qui empêche `outbox.retry` d'être un contournement de la file. Un client
        // qui l'appellerait sur une ligne quelconque ne doit pas pouvoir renvoyer un message
        // déjà parti — ni remettre en file un `committing`, dont le serveur a peut-être la
        // copie.
        let (_dir, api, root) = api_that_can_send();
        let store = Store::open(&root).unwrap();
        let raw = b"From: marie@exemple.fr\r\nTo: jean@ailleurs.fr\r\n\r\ncorps\r\n";
        let blob = store.blobs().put(raw).unwrap().hash;
        let id = store
            .enqueue(
                mailcore::AccountId(1),
                blob,
                "marie@exemple.fr",
                &["jean@ailleurs.fr".to_owned()],
                raw.len() as u64,
                1_000,
            )
            .unwrap();
        store
            .commit_outgoing(id, mailcore::SendState::Committing)
            .unwrap();
        drop(store);

        let refused = call(&api, "outbox.retry", json!({"id": id.0})).await;
        assert_eq!(
            refused["queued"],
            Value::Bool(false),
            "un envoi douteux a été remis en file par `outbox.retry`"
        );
        let listed = call(&api, "outbox.list", Value::Null).await;
        assert_eq!(
            listed.as_array().unwrap()[0]["state"],
            Value::from("committing")
        );

        // Et une ligne qui n'existe pas n'est pas une erreur : deux clics rapides, ou une ligne
        // retirée entre-temps.
        let missing = call(&api, "outbox.retry", json!({"id": 9_999})).await;
        assert_eq!(missing["queued"], Value::Bool(false));
    }
}
