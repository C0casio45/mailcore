//! La page de paramètres : les comptes, et par où ils lisent et envoient.
//!
//! ## Pourquoi une page et pas seulement `mail account …`
//!
//! Le secret. En ligne de commande il passe par un terminal — donc par un historique, une table
//! de processus, un `script` qui enregistre la session. Ici il va du clavier au trousseau du
//! système sans que personne d'autre le voie, et surtout sans passer par quelqu'un qui écrit la
//! commande à la place de l'utilisateur.
//!
//! ## Rien de tout ça ne passe par l'API
//!
//! C'est la règle de `Link::stage`, appliquée à un secret au lieu d'un chemin. Une méthode
//! `accounts.add` voudrait dire qu'un client qui détient le jeton du démon peut écrire dans le
//! **trousseau de la machine du démon** — et que le secret traverse le JSON-RPC pour y arriver.
//! La CLI refuse déjà `mail account …` en mode `--daemon`, pour cette raison exacte ; la page la
//! refuse pareillement, et le dit.
//!
//! Même raisonnement pour la lecture : `accounts.list` ne rend **ni hôte ni port** — critère 7
//! de `docs/PHASE-3.md` — et ce n'est pas une omission à corriger. Un client distant n'a aucune
//! raison de connaître l'infrastructure de lecture de quelqu'un. La page, elle, est dans le même
//! processus que le store, donc elle lit directement.
//!
//! ## Le trousseau survit au store
//!
//! Découvert le 2026-09-11 : le store de production était vide, et les cinq entrées du
//! Credential Manager étaient intactes. Redéclarer un compte OAuth2 aurait refait un
//! consentement complet — navigateur, identifiant client, écran du fournisseur — pour aboutir
//! au jeton déjà rangé. La page demande donc `mailauth::session::has_secret` **avant** de
//! demander quoi que ce soit à l'utilisateur, et un compte dont le secret est là se réadopte
//! sans qu'on lui demande rien.

/// Un compte, tel que la page a besoin de le voir.
///
/// Local, et sans équivalent dans `mailapi::dto` : voir l'en-tête du module.
#[derive(Debug, Clone)]
pub struct AccountDetail {
    /// L'identifiant, celui que `mail account list` affiche.
    pub id: i64,
    /// Le nom lisible.
    pub name: String,
    /// Le serveur de lecture, `hôte:port`.
    pub server: String,
    /// L'identifiant présenté au serveur.
    pub username: String,
    /// `tls` ou `starttls`.
    pub security: String,
    /// `password` ou `oauth2`.
    pub auth: String,
    /// Vrai si le trousseau porte de quoi authentifier ce compte.
    ///
    /// **Sa présence, jamais sa valeur.** Voir `mailauth::is_stored`.
    pub secret: bool,
    /// Le serveur de soumission, quand il y en a un. `None` veut dire « ce compte ne peut pas
    /// envoyer », et c'est un état valide.
    pub submission: Option<String>,
    /// Faux quand la synchronisation est en pause.
    pub enabled: bool,
}

/// Ce que le formulaire de déclaration a rassemblé.
///
/// ## `Debug` est écrit à la main, et c'est une règle du projet
///
/// `#[derive(Debug)]` sur un type qui porte un secret est le **défaut dangereux** : il a déjà
/// affiché le mot de passe de `Credential`, des deux côtés du projet. Ici, `Request` dérive
/// `Debug` et une demande qui échoue peut se retrouver dans un journal.
#[derive(Clone, Default)]
pub struct AccountForm {
    /// Nom d'hôte du serveur IMAP.
    pub host: String,
    /// Port, vide pour le défaut du mode de chiffrement.
    pub port: String,
    /// L'identifiant présenté au serveur, en général l'adresse complète.
    pub username: String,
    /// `tls` ou `starttls`. Il n'y a pas de mode en clair.
    pub security: String,
    /// `password` ou `oauth2`.
    pub auth: String,
    /// Le secret, **seulement s'il faut l'écrire**. Vide quand le trousseau en a déjà un.
    pub secret: String,
}

impl std::fmt::Debug for AccountForm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Tout sauf le secret, dont même la longueur ne sort pas — elle dit quelque chose.
        f.debug_struct("AccountForm")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("username", &self.username)
            .field("security", &self.security)
            .field("auth", &self.auth)
            .field(
                "secret",
                &if self.secret.is_empty() {
                    ""
                } else {
                    "<masqué>"
                },
            )
            .finish()
    }
}

impl AccountForm {
    /// Un formulaire vide, prérempli avec les défauts les plus sûrs.
    #[must_use]
    pub fn new() -> Self {
        Self {
            security: "tls".to_owned(),
            auth: "password".to_owned(),
            ..Self::default()
        }
    }
}

/// Ce que le formulaire de consentement OAuth2 a rassemblé.
///
/// ## L'identifiant client n'est pas un secret, le secret client est traité comme s'il l'était
///
/// L'identifiant apparaît dans l'URL de consentement, que le navigateur affiche : le masquer
/// serait du théâtre. Le secret client, chez Google, n'en est pas un au sens cryptographique —
/// une application installée le porte dans son binaire, et c'est la raison d'être de PKCE. Mais
/// il a la forme d'un identifiant, et demander à l'utilisateur de distinguer deux régimes de
/// confidentialité pour deux valeurs collées l'une à l'autre dans la même console est le genre
/// de nuance qui finit par coûter une fuite. Un seul régime, le plus strict. C'est déjà le choix
/// de `mail account add`, repris tel quel.
#[derive(Clone, Default)]
pub struct ConsentForm {
    /// Le serveur IMAP, qui détermine le fournisseur.
    pub host: String,
    /// L'identifiant du compte, passé en `login_hint`.
    pub username: String,
    /// L'identifiant client, obtenu dans la console du fournisseur.
    pub client_id: String,
    /// Le secret client. Vide pour un fournisseur qui n'en impose pas — Microsoft, en
    /// application publique. En envoyer un vide fait refuser l'échange, d'où le `filter`.
    pub client_secret: String,
    /// Port de bouclage épinglé, pour un fournisseur qui compare l'URI de redirection.
    pub redirect_port: String,
}

impl std::fmt::Debug for ConsentForm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConsentForm")
            .field("host", &self.host)
            .field("username", &self.username)
            .field("client_id", &self.client_id)
            .field(
                "client_secret",
                &if self.client_secret.is_empty() {
                    ""
                } else {
                    "<masqué>"
                },
            )
            .field("redirect_port", &self.redirect_port)
            .finish()
    }
}

/// Ce que le formulaire de soumission a rassemblé.
///
/// Pas de secret ici, et c'est le point : celui de l'envoi est celui de la lecture, déjà dans le
/// trousseau. `Debug` peut donc être dérivé, contrairement à [`AccountForm`].
#[derive(Debug, Clone, Default)]
pub struct SubmissionForm {
    /// Le compte concerné.
    pub account: i64,
    /// Nom d'hôte du serveur de soumission.
    pub host: String,
    /// Port, vide pour le défaut du mode de chiffrement.
    pub port: String,
    /// `tls` (465) ou `starttls` (587).
    pub security: String,
}

/// Ce que la page relève pendant le dessin, et que l'interface exécute après.
///
/// Le même motif que `Pending` dans la fenêtre de rédaction : la page reçoit de quoi dessiner,
/// pas de quoi décider.
#[derive(Debug, Default)]
pub enum Outcome {
    /// Rien à faire.
    #[default]
    Idle,
    /// Déclarer, ou redéclarer, un compte.
    Declare(Box<AccountForm>),
    /// Écrire le serveur de soumission d'un compte.
    Submission(Box<SubmissionForm>),
    /// Oublier le secret d'un compte et mettre sa synchronisation en pause.
    ///
    /// Il n'y a pas de variante « relire » : la page lit à l'ouverture et après chaque
    /// écriture, et en mode embarqué la coquille est le seul écrivain. Un bouton pour un cas
    /// qui ne se produit pas est du bruit dans un écran qui parle de secrets.
    Forget(i64),
    /// Moissonner ce compte.
    ///
    /// ## Pourquoi il est ici, alors que ce n'est pas un paramètre
    ///
    /// Parce que c'est l'étape suivante, et qu'elle n'existait nulle part dans la coquille :
    /// un compte déclaré sans dossiers ne montre rien, et l'utilisateur devait ouvrir un
    /// terminal pour `mail sync`. Une page de configuration qui laisse son travail à moitié
    /// fait renvoie à l'outil qu'elle était censée remplacer.
    ///
    /// Celle-ci **passe** par l'API, contrairement à ses voisines : `jobs.start` existe, elle
    /// ne touche à aucun secret, et le démon sait déjà la servir.
    Sync(i64),
    /// Obtenir un jeton OAuth2 neuf : navigateur, consentement, échange.
    Consent(Box<ConsentForm>),
}

/// L'état de la page.
#[derive(Debug, Default)]
pub struct Page {
    /// Les comptes, tels que le dernier relevé les a vus.
    pub accounts: Vec<AccountDetail>,
    /// Le formulaire de déclaration, quand il est ouvert.
    pub declaring: Option<AccountForm>,
    /// Le formulaire de soumission, quand il est ouvert.
    pub submitting: Option<SubmissionForm>,
    /// Le formulaire de consentement OAuth2, quand il est ouvert.
    pub consenting: Option<ConsentForm>,
    /// L'URL de consentement, quand un consentement est en cours.
    ///
    /// **Affichée, pas seulement ouverte dans le navigateur.** L'ouverture peut échouer — pas
    /// de navigateur par défaut, session distante — et cette adresse est alors le seul moyen de
    /// finir. C'est la même raison qui la fait imprimer par `mail account add`.
    pub consent_url: Option<String>,
    /// Le compte dont on vient de demander l'oubli, en attente de confirmation.
    ///
    /// Une confirmation, parce que le geste efface un secret : le refaire demande un mot de
    /// passe applicatif ou un consentement complet.
    pub forgetting: Option<i64>,
    /// Ce que la dernière écriture a répondu.
    pub message: Option<String>,
    /// Vrai si la dernière réponse était un refus.
    pub failed: bool,
    /// Vrai quand le service est distant : la page ne peut alors rien écrire.
    pub remote: bool,
}

/// L'hôte de soumission le plus probable pour un hôte de lecture.
///
/// **Une suggestion, jamais une valeur écrite.** `imap.gmail.com` → `smtp.gmail.com` est vrai,
/// et la déduction marche jusqu'au jour où elle échoue : ce jour-là elle envoie le message au
/// mauvais endroit. La page la propose dans un bouton que l'utilisateur clique, ce qui en fait
/// son choix et non le nôtre — c'est la même règle que `mail account submission`.
#[must_use]
pub fn suggested_submission(imap_host: &str) -> Option<String> {
    let rest = imap_host.strip_prefix("imap.")?;
    Some(format!("smtp.{rest}"))
}

impl Page {
    /// Dessine la page et rend ce qu'elle demande.
    pub fn show(&mut self, ui: &mut egui::Ui) -> Outcome {
        let mut outcome = Outcome::Idle;

        if self.remote {
            // Un refus qui dit pourquoi, comme celui de `Link::stage`.
            ui.colored_label(
                crate::theme::accent(ui.ctx()),
                "Ce service est distant. Les comptes se déclarent sur la machine du démon : \
                 c'est son trousseau qui porte le secret, et un secret ne traverse pas le \
                 réseau pour aller s'y ranger.",
            );
            ui.add_space(8.0);
        }

        if let Some(message) = &self.message {
            let color = if self.failed {
                crate::theme::accent(ui.ctx())
            } else {
                ui.visuals().text_color()
            };
            ui.colored_label(color, message);
            ui.add_space(8.0);
        }

        self.list(ui, &mut outcome);
        ui.separator();
        self.declaration(ui, &mut outcome);
        self.submission(ui, &mut outcome);
        self.consent(ui, &mut outcome);
        outcome
    }

    /// Le formulaire de consentement OAuth2, et l'attente du navigateur.
    fn consent(&mut self, ui: &mut egui::Ui, outcome: &mut Outcome) {
        if let Some(url) = &self.consent_url {
            ui.separator();
            ui.strong("Consentement en cours");
            ui.label(
                "Votre navigateur devrait s'être ouvert. Autorisez l'accès, puis revenez ici : \
                 la fenêtre se met à jour toute seule.",
            );
            ui.add_space(4.0);
            ui.weak("Si rien ne s'est ouvert, collez cette adresse :");
            // Sélectionnable et copiable, comme la source d'un message : un lien qu'on ne peut
            // pas copier ne sert à rien dans le cas précis où on en a besoin.
            let mut shown = url.clone();
            ui.add(
                egui::TextEdit::multiline(&mut shown)
                    .font(egui::TextStyle::Monospace)
                    .desired_width(f32::INFINITY)
                    .desired_rows(3),
            );
            ui.add_space(4.0);
            ui.weak("L'attente expire au bout de cinq minutes.");
            return;
        }

        let Some(form) = &mut self.consenting else {
            return;
        };

        ui.separator();
        ui.strong(format!("Consentement OAuth2 — {}", form.username));
        ui.label(
            "Le fournisseur ne délivre de jeton qu'à une application déclarée, et cette \
             application est la vôtre.",
        );
        ui.add_space(4.0);
        // Les étapes, ici plutôt que dans un lien : quelqu'un qui en est là n'a pas envie de
        // chercher. C'est le texte de `mail account add`, qui a déjà servi.
        ui.weak(
            "Google : console.cloud.google.com → créer un projet → activer l'API Gmail → \
             écran de consentement externe, votre adresse en « utilisateur de test » → \
             Identifiants → ID client OAuth → « Application de bureau ».",
        );

        ui.add_space(6.0);
        egui::Grid::new("mailcore-consent-form")
            .num_columns(2)
            .spacing([8.0, 6.0])
            .show(ui, |ui| {
                ui.label("Identifiant client");
                ui.vertical(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut form.client_id)
                            .desired_width(f32::INFINITY),
                    );
                    // Dit, parce que l'inverse se suppose : un champ à côté d'un champ masqué a
                    // l'air d'un secret qu'on aurait oublié de masquer.
                    ui.weak("Ce n'est pas un secret : il apparaît dans l'URL de consentement.");
                });
                ui.end_row();

                ui.label("Secret client");
                ui.vertical(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut form.client_secret)
                            .password(true)
                            .desired_width(f32::INFINITY),
                    );
                    ui.weak("Vide si le fournisseur n'en impose pas — Microsoft, par exemple.");
                });
                ui.end_row();

                ui.label("Port de redirection");
                ui.horizontal(|ui| {
                    ui.text_edit_singleline(&mut form.redirect_port);
                    ui.weak("vide dans le cas normal");
                });
                ui.end_row();
            });

        ui.add_space(6.0);
        let mut cancel = false;
        ui.horizontal(|ui| {
            let ready = !form.client_id.trim().is_empty();
            if ui
                .add_enabled(ready, egui::Button::new("Ouvrir le navigateur"))
                .on_disabled_hover_text("L'identifiant client, au minimum.")
                .clicked()
            {
                *outcome = Outcome::Consent(Box::new(form.clone()));
            }
            cancel = ui.button("Annuler").clicked();
        });
        if cancel {
            self.consenting = None;
        }
    }

    /// La liste des comptes et ce qu'on peut en faire.
    fn list(&mut self, ui: &mut egui::Ui, outcome: &mut Outcome) {
        if self.accounts.is_empty() {
            ui.label("Aucun compte déclaré.");
        }

        let rows = self.accounts.clone();
        for account in &rows {
            ui.horizontal_wrapped(|ui| {
                ui.strong(format!("#{} {}", account.id, account.name));
                if !account.enabled {
                    ui.colored_label(crate::theme::accent(ui.ctx()), "en pause");
                }
            });
            ui.horizontal_wrapped(|ui| {
                ui.add_space(12.0);
                ui.weak(format!(
                    "{} · {} · {} · {}",
                    account.server, account.username, account.security, account.auth
                ));
            });
            ui.horizontal_wrapped(|ui| {
                ui.add_space(12.0);
                // **La présence du secret, jamais sa valeur.** Et elle est dite dans les deux
                // sens : un compte sans secret ne synchronisera pas, et le silence là-dessus
                // ferait chercher la panne du mauvais côté.
                if account.secret {
                    ui.weak("secret dans le trousseau");
                } else {
                    ui.colored_label(crate::theme::accent(ui.ctx()), "aucun secret enregistré");
                }
                // **Le geste que la phrase appelle, à portée de clic.** Un compte OAuth2 sans
                // jeton — ou dont le jeton a été révoqué — n'a qu'une issue, et la nommer sans
                // l'offrir renverrait au terminal que cette page remplace. Offert aussi quand
                // un jeton existe : un jeton périmé est présent et inutilisable, et c'est le
                // cas le plus fréquent après une semaine.
                if !self.remote
                    && account.auth == "oauth2"
                    && ui
                        .small_button(if account.secret {
                            "Refaire le consentement…"
                        } else {
                            "Obtenir un jeton…"
                        })
                        .on_hover_text(
                            "Ouvre le navigateur chez le fournisseur. Un jeton de \
                             rafraîchissement d'un écran de consentement en mode « test » \
                             expire au bout de sept jours.",
                        )
                        .clicked()
                {
                    self.consenting = Some(ConsentForm {
                        host: server_host(&account.server).to_owned(),
                        username: account.username.clone(),
                        ..ConsentForm::default()
                    });
                }
            });
            ui.horizontal_wrapped(|ui| {
                ui.add_space(12.0);
                match &account.submission {
                    Some(server) => ui.weak(format!("envoi par {server}")),
                    // Pas une erreur : un compte qui ne peut pas envoyer est un état valide, et
                    // c'est exactement ce que `can_send` dit à la fenêtre de rédaction.
                    None => ui.weak("aucun serveur d'envoi — ce compte ne peut pas envoyer"),
                };
            });

            ui.horizontal_wrapped(|ui| {
                ui.add_space(12.0);
                let writable = !self.remote;
                // La moisson est offerte même à distance : c'est une tâche de fond du démon,
                // qui lit son propre trousseau. La même exception que `mail sync --daemon`.
                let syncable = account.secret && account.enabled;
                if ui
                    .add_enabled(syncable, egui::Button::new("Synchroniser"))
                    .on_disabled_hover_text(if account.secret {
                        "Ce compte est en pause : le redéclarer le réactive."
                    } else {
                        "Aucun secret enregistré : la connexion échouerait."
                    })
                    .on_hover_text(
                        "Découvre les dossiers et moissonne. La progression s'affiche dans \
                         « Importer… ».",
                    )
                    .clicked()
                {
                    *outcome = Outcome::Sync(account.id);
                }
                if ui
                    .add_enabled(writable, egui::Button::new("Serveur d'envoi…"))
                    .clicked()
                {
                    self.submitting = Some(SubmissionForm {
                        account: account.id,
                        host: String::new(),
                        port: String::new(),
                        security: "starttls".to_owned(),
                    });
                }
                if self.forgetting == Some(account.id) {
                    ui.colored_label(crate::theme::accent(ui.ctx()), "Oublier le secret ?");
                    if ui
                        .button("Oui, oublier")
                        .on_hover_text(
                            "Efface le secret du trousseau et met la synchronisation en pause. \
                             Le courrier déjà téléchargé reste. Refaire ce compte demandera un \
                             mot de passe ou un consentement complet.",
                        )
                        .clicked()
                    {
                        *outcome = Outcome::Forget(account.id);
                        self.forgetting = None;
                    }
                    if ui.button("Annuler").clicked() {
                        self.forgetting = None;
                    }
                } else if ui
                    .add_enabled(writable, egui::Button::new("Oublier…"))
                    .clicked()
                {
                    self.forgetting = Some(account.id);
                }
            });
            ui.add_space(6.0);
        }
    }

    /// Le formulaire de déclaration.
    fn declaration(&mut self, ui: &mut egui::Ui, outcome: &mut Outcome) {
        let Some(form) = &mut self.declaring else {
            if ui
                .add_enabled(!self.remote, egui::Button::new("Déclarer un compte…"))
                .clicked()
            {
                self.declaring = Some(AccountForm::new());
            }
            return;
        };

        ui.strong("Déclarer un compte");
        egui::Grid::new("mailcore-account-form")
            .num_columns(2)
            .spacing([8.0, 6.0])
            .show(ui, |ui| {
                ui.label("Serveur IMAP");
                ui.text_edit_singleline(&mut form.host);
                ui.end_row();

                ui.label("Port");
                ui.horizontal(|ui| {
                    ui.text_edit_singleline(&mut form.port);
                    ui.weak("vide = 993 en tls, 143 en starttls");
                });
                ui.end_row();

                ui.label("Identifiant");
                ui.text_edit_singleline(&mut form.username);
                ui.end_row();

                ui.label("Chiffrement");
                ui.horizontal(|ui| {
                    // Pas de mode en clair dans la liste : un IMAP non chiffré transporte le
                    // mot de passe sur le réseau, et il n'existe pas de configuration où on
                    // l'accepterait. L'absence est la garantie.
                    ui.selectable_value(&mut form.security, "tls".to_owned(), "tls");
                    ui.selectable_value(&mut form.security, "starttls".to_owned(), "starttls");
                });
                ui.end_row();

                ui.label("Mécanisme");
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut form.auth, "password".to_owned(), "mot de passe");
                    ui.selectable_value(&mut form.auth, "oauth2".to_owned(), "oauth2");
                });
                ui.end_row();

                ui.label("Secret");
                ui.vertical(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut form.secret)
                            .password(true)
                            .hint_text("laisser vide si le trousseau en a déjà un"),
                    );
                    ui.weak(
                        "Il va directement au trousseau du système. Il n'est écrit dans aucun \
                         fichier du profil, et il ne traverse aucune API.",
                    );
                });
                ui.end_row();
            });

        if form.auth == "oauth2" {
            ui.add_space(4.0);
            // Dit **avant** le clic, et pas en refus après : la page ne sait pas encore si le
            // trousseau a le jeton, mais elle sait qu'elle ne saura pas en obtenir un.
            ui.weak(
                "oauth2 : ce formulaire réadopte un jeton déjà dans le trousseau. Obtenir un \
                 jeton neuf demande un consentement dans le navigateur, qui n'est pas encore \
                 dans cette page — `mail account add --auth oauth2 --client-id …` le fait.",
            );
        }

        ui.add_space(6.0);
        // Le drapeau est relevé pendant le dessin et appliqué après : la fermeture emprunte
        // `form`, qui vit dans `self.declaring`, et effacer le champ pendant qu'on le dessine
        // n'est pas plus permis ici qu'ailleurs. Le même motif que `Pending`.
        let mut cancel = false;
        ui.horizontal(|ui| {
            let ready = !form.host.trim().is_empty() && !form.username.trim().is_empty();
            if ui
                .add_enabled(ready, egui::Button::new("Enregistrer"))
                .on_disabled_hover_text("Un serveur et un identifiant, au minimum.")
                .clicked()
            {
                *outcome = Outcome::Declare(Box::new(form.clone()));
            }
            cancel = ui.button("Annuler").clicked();
        });
        if cancel {
            self.declaring = None;
        }
    }

    /// Le formulaire du serveur de soumission.
    fn submission(&mut self, ui: &mut egui::Ui, outcome: &mut Outcome) {
        let Some(form) = &mut self.submitting else {
            return;
        };
        let known = self.accounts.iter().find(|it| it.id == form.account);

        ui.separator();
        ui.strong(format!("Serveur d'envoi du compte #{}", form.account));

        if let Some(account) = known
            && let Some(suggestion) = suggested_submission(server_host(&account.server))
        {
            ui.horizontal_wrapped(|ui| {
                ui.weak("Hôte plausible pour ce fournisseur :");
                // **Un bouton, pas un préremplissage.** La valeur n'est écrite que si
                // l'utilisateur la choisit : deviner le serveur d'envoi enverrait le message au
                // mauvais endroit le jour où la déduction est fausse.
                if ui.button(&suggestion).clicked() {
                    form.host.clone_from(&suggestion);
                }
            });
        }

        egui::Grid::new("mailcore-submission-form")
            .num_columns(2)
            .spacing([8.0, 6.0])
            .show(ui, |ui| {
                ui.label("Serveur");
                ui.text_edit_singleline(&mut form.host);
                ui.end_row();

                ui.label("Port");
                ui.horizontal(|ui| {
                    ui.text_edit_singleline(&mut form.port);
                    ui.weak("vide = 465 en tls, 587 en starttls — jamais 25");
                });
                ui.end_row();

                ui.label("Chiffrement");
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut form.security, "tls".to_owned(), "tls");
                    ui.selectable_value(&mut form.security, "starttls".to_owned(), "starttls");
                });
                ui.end_row();
            });

        ui.add_space(4.0);
        ui.weak(
            "Le secret est celui de la lecture, déjà dans le trousseau : rien à retaper. Le mode \
             de chiffrement, lui, n'est pas déduit — le deviner le rétrograderait sans le dire.",
        );

        ui.add_space(6.0);
        let mut cancel = false;
        ui.horizontal(|ui| {
            let ready = !form.host.trim().is_empty();
            if ui
                .add_enabled(ready, egui::Button::new("Enregistrer"))
                .on_disabled_hover_text("Un serveur, au minimum.")
                .clicked()
            {
                *outcome = Outcome::Submission(Box::new(form.clone()));
            }
            cancel = ui.button("Annuler").clicked();
        });
        if cancel {
            self.submitting = None;
        }
    }
}

/// L'hôte seul, dans un `hôte:port`.
fn server_host(server: &str) -> &str {
    server.split_once(':').map_or(server, |(host, _)| host)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_form_never_prints_its_secret() {
        // La règle du projet, et elle a déjà été enfreinte : `#[derive(Debug)]` affichait le
        // mot de passe de `Credential`, des deux côtés.
        let form = AccountForm {
            host: "imap.exemple.fr".to_owned(),
            port: String::new(),
            username: "marie@exemple.fr".to_owned(),
            security: "tls".to_owned(),
            auth: "password".to_owned(),
            secret: "correct-cheval-pile-agrafe".to_owned(),
        };
        let shown = format!("{form:?}");
        assert!(!shown.contains("correct-cheval-pile-agrafe"));
        assert!(shown.contains("<masqué>"));
        // Le contrôle inverse : le reste est bien affiché, sinon le test passerait sur un
        // `Debug` qui ne dirait rien du tout et ne prouverait pas que le masque est ciblé.
        assert!(shown.contains("imap.exemple.fr"));
        assert!(shown.contains("marie@exemple.fr"));
    }

    #[test]
    fn an_empty_secret_is_not_masked_because_there_is_nothing_to_mask() {
        let form = AccountForm::new();
        let shown = format!("{form:?}");
        assert!(!shown.contains("<masqué>"));
    }

    #[test]
    fn a_request_carrying_a_form_does_not_leak_it_either() {
        // Ce qui compte vraiment : `Request` dérive `Debug`, et une demande qui échoue peut
        // être journalisée. Le masque doit tenir **à travers** l'enveloppe.
        let form = AccountForm {
            secret: "tr0p-secret".to_owned(),
            ..AccountForm::new()
        };
        let request = crate::worker::Request::DeclareAccount(Box::new(form));
        assert!(!format!("{request:?}").contains("tr0p-secret"));
    }

    #[test]
    fn a_consent_form_masks_its_client_secret_but_not_its_client_id() {
        // La distinction est volontaire et mérite d'être verrouillée : l'identifiant client
        // apparaît dans l'URL de consentement, que le navigateur affiche — le masquer serait du
        // théâtre, et ferait croire à un secret de plus à protéger.
        let form = ConsentForm {
            host: "imap.gmail.com".to_owned(),
            username: "marie@exemple.fr".to_owned(),
            client_id: "123-abc.apps.googleusercontent.com".to_owned(),
            client_secret: "GOCSPX-jamais-affiche".to_owned(),
            redirect_port: String::new(),
        };
        let shown = format!("{form:?}");
        assert!(!shown.contains("GOCSPX-jamais-affiche"));
        assert!(shown.contains("<masqué>"));
        assert!(shown.contains("123-abc.apps.googleusercontent.com"));
    }

    #[test]
    fn a_request_carrying_a_consent_form_does_not_leak_it_either() {
        let form = ConsentForm {
            client_secret: "GOCSPX-tr0p-secret".to_owned(),
            ..ConsentForm::default()
        };
        let request = crate::worker::Request::Consent(Box::new(form));
        assert!(!format!("{request:?}").contains("GOCSPX-tr0p-secret"));
    }

    #[test]
    fn the_suggested_submission_host_is_only_a_suggestion_for_hosts_it_recognises() {
        assert_eq!(
            suggested_submission("imap.gmail.com").as_deref(),
            Some("smtp.gmail.com")
        );
        // Et rien pour un hôte qui ne suit pas la convention : une suggestion inventée est
        // pire que pas de suggestion, parce qu'elle a l'air d'un renseignement.
        assert_eq!(suggested_submission("mail.perso.invalid"), None);
        assert_eq!(suggested_submission("courrier.exemple.fr"), None);
    }

    #[test]
    fn the_host_of_a_server_line_drops_its_port() {
        assert_eq!(server_host("imap.gmail.com:993"), "imap.gmail.com");
        assert_eq!(server_host("imap.gmail.com"), "imap.gmail.com");
    }

    // ------------------------------------------------------------------
    // La page dessinée, sans fenêtre.
    //
    // `egui::__run_test_ui` monte un contexte sans police et exécute le code de dessin. C'est
    // ce qui manquait au relevé du matin : la page avait quatre cents lignes de dessin et
    // aucune couverture, alors qu'une panique y serait un plantage sous les yeux de
    // l'utilisateur, dans l'écran qu'il ouvre en premier.
    // ------------------------------------------------------------------

    /// Dessine la page une fois et rend ce qu'elle a demandé.
    fn draw(page: &mut Page) -> Outcome {
        let mut outcome = Outcome::Idle;
        egui::__run_test_ui(|ui| {
            outcome = page.show(ui);
        });
        outcome
    }

    fn an_account() -> AccountDetail {
        AccountDetail {
            id: 1,
            name: "marie@exemple.fr".to_owned(),
            server: "imap.gmail.com:993".to_owned(),
            username: "marie@exemple.fr".to_owned(),
            security: "tls".to_owned(),
            auth: "oauth2".to_owned(),
            secret: true,
            submission: Some("smtp.gmail.com:587 starttls".to_owned()),
            enabled: true,
        }
    }

    #[test]
    fn every_state_of_the_page_draws_without_panicking() {
        // Un état par branche du dessin, y compris ceux qu'on ne voit qu'après un échec.
        let mut states: Vec<Page> = vec![
            Page::default(),
            Page {
                remote: true,
                ..Page::default()
            },
            Page {
                accounts: vec![an_account()],
                ..Page::default()
            },
            Page {
                // Le compte au pire état : en pause, sans secret, sans serveur d'envoi — donc
                // les trois libellés d'alerte et les deux boutons grisés à la fois.
                accounts: vec![AccountDetail {
                    secret: false,
                    enabled: false,
                    submission: None,
                    ..an_account()
                }],
                forgetting: Some(1),
                message: Some("un refus".to_owned()),
                failed: true,
                ..Page::default()
            },
            Page {
                declaring: Some(AccountForm::new()),
                ..Page::default()
            },
            Page {
                // Le formulaire en `oauth2` porte un avertissement de plus.
                declaring: Some(AccountForm {
                    auth: "oauth2".to_owned(),
                    ..AccountForm::new()
                }),
                ..Page::default()
            },
            Page {
                accounts: vec![an_account()],
                submitting: Some(SubmissionForm {
                    account: 1,
                    security: "starttls".to_owned(),
                    ..SubmissionForm::default()
                }),
                ..Page::default()
            },
            Page {
                // Un serveur de soumission demandé pour un compte que la page ne connaît pas :
                // la suggestion n'a alors rien à quoi se raccrocher.
                submitting: Some(SubmissionForm {
                    account: 99,
                    ..SubmissionForm::default()
                }),
                ..Page::default()
            },
            Page {
                consenting: Some(ConsentForm::default()),
                ..Page::default()
            },
            Page {
                consent_url: Some("https://accounts.example/auth?x=1".to_owned()),
                ..Page::default()
            },
        ];
        for page in &mut states {
            draw(page);
        }
    }

    #[test]
    fn a_page_that_nobody_clicked_asks_for_nothing() {
        // **La propriété qui compte.** Une page qui rendrait `Declare` sans clic écrirait un
        // compte — et un secret — à chaque image. Le relevé est fait sur tous les états, parce
        // qu'un formulaire ouvert est précisément celui qui porte le bouton dangereux.
        let mut states = [
            Page::default(),
            Page {
                accounts: vec![an_account()],
                declaring: Some(AccountForm::new()),
                submitting: Some(SubmissionForm::default()),
                consenting: Some(ConsentForm::default()),
                forgetting: Some(1),
                ..Page::default()
            },
        ];
        for page in &mut states {
            assert!(
                matches!(draw(page), Outcome::Idle),
                "la page a demandé quelque chose sans qu'on clique"
            );
        }
    }

    #[test]
    fn drawing_the_page_twice_leaves_its_forms_where_they_were() {
        // Le dessin ne décide pas : il relève. Un formulaire qui se refermerait tout seul à la
        // deuxième image ferait disparaître la saisie en cours sous les doigts.
        let mut page = Page {
            declaring: Some(AccountForm {
                host: "imap.gmail.com".to_owned(),
                ..AccountForm::new()
            }),
            consenting: Some(ConsentForm::default()),
            ..Page::default()
        };
        draw(&mut page);
        draw(&mut page);
        assert_eq!(
            page.declaring.as_ref().map(|it| it.host.as_str()),
            Some("imap.gmail.com")
        );
        assert!(page.consenting.is_some());
    }
}
