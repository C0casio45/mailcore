//! `mail send` — le premier morceau irréparable du projet.
//!
//! ## Ce que la commande fait dans quel ordre, et pourquoi cet ordre
//!
//! 1. compose le message et le **refuse** s'il n'est pas envoyable — une adresse qui porte un
//!    retour à la ligne, aucun destinataire — ce qui n'ouvre aucune connexion ;
//! 2. écrit les octets dans le magasin de blobs, puis la ligne de file, **et valide** ;
//! 3. seulement ensuite, essaie de remettre.
//!
//! Entre 2 et 3, un `Ctrl-C` ne perd rien : le message est en file, et le passage suivant le
//! reprend. C'est l'inverse d'une commande qui enverrait puis enregistrerait, où le même
//! `Ctrl-C` perdrait le message ou le doublerait.
//!
//! ## `--dry-run` n'ouvre pas de connexion, et c'est le premier essai à faire
//!
//! Il assemble, affiche les en-têtes et l'enveloppe, et s'arrête. C'est ce qui permet de
//! vérifier qu'une copie cachée est bien dans l'enveloppe et **dans aucun en-tête** avant
//! d'envoyer quoi que ce soit à qui que ce soit.

use anyhow::{Context, Result, bail};
use camino::Utf8PathBuf;
use mailcore::{SendState, Store};
use mailsmtp::client::Credential;
use mailsmtp::compose::{Address, Draft};

use mailapi::human;
use mailsmtp::queue::{Outcome, deliver_one};
use mailsmtp::submit::Submitter;

/// Ce que la ligne de commande dit du message à envoyer.
#[derive(Debug, Clone)]
pub struct Message {
    /// Le compte qui envoie.
    pub account: i64,
    /// Les destinataires visibles.
    pub to: Vec<String>,
    /// En copie, visibles.
    pub cc: Vec<String>,
    /// En copie **cachée** : dans l'enveloppe, dans aucun en-tête.
    pub bcc: Vec<String>,
    /// Le sujet.
    pub subject: String,
    /// Le corps en texte brut. Lu sur l'entrée standard s'il est absent.
    pub body: Option<String>,
    /// Assembler et afficher, sans ouvrir de connexion.
    pub dry_run: bool,
    /// Les fichiers à joindre. Rangés dans le magasin de blobs avant l'assemblage.
    pub attach: Vec<Utf8PathBuf>,
    /// Joindre la signature du compte.
    ///
    /// L'assemblage est `Draft::sign_with`, la même fonction que celle qu'`outbox.send`
    /// appelle : « comment une signature rejoint un message » n'a qu'une implémentation.
    pub signature: bool,
}

/// Compose, met en file, et essaie de remettre.
///
/// # Errors
///
/// Si le compte est inconnu, sans serveur de soumission, si le message n'est pas envoyable, ou
/// si le store est illisible. **Un refus du serveur n'est pas une erreur de cette fonction** :
/// il est écrit dans la file et affiché ; le code de sortie le dit.
pub fn run(root: Option<&Utf8PathBuf>, it: &Message) -> Result<()> {
    let store = open(root)?;
    let accounts = store.full_accounts()?;
    let account = accounts
        .iter()
        .find(|candidate| candidate.id.0 == it.account)
        .with_context(|| format!("aucun compte #{} — voir `mail account list`", it.account))?;
    let reading = account.server.as_ref().with_context(|| {
        format!(
            "le compte #{} n'a pas de serveur : il ne peut pas envoyer",
            it.account
        )
    })?;

    // L'expéditeur est l'identifiant du compte. Pas un `--from` : envoyer sous une autre
    // adresse que celle du compte se fait refuser par tous les serveurs de soumission du
    // corpus, et l'option ferait croire le contraire.
    // Le nom affiché n'est joint que s'il **dit quelque chose de plus** que l'adresse. Chez la
    // plupart des comptes du corpus, `display_name` est l'adresse elle-même : le joindre
    // donnerait `From: "marie@exemple.fr" <marie@exemple.fr>`, que tous les lecteurs affichent
    // en double.
    let name = Some(account.display_name.as_str()).filter(|it| *it != reading.username);
    let from = Address::parse(&reading.username, name).with_context(|| {
        format!(
            "l'identifiant du compte #{} n'est pas une adresse valide",
            it.account
        )
    })?;

    let mut draft = Draft::new(from, parse_all(&it.to)?, &it.subject, &body(it)?);
    draft.cc = parse_all(&it.cc)?;
    draft.bcc = parse_all(&it.bcc)?;

    // La signature d'abord, les pièces jointes ensuite : `sign_with` réécrit le corps et sa
    // partie HTML, et il n'a rien à voir avec ce qui est joint.
    if it.signature {
        match store.signature(account.id)? {
            Some(signature) => draft.sign_with(&signature),
            // Le dire plutôt que de signer avec du vide : quelqu'un qui passe `--signature`
            // croit en avoir une, et découvrir l'inverse à la réception serait tard.
            None => bail!(
                "le compte #{} n'a pas de signature — la coquille en pose une, ou \
                 `accounts.set_signature` par l'API",
                it.account
            ),
        }
    }

    // Les pièces jointes sont rangées dans le magasin **avant** l'assemblage : le brouillon les
    // désigne par leur contenu, jamais par un chemin. Voir `compose::Attachment`.
    for path in &it.attach {
        draft.attachments.push(attach(&store, path)?);
    }

    let now = now();
    let recipients: Vec<String> = draft
        .envelope_recipients()
        .iter()
        .map(|address| address.addr().to_owned())
        .collect();

    if it.dry_run {
        return show(&store, &draft, now, &recipients);
    }

    let submission = account.submission.as_ref().with_context(|| {
        format!(
            "le compte #{} n'a pas de serveur d'envoi — `mail account submission --account {}`",
            it.account, it.account
        )
    })?;

    // **Une seule fonction assemble et met en file**, partagée avec `outbox.send` : deux copies
    // divergeaient déjà le jour où l'assemblage est devenu streamé.
    // **Sans maintien, parce que cette commande remet elle-même** : la connexion s'ouvre trois
    // lignes plus bas, sur la ligne nommée, sans passer par `Store::deliverable`. Une fenêtre
    // de rétractation n'y retarderait rien — elle annoncerait un délai qui n'existe pas. Celui
    // qui veut la fenêtre passe par la coquille ou par `outbox.send`, dont le facteur remet.
    let (id, size) = mailsmtp::queue::stage(&store, account.id, &draft, now, 0)
        .context("mise en file du message")?;
    println!("Message #{} en file ({}).", id.0, human::bytes(size));

    // Le secret est celui de la lecture, et le branchement sur le mécanisme est chez
    // `mailauth` : deux copies finiraient par ne plus lire la même entrée de trousseau.
    let secret = mailauth::session::secret_for(
        &reading.host,
        &reading.username,
        submission.auth.as_str(),
        now,
    )
    .with_context(|| format!("secret du compte #{}", it.account))?;

    let job = store
        .outgoing(id)?
        .context("la ligne de file vient de disparaître")?;
    let mut transport = Submitter {
        server: submission,
        credential: Credential::for_auth(submission.auth, &secret),
    };

    let outcome = deliver_one(&store, &mut transport, &job, now).context("remise")?;
    report(&store, id, outcome)
}

/// Vide la file : tout ce qui est remettable, une fois chacun.
///
/// ## Ce que cette commande ne touche pas
///
/// Les messages **douteux**. Ils ne sont pas remettables, et c'est la règle du critère 2 de
/// `docs/PHASE-3.md` : le serveur a peut-être le sien, et le renvoyer risque un doublon chez le
/// destinataire. `mail doctor` les compte et dit qu'ils attendent une décision.
///
/// # Errors
///
/// Si le store est illisible. Un refus du serveur n'est pas une erreur : il est affiché, et le
/// code de sortie le dit.
pub fn flush(root: Option<&Utf8PathBuf>) -> Result<()> {
    let store = open(root)?;
    let accounts = store.full_accounts()?;
    let now = now();
    let pending = store.deliverable(now, 100)?;

    if pending.is_empty() {
        let doubtful = store.doubtful()?.len();
        println!("Rien à remettre.");
        if doubtful > 0 {
            println!(
                "{doubtful} message(s) douteux attendent une décision — `mail doctor` pour les \
                 voir."
            );
        }
        return Ok(());
    }

    let mut failures = 0_usize;
    for job in pending {
        let Some(account) = accounts.iter().find(|it| it.id == job.account) else {
            println!("#{} : compte inconnu, ignoré.", job.id.0);
            failures += 1;
            continue;
        };
        let (Some(reading), Some(submission)) = (&account.server, &account.submission) else {
            println!(
                "#{} : le compte n'a pas de serveur d'envoi, ignoré.",
                job.id.0
            );
            failures += 1;
            continue;
        };
        let secret = match mailauth::session::secret_for(
            &reading.host,
            &reading.username,
            submission.auth.as_str(),
            now,
        ) {
            Ok(secret) => secret,
            Err(source) => {
                println!("#{} : {source}", job.id.0);
                failures += 1;
                continue;
            }
        };
        let mut transport = Submitter {
            server: submission,
            credential: Credential::for_auth(submission.auth, &secret),
        };
        match deliver_one(&store, &mut transport, &job, now) {
            Ok(outcome) => {
                if report(&store, job.id, outcome).is_err() {
                    failures += 1;
                }
            }
            Err(source) => {
                println!("#{} : {source}", job.id.0);
                failures += 1;
            }
        }
    }

    if failures > 0 {
        bail!("{failures} message(s) ne sont pas partis");
    }
    Ok(())
}

/// Affiche la file, états compris.
///
/// # Errors
///
/// Si le store est illisible.
pub fn list(root: Option<&Utf8PathBuf>) -> Result<()> {
    let store = open(root)?;
    let outbox = store.outbox()?;
    if outbox.is_empty() {
        println!("File d'envoi vide.");
        return Ok(());
    }
    for job in &outbox {
        let state = match job.state {
            SendState::Queued => "en attente",
            SendState::Sending => "interrompu avant le corps — sera repris",
            SendState::Committing => "DOUTEUX — peut-être parti, aucune reprise automatique",
            SendState::Sent => "envoyé",
            SendState::Failed => "échoué",
        };
        println!(
            "#{} {} → {} [{state}]",
            job.id.0,
            job.sender,
            job.recipients.join(", "),
        );
        if let Some(error) = &job.last_error {
            println!("    {error}");
        }
        // **Le geste, à côté de la phrase qui le demande.** Le critère 8 veut que l'utilisateur
        // voie quoi faire ; lui dire « renvoyez le message » sans dire comment le renvoyer
        // laisse le travail à moitié fait, et la seule autre sortie de `failed` est `--forget`,
        // qui jette le message.
        if job.resendable {
            println!(
                "    `mail outbox --retry {}` pour le renvoyer tel quel.",
                job.id.0
            );
        }
    }
    Ok(())
}

/// Dit ce qu'une remise a donné, et échoue quand il le faut.
fn report(store: &Store, id: mailcore::OutboxId, outcome: Outcome) -> Result<()> {
    let line = store.outgoing(id)?;
    let error = line.as_ref().and_then(|it| it.last_error.clone());
    match outcome {
        Outcome::Sent => {
            println!("#{} envoyé.", id.0);
            Ok(())
        }
        Outcome::Deferred => {
            println!(
                "#{} reporté : {}",
                id.0,
                error.unwrap_or_else(|| "refus passager".to_owned())
            );
            println!("`mail send --flush` réessaiera quand le recul sera écoulé.");
            Ok(())
        }
        Outcome::Failed => {
            bail!(
                "#{} refusé : {}",
                id.0,
                error.unwrap_or_else(|| "refus définitif".to_owned())
            )
        }
        Outcome::Doubtful => {
            // **Pas une erreur, et pas un succès.** L'utilisateur doit décider, et le message
            // doit être assez clair pour qu'il décide sans lire le code.
            println!("#{} : ÉTAT INCERTAIN.", id.0);
            if let Some(error) = error {
                println!("    {error}");
            }
            println!();
            println!(
                "La coupure est tombée entre le point final et la réponse du serveur. Il a \
                 peut-être le message."
            );
            println!(
                "Rien ne sera renvoyé automatiquement : le renvoyer risque un doublon chez le \
                 destinataire."
            );
            println!("Vérifier dans les messages envoyés du fournisseur avant de décider.");
            bail!("#{} est dans un état incertain", id.0)
        }
    }
}

/// Affiche ce qui partirait, sans rien ouvrir.
fn show(store: &Store, draft: &Draft, now: i64, recipients: &[String]) -> Result<()> {
    println!("Enveloppe SMTP");
    println!("  MAIL FROM: <{}>", draft.from.addr());
    for recipient in recipients {
        println!("  RCPT TO:   <{recipient}>");
    }
    println!();

    // **Écrit dans un compteur, pas dans un tampon.** Un `--dry-run` sur 25 Mo de pièces
    // jointes ne doit pas coûter 34 Mo de mémoire pour afficher huit lignes d'en-têtes.
    // Les en-têtes sont retenus jusqu'à la ligne vide, le reste est compté et jeté.
    let mut peek = Peek::default();
    draft
        .write_to(&mut peek, now, &mut |hash| {
            store
                .blobs()
                .open(hash)
                .map(|it| Box::new(it) as Box<dyn std::io::Read>)
                .map_err(|source| mailsmtp::Error::Unsendable {
                    reason: format!("pièce jointe introuvable : {source}"),
                })
        })
        .context("assemblage du message")?;

    println!("Message ({})", human::bytes(peek.total));
    // Les en-têtes seuls : le corps est ce que l'utilisateur vient de taper, il n'a pas besoin
    // de le relire. Ce qu'il ne peut pas vérifier autrement, c'est **quels en-têtes** partent.
    for line in String::from_utf8_lossy(&peek.head).lines() {
        if line.is_empty() {
            break;
        }
        println!("  {line}");
    }
    if !draft.attachments.is_empty() {
        println!();
        println!("Pièces jointes");
        for attachment in &draft.attachments {
            println!(
                "  {} — {} ({} une fois encodée)",
                attachment.filename,
                human::bytes(attachment.size),
                human::bytes(attachment.encoded_len()),
            );
        }
    }
    println!();
    println!("Rien n'a été envoyé, et aucune connexion n'a été ouverte.");
    if !draft.attachments.is_empty() {
        // **Un `--dry-run` qui écrit quand même**, et il vaut mieux le dire. Une pièce jointe
        // doit être dans le magasin pour que l'assemblage puisse la lire en flux : l'éviter
        // demanderait un second chemin d'assemblage qui lise depuis un fichier, donc une
        // deuxième implémentation de ce que le critère 3 vient de rendre correct.
        println!(
            "Les pièces jointes ont été rangées dans le magasin — `mail doctor` les compte \n             comme orphelines tant qu'aucun message ne les désigne."
        );
    }
    Ok(())
}

/// Un puits qui garde le début et compte le reste.
///
/// Sert à `--dry-run` : les en-têtes se lisent, le corps se compte. Sans lui, afficher les
/// en-têtes d'un message de 25 Mo demanderait de l'assembler en mémoire — ce que le critère 3
/// interdit précisément.
#[derive(Debug, Default)]
struct Peek {
    /// Le début du message, borné.
    head: Vec<u8>,
    /// Tout ce qui a été écrit.
    total: u64,
}

/// Combien d'octets de tête garder.
///
/// 8 Kio : bien plus que les en-têtes d'un message ordinaire — quelques centaines d'octets — et
/// assez pour qu'un `References` d'un fil de cinquante messages y tienne en entier.
const PEEK: usize = 8 * 1024;

impl std::io::Write for Peek {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        let room = PEEK.saturating_sub(self.head.len());
        if room > 0 {
            self.head
                .extend_from_slice(&buffer[..room.min(buffer.len())]);
        }
        self.total = self.total.saturating_add(buffer.len() as u64);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Range un fichier dans le magasin de blobs, et rend la pièce jointe qui le désigne.
///
/// ## Le fichier est lu en flux
///
/// `put_reader` hache et compresse au passage : une pièce de 25 Mo se range sans être chargée.
/// C'est le critère 3, et c'est aussi la règle 4 du `CLAUDE.md`.
///
/// ## Le type MIME est déduit de l'extension, et c'est assumé
///
/// Deviner d'après le contenu — les octets magiques — demanderait une table de signatures, et
/// se tromper n'a pas de conséquence grave : le destinataire voit un
/// `application/octet-stream`, et son client déduit lui-même. Une extension inconnue donne donc
/// le type générique plutôt qu'un refus.
fn attach(store: &Store, path: &Utf8PathBuf) -> Result<mailsmtp::compose::Attachment> {
    let mut file =
        std::fs::File::open(path.as_std_path()).with_context(|| format!("ouverture de {path}"))?;
    let size = file
        .metadata()
        .with_context(|| format!("taille de {path}"))?
        .len();
    let put = store
        .blobs()
        .put_reader(&mut file)
        .with_context(|| format!("mise au magasin de {path}"))?;

    let filename = path.file_name().unwrap_or("piece-jointe").to_owned();
    Ok(mailsmtp::compose::Attachment {
        mime: mailsmtp::compose::mime_for(&filename).to_owned(),
        filename,
        blob: put.hash,
        size,
    })
}

/// Le corps : l'option, ou l'entrée standard.
fn body(it: &Message) -> Result<String> {
    if let Some(body) = &it.body {
        return Ok(body.clone());
    }
    use std::io::Read as _;
    let mut buffer = String::new();
    std::io::stdin()
        .read_to_string(&mut buffer)
        .context("lecture du corps sur l'entrée standard")?;
    if buffer.trim().is_empty() {
        bail!("corps vide : `--body` ou du texte sur l'entrée standard");
    }
    Ok(buffer)
}

/// Analyse une liste d'adresses, et refuse la première mauvaise.
///
/// Le refus est **global** : envoyer à trois destinataires sur quatre en taisant le quatrième
/// laisserait l'utilisateur croire que tout est parti.
fn parse_all(raw: &[String]) -> Result<Vec<Address>> {
    raw.iter()
        .map(|it| Address::parse(it, None).with_context(|| format!("adresse invalide : {it:?}")))
        .collect()
}

/// Ouvre le store local.
fn open(root: Option<&Utf8PathBuf>) -> Result<Store> {
    let root = crate::store_root(root)?;
    Store::open(&root).with_context(|| format!("ouverture du store {root}"))
}

/// Secondes Unix.
fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| {
            i64::try_from(since.as_secs()).unwrap_or(i64::MAX)
        })
}

/// Tranche le doute sur un message, **sur décision de l'utilisateur**.
///
/// ## La seule sortie de l'état incertain
///
/// Rien dans le programme ne l'appelle tout seul : ni le facteur du démon, ni `--flush`, ni un
/// redémarrage. C'est le critère 2 de `docs/PHASE-3.md` — un message peut-être parti ne repart
/// pas sans qu'un humain l'ait demandé.
///
/// `Store::resolve_doubt` refuse toute ligne qui n'est pas douteuse, ce qui empêche cette
/// commande d'être un contournement de la file : `--resend` sur un message déjà envoyé ne
/// l'enverra pas deux fois.
///
/// ## Elle ne remet rien elle-même
///
/// `--resend` remet la ligne en file. C'est `mail send --flush`, ou le facteur du démon, qui la
/// remettra — donc la commande rend la main tout de suite, et le message part quand il part.
///
/// # Errors
///
/// Si le store est illisible, si la décision est inconnue, ou si la ligne n'est pas douteuse.
pub fn decide(root: Option<&Utf8PathBuf>, id: i64, decision: &str) -> Result<()> {
    let store = open(root)?;
    let choice = mailcore::store::outbox::Decision::parse(decision)
        .with_context(|| format!("décision inconnue : {decision}"))?;
    let target = mailcore::OutboxId(id);

    if !store.resolve_doubt(target, choice)? {
        // Le message dit **pourquoi** : « refusé » sans raison ferait chercher au mauvais
        // endroit. L'état actuel est l'information utile.
        let state = store.outgoing(target)?.map_or_else(
            || "inexistant".to_owned(),
            |it| it.state.as_str().to_owned(),
        );
        bail!(
            "le message #{id} est à l'état « {state} » : seul un envoi incertain se tranche. \
             `mail outbox` pour voir la file."
        );
    }

    match choice {
        mailcore::store::outbox::Decision::Resend => {
            println!("#{id} remis en file.");
            println!("`mail send --flush` pour le remettre maintenant, ou le démon s'en chargera.");
        }
        mailcore::store::outbox::Decision::Accept => {
            println!("#{id} marqué envoyé. Aucun octet n'est parti.");
        }
    }
    Ok(())
}

/// Remet en file un envoi **échoué**.
///
/// ## Pourquoi ce n'est pas `--resend`
///
/// `--resend` sort de l'état incertain, où le serveur a peut-être le message : c'est un pari, et
/// il demande d'avoir vérifié chez le fournisseur. `--retry` sort de `failed`, où le serveur a
/// refusé — donc n'a rien pris, donc **aucun doublon possible**. Deux gestes, deux risques
/// différents, deux options : les confondre sous un nom ferait prendre le pari sans le savoir.
///
/// ## Elle ne renvoie rien elle-même
///
/// Elle remet la ligne en file et rend la main. C'est `mail send --flush`, ou le facteur du
/// démon, qui parlera au serveur — la règle 3 du `CLAUDE.md` vaut aussi pour la CLI.
///
/// # Errors
///
/// Si le store est illisible, ou si la ligne n'est pas à l'état `failed`.
pub fn retry(root: Option<&Utf8PathBuf>, id: i64) -> Result<()> {
    let store = open(root)?;
    let target = mailcore::OutboxId(id);

    if !store.retry_outgoing(target)? {
        let state = store.outgoing(target)?.map_or_else(
            || "inexistant".to_owned(),
            |it| it.state.as_str().to_owned(),
        );
        bail!(
            "le message #{id} est à l'état « {state} » : seul un envoi échoué se renvoie. \
             `mail outbox` pour voir la file."
        );
    }

    println!("#{id} remis en file, compteur de tentatives remis à zéro.");
    println!("`mail send --flush` pour l'envoyer maintenant, ou le démon s'en chargera.");
    Ok(())
}

/// Retire une ligne **finie** de la file.
///
/// ## Ce qu'elle refuse, et pourquoi
///
/// Une ligne en cours ou douteuse. Retirer une ligne en cours perdrait un message que le
/// facteur allait remettre ; retirer une ligne douteuse effacerait la trace d'un message
/// peut-être parti — exactement l'information que le critère 2 existe pour conserver.
///
/// ## Le blob reste
///
/// Il devient orphelin, et `mail doctor` le compte. `mail doctor --purge-orphans` le supprime,
/// et il demande explicitement qu'aucun import ne tourne — un blob fraîchement écrit ressemble
/// à un orphelin.
///
/// # Errors
///
/// Si le store est illisible, ou si la ligne n'est pas retirable.
pub fn forget(root: Option<&Utf8PathBuf>, id: i64) -> Result<()> {
    let store = open(root)?;
    let target = mailcore::OutboxId(id);
    if !store.forget_outgoing(target)? {
        let state = store.outgoing(target)?.map_or_else(
            || "inexistant".to_owned(),
            |it| it.state.as_str().to_owned(),
        );
        bail!(
            "le message #{id} est à l'état « {state} » : seuls les envois finis — envoyés ou \
             échoués — se retirent. `mail outbox` pour voir la file."
        );
    }
    println!("#{id} retiré de la file.");
    println!("Son contenu devient un blob orphelin — `mail doctor` le compte.");
    Ok(())
}

/// Annule une ligne **qui n'est pas encore partie**.
///
/// ## Ce qu'elle refuse, et pourquoi
///
/// Tout ce qui n'est pas `queued`. C'est la symétrique exacte de [`forget`], qui ne retire que
/// des lignes finies : entre les deux, `sending` et `committing` ne sortent par aucune des deux,
/// parce que ce sont les états où personne ne sait ce que le serveur a vu.
///
/// ## Le message d'échec nomme l'état, et il a une raison de le faire
///
/// « Trop tard » sans rien d'autre laisserait croire à un bogue. L'état dit lequel des trois cas
/// on regarde : parti pour de bon, en cours, ou douteux — et le troisième renvoie vers la
/// décision, qui est un geste différent.
///
/// # Errors
///
/// Si le store est illisible, ou si la ligne n'est plus annulable.
pub fn cancel(root: Option<&Utf8PathBuf>, id: i64) -> Result<()> {
    let store = open(root)?;
    let target = mailcore::OutboxId(id);
    if !store.cancel_outgoing(target)? {
        let state = store.outgoing(target)?.map_or_else(
            || "inexistant".to_owned(),
            |it| it.state.as_str().to_owned(),
        );
        bail!(
            "le message #{id} est à l'état « {state} » : l'annulation ne retire que ce qui \
             n'est pas encore parti. `mail outbox` pour voir la file."
        );
    }
    println!("#{id} annulé : il ne partira pas.");
    println!("Son contenu devient un blob orphelin — `mail doctor` le compte.");
    Ok(())
}
