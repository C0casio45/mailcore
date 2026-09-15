//! La moisson : décider quoi demander, puis l'écrire dans le store.
//!
//! ## Le plan est séparé de son exécution, et c'est délibéré
//!
//! [`plan`] est une **fonction pure** : l'état local, l'état distant, et la réponse à « que
//! faut-il demander ? ». Aucun réseau, aucun store. Les cas qui décident de la correction d'une
//! synchronisation — `UIDVALIDITY` changé, `CONDSTORE` absent, rien de neuf — s'y testent
//! exhaustivement, sans monter quoi que ce soit.
//!
//! L'exécution, elle, se teste contre `mailfake`. Les deux ensembles de tests ne se
//! remplacent pas : un plan juste mal exécuté et un plan faux bien exécuté produisent le même
//! symptôme, et il faut pouvoir les distinguer.
//!
//! ## Ce qu'un serveur sans `CONDSTORE` coûte
//!
//! Il n'y a **pas de « rien à faire »** sans `CONDSTORE`. Un serveur qui ne suit pas les
//! `MODSEQ` ne sait pas dire « rien n'a changé » : un drapeau modifié ne bouge ni `UIDNEXT`,
//! ni le nombre de messages. La seule façon de le savoir est de redemander **tous** les
//! drapeaux, et de comparer.
//!
//! C'est le prix, et il est nommé plutôt que caché : le critère 3 de `docs/PHASE-2.md` — une
//! synchronisation incrémentale sans rien de neuf en moins de 5 s et **0 octet écrit** — est
//! alors atteint parce que la comparaison ne trouve rien à écrire, pas parce qu'on a sauté la
//! demande.
//!
//! ## Les suppressions, et les trois chemins pour les voir
//!
//! `CONDSTORE` seul **ne signale pas les purges** : un message effacé ne bouge ni `UIDNEXT`, ni
//! le nombre de messages annoncé par `EXISTS`… sauf que si, justement, il fait baisser
//! `EXISTS`. Les trois chemins, du meilleur au pire :
//!
//! 1. **`QRESYNC`** (RFC 7162). L'`EXAMINE` rend directement `VANISHED (EARLIER)` : les purges
//!    depuis un `MODSEQ` donné, en une ligne ou en aucune. C'est le chemin idéal, et **aucun
//!    des serveurs du corpus réel ne l'offre** — mesuré le 2026-09-08 : ni Gmail, ni le Dovecot
//!    de `mail.perso.invalid` ne l'annoncent. Il est implémenté et testé contre `mailfake` ;
//!    il attend un serveur qui le serve.
//!
//! 2. **`EXISTS` contre le nombre de copies connues.** Si le plan dit qu'aucun UID n'est
//!    apparu et que le serveur compte autant de messages que nous, aucune purge n'a pu avoir
//!    lieu : une purge ferait baisser `EXISTS`, et rien ne peut la compenser puisque rien
//!    n'est arrivé. Gratuit — le nombre est déjà dans la réponse à l'`EXAMINE`.
//!
//! 3. **Le balayage** `UID FETCH 1:* (UID)`, quand les deux premiers ne s'appliquent pas. Une
//!    ligne par message, à chaque passage. Mesuré à 67 s sur un compte de 51 496 messages où
//!    rien n'avait changé, ce qui est ce que le deuxième chemin a supprimé.
//!
//! ## La reprise d'un dossier interrompu
//!
//! Une ligne de `remote_uids` n'existe que si le corps correspondant a été écrit, dans la même
//! transaction. Elle est donc la preuve qu'on possède le message, et **aucun corps déjà
//! possédé n'est redemandé** — quel que soit le plan.
//!
//! Ça change ce que coûte une coupure. Avant, un dossier coupé repartait en moisson complète,
//! qui commençait par effacer ce qu'il savait : mesuré après une coupure réelle de Gmail, 25 028
//! corps retéléchargés pour trouver 2 messages nouveaux. L'effacement est maintenant réservé au
//! seul cas qui l'exige — un `UIDVALIDITY` changé, où les UID ne désignent plus rien.

use std::collections::HashMap;
use std::io::{Read, Write};

use mailcore::{
    Account, AccountId, AuthKind, FolderId, MessageFlags, Progress, RemoteCopy, Store, SyncState,
};

use crate::client::{Client, Fetched};
use crate::error::Result;

/// Nombre d'UID demandés par lot de corps.
///
/// ## Pourquoi 100 et pas 1, ni tout
///
/// Un UID par commande ferait un aller-retour réseau par message : 100 000 allers-retours sur
/// le plus gros dossier du corpus. Tout en une commande ne coûte rien de plus en mémoire — la
/// lecture est streamante — mais **une coupure perdrait le lot entier**, et un lot entier est
/// alors le dossier.
///
/// 100 est le compromis : cent allers-retours pour 10 000 messages, et une coupure ne coûte
/// au pire que cent messages à retélécharger.
///
/// **Ce n'est qu'une des deux bornes.** Voir [`BATCH_BYTES`] : compter les messages ne borne
/// pas la mémoire, et l'affirmation contraire a tenu jusqu'au 2026-09-09.
const BATCH: usize = 100;

/// Octets de corps qu'un lot accepte de tenir en mémoire.
///
/// ## Pourquoi la borne en nombre ne suffisait pas
///
/// La réserve du critère 2 disait : « le lot de cent messages borne la mémoire par
/// construction ». C'est faux — il borne le **nombre**, pas les octets. Cent messages de 3 Ko
/// font 300 Ko ; cent messages de 25 Mo font 2,5 Go, et le plafond par message étant de 128 Mio
/// le pire cas théorique dépasse les douze gigaoctets.
///
/// Mesuré le 2026-09-09, en levant justement cette réserve : **266 Mio de crête** sur une
/// moisson complète de `contact@perso.invalid`, dont les messages font 580 Ko en moyenne. Le
/// seuil de 500 Mo était tenu, à moitié près, sur deux mille messages — et il n'y avait aucune
/// raison de croire qu'il le resterait sur un compte aux pièces jointes plus grosses.
///
/// 32 Mio : assez pour qu'un lot de cent messages ordinaires passe entier — le corpus réel fait
/// 62 Ko de moyenne, donc 6 Mio le lot — et assez peu pour qu'un dossier de pièces jointes
/// lourdes se découpe au lieu de gonfler.
///
/// ## La taille vient du serveur, et c'est une annonce
///
/// `RFC822.SIZE` arrive **dans le même aller-retour** que la liste des UID, donc la borne ne
/// coûte rien. Mais c'est ce que le serveur déclare : s'il mentait, la borne redeviendrait
/// celle du nombre. Le plafond par message reste la défense contre un littéral surdimensionné.
const BATCH_BYTES: u64 = 32 * 1024 * 1024;

/// Ce que la moisson va demander au serveur.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plan {
    /// Tout : jamais synchronisé, ou `UIDVALIDITY` a changé.
    ///
    /// **L'état local doit être remis à zéro avant**, sinon les anciens UID restent liés à des
    /// messages que le serveur numérote autrement.
    Full,

    /// `CONDSTORE` : les drapeaux modifiés depuis `since`, et les corps au-delà de `new_from`.
    Incremental {
        /// Le `MODSEQ` de référence.
        since: u64,
        /// Le premier UID dont le corps manque.
        new_from: u32,
    },

    /// Sans `CONDSTORE` : **tous** les drapeaux, et les corps au-delà de `new_from`.
    Rescan {
        /// Le premier UID dont le corps manque.
        new_from: u32,
    },

    /// Rien à demander. Atteignable uniquement avec `CONDSTORE` — voir le module.
    UpToDate,
}

/// Pourquoi une moisson complète.
///
/// Journalisé, jamais deviné : « le dossier a été retéléchargé » sans raison est le genre
/// d'événement qu'on ne peut pas expliquer six mois plus tard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FullReason {
    /// Aucun passage précédent.
    Never,
    /// `UIDVALIDITY` a changé : tous les UID connus sont invalides.
    UidvalidityChanged,
    /// Le serveur n'annonce pas d'`UIDVALIDITY` : aucun UID n'est fiable d'un passage à
    /// l'autre.
    NoUidvalidity,
}

/// Le bilan d'un passage.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Report {
    /// Messages dont le corps a été téléchargé.
    pub fetched: usize,
    /// Contenus **nouveaux** dans le store.
    pub stored: usize,
    /// Contenus déjà présents — la dédup à l'œuvre.
    pub duplicates: usize,
    /// Copies enregistrées ou mises à jour.
    pub copies: usize,
    /// Copies disparues côté serveur.
    pub vanished: usize,
    /// Références dont les drapeaux ont été recalculés.
    pub reflagged: usize,
    /// Vrai si la moisson a été complète.
    pub full: bool,
}

impl Report {
    /// Vrai si rien n'a été écrit.
    ///
    /// C'est la formulation testable du critère 3 : « 0 octet écrit ».
    #[must_use]
    pub const fn wrote_nothing(&self) -> bool {
        self.stored == 0 && self.copies == 0 && self.vanished == 0
    }
}

/// Décide de la moisson. **Fonction pure.**
///
/// `condstore` dit si le serveur a réellement activé `CONDSTORE` — pas s'il l'a annoncé. Un
/// serveur qui annonce puis refuse doit emprunter le chemin de repli, et c'est pour ça que le
/// paramètre est le résultat de l'`ENABLE` et non une capacité.
#[must_use]
pub fn plan(
    local: &SyncState,
    remote: &crate::Selected,
    condstore: bool,
) -> (Plan, Option<FullReason>) {
    // Sans `UIDVALIDITY` distant, aucun UID n'est fiable d'un passage à l'autre : le serveur
    // ne promet rien. Une moisson complète à chaque fois est le seul comportement correct.
    let Some(remote_validity) = remote.uidvalidity else {
        return (Plan::Full, Some(FullReason::NoUidvalidity));
    };
    let Some(local_validity) = local.uidvalidity else {
        return (Plan::Full, Some(FullReason::Never));
    };
    if local_validity != remote_validity {
        return (Plan::Full, Some(FullReason::UidvalidityChanged));
    }

    // `UIDNEXT` local absent alors que la validité est connue : l'état est incohérent, et
    // deviner serait pire que refaire. Ça arrive après une remise à zéro partielle.
    let new_from = local.uidnext.unwrap_or(1);

    if condstore
        && let (Some(local_modseq), Some(remote_modseq)) =
            (local.highest_modseq, remote.highest_modseq)
    {
        let nothing_new = remote.uidnext.is_none_or(|it| it <= new_from);
        if remote_modseq <= local_modseq && nothing_new {
            return (Plan::UpToDate, None);
        }
        return (
            Plan::Incremental {
                since: local_modseq,
                new_from,
            },
            None,
        );
    }

    (Plan::Rescan { new_from }, None)
}

/// Un dossier découvert côté serveur.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Discovered {
    /// Le dossier tel que le store le connaît.
    pub folder: FolderId,
    /// Le nom **en octets**, à renvoyer au serveur.
    pub remote_name: Vec<u8>,
    /// Le chemin affiché, décodé.
    pub path: String,
}

/// Demande au serveur quels dossiers existent, et les crée dans le store.
///
/// ## Le nom voyage en deux exemplaires, et il le faut
///
/// `remote_name` est la suite d'octets du serveur : c'est elle qui repart dans un `EXAMINE`.
/// `path` est son décodage UTF-7 modifié, pour l'affichage — et il peut être approximatif
/// sans conséquence, parce que le protocole ne le voit jamais. Voir [`crate::mutf7`].
///
/// ## Ce qui est écarté, et pourquoi
///
/// - **`\Noselect`** : un nœud de hiérarchie sans contenu, comme le `[Gmail]` de Gmail.
///   L'`EXAMINE` rendrait un `NO`, ce qui ferait échouer une synchronisation par ailleurs
///   correcte ;
/// - **un nom vide**, qui ne désigne rien qu'on puisse ouvrir.
///
/// Un dossier écarté est **compté et journalisé**, jamais ignoré en silence : un dossier
/// manquant dans la liste est du courrier perdu de vue, et il faut pouvoir savoir pourquoi.
///
/// ## Les dossiers qui ont disparu du serveur restent
///
/// La découverte ajoute, elle ne retire pas. Supprimer un dossier local parce qu'il n'est plus
/// annoncé retirerait les références qu'il porte — donc ferait disparaître du courrier de la
/// liste — sur la foi d'un `LIST` qui peut être incomplet parce que le serveur était en cours
/// de démarrage. Le nettoyage sera une action explicite, jamais une conséquence d'une sync.
///
/// # Errors
///
/// Voir [`Client::command`], plus [`crate::Error::Store`].
pub fn discover<S: Read + Write>(
    client: &mut Client<S>,
    store: &Store,
    account: AccountId,
) -> Result<Vec<Discovered>> {
    let listed = client.list()?;
    let mut out = Vec::with_capacity(listed.len());
    let mut skipped = 0_usize;

    let writer = store.writer()?;
    for mailbox in &listed {
        if !mailbox.selectable() {
            tracing::debug!(
                path = %crate::mutf7::decode(&mailbox.name),
                "dossier non sélectionnable écarté"
            );
            skipped += 1;
            continue;
        }
        if mailbox.name.is_empty() {
            tracing::warn!("dossier au nom vide écarté");
            skipped += 1;
            continue;
        }

        let path = display_path(mailbox);
        let folder = writer.upsert_folder(account, &path, mailbox.kind(&path))?;
        // Le nom du serveur est écrit tout de suite, **sans toucher aux compteurs** : c'est
        // `harvest` qui les remplira. Un `set_sync_state` complet ici remettrait à zéro
        // l'`UIDVALIDITY` d'un dossier déjà synchronisé, donc déclencherait une moisson
        // complète à chaque découverte.
        writer.set_remote_name(folder, &mailbox.name)?;
        out.push(Discovered {
            folder,
            remote_name: mailbox.name.clone(),
            path,
        });
    }
    writer.commit()?;

    tracing::info!(
        account = account.0,
        found = out.len(),
        skipped,
        "dossiers découverts"
    );
    Ok(out)
}

/// Le chemin affiché d'une boîte : décodé, et hiérarchisé avec des `/`.
///
/// ## Pourquoi le séparateur est normalisé
///
/// Les serveurs n'utilisent pas le même : `/` chez la plupart, `.` chez Courier, `\` chez de
/// vieux Exchange. Le store, lui, a une convention unique — `folders.path` est séparé par `/`,
/// et `FolderKind::guess` compare le dernier segment après avoir découpé sur `/`.
///
/// Sans normalisation, un `INBOX.Corbeille` de Courier ne serait pas reconnu comme une
/// corbeille, et l'arborescence s'afficherait à plat.
///
/// Un `/` **déjà présent dans un nom** dont le séparateur est autre chose devient donc un
/// faux niveau de hiérarchie. C'est un cas rare et sans dégât — un dossier au mauvais endroit
/// dans l'arbre affiché — alors que l'inverse, ne pas normaliser, casse la détection de rôle
/// sur tous les serveurs qui n'utilisent pas `/`.
fn display_path(mailbox: &crate::client::Listed) -> String {
    let decoded = crate::mutf7::decode(&mailbox.name);
    match mailbox.delimiter {
        Some(b'/') | None => decoded,
        Some(separator) => decoded.replace(separator as char, "/"),
    }
}

/// Le bilan d'un compte entier.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AccountReport {
    /// Dossiers découverts et sélectionnables.
    pub folders: usize,
    /// Corps téléchargés, tous dossiers confondus.
    pub fetched: usize,
    /// Contenus nouveaux dans le store.
    pub stored: usize,
    /// Contenus déjà présents — la dédup à l'œuvre.
    pub duplicates: usize,
    /// Copies disparues côté serveur.
    pub vanished: usize,
    /// Dossiers dont le `LIST-STATUS` a montré qu'il n'y avait rien à faire.
    ///
    /// Ce ne sont pas des dossiers ratés : ce sont ceux dont l'aller-retour `EXAMINE` a été
    /// évité. Le compteur existe parce qu'un raccourci qu'on ne peut pas voir agir est un
    /// raccourci qu'on ne peut pas déboguer — voir [`sync_account_over`].
    pub skipped: usize,
    /// Ce qui a échoué, un message par dossier fautif.
    ///
    /// **Un dossier qui échoue n'arrête pas le compte.** Un dossier au nom exotique, une boîte
    /// qu'un administrateur a rendue inaccessible : le reste doit se synchroniser. Les échecs
    /// remontent ici plutôt qu'en `Err`, parce que « neuf dossiers sur dix » est un résultat et
    /// pas une erreur.
    pub failures: Vec<String>,
    /// Vrai si le passage a été interrompu à la demande.
    pub cancelled: bool,
}

impl AccountReport {
    /// La part du reçu qui était déjà dans le store, en pourcentage.
    ///
    /// Le pari de la phase 1, affiché à chaque passage. Rend `None` quand rien n'a été reçu :
    /// un taux calculé sur zéro serait un chiffre inventé.
    #[must_use]
    pub fn dedup_ratio(&self) -> Option<f64> {
        if self.fetched == 0 {
            return None;
        }
        #[allow(clippy::cast_precision_loss)]
        Some(self.duplicates as f64 / self.fetched as f64 * 100.0)
    }
}

/// Synchronise un compte entier sur une connexion **déjà authentifiée**.
///
/// ## Pourquoi le client est un paramètre
///
/// La même raison que pour [`Client`] : cette fonction est celle que les tests exercent contre
/// `mailfake`, qui parle en clair. [`crate::connect`] refuse — à juste titre — un serveur non
/// chiffré, donc une fonction qui établirait elle-même la connexion serait intestable.
///
/// [`sync_account`] est la couche au-dessus : elle chiffre, s'authentifie, puis appelle
/// celle-ci.
///
/// ## L'annulation est vérifiée entre les dossiers **et** entre les lots
///
/// Entre les dossiers ne suffit pas : le plus gros dossier du corpus réel fait 1,4 Go, et une
/// annulation qui attendrait la fin de celui-là n'en serait pas une. La progression est donc
/// passée jusqu'à [`harvest_watched`], qui la consulte à chaque lot de cent messages.
///
/// # Errors
///
/// Seulement ce qui empêche de continuer : un `LIST` refusé, un store illisible. Un dossier
/// qui échoue est rangé dans [`AccountReport::failures`].
pub fn sync_account_over<S: Read + Write>(
    client: &mut Client<S>,
    store: &Store,
    account: AccountId,
    progress: &Progress,
) -> Result<AccountReport> {
    // **La liste d'avant authentification n'est pas la bonne.** Un serveur a le droit
    // d'annoncer plus une fois connecté, et Dovecot le fait. Sans cette ligne, le compte
    // `contact@perso.invalid` se synchronisait sans `CONDSTORE`, sans `QRESYNC` et sans
    // `LIST-STATUS` — que son serveur annonce tous les trois — parce qu'on lui avait demandé
    // ses capacités trop tôt. Mesuré le 2026-09-08.
    //
    // Gratuit quand le serveur les a déjà données dans le `OK` de l'authentification, ce qui
    // est le cas courant.
    client.refresh_capabilities_if_silent()?;

    let folders = discover(client, store, account)?;

    // **Une seule fois, et avant le premier `EXAMINE`.** `ENABLE` n'est valide qu'en état
    // authentifié (RFC 5161 §3.1) ; après la première boîte sélectionnée, il est hors
    // protocole. `discover` ne fait qu'un `LIST`, qui ne sélectionne rien.
    let enabled = enable(client)?;
    tracing::info!(
        account = account.0,
        condstore = enabled.condstore,
        qresync = enabled.qresync,
        "extensions négociées"
    );

    let mut report = AccountReport {
        folders: folders.len(),
        ..AccountReport::default()
    };

    // **L'état de toutes les boîtes en une commande, quand le serveur sait le faire.**
    //
    // Un `EXAMINE` par dossier coûte un aller-retour par dossier, y compris pour les dossiers
    // vides et ceux où rien n'a changé. Mesuré le 2026-09-08 sur les cinq comptes réels : 260
    // ms l'aller-retour sur la plus lente des connexions, donc 4,8 s pour dix-huit dossiers,
    // contre 280 ms pour la même information en un seul `LIST … RETURN (STATUS …)`.
    //
    // Ce qui suit ne décide de rien de plus que ce que le chemin normal déciderait : les mêmes
    // nombres passent par la même fonction [`plan`], et le raccourci n'est pris que là où elle
    // répond `UpToDate`. C'est une économie d'aller-retour, pas une règle de synchronisation
    // en plus.
    let statuses = statuses(client, enabled);

    // Les dossiers sautés gardent leur état, mais pas leur date : `synced_at` doit dire quand
    // on a **vérifié**, pas quand on a écrit pour la dernière fois. Toutes les dates partent
    // dans une seule transaction à la fin, pour ne pas payer une écriture par dossier évité.
    let mut touched: Vec<(FolderId, SyncState)> = Vec::new();

    for folder in &folders {
        if progress.is_cancelled() {
            report.cancelled = true;
            tracing::info!(account = account.0, "synchronisation annulée");
            return Ok(report);
        }

        match up_to_date(store, folder, statuses.get(&folder.remote_name), enabled) {
            Ok(Some(state)) => {
                tracing::debug!(
                    folder = folder.folder.0,
                    path = %folder.path,
                    "à jour d'après LIST-STATUS : EXAMINE évité"
                );
                report.skipped += 1;
                touched.push((folder.folder, state));
                continue;
            }
            Ok(None) => {}
            // Une lecture du store qui échoue ici ne doit pas sauter le dossier : elle le
            // renvoie au chemin normal, qui refera la lecture et échouera pour de bon si le
            // store est vraiment cassé. Un raccourci ne doit jamais être la raison pour
            // laquelle du courrier n'arrive pas.
            Err(source) => tracing::warn!(
                folder = folder.folder.0,
                %source,
                "état local illisible avant le raccourci : moisson normale"
            ),
        }

        match harvest_watched(
            client,
            store,
            folder.folder,
            &folder.remote_name,
            enabled,
            progress,
        ) {
            Ok(one) => {
                report.fetched += one.fetched;
                report.stored += one.stored;
                report.duplicates += one.duplicates;
                report.vanished += one.vanished;
            }
            Err(source) => {
                tracing::warn!(dossier = %folder.path, %source, "dossier en échec");
                report.failures.push(format!("{} : {source}", folder.path));
            }
        }
    }

    if !touched.is_empty() {
        let writer = store.writer()?;
        for (folder, state) in &touched {
            writer.set_sync_state(*folder, state)?;
        }
        writer.commit()?;
    }

    report.cancelled = progress.is_cancelled();
    tracing::info!(
        account = account.0,
        folders = report.folders,
        skipped = report.skipped,
        "compte synchronisé"
    );
    Ok(report)
}

/// Demande l'état de toutes les boîtes en une commande, ou rend une carte vide.
///
/// ## Un échec ici ne coûte rien
///
/// Trois façons de ne rien obtenir, et toutes les trois se terminent pareil — par une carte
/// vide, donc par le chemin d'avant, dossier par dossier :
///
/// - le serveur n'annonce pas `LIST-STATUS` ;
/// - il l'annonce et refuse la commande — c'est [`mailfake::Fault::CapabilityThenRefusal`] et
///   ce n'est pas une hypothèse d'école ;
/// - il répond, mais pour une partie seulement des boîtes, ce que la RFC 5819 §2 l'autorise à
///   faire.
///
/// C'est la raison pour laquelle cette commande est envoyée **à part** du `LIST` de
/// [`discover`] plutôt que fondue dedans, ce qui aurait économisé un aller-retour de plus :
/// un `LIST-STATUS` refusé emporterait alors la découverte des dossiers avec lui. Un
/// aller-retour contre l'impossibilité de casser la découverte, le change est bon.
fn statuses<S: Read + Write>(
    client: &mut Client<S>,
    enabled: Enabled,
) -> HashMap<Vec<u8>, crate::Selected> {
    if !client.has("LIST-STATUS") {
        return HashMap::new();
    }

    // `HIGHESTMODSEQ` seulement si `CONDSTORE` a été **activé**, pas seulement annoncé : le
    // demander autrement est un `BAD` chez un serveur qui applique la RFC 7162 à la lettre, et
    // ce `BAD` emporterait l'état de toutes les autres boîtes.
    let items = if enabled.condstore {
        "MESSAGES UIDNEXT UIDVALIDITY HIGHESTMODSEQ"
    } else {
        "MESSAGES UIDNEXT UIDVALIDITY"
    };

    match client.list_status(items) {
        Ok(found) => {
            let out: HashMap<Vec<u8>, crate::Selected> = found
                .into_iter()
                .filter_map(|it| Some((it.name.clone(), it.as_selected()?)))
                .collect();
            tracing::info!(boites = out.len(), "état des boîtes en une commande");
            out
        }
        Err(source) => {
            tracing::info!(%source, "LIST-STATUS annoncé mais inutilisable : moisson dossier par dossier");
            HashMap::new()
        }
    }
}

/// Dit si un dossier peut être sauté, et rend l'état à réécrire si c'est le cas.
///
/// ## Le raccourci ne connaît aucune règle que la moisson ignore
///
/// La décision est prise par [`plan`], la même fonction, sur les mêmes quatre nombres —
/// `UIDVALIDITY`, `UIDNEXT`, `HIGHESTMODSEQ`, et le compte de messages. La seule différence est
/// d'où ils viennent : un `STATUS` groupé plutôt qu'un `EXAMINE` par dossier. Si `plan` répond
/// autre chose que `UpToDate`, le dossier repart par le chemin normal sans rien avoir perdu.
///
/// ## Et la deuxième condition est celle des purges
///
/// `UpToDate` dit « rien n'est arrivé ». Il ne dit pas « rien n'a disparu » : sans `QRESYNC`,
/// une purge ne s'annonce pas. Le compte de messages répond — s'il est celui qu'on connaît et
/// que rien n'est arrivé, aucune purge n'a pu avoir lieu, puisqu'une purge ferait baisser ce
/// compte et que rien ne peut la compenser.
///
/// C'est mot pour mot le raisonnement que [`harvest_watched`] tient déjà sur `EXISTS` après
/// l'`EXAMINE`. Il est repris ici parce qu'il vaut sur le même nombre, obtenu plus tôt et
/// moins cher.
///
/// ## Une seule raison **locale** de ne pas sauter
///
/// Tout ce qui précède regarde le serveur. Une marque `\Seen` posée localement, elle, ne change
/// rien de ce que le serveur annonce — donc le raccourci l'évitait, et la marque ne partait
/// jamais. C'est le seul état local qui compte ici, et le refus est en tête de fonction pour que
/// la suite reste ce qu'elle était : une lecture de ce que le serveur dit.
fn up_to_date(
    store: &Store,
    folder: &Discovered,
    remote: Option<&crate::Selected>,
    enabled: Enabled,
) -> Result<Option<SyncState>> {
    let Some(remote) = remote else {
        return Ok(None);
    };

    // **Une poussée en attente est une raison locale de ne pas éviter le dossier.** Le reste de
    // cette fonction ne regarde que ce que le serveur dit ; il n'a aucune raison d'avoir changé
    // parce que l'utilisateur a ouvert un message chez lui.
    //
    // Sans ce test, le défaut était complet et silencieux : `mail doctor` comptait la poussée
    // en attente, la moisson évitait les vingt-neuf dossiers du compte, et le `\Seen` ne
    // partait jamais. Trouvé sur le corpus réel — `skipped=29` dans le journal du démon — et
    // par aucun test, parce qu'aucun test n'avait à la fois un dossier à jour et une marque en
    // attente.
    if !store.pending_seen(folder.folder)?.is_empty() {
        return Ok(None);
    }

    let local = store.sync_state(folder.folder)?;
    let (plan, _) = plan(&local, remote, enabled.condstore);
    if plan != Plan::UpToDate || u64::from(remote.exists) != store.copies_in(folder.folder)? {
        return Ok(None);
    }

    // Le nom distant est réécrit avec le reste : c'est ce que ferait la moisson, et un dossier
    // renommé côté serveur doit voir son nom suivre même quand rien d'autre ne bouge.
    Ok(Some(SyncState {
        remote_name: folder.remote_name.clone(),
        synced_at: Some(now()),
        ..local
    }))
}

/// De quoi s'authentifier, sans dire d'où ça vient.
///
/// ## Pourquoi un type et pas une `&str`
///
/// Parce que les deux secrets ne se placent pas au même endroit dans le protocole. Un jeton
/// d'accès envoyé dans un `LOGIN` **part en clair dans le champ mot de passe**, et un serveur
/// journalise les échecs de `LOGIN` avec l'identifiant : un jeton porteur y finirait écrit sur
/// le disque de quelqu'un d'autre.
///
/// Avec une `&str` unique, cette confusion est une ligne à écrire de travers. Avec deux
/// variantes, elle ne compile pas.
#[derive(Clone, Copy)]
pub enum Credential<'a> {
    /// Un mot de passe, ou un mot de passe applicatif : `LOGIN`.
    Password(&'a str),
    /// Un jeton d'accès OAuth2 : `AUTHENTICATE XOAUTH2`.
    ///
    /// L'appelant l'a obtenu de `mailauth::session::access_token`, qui l'a rafraîchi si
    /// nécessaire. `mailsync` ne sait pas rafraîchir et n'a pas à savoir : il ne dépend pas de
    /// `mailauth`, et c'est ce qui garde un compte à mot de passe applicatif hors de l'arbre de
    /// dépendances d'OAuth2.
    Bearer(&'a str),
}

/// Le `Debug` est **écrit à la main**, et il ne montre que le mécanisme.
///
/// La dérivation affichait `Password("mot-de-passe")`, donc un `tracing::debug!(?credential)`
/// ailleurs dans le programme aurait mis le secret dans un journal — `docs/PRIVACY.md` §8. Le
/// défaut est dangereux pour exactement les deux types du projet qui portent un secret, et
/// correct pour tout le reste ; trouvé côté SMTP, corrigé des deux côtés.
impl std::fmt::Debug for Credential<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mechanism = match self {
            Self::Password(_) => "Password",
            Self::Bearer(_) => "Bearer",
        };
        write!(f, "Credential::{mechanism}(<masqué>)")
    }
}

impl<'a> Credential<'a> {
    /// La pièce qui correspond au mécanisme déclaré par le compte.
    ///
    /// C'est **le seul** endroit où un secret est aiguillé vers une variante, et il est écrit
    /// une fois. L'appelant lit le secret au trousseau — `mailauth::session::secret_for` fait
    /// la lecture symétrique — puis passe ici sans avoir à rebrancher sur le mécanisme, ce qui
    /// est l'endroit exact où une deuxième copie du branchement finirait par diverger.
    #[must_use]
    pub const fn for_auth(auth: AuthKind, secret: &'a str) -> Self {
        match auth {
            AuthKind::Password => Self::Password(secret),
            AuthKind::OAuth2 => Self::Bearer(secret),
        }
    }

    /// L'`AuthKind` que cette pièce sait servir.
    const fn kind(self) -> AuthKind {
        match self {
            Self::Password(_) => AuthKind::Password,
            Self::Bearer(_) => AuthKind::OAuth2,
        }
    }
}

/// Synchronise un compte : chiffre, s'authentifie, moissonne.
///
/// Le secret est passé par l'appelant, qui l'a lu du trousseau du système. Cette fonction ne
/// sait pas d'où il vient et ne le journalise pas.
///
/// ## Le désaccord entre le compte et la pièce est un refus, pas un repli
///
/// Un compte déclaré `oauth2` avec un mot de passe en main — ou l'inverse — est un défaut de
/// l'appelant. Deviner enverrait le secret dans un champ qui ne l'attend pas, exactement ce que
/// `AuthKind::parse` refuse déjà de faire à la lecture du store.
///
/// # Errors
///
/// Voir [`Error`] : un refus d'authentification n'est pas réessayable, une coupure réseau
/// l'est. [`Error::NotSyncable`] pour un compte sans serveur, ou pour une pièce qui ne
/// correspond pas au mécanisme déclaré.
///
/// [`Error`]: crate::Error
pub fn sync_account(
    store: &Store,
    account: &Account,
    credential: Credential<'_>,
    progress: &Progress,
) -> Result<AccountReport> {
    let Some(server) = account.server.as_ref() else {
        return Err(crate::Error::NotSyncable {
            account: account.id.0,
            reason: "aucun serveur configuré".to_owned(),
        });
    };
    if server.auth != credential.kind() {
        return Err(crate::Error::NotSyncable {
            account: account.id.0,
            reason: format!(
                "compte déclaré `{}` mais authentification fournie `{}`",
                server.auth.as_str(),
                credential.kind().as_str()
            ),
        });
    }

    let mut client = crate::connect(server)?;
    match credential {
        Credential::Password(secret) => client.login(&server.username, secret)?,
        Credential::Bearer(token) => client.authenticate_xoauth2(&server.username, token)?,
    }
    let report = sync_account_over(&mut client, store, account.id, progress);
    client.logout();
    report
}

/// Moissonne un dossier.
///
/// ## L'ordre des opérations, et pourquoi il ne peut pas changer
///
/// 1. `EXAMINE` — en lecture seule, voir le module `lib`.
/// 2. Lire l'état local, décider du plan.
/// 3. Si le plan est complet, **oublier l'état local d'abord**. Garder les anciens UID à côté
///    d'un `UIDVALIDITY` neuf lierait des copies à des messages que le serveur numérote
///    autrement.
/// 4. Moissonner les corps par lots, chacun dans sa transaction.
/// 5. Moissonner les drapeaux.
/// 6. Balayer les UID pour trouver ce qui a disparu.
/// 7. Recalculer les drapeaux des références, puis **écrire l'état de synchronisation en
///    dernier**. Il est le témoin de ce qui est fait : l'écrire avant rendrait un dossier à
///    moitié moissonné indistinguable d'un dossier à jour.
///
/// # Errors
///
/// Toutes les variantes de [`Error`]. Une erreur laisse le store **cohérent** : les lots
/// validés le restent, l'état de synchronisation n'a pas avancé, et le passage suivant
/// reprendra là où celui-ci s'est arrêté.
pub fn harvest<S: Read + Write>(
    client: &mut Client<S>,
    store: &Store,
    folder: FolderId,
    mailbox: &[u8],
) -> Result<Report> {
    // Une progression neuve : rien à rapporter, rien à annuler. C'est la forme simple, celle
    // des tests et d'un appel isolé.
    //
    // La négociation est faite ici parce qu'un appel isolé part d'une connexion fraîche. Une
    // boucle sur plusieurs dossiers doit appeler [`enable`] **une fois** avant le premier
    // dossier — `ENABLE` n'est pas valide une fois une boîte sélectionnée — et c'est ce que
    // fait [`sync_account_over`].
    let enabled = enable(client)?;
    harvest_watched(client, store, folder, mailbox, enabled, &Progress::new())
}

/// Pousse les `\Seen` en attente pour un dossier, **avant** de relire ses drapeaux.
///
/// ## L'ordre est ce qui rend la marque durable
///
/// La moisson relit `remote_uids.flags` depuis le serveur. Si elle le faisait avant la poussée,
/// elle réécrirait « non lu » par-dessus la marque locale, et le message redeviendrait non lu
/// tout seul — le défaut que `Store::mark_seen` et sa file existent pour empêcher.
///
/// ## Elle demande `SELECT`, donc le droit d'écrire, et seulement quand il y a à écrire
///
/// `UID STORE` sur une boîte ouverte en `EXAMINE` est refusé. Le dossier est donc rouvert en
/// `SELECT` — mais uniquement s'il a une poussée en attente. Partout ailleurs, `EXAMINE` reste
/// le chemin, et la règle de la phase 2 tient : rien n'écrit côté serveur sauf `\Seen`.
///
/// ## Un échec n'interrompt pas la moisson
///
/// Une boîte partagée en lecture seule, un serveur qui refuse : la poussée reste en attente et
/// la moisson continue. Le pire cas est un message qui reste « non lu » côté serveur alors qu'il
/// est lu localement, ce qui est un désagrément ; abandonner la moisson pour ça perdrait du
/// courrier de vue.
///
/// **La poussée n'est oubliée qu'après la réponse du serveur.** L'oublier avant perdrait la
/// marque : la relecture qui suit écrirait « non lu » et plus rien ne saurait qu'il fallait
/// pousser. Une poussée refaite est sans effet — `+FLAGS` est idempotent — donc le sens de
/// l'erreur est le bon.
///
/// # Errors
///
/// [`Error::Store`] si la file est illisible. Un refus du serveur est **journalisé, pas
/// propagé** : voir ci-dessus.
pub fn push_seen<S: Read + Write>(
    client: &mut Client<S>,
    store: &Store,
    folder: FolderId,
    mailbox: &[u8],
) -> Result<usize> {
    let pending = store.pending_seen(folder)?;
    if pending.is_empty() {
        return Ok(0);
    }
    let uids: Vec<u32> = pending.iter().map(|it| it.uid).collect();
    let Some(set) = uid_set(&uids) else {
        return Ok(0);
    };

    // Le dossier est rouvert en écriture. Un refus ici — `\Noselect`, boîte partagée — laisse
    // la poussée en attente et rend la main.
    if let Err(source) = client.select(mailbox) {
        tracing::warn!(
            folder = folder.0,
            %source,
            "dossier non ouvrable en écriture : les marques restent en attente"
        );
        return Ok(0);
    }
    if let Err(source) = client.store_seen(&set) {
        tracing::warn!(
            folder = folder.0,
            %source,
            pending = uids.len(),
            "poussée du drapeau lu refusée : les marques restent en attente"
        );
        return Ok(0);
    }

    let forgotten = store.forget_pending_seen(folder, &uids)?;
    tracing::info!(folder = folder.0, pushed = forgotten, "drapeau lu poussé");
    Ok(forgotten)
}

/// La même, en rendant compte et en acceptant d'être interrompue.
///
/// ## Pourquoi deux fonctions
///
/// La progression n'intéresse qu'un appelant : le job de fond du démon. Lui imposer à la
/// vingtaine d'appels des tests ajouterait un `&Progress::new()` partout sans rien dire de
/// plus. Le couple `harvest` / `harvest_watched` est la même convention que `sort` / `sort_by`.
///
/// ## Ce que l'annulation garantit, et ce qu'elle ne garantit pas
///
/// Elle est **coopérative et par lot** : la moisson s'arrête à la fin du lot de cent messages
/// en cours, pas au milieu. Ce qui est validé reste validé — l'adressage par contenu fait
/// qu'un passage partiel est valide, et le passage suivant reprend là où celui-ci s'est
/// arrêté.
///
/// Ce qu'elle ne fait pas : interrompre un `UID FETCH` déjà émis. Un lot de cent messages
/// arrive en entier ou pas du tout.
///
/// # Errors
///
/// Voir [`harvest`].
pub fn harvest_watched<S: Read + Write>(
    client: &mut Client<S>,
    store: &Store,
    folder: FolderId,
    mailbox: &[u8],
    enabled: Enabled,
    progress: &Progress,
) -> Result<Report> {
    // **Avant tout le reste.** La moisson qui suit relit les drapeaux depuis le serveur ; les
    // pousser après reviendrait à écrire « non lu » par-dessus une marque locale, et le message
    // redeviendrait non lu tout seul. Sans poussée en attente, la fonction ne fait rien et
    // n'ouvre rien en écriture — voir [`push_seen`].
    push_seen(client, store, folder, mailbox)?;

    let local = store.sync_state(folder)?;

    // **Le chemin `QRESYNC`.** Il demande l'`UIDVALIDITY` et le `MODSEQ` qu'on connaît déjà ;
    // sans eux il n'y a rien à reprendre, et un `EXAMINE` nu est la bonne commande.
    let resync = match (enabled.qresync, local.uidvalidity, local.highest_modseq) {
        (true, Some(uidvalidity), Some(modseq)) => {
            Some(client.examine_qresync(mailbox, uidvalidity, modseq)?)
        }
        _ => None,
    };
    let remote = match &resync {
        Some(resynced) => resynced.selected.clone(),
        None => client.examine(mailbox)?,
    };
    let (plan, reason) = plan(&local, &remote, enabled.condstore);

    // **Le serveur a-t-il vraiment honoré le paramètre ?** S'il a ignoré notre `UIDVALIDITY`,
    // il répond comme à un `EXAMINE` nu : pas de `VANISHED`, donc « rien n'a disparu » serait
    // une conclusion fausse. Et sur une moisson complète, ce qu'il a pu dire des purges
    // d'avant ne veut plus rien dire, puisqu'on efface tout.
    let resynced = resync.filter(|resynced| {
        plan != Plan::Full && resynced.selected.uidvalidity == local.uidvalidity
    });

    tracing::info!(
        folder = folder.0,
        ?plan,
        ?reason,
        condstore = enabled.condstore,
        qresync = resynced.is_some(),
        exists = remote.exists,
        "plan de moisson"
    );

    let mut report = Report {
        full: plan == Plan::Full,
        ..Report::default()
    };

    // **On n'efface que si les UID connus sont devenus faux.**
    //
    // `Plan::Full` couvre trois situations très différentes, et la première version les
    // traitait toutes en effaçant : un `UIDVALIDITY` changé — où les UID ne désignent plus
    // rien, donc où il *faut* effacer — mais aussi un dossier jamais synchronisé, où il n'y a
    // rien à effacer, et surtout un dossier **interrompu**, où l'effacement jette exactement ce
    // qui permettrait de reprendre.
    //
    // Mesuré le 2026-09-08 : après une coupure de Gmail sur deux dossiers, la passe suivante a
    // retéléchargé 25 028 corps pour trouver 2 messages nouveaux. Correct — la dédup n'a rien
    // stocké deux fois — mais la bande passante était repayée en entier.
    if matches!(
        reason,
        Some(FullReason::UidvalidityChanged | FullReason::NoUidvalidity)
    ) {
        forget_folder(store, folder)?;
    }

    // **Le témoin de reprise.** Écrit *avant* de télécharger quoi que ce soit, il laisse une
    // trace de la validité contre laquelle les copies de ce passage sont enregistrées. Sans
    // lui, un dossier coupé au milieu repartirait en `Full` sans qu'on puisse vérifier que ses
    // UID valent encore quelque chose — et les réutiliser serait alors un pari.
    //
    // `uidnext` et `highest_modseq` restent absents : rien n'est encore acquis. Le plan suivant
    // sera donc un `Rescan` depuis 1, qui redemande tous les drapeaux — ce qu'il faut, puisque
    // ceux des messages sautés n'ont pas été revus.
    if plan == Plan::Full && remote.uidvalidity.is_some() {
        let writer = store.writer()?;
        writer.set_sync_state(
            folder,
            &SyncState {
                remote_name: mailbox.to_vec(),
                uidvalidity: remote.uidvalidity,
                ..SyncState::default()
            },
        )?;
        writer.commit()?;
    }

    // --- les corps ---
    let bodies_from = match &plan {
        Plan::Full => Some(1),
        // **`n:*` ne veut pas dire « rien » quand `n` dépasse le plus grand UID.** La RFC 3501
        // dit qu'un ensemble n'est pas ordonné : `1000:*` sur une boîte dont le plus grand UID
        // est 999 se lit `999:1000`, donc le serveur rend **le dernier message**.
        //
        // Sans cette garde, chaque passage incrémental retéléchargeait le dernier message de
        // chaque dossier. Mesuré sur un vrai serveur le 2026-09-03 : 13 corps redemandés pour
        // rien, un par dossier non vide. Avec une pièce jointe de 25 Mo, c'est 25 Mo par
        // dossier et par passage.
        //
        // `UIDNEXT` distant répond exactement à la question : s'il n'a pas dépassé ce qu'on a,
        // il n'y a aucun corps à demander. Absent, on demande — un serveur qui ne le donne pas
        // ne nous laisse pas le choix.
        Plan::Incremental { new_from, .. } | Plan::Rescan { new_from } => remote
            .uidnext
            .is_none_or(|next| next > *new_from)
            .then_some(*new_from),
        Plan::UpToDate => None,
    };
    let mut skipped = 0_usize;
    if let Some(from) = bodies_from {
        // `RFC822.SIZE` en même temps que l'UID : le serveur le donne **dans le même
        // aller-retour**, et c'est ce qui permet de borner un lot en octets plus loin.
        let uids = client.uid_fetch(&format!("{from}:*"), "UID RFC822.SIZE")?;
        let announced: HashMap<u32, u64> = uids
            .iter()
            .filter_map(|it| it.size.map(|size| (it.uid, size)))
            .collect();
        let mut wanted: Vec<u32> = uids.iter().map(|it| it.uid).collect();
        wanted.sort_unstable();
        wanted.dedup();

        // **Ce qu'on a déjà ne se redemande pas.**
        //
        // Une ligne de `remote_uids` n'existe que si le corps correspondant a été écrit : c'est
        // la dernière chose que fait `fetch_bodies`, dans la même transaction. Elle est donc la
        // preuve qu'on possède le message, et la seule situation où elle mentirait — un
        // `UIDVALIDITY` changé — vient d'être traitée par l'effacement ci-dessus.
        //
        // C'est ce qui transforme une reprise après coupure en reprise réelle : le dossier
        // coupé de Gmail redemandait ses 18 300 corps, il ne redemandera plus que ce qui manque.
        let known: std::collections::HashSet<u32> = store.known_uids(folder)?.into_iter().collect();
        let before = wanted.len();
        wanted.retain(|uid| !known.contains(uid));
        skipped = before - wanted.len();
        if skipped > 0 {
            tracing::info!(
                folder = folder.0,
                skipped,
                restants = wanted.len(),
                "reprise : corps déjà présents non redemandés"
            );
        }

        // Le total est **découvert** ici, dossier par dossier, comme celui de l'import est
        // découvert fichier par fichier. Il augmente donc pendant le job, et c'est plus
        // honnête qu'un total inventé au départ.
        progress.set_total(progress.total().saturating_add(wanted.len() as u64));

        // Le découpage est journalisé : une borne écrite dans une constante est une intention,
        // un nombre de lots dans un journal est une vérification. C'est ce qui permettra de
        // voir, sur un compte à grosses pièces jointes, que les lots se sont bien resserrés.
        let planned = batches(&wanted, &announced);
        tracing::debug!(
            folder = folder.0,
            lots = planned.len(),
            plus_gros = planned.iter().map(Vec::len).max().unwrap_or(0),
            tailles_annoncees = announced.len(),
            "lots de corps"
        );
        for chunk in planned {
            if progress.is_cancelled() {
                tracing::info!(folder = folder.0, "moisson interrompue entre deux lots");
                // **On sort sans écrire l'état de synchronisation.** C'est ce qui fait qu'un
                // passage interrompu se reprend au lieu de se croire fini : le témoin n'a pas
                // avancé, donc la prochaine moisson redemandera ce qui manque.
                return Ok(report);
            }
            report = fetch_bodies(client, store, folder, &chunk, report)?;
            progress.advance(chunk.len() as u64);
        }
    }

    // --- les drapeaux ---
    let flags_set = match &plan {
        // **Sauf si elle en a sauté.** Une moisson complète prend les drapeaux avec les corps,
        // donc les messages dont le corps n'a pas été redemandé n'auraient pas vu passer un
        // changement de drapeau survenu pendant l'interruption. Les redemander tous coûte une
        // réponse sans corps ; les oublier laisserait un message lu s'afficher non lu sans que
        // rien ne le corrige jamais.
        Plan::Full if skipped > 0 => Some((String::from("1:*"), None)),
        // Une moisson complète a déjà pris les drapeaux avec les corps.
        Plan::Full | Plan::UpToDate => None,
        // Avec `QRESYNC`, l'`EXAMINE` les a déjà rendus : les redemander serait un aller-retour
        // pour la même information.
        _ if resynced.is_some() => None,
        Plan::Incremental { since, .. } => Some((String::from("1:*"), Some(*since))),
        Plan::Rescan { .. } => Some((String::from("1:*"), None)),
    };
    if let Some((set, since)) = flags_set {
        let items = match since {
            Some(since) => format!("UID FLAGS) (CHANGEDSINCE {since}"),
            None => "UID FLAGS".to_owned(),
        };
        let fetched = client.uid_fetch(&set, &items)?;
        report.copies += apply_flags(store, folder, &fetched)?;
    }

    // --- ce qui a disparu ---
    //
    // **C'est ici que `QRESYNC` paie.** Le balayage demande une ligne par message à chaque
    // passage, qu'il se soit passé quelque chose ou non : 51 496 lignes pour le plus gros
    // compte du corpus réel, mesurées à 67 s le 2026-09-08 sur une synchronisation où rien
    // n'avait changé. `VANISHED` rend la même information en une ligne, ou en aucune.
    report.vanished += match &resynced {
        Some(resynced) => {
            report.copies += apply_flags(store, folder, &resynced.changed)?;
            forget_uids(store, folder, &resynced.vanished)?
        }
        // **La porte de sortie pour les serveurs sans `QRESYNC`, et c'est le cas commun.**
        //
        // Gmail ne l'annonce pas, et c'est quatre comptes du corpus sur cinq. La première
        // version de ce commentaire disait « ni Gmail ni Dovecot » : c'était faux pour le
        // Dovecot, qui l'annonce **une fois authentifié** et à qui on demandait ses capacités
        // trop tôt. Le relevé n'était pas mauvais, la question était posée au mauvais moment —
        // voir `Client::refresh_capabilities_if_silent`.
        //
        // Le balayage reste donc le chemin normal chez Gmail, avec son coût d'une ligne par
        // message et par passage.
        //
        // `EXISTS` répond gratuitement à la question qui compte : **le serveur en a-t-il autant
        // que nous ?**
        //
        // ## Le compte suffit, et la première version en demandait trop
        //
        // Elle exigeait aussi `Plan::UpToDate`, avec cet argument : « un message purgé et un
        // message reçu laissent `EXISTS` inchangé ». L'argument porte sur `EXISTS` comparé à
        // lui-même d'un passage à l'autre. Il ne porte pas sur la comparaison qui est faite
        // ici, et qui est plus forte.
        //
        // Soit `K` l'ensemble des UID qu'on connaît **après ce passage** — donc y compris ceux
        // qu'on vient d'apprendre, puisque le passage des corps a énuméré tout ce qui est
        // au-delà de notre `UIDNEXT`. Soit `S` celui du serveur, dont `EXISTS` donne le
        // cardinal. Les UID appris sont dans `S` par construction ; les purgés sont dans `K` et
        // pas dans `S`. Donc `|K| − |S|` **est** le nombre de purges, et `|K| = EXISTS`
        // implique qu'il n'y en a aucune.
        //
        // Le contre-exemple de la première version se résout tout seul : purger l'UID 3 et
        // recevoir l'UID 11 laisse `EXISTS` à 10, mais `K` en compte 11 — l'inégalité apparaît,
        // et le balayage a lieu.
        //
        // ## Ce que ça a coûté de trop demander
        //
        // Mesuré le 2026-09-09 : chez Gmail, le `MODSEQ` est celui du **compte**, donc une
        // seule arrivée sort **tous** les dossiers du compte de `UpToDate`. Le balayage
        // reprenait alors partout, à une ligne par message : 2 min 21 s pour trente-six
        // messages arrivés sur le corpus réel, dont l'essentiel en `UID FETCH 1:* (UID)` sur
        // des dossiers de vingt-cinq mille messages qui n'avaient rien perdu.
        //
        // ## Ce que la condition ne couvre pas, et qui est borné
        //
        // `EXISTS` a été lu à l'`EXAMINE`, donc avant la moisson. Une purge survenue **après**
        // cette lecture peut laisser les comptes égaux et passer inaperçue jusqu'au passage
        // suivant, où `EXISTS` sera frais. C'est une exposition d'un passage, elle se corrige
        // toute seule, et la version d'avant l'avait exactement la même.
        None if u64::from(remote.exists) == store.copies_in(folder)? => {
            tracing::debug!(
                folder = folder.0,
                exists = remote.exists,
                "le compte tombe juste : balayage évité"
            );
            0
        }
        None => {
            tracing::debug!(
                folder = folder.0,
                exists = remote.exists,
                connues = store.copies_in(folder)?,
                ?plan,
                "balayage"
            );
            sweep(client, store, folder)?
        }
    };

    // --- les drapeaux des références, puis l'état ---
    //
    // **Seulement si une copie a bougé.** Les drapeaux d'une référence sont *dérivés* des
    // copies du dossier : si aucune copie n'a été écrite, retirée ni remise à jour dans ce
    // passage, la valeur calculée est forcément celle déjà en place.
    //
    // La requête n'écrivait plus rien depuis le correctif du même jour, mais elle **lisait**
    // toujours tout : une sous-requête corrélée par référence, soit 9 671 lignes pour une seule
    // boîte du corpus réel. Mesuré à ~0,8 s par dossier sur une synchronisation où il ne se
    // passait rien, c'est-à-dire l'essentiel de ce qui restait du critère 3.
    if report.stored > 0 || report.copies > 0 || report.vanished > 0 {
        let writer = store.writer()?;
        report.reflagged = writer.refresh_ref_flags(folder)?;
        writer.commit()?;
    }

    let state = SyncState {
        remote_name: mailbox.to_vec(),
        uidvalidity: remote.uidvalidity,
        uidnext: remote.uidnext,
        highest_modseq: remote.highest_modseq,
        synced_at: Some(now()),
        subscribed: local.subscribed,
    };
    let writer = store.writer()?;
    writer.set_sync_state(folder, &state)?;
    writer.commit()?;

    tracing::info!(folder = folder.0, ?report, "moisson terminée");
    Ok(report)
}

/// Télécharge et écrit les corps d'un lot d'UID.
///
/// Chaque lot est **une transaction**. Une coupure au milieu perd ce lot et rien d'autre.
fn fetch_bodies<S: Read + Write>(
    client: &mut Client<S>,
    store: &Store,
    folder: FolderId,
    uids: &[u32],
    mut report: Report,
) -> Result<Report> {
    let Some(set) = uid_set(uids) else {
        return Ok(report);
    };

    // Collecté par lot, puis écrit : la lecture est streamante côté client, et un lot de cent
    // messages tient en mémoire. Écrire dans la fermeture demanderait de tenir la transaction
    // ouverte pendant le réseau, ce qui bloquerait les lecteurs du store pour la durée du
    // téléchargement.
    let mut batch: Vec<Fetched> = Vec::with_capacity(uids.len());
    client.uid_fetch_each(&set, "UID FLAGS BODY[]", &mut |fetched| {
        batch.push(fetched);
        Ok(())
    })?;

    // **Les blobs sont écrits avant d'ouvrir la transaction**, et c'est un correctif.
    //
    // Ils l'étaient dedans, ce qui tenait le verrou d'écriture de l'index pendant cent
    // créations de fichiers compressés — quelques mégaoctets, et sur Windows autant de
    // passages d'antivirus. Mesuré le 2026-09-09 par le banc du critère 4 : cinq moissons
    // concurrentes sur le même store, et l'une d'elles échouait en `database is locked` après
    // les 5 s de `busy_timeout`.
    //
    // Le déplacement ne change **rien** aux garanties, parce qu'une écriture de fichier n'a
    // jamais été transactionnelle : un `ROLLBACK` ne l'aurait pas défaite. Ce que la
    // transaction protège est l'ensemble des lignes, et il reste entier.
    //
    // Ce qu'on risque en plus est un blob écrit dont les lignes ne le sont pas — sur une
    // coupure entre les deux. C'est le cas « blob non référencé », que l'adressage par contenu
    // rend inoffensif : le passage suivant le retrouve par son empreinte et ne le réécrit pas.
    //
    // **Personne ne les compte, en revanche.** `mail doctor` rapporte les *références*
    // orphelines — une référence vers un message absent — pas les blobs que plus aucun message
    // ne désigne. Vérifié le 2026-09-09 : il n'y a aucun contrôle de ce côté-là. Le déplacement
    // n'invente pas ce trou, puisqu'une coupure au milieu d'un lot laissait déjà le blob écrit
    // sans ses lignes — un `ROLLBACK` n'efface pas un fichier — mais il le rend un peu plus
    // probable, et c'est une raison de plus de le combler.
    let mut prepared = Vec::with_capacity(batch.len());
    for fetched in &batch {
        let Some(body) = fetched.body.as_deref() else {
            // Un `FETCH` sans corps là où on en demandait un : le serveur n'a pas répondu à
            // la question. Compté, pas inventé.
            tracing::warn!(uid = fetched.uid, "réponse FETCH sans corps");
            continue;
        };
        report.fetched += 1;

        let outcome = store.blobs().put(body)?;
        if outcome.created {
            report.stored += 1;
        } else {
            report.duplicates += 1;
        }
        prepared.push((fetched, body, outcome));
    }

    let writer = store.writer()?;
    for (fetched, body, outcome) in prepared {
        let (message, _) = writer.insert_message(&new_message(body, &outcome.hash)?)?;
        let flags = flags_of(&fetched.flags);
        writer.insert_ref(message, folder, date_of(body), flags)?;
        if writer.record_copy(&RemoteCopy {
            folder,
            uid: fetched.uid,
            message,
            flags,
            modseq: fetched.modseq,
        })? {
            report.copies += 1;
        }
    }
    writer.commit()?;
    Ok(report)
}

/// Applique des drapeaux à des copies déjà connues.
///
/// Une copie inconnue est **ignorée** et non créée : sans corps, on n'a pas de contenu à quoi
/// la rattacher. Elle sera prise au prochain passage, par le chemin des corps.
fn apply_flags(store: &Store, folder: FolderId, fetched: &[Fetched]) -> Result<usize> {
    let known = store.known_uids(folder)?;
    let writer = store.writer()?;
    let mut count = 0;
    for item in fetched {
        if !known.contains(&item.uid) {
            continue;
        }
        let Some(message) = store.copy_message(folder, item.uid)? else {
            continue;
        };
        // Compté seulement si quelque chose a changé : voir `record_copy`. Une relecture
        // de drapeaux identiques n'est pas une écriture, et le bilan ne doit pas la
        // présenter comme telle.
        if writer.record_copy(&RemoteCopy {
            folder,
            uid: item.uid,
            message,
            flags: flags_of(&item.flags),
            modseq: item.modseq,
        })? {
            count += 1;
        }
    }
    writer.commit()?;
    Ok(count)
}

/// Trouve ce qui a disparu côté serveur, et l'oublie.
///
/// Voir le module : `CONDSTORE` seul ne signale pas les purges, donc le balayage est le seul
/// moyen. Il ne demande que des UID — pas de corps, une réponse.
/// Retire les copies d'une liste d'UID, et les références devenues orphelines.
///
/// Partagé par le balayage et par `VANISHED` : les deux répondent à la même question — quels
/// UID ne sont plus là — et la réponse doit être appliquée de la même façon, sinon les deux
/// chemins divergent sur ce qui est le cœur du modèle de données.
fn forget_uids(store: &Store, folder: FolderId, gone: &[u32]) -> Result<usize> {
    if gone.is_empty() {
        return Ok(0);
    }
    let writer = store.writer()?;
    let mut count = 0;
    for &uid in gone {
        // **La valeur rendue est ce qui empêche le bug que `remote_uids` existe pour éviter.**
        // Une copie disparue ne retire la référence que si c'était la dernière ; tant qu'une
        // autre existe, le message est encore dans la boîte.
        //
        // `forget_copy` rend `None` pour un UID qu'on ne connaissait pas. `VANISHED` peut en
        // citer — le serveur annonce ce qui a disparu depuis un `MODSEQ`, sans savoir ce qu'on
        // avait vu. Ce n'est pas une anomalie, et ça ne compte pas comme une disparition.
        match writer.forget_copy(folder, uid)? {
            Some(message) => {
                writer.remove_ref(message, folder)?;
                count += 1;
            }
            None => continue,
        }
    }
    writer.commit()?;
    Ok(count)
}

/// Ce que le serveur a accepté d'activer sur cette connexion.
///
/// ## Pourquoi c'est décidé une fois, et pas par dossier
///
/// `ENABLE` n'est valide **qu'en état authentifié** (RFC 5161 §3.1) : après le premier
/// `EXAMINE`, la connexion est en état sélectionné et un `ENABLE` y est hors protocole. La
/// première version l'envoyait à chaque dossier, après l'`EXAMINE` — Gmail le tolérait, ce qui
/// est exactement le genre de tolérance sur laquelle on ne peut pas compter.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Enabled {
    /// `CONDSTORE` : les `MODSEQ`, donc les moissons incrémentales.
    pub condstore: bool,
    /// `QRESYNC` : les purges sans balayage. Implique `condstore`.
    pub qresync: bool,
}

/// Négocie les extensions, **avant tout `EXAMINE`**.
///
/// `QRESYNC` d'abord : la RFC 7162 §3.2.3 dit qu'il active `CONDSTORE` avec lui, donc envoyer
/// les deux `ENABLE` serait au mieux inutile.
///
/// # Errors
///
/// [`crate::Error::Network`]. Un refus du serveur n'est pas une erreur : il y a un repli.
pub fn enable<S: Read + Write>(client: &mut Client<S>) -> Result<Enabled> {
    if client.enable_qresync()? {
        return Ok(Enabled {
            condstore: true,
            qresync: true,
        });
    }
    Ok(Enabled {
        condstore: client.enable_condstore()?,
        qresync: false,
    })
}

fn sweep<S: Read + Write>(
    client: &mut Client<S>,
    store: &Store,
    folder: FolderId,
) -> Result<usize> {
    // **Un ensemble, pas un `Vec`.** La première version faisait `present.contains(it)` dans
    // une boucle sur les UID connus : O(n²). Sur le plus gros compte du corpus réel — 51 496
    // références — ça fait 2,6 milliards de comparaisons par passage, et c'est ce qui rendait
    // une synchronisation sans le moindre changement plus lente qu'un `LIST` complet.
    //
    // Mesuré le 2026-09-08 : 67 s pour ce compte, contre 1,1 s pour un compte de 2 800
    // messages. Le coût suivait le carré du nombre de messages, pas le nombre de dossiers.
    let present: std::collections::HashSet<u32> = client
        .uid_fetch("1:*", "UID")?
        .iter()
        .map(|it| it.uid)
        .collect();
    let known = store.known_uids(folder)?;
    let gone: Vec<u32> = known
        .into_iter()
        .filter(|it| !present.contains(it))
        .collect();
    forget_uids(store, folder, &gone)
}

/// Oublie tout ce qu'on savait d'un dossier côté serveur.
///
/// Les **contenus restent** : ils sont adressés par contenu et partagés avec d'autres
/// dossiers. Seules les copies et l'état de synchronisation partent, ce qui est exactement ce
/// qu'un `UIDVALIDITY` changé invalide.
fn forget_folder(store: &Store, folder: FolderId) -> Result<()> {
    let writer = store.writer()?;
    writer.forget_copies(folder)?;
    writer.set_sync_state(folder, &SyncState::default())?;
    writer.commit()?;
    Ok(())
}

/// Traduit les drapeaux IMAP en drapeaux du store.
///
/// Un drapeau inconnu — un mot-clé propre au serveur, `$Forwarded`, `NonJunk` — est **ignoré
/// sans erreur**. Ils sont légaux et courants ; échouer dessus rendrait la moisson
/// impossible chez la plupart des fournisseurs.
fn flags_of(names: &[String]) -> MessageFlags {
    let mut out = MessageFlags::empty();
    for name in names {
        let flag = match name.to_ascii_lowercase().as_str() {
            "\\seen" => MessageFlags::SEEN,
            "\\flagged" => MessageFlags::FLAGGED,
            "\\answered" => MessageFlags::ANSWERED,
            "\\draft" => MessageFlags::DRAFT,
            "\\deleted" => MessageFlags::DELETED,
            _ => continue,
        };
        out = out.union(flag);
    }
    out
}

/// Un ensemble d'UID pour le protocole, en plages compactes.
///
/// `1,2,3,7,8` devient `1:3,7:8`. Ce n'est pas de la coquetterie : une commande IMAP a une
/// longueur maximale en pratique, et cent UID à sept chiffres en liste plate font 800 octets
/// contre une vingtaine en plages.
fn uid_set(uids: &[u32]) -> Option<String> {
    let mut sorted = uids.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    let first = *sorted.first()?;

    let mut parts = Vec::new();
    let mut start = first;
    let mut previous = first;
    for uid in sorted.iter().skip(1).copied() {
        if uid == previous + 1 {
            previous = uid;
            continue;
        }
        parts.push(range(start, previous));
        start = uid;
        previous = uid;
    }
    parts.push(range(start, previous));
    Some(parts.join(","))
}

/// Une plage, ou un UID seul si les bornes se touchent.
fn range(start: u32, end: u32) -> String {
    if start == end {
        start.to_string()
    } else {
        format!("{start}:{end}")
    }
}

/// L'instant présent, en secondes Unix.
///
/// Une horloge reculée rend zéro plutôt que de paniquer : un `synced_at` faux fait refaire du
/// travail, un panic fait perdre la synchronisation.
fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |it| i64::try_from(it.as_secs()).unwrap_or(0))
}

/// Extrait les métadonnées d'un message pour le store.
fn new_message<'a>(body: &'a [u8], hash: &mailcore::BlobHash) -> Result<mailcore::NewMessage<'a>> {
    let _ = hash;
    crate::headers::parse(body)
}

/// La date d'un message, en secondes Unix.
fn date_of(body: &[u8]) -> i64 {
    crate::headers::date(body)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::Selected;

    fn local(validity: Option<u32>, uidnext: Option<u32>, modseq: Option<u64>) -> SyncState {
        SyncState {
            remote_name: b"INBOX".to_vec(),
            uidvalidity: validity,
            uidnext,
            highest_modseq: modseq,
            synced_at: Some(1),
            subscribed: true,
        }
    }

    fn remote(validity: Option<u32>, uidnext: Option<u32>, modseq: Option<u64>) -> Selected {
        Selected {
            exists: 3,
            uidvalidity: validity,
            uidnext,
            highest_modseq: modseq,
        }
    }

    #[test]
    fn a_folder_never_synced_gets_a_full_harvest() {
        let (plan, reason) = plan(
            &local(None, None, None),
            &remote(Some(1), Some(4), Some(3)),
            true,
        );
        assert_eq!(plan, Plan::Full);
        assert_eq!(reason, Some(FullReason::Never));
    }

    #[test]
    fn a_changed_uidvalidity_gets_a_full_harvest() {
        // Tous les UID connus sont invalides d'un coup.
        let (plan, reason) = plan(
            &local(Some(1000), Some(4), Some(3)),
            &remote(Some(1001), Some(4), Some(3)),
            true,
        );
        assert_eq!(plan, Plan::Full);
        assert_eq!(reason, Some(FullReason::UidvalidityChanged));
    }

    #[test]
    fn a_server_without_uidvalidity_never_gets_an_incremental_harvest() {
        // Il ne promet rien sur la stabilité des UID : s'appuyer dessus serait supposer une
        // garantie qu'on n'a pas.
        let (plan, reason) = plan(
            &local(Some(1000), Some(4), Some(3)),
            &remote(None, Some(4), Some(3)),
            true,
        );
        assert_eq!(plan, Plan::Full);
        assert_eq!(reason, Some(FullReason::NoUidvalidity));
    }

    #[test]
    fn nothing_new_with_condstore_is_up_to_date() {
        let (plan, reason) = plan(
            &local(Some(1000), Some(4), Some(3)),
            &remote(Some(1000), Some(4), Some(3)),
            true,
        );
        assert_eq!(plan, Plan::UpToDate);
        assert_eq!(reason, None);
    }

    #[test]
    fn a_higher_modseq_with_condstore_is_incremental() {
        let (plan, _) = plan(
            &local(Some(1000), Some(4), Some(3)),
            &remote(Some(1000), Some(4), Some(9)),
            true,
        );
        assert_eq!(
            plan,
            Plan::Incremental {
                since: 3,
                new_from: 4
            }
        );
    }

    #[test]
    fn a_higher_uidnext_with_condstore_is_incremental_even_at_the_same_modseq() {
        // Un serveur peut annoncer un `UIDNEXT` plus haut sans que `HIGHESTMODSEQ` ait bougé
        // dans ce qu'il nous a dit. Croire au seul `MODSEQ` raterait les nouveaux messages.
        let (plan, _) = plan(
            &local(Some(1000), Some(4), Some(3)),
            &remote(Some(1000), Some(12), Some(3)),
            true,
        );
        assert_eq!(
            plan,
            Plan::Incremental {
                since: 3,
                new_from: 4
            }
        );
    }

    #[test]
    fn without_condstore_there_is_no_up_to_date() {
        // **Le point du module.** Un drapeau modifié ne bouge ni `UIDNEXT` ni le nombre de
        // messages : sans `MODSEQ`, la seule façon de savoir est de tout redemander.
        let (plan, reason) = plan(
            &local(Some(1000), Some(4), Some(3)),
            &remote(Some(1000), Some(4), None),
            false,
        );
        assert_eq!(plan, Plan::Rescan { new_from: 4 });
        assert_eq!(reason, None);
    }

    #[test]
    fn a_server_that_announced_condstore_then_refused_it_takes_the_fallback() {
        // Le paramètre est le résultat de l'`ENABLE`, pas la capacité annoncée.
        let (plan, _) = plan(
            &local(Some(1000), Some(4), Some(3)),
            &remote(Some(1000), Some(4), Some(3)),
            false,
        );
        assert_eq!(plan, Plan::Rescan { new_from: 4 });
    }

    #[test]
    fn condstore_without_a_local_modseq_takes_the_fallback() {
        // L'état local peut venir d'un passage antérieur à l'activation de `CONDSTORE`.
        let (plan, _) = plan(
            &local(Some(1000), Some(4), None),
            &remote(Some(1000), Some(4), Some(3)),
            true,
        );
        assert_eq!(plan, Plan::Rescan { new_from: 4 });
    }

    #[test]
    fn a_missing_local_uidnext_starts_from_the_first_uid() {
        // Un état à moitié écrit ne doit pas produire une plage vide, qui raterait tout.
        let (plan, _) = plan(
            &local(Some(1000), None, None),
            &remote(Some(1000), Some(9), None),
            false,
        );
        assert_eq!(plan, Plan::Rescan { new_from: 1 });
    }

    #[test]
    fn a_uid_set_is_written_in_ranges() {
        // Cent UID à sept chiffres en liste plate font 800 octets ; en plages, une vingtaine.
        assert_eq!(uid_set(&[1, 2, 3, 7, 8]).as_deref(), Some("1:3,7:8"));
        assert_eq!(uid_set(&[5]).as_deref(), Some("5"));
        assert_eq!(uid_set(&[9, 1, 2]).as_deref(), Some("1:2,9"));
        assert_eq!(uid_set(&[]), None);
    }

    #[test]
    fn a_uid_set_deduplicates() {
        // `mailfake::Fault::DuplicateUid` rend deux fois le même UID : le demander deux fois
        // serait une commande absurde, et l'ignorer est le comportement idempotent voulu.
        assert_eq!(uid_set(&[3, 3, 4]).as_deref(), Some("3:4"));
    }

    #[test]
    fn a_uid_set_does_not_overflow_at_the_last_uid() {
        assert_eq!(uid_set(&[u32::MAX]).as_deref(), Some("4294967295"));
        assert_eq!(
            uid_set(&[u32::MAX - 1, u32::MAX]).as_deref(),
            Some("4294967294:4294967295")
        );
    }

    #[test]
    fn known_imap_flags_are_translated() {
        let flags = flags_of(&[
            "\\Seen".to_owned(),
            "\\Flagged".to_owned(),
            "\\Answered".to_owned(),
            "\\Draft".to_owned(),
            "\\Deleted".to_owned(),
        ]);
        for expected in [
            MessageFlags::SEEN,
            MessageFlags::FLAGGED,
            MessageFlags::ANSWERED,
            MessageFlags::DRAFT,
            MessageFlags::DELETED,
        ] {
            assert!(flags.contains(expected));
        }
    }

    #[test]
    fn imap_flags_are_case_insensitive() {
        // La RFC 3501 les définit insensibles à la casse, et les serveurs en profitent.
        assert!(flags_of(&["\\SEEN".to_owned()]).contains(MessageFlags::SEEN));
        assert!(flags_of(&["\\seen".to_owned()]).contains(MessageFlags::SEEN));
    }

    #[test]
    fn an_unknown_keyword_is_ignored_not_an_error() {
        // `$Forwarded`, `NonJunk`, `$Junk` : légaux et courants. Échouer dessus rendrait la
        // moisson impossible chez la plupart des fournisseurs.
        let flags = flags_of(&[
            "$Forwarded".to_owned(),
            "NonJunk".to_owned(),
            "\\Seen".to_owned(),
        ]);
        assert_eq!(flags, MessageFlags::SEEN);
    }

    #[test]
    fn a_report_that_wrote_nothing_is_recognised() {
        // La formulation testable du critère 3.
        assert!(Report::default().wrote_nothing());
        assert!(
            !Report {
                stored: 1,
                ..Report::default()
            }
            .wrote_nothing()
        );
        assert!(
            !Report {
                vanished: 1,
                ..Report::default()
            }
            .wrote_nothing()
        );
    }
}

/// Découpe les UID en lots bornés **en nombre et en octets**. Fonction pure.
///
/// ## Les deux bornes, et pourquoi il en faut deux
///
/// [`BATCH`] borne les allers-retours : un UID par commande ferait cent mille allers-retours
/// sur le plus gros dossier du corpus. [`BATCH_BYTES`] borne la mémoire : cent messages
/// n'occupent pas la même place selon qu'ils font trois kilooctets ou vingt-cinq mégaoctets, et
/// la première version ne comptait que les messages.
///
/// ## Un message plus gros que la borne part seul
///
/// Sinon il ne partirait jamais. Un lot d'un seul message de 40 Mo tient 40 Mo en mémoire, ce
/// qui dépasse la borne — c'est inévitable, le plafond par message est de 128 Mio et un message
/// se télécharge en entier ou pas du tout. La borne dit « pas plusieurs gros ensemble », pas
/// « jamais de gros ».
///
/// ## Une taille inconnue compte pour la moyenne du corpus
///
/// Un serveur peut ne pas rendre `RFC822.SIZE`. Le compter pour zéro ramènerait la borne à celle
/// du nombre — exactement le défaut qu'on corrige. Le compter pour le plafond ferait des lots
/// d'un seul message, donc un aller-retour par message. La moyenne mesurée du corpus réel — 62 Ko
/// pour 105 508 références — est l'estimation la moins fausse des trois.
fn batches(wanted: &[u32], announced: &HashMap<u32, u64>) -> Vec<Vec<u32>> {
    /// Ce qu'on suppose d'un message dont le serveur n'annonce pas la taille.
    const ASSUMED: u64 = 64 * 1024;

    let mut out = Vec::new();
    let mut current: Vec<u32> = Vec::new();
    let mut bytes = 0_u64;

    for &uid in wanted {
        let size = announced.get(&uid).copied().unwrap_or(ASSUMED);
        // Le lot se ferme **avant** d'ajouter celui qui le ferait déborder, et jamais sur un
        // lot vide : c'est ce qui fait qu'un message plus gros que la borne part seul.
        if !current.is_empty() && (current.len() >= BATCH || bytes + size > BATCH_BYTES) {
            out.push(std::mem::take(&mut current));
            bytes = 0;
        }
        current.push(uid);
        bytes = bytes.saturating_add(size);
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod batch_tests {
    use super::{BATCH, BATCH_BYTES, batches};
    use std::collections::HashMap;

    /// Des UID de 1 à `count`, tous de la même taille annoncée.
    fn uniform(count: u32, size: u64) -> (Vec<u32>, HashMap<u32, u64>) {
        let uids: Vec<u32> = (1..=count).collect();
        let sizes = uids.iter().map(|&uid| (uid, size)).collect();
        (uids, sizes)
    }

    #[test]
    fn nothing_wanted_makes_no_batch() {
        assert!(batches(&[], &HashMap::new()).is_empty());
    }

    #[test]
    fn small_messages_fill_a_batch_up_to_the_count() {
        // Le cas ordinaire du corpus : 3 Ko le message, donc c'est le nombre qui ferme le lot.
        let (uids, sizes) = uniform(250, 3 * 1024);
        let made = batches(&uids, &sizes);
        assert_eq!(made.len(), 3, "250 messages en lots de {BATCH}");
        assert_eq!(made[0].len(), BATCH);
        assert_eq!(made[2].len(), 50);
    }

    #[test]
    fn big_messages_close_a_batch_before_the_count() {
        // **Le cas qui a coûté 266 Mio.** Des messages de 8 Mo : quatre remplissent la borne
        // d'octets, très loin des cent que l'ancien découpage aurait pris ensemble.
        let (uids, sizes) = uniform(20, 8 * 1024 * 1024);
        let made = batches(&uids, &sizes);
        assert!(
            made.iter().all(|it| it.len() <= 4),
            "un lot dépasse la borne d'octets : {:?}",
            made.iter().map(Vec::len).collect::<Vec<_>>()
        );
        assert_eq!(
            made.iter().map(Vec::len).sum::<usize>(),
            20,
            "des UID perdus"
        );
    }

    #[test]
    fn a_message_bigger_than_the_bound_goes_alone_rather_than_never() {
        let uids = vec![1, 2, 3];
        let mut sizes = HashMap::new();
        sizes.insert(1, 1024);
        sizes.insert(2, BATCH_BYTES * 2);
        sizes.insert(3, 1024);
        let made = batches(&uids, &sizes);

        assert_eq!(made, vec![vec![1], vec![2], vec![3]], "{made:?}");
    }

    #[test]
    fn an_unannounced_size_does_not_disable_the_bound() {
        // Le compter pour zéro ramènerait la borne à celle du nombre — le défaut qu'on corrige.
        let uids: Vec<u32> = (1..=1000).collect();
        let made = batches(&uids, &HashMap::new());
        assert!(
            made.iter().all(|it| it.len() <= BATCH),
            "la borne en nombre ne tient plus"
        );
        assert_eq!(made.iter().map(Vec::len).sum::<usize>(), 1000);
    }

    #[test]
    fn every_uid_is_in_exactly_one_batch_and_in_order() {
        // Un découpage qui perdrait ou dupliquerait un UID ferait manquer du courrier, ou le
        // retéléchargerait. L'ordre compte pour la reprise : les lots validés sont les premiers.
        let (uids, sizes) = uniform(137, 700 * 1024);
        let flat: Vec<u32> = batches(&uids, &sizes).into_iter().flatten().collect();
        assert_eq!(flat, uids);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod credential_tests {
    use super::Credential;
    use mailcore::AuthKind;

    #[test]
    fn the_debug_output_never_carries_the_secret() {
        // Le symétrique du test de `mailsmtp::submit`. La dérivation de `Debug` affichait le
        // secret ; un `tracing::debug!(?credential)` l'aurait mis dans un journal.
        for credential in [
            Credential::for_auth(AuthKind::Password, "mot-de-passe-tres-secret"),
            Credential::for_auth(AuthKind::OAuth2, "ya29.jeton-tres-secret"),
        ] {
            let shown = format!("{credential:?}");
            assert!(
                !shown.contains("secret"),
                "le secret apparaît dans le Debug : {shown}"
            );
            // Et le mécanisme, lui, doit rester lisible : c'est ce qui rend un journal utile.
            assert!(
                shown.contains("Password") || shown.contains("Bearer"),
                "le mécanisme a disparu du Debug : {shown}"
            );
        }
    }
}
