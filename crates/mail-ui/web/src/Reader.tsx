/**
 * Le volet de lecture. **Critère 8 : zéro requête déclenchée par le contenu d'un message.**
 *
 * ## Les deux barrières, et laquelle est appliquée où
 *
 * L'assainissement a déjà eu lieu **côté démon** (`mailhtml::sanitize`) : ce qui arrive ici ne
 * contient plus de `<script>`, plus de `<style>`, plus d'URL distante en position chargeable.
 * C'est la deuxième ceinture, celle que nous écrivons.
 *
 * La première — la CSP — est appliquée par le moteur de rendu, et c'est ce fichier qui la
 * pose. Elle vient du démon avec le corps (`Html.csp`), jamais d'une constante recopiée ici :
 * une deuxième copie, même identique le jour où elle est écrite, est exactement le point de
 * défaillance unique que `docs/PRIVACY.md` cherche à écarter.
 *
 * ## `srcdoc` plutôt qu'une URL, et ce que ça impose
 *
 * Le corps n'a pas d'URL : il arrive dans une réponse JSON. Il est donc injecté par `srcdoc`.
 * Conséquence directe : **la CSP ne peut pas être un en-tête HTTP** — il n'y a pas de réponse
 * HTTP pour la porter — elle passe par un `<meta http-equiv="Content-Security-Policy">` en tête
 * du document. C'est un mécanisme équivalent pour toutes les directives qu'on utilise.
 *
 * Deux directives ne fonctionnent **pas** en `<meta>` : `sandbox` et `frame-ancestors`. Le
 * `sandbox` est donc posé en attribut de l'`<iframe>`, ce qui est de toute façon le bon
 * endroit ; `frame-ancestors` n'a pas d'objet ici, rien n'encadre ce document.
 *
 * ## Un effet de bord qui renforce le confinement
 *
 * Le `sandbox` du démon n'accorde pas `allow-same-origin`. Le document de l'`<iframe>` a donc
 * une **origine opaque**, et `'self'` dans la CSP ne désigne alors plus rien du tout : même
 * `img-src 'self'` ne peut charger aucune image. C'est plus strict que ce que la politique
 * laisse croire, et c'est tant mieux — mais il faut le savoir avant de compter sur `'self'`
 * pour servir un jour les images `cid:` d'un message. Elles devront être des `data:`.
 *
 * ## Pourquoi le corps est réécrit à chaque message plutôt que muté
 *
 * Changer de message remplace le `srcdoc`, ce qui recrée le document. C'est délibéré :
 * réutiliser un document déjà chargé pour y injecter un autre corps voudrait dire faire
 * confiance à l'absence de résidus du précédent. Un document neuf n'a pas d'état.
 */

import { For, Show, createMemo } from "solid-js";
import type { Message, Tracker } from "./types";

/** Ce que le volet a besoin de savoir. */
export interface ReaderProps {
  /** Le message ouvert, ou `null` si rien n'est sélectionné. */
  message: Message | null;
  /** Vrai pendant le chargement. */
  loading: boolean;
  /** L'erreur d'ouverture, s'il y en a une. */
  error: string | null;
  /** Vrai si l'utilisateur a débloqué les images pour ce message. */
  imagesShown: boolean;
  /** Demande un nouveau rendu avec les images. */
  onShowImages: () => void;
}

/** Le libellé d'un signal de traçage. */
function trackerLabel(kind: Tracker["kind"]): string {
  switch (kind) {
    case "pixel":
      return "pixel espion";
    case "known_domain":
      return "domaine de traçage connu";
    case "correlated_id":
      return "identifiant qui vous désigne";
  }
}

/**
 * Le document complet à injecter dans l'`<iframe>`.
 *
 * La CSP est en première position dans le `<head>` : un `<meta>` de politique n'agit que sur
 * ce qui le suit, donc tout ce qui pourrait charger quelque chose doit venir après.
 */
function srcdoc(csp: string, body: string): string {
  return `<!doctype html>
<html><head>
<meta http-equiv="Content-Security-Policy" content="${csp.replaceAll('"', "&quot;")}">
<meta charset="utf-8">
<style>
  /* Le style du conteneur, pas du message : le message garde le sien. Police système et
     couleurs héritées du thème, pour qu'un mail sans mise en forme ne détonne pas. */
  html { color-scheme: light dark; }
  body {
    margin: 0; padding: 12px 14px;
    font: 13px/1.45 system-ui, "Segoe UI", sans-serif;
    color: CanvasText; background: Canvas;
    overflow-wrap: break-word;
  }
  img { max-width: 100%; height: auto; }
  table { max-width: 100%; }
  blockquote {
    margin: 8px 0; padding-left: 10px;
    border-left: 2px solid GrayText; color: GrayText;
  }
  a { color: LinkText; }
</style>
</head><body>${body}</body></html>`;
}

export function Reader(props: ReaderProps) {
  const html = createMemo(() => props.message?.html);

  /** Le document à rendre. Recalculé quand le message ou la politique change. */
  const document_ = createMemo(() => {
    const it = html();
    if (it === undefined) return null;
    return srcdoc(it.csp, it.html);
  });

  return (
    <div class="reader">
      <Show when={props.error !== null}>
        <div class="reader-state error">
          <strong>Ouverture impossible.</strong>
          <span>{props.error}</span>
        </div>
      </Show>

      <Show when={props.message === null && props.error === null}>
        <div class="reader-state muted">
          {props.loading ? "Ouverture…" : "Aucun message sélectionné."}
        </div>
      </Show>

      <Show when={props.message !== null}>
        {(() => {
          const message = props.message!;
          return (
            <>
              <header class="reader-head">
                <h1 class="reader-subject">{message.row.subject || "(sans sujet)"}</h1>
                <div class="reader-meta">
                  <span class="reader-from">
                    {message.row.from_name ?? message.row.from}
                    <Show when={message.row.from_name !== null}>
                      <span class="reader-address"> &lt;{message.row.from}&gt;</span>
                    </Show>
                  </span>
                  <span class="reader-date">
                    {message.row.date > 0
                      ? new Date(message.row.date * 1000).toLocaleString()
                      : "date inconnue"}
                  </span>
                </div>
                <Show when={message.to.length > 0}>
                  <div class="reader-to">À : {message.to.join(", ")}</div>
                </Show>
              </header>

              {/* Le bandeau de `docs/PRIVACY.md` §2. Il ne s'affiche que s'il y a quelque
                  chose à dire, et le déblocage vaut pour ce message uniquement. */}
              <Show when={(html()?.blocked_images ?? 0) > 0 || (html()?.trackers.length ?? 0) > 0}>
                <div class="banner">
                  <span>
                    <Show when={(html()?.blocked_images ?? 0) > 0}>
                      <strong>{html()!.blocked_images}</strong> image
                      {html()!.blocked_images === 1 ? "" : "s"} distante
                      {html()!.blocked_images === 1 ? "" : "s"} bloquée
                      {html()!.blocked_images === 1 ? "" : "s"}
                    </Show>
                    <Show when={(html()?.trackers.length ?? 0) > 0}>
                      {(html()?.blocked_images ?? 0) > 0 ? " — " : ""}
                      <strong>{html()!.trackers.length}</strong> signal
                      {html()!.trackers.length === 1 ? "" : "s"} de traçage
                    </Show>
                  </span>
                  <Show when={!props.imagesShown && (html()?.blocked_images ?? 0) > 0}>
                    <button type="button" onClick={props.onShowImages}>
                      Afficher les images
                    </button>
                  </Show>
                </div>
                <Show when={(html()?.trackers.length ?? 0) > 0}>
                  <ul class="trackers">
                    <For each={html()!.trackers}>
                      {(tracker) => (
                        <li>
                          <code>{tracker.host}</code>
                          <span class="muted"> — {trackerLabel(tracker.kind)}</span>
                        </li>
                      )}
                    </For>
                  </ul>
                </Show>
              </Show>

              {/* Le corps. `sandbox` vient du démon, comme la CSP : source unique. */}
              <Show
                when={document_() !== null}
                fallback={<pre class="reader-text">{message.body}</pre>}
              >
                <iframe
                  class="reader-body"
                  title="Corps du message"
                  sandbox={html()!.sandbox}
                  srcdoc={document_()!}
                  /* `referrerpolicy` en ceinture : la CSP empêche déjà toute requête, mais si
                     une future politique en autorisait une, elle ne doit pas dire d'où elle
                     vient. */
                  referrerpolicy="no-referrer"
                />
              </Show>

              <Show when={html()?.truncated === true}>
                <div class="reader-state muted">
                  Le corps a été tronqué : il dépassait la taille maximale servie.
                </div>
              </Show>

              <Show when={message.attachments.length > 0}>
                <footer class="attachments">
                  <div class="attachments-title">
                    {message.attachments.length} pièce
                    {message.attachments.length === 1 ? "" : "s"} jointe
                    {message.attachments.length === 1 ? "" : "s"}
                    <span class="muted"> — listées, pas ouvrables en phase 1</span>
                  </div>
                  <ul>
                    <For each={message.attachments}>
                      {(attachment) => (
                        <li>
                          <span>{attachment.name ?? "(sans nom)"}</span>
                          <span class="muted">
                            {" "}
                            {attachment.mime} · {Math.ceil(attachment.size / 1024)} Kio
                          </span>
                        </li>
                      )}
                    </For>
                  </ul>
                </footer>
              </Show>
            </>
          );
        })()}
      </Show>
    </div>
  );
}
