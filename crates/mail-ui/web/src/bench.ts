/**
 * Le pont de mesure des critères 1 et 2, et le banc de défilement.
 *
 * ## Pourquoi ce fichier existe
 *
 * `docs/PHASE-1.md` demande des critères **mesurés, pas estimés**. Les critères 1 et 2 se
 * jouent dans le webview, à un endroit où aucun outil Rust ne peut lire l'horloge : le temps
 * d'analyse du JavaScript, le premier dessin et la durée d'une image de défilement ne sont
 * connus que d'ici.
 *
 * Ce module ne fait donc que **relever et transmettre**. Il ne décide rien : la coquille
 * consigne le relevé sur sa sortie d'erreur, et `cargo xtask measure-ui` le lit. Le chiffre
 * final vient d'un processus extérieur qui chronomètre depuis le lancement de l'exécutable,
 * ce que la page ne peut pas voir — elle ne connaît pas le temps passé avant son propre
 * chargement.
 *
 * ## Ce que le banc de défilement mesure, et ce qu'il ne mesure pas
 *
 * Il mesure **le coût de nos images** : le temps entre deux `requestAnimationFrame` pendant
 * que la position de défilement change et que les lignes se réécrivent. C'est la part dont le
 * code est responsable, et la seule qu'on puisse corriger.
 *
 * Il ne mesure pas la latence d'entrée du compositeur, ni le défilement à la molette réelle :
 * ça demande un pilote de navigateur. Le relevé est donc un plancher honnête, et il est
 * annoncé comme tel dans le rapport.
 *
 * Le banc ne s'exécute que sur demande explicite de l'outillage — `MAILCORE_UI_BENCH` côté
 * coquille, `?bench=` en mode onglet. Aucun de ces chemins n'est atteint en usage normal.
 */

import { embedded, shell } from "./api";

/** Les mesures que l'outillage sait demander. */
export type Bench = "startup" | "scroll";

/** Une étape du démarrage. `paint` est le critère 1 ; `settled` ajoute le premier aller-retour. */
export type Phase = "paint" | "settled";

/** Le relevé d'un banc de défilement. Millisecondes, sauf mention. */
export interface Frames {
  /** Lignes réellement présentes dans la liste pendant le défilement. */
  rows: number;
  /** Images observées. */
  frames: number;
  /** Distance parcourue, en pixels. */
  distance: number;
  /**
   * La cadence observée au repos, juste avant de défiler.
   *
   * **C'est ce qui rend les deltas interprétables** — et ce qui a montré qu'ils ne suffisent
   * pas. Deux exécutions du même code ont donné 18,1 ms puis 31,2 ms au repos : le compositeur
   * ralentit une fenêtre qui n'a pas le premier plan, et aucune de ces valeurs ne dit quoi que
   * ce soit sur notre code. D'où `workP95` ci-dessous, qui ne dépend pas de la cadence.
   */
  baseline: number;
  p50: number;
  p95: number;
  worst: number;
  /**
   * Images perdues : au-delà d'une fois et demie la cadence observée au repos.
   *
   * Le défilement fait-il **sauter** des images par rapport à ce que la machine présentait
   * juste avant ? Une image à 18 ms sur un écran qui en présente une toutes les 18 ms n'est
   * pas un retard.
   */
  dropped: number;
  /**
   * Le travail effectué dans l'image, au repos puis en défilant — p50 et p95, en ms.
   *
   * ## Pourquoi cette mesure existe, et pourquoi c'est elle qui compte
   *
   * L'argument `timestamp` de `requestAnimationFrame` est **l'heure de début de l'image**,
   * décidée par le compositeur. Ce que `performance.now()` rend au tout début du rappel est
   * l'heure où notre code reprend la main. L'écart entre les deux est le temps passé, dans
   * cette image, avant nous : et c'est là que tourne la mise à jour du défilement, parce que
   * les « scroll steps » du navigateur s'exécutent avant les rappels d'animation.
   *
   * Autrement dit : cet écart **est** le coût que notre liste impose à l'image — la
   * réécriture des lignes par les signaux, le style et la mise en page qui suivent. Il ne
   * dépend ni de la fréquence de l'écran, ni du fait que la fenêtre ait le premier plan. C'est
   * le seul chiffre du banc qui se compare honnêtement au budget de 16,7 ms.
   */
  restWork: number;
  workP50: number;
  workP95: number;
  workWorst: number;
}

/** Traduit une demande de l'outillage, en refusant ce qu'on ne sait pas faire. */
function parse(asked: string | null): Bench | null {
  if (asked === "startup" || asked === "scroll") return asked;
  return null;
}

/**
 * La mesure demandée, ou `null` en usage normal.
 *
 * En mode onglet, la demande passe par la requête (`?bench=scroll`) : ça permet de mettre le
 * banc au point dans un navigateur, où les outils de développement sont ouvrables, avant de
 * le lancer dans le webview de la coquille où ils ne le sont pas.
 */
export async function requested(): Promise<Bench | null> {
  if (embedded()) {
    try {
      return parse(await shell<string | null>("bench"));
    } catch {
      // Une coquille sans la commande est une coquille plus ancienne. Pas une raison de
      // refuser de démarrer : il n'y a simplement pas de banc à exécuter.
      return null;
    }
  }
  return parse(new URLSearchParams(window.location.search).get("bench"));
}

/** Attend que le navigateur ait réellement dessiné, et pas seulement accepté nos écritures. */
export function painted(): Promise<void> {
  return new Promise((resolve) => {
    // Deux images : la première rend la main *avant* le dessin de la mise en page qu'on vient
    // d'écrire, la seconde est donc la première à s'exécuter après que le pixel existe.
    requestAnimationFrame(() => requestAnimationFrame(() => resolve()));
  });
}

/**
 * Transmet une étape du démarrage — critère 1.
 *
 * `performance.now()` compte depuis l'origine temporelle du document, c'est-à-dire le début
 * de la navigation. Il ne compte donc **pas** le lancement du processus ni la création du
 * webview : c'est la coquille qui ajoute cette part, avec son propre chronomètre.
 */
export async function phase(name: Phase, rows: number): Promise<void> {
  const ms = Math.round(performance.now() * 10) / 10;
  if (embedded()) {
    try {
      await shell("ready", { marks: { phase: name, ms, rows } });
    } catch {
      /* Une coquille qui n'écoute pas ne doit pas empêcher l'application de tourner. */
    }
    return;
  }
  // En mode onglet, la console est le seul destinataire possible.
  console.info(`MESURE-UI critere=1 phase=${name} page_ms=${ms} lignes=${rows}`);
}

/** Le percentile d'une série déjà triée, par interpolation basse. */
function percentile(sorted: number[], fraction: number): number {
  if (sorted.length === 0) return 0;
  const rank = Math.min(sorted.length - 1, Math.floor(fraction * sorted.length));
  return sorted[rank] ?? 0;
}

/**
 * Une image dépassant ce multiple de la cadence de référence est comptée perdue.
 *
 * 1,5 et non 1,1 : la cadence de présentation n'est jamais parfaitement régulière, et une
 * image manquée se voit comme un intervalle **doublé**, pas comme quelques pour cent de plus.
 */
const DROP_FACTOR = 1.5;

/** Ce qu'une série d'images apprend : leur cadence, et le travail fait dedans. */
interface Series {
  /** Intervalles entre débuts d'images, en ms. La cadence du compositeur. */
  deltas: number[];
  /**
   * Temps écoulé dans l'image avant que notre rappel reprenne la main, en ms.
   *
   * C'est le travail que le défilement a imposé au navigateur pendant cette image — voir
   * `Frames.workP95`.
   */
  work: number[];
}

/** Relève une série d'images, en appelant `advance` avant chacune. */
function series(count: number, advance: (index: number) => void): Promise<Series> {
  return new Promise((resolve) => {
    const deltas: number[] = [];
    const work: number[] = [];
    let previous: number | null = null;
    let index = 0;

    const tick = (now: number) => {
      // Pris en tout premier : chaque instruction avant celle-ci s'ajouterait à la mesure.
      const entered = performance.now();
      if (previous !== null) {
        deltas.push(now - previous);
        work.push(entered - now);
      }
      previous = now;

      if (index >= count) {
        resolve({ deltas, work });
        return;
      }
      index += 1;
      advance(index);
      requestAnimationFrame(tick);
    };

    requestAnimationFrame(tick);
  });
}

/**
 * Fait défiler la liste et relève la durée de chaque image — critère 2.
 *
 * ## Deux phases, et deux questions différentes
 *
 * 1. **Au repos**, sans rien toucher : la cadence que la machine présente, et le travail
 *    qu'une image coûte quand on ne demande rien. C'est la ligne de base des deux mesures.
 * 2. **En défilant** : la position avance d'un pas à chaque `requestAnimationFrame`, donc
 *    chaque image porte exactement un déplacement. Une minuterie donnerait des pas
 *    irréguliers et des durées qui ne voudraient rien dire.
 *
 * De la cadence, on ne tire qu'une chose : le défilement a-t-il fait **sauter** des images par
 * rapport au repos. La valeur absolue ne dit rien — le compositeur ralentit une fenêtre qui
 * n'a pas le premier plan, et deux exécutions ont donné 55 Hz puis 32 Hz sans que le code
 * change.
 *
 * Le chiffre qui se compare au budget de 16,7 ms est `workP95` : le temps passé dans l'image
 * avant que notre rappel reprenne la main, c'est-à-dire la mise à jour du défilement.
 *
 * Rend `null` si la liste n'est pas à l'écran — mieux vaut pas de chiffre qu'un chiffre pris
 * sur un conteneur vide.
 */
export async function measureScroll(
  rows: number,
  frames = 600,
  idle = 120,
): Promise<Frames | null> {
  const viewport = document.querySelector<HTMLDivElement>(".list");
  if (viewport === null) return null;

  const distance = viewport.scrollHeight - viewport.clientHeight;
  if (distance <= 0) return null;

  const ascending = (series: number[]) => series.slice().sort((a, b) => a - b);

  const resting = await series(idle, () => {});
  const baseline = percentile(ascending(resting.deltas), 0.5);
  const restWork = percentile(ascending(resting.work), 0.5);

  const step = distance / frames;
  const scrolling = await series(frames, (index) => {
    viewport.scrollTop = index * step;
  });

  const deltas = ascending(scrolling.deltas);
  const work = ascending(scrolling.work);
  const ceiling = baseline * DROP_FACTOR;
  return {
    rows,
    frames: scrolling.deltas.length,
    distance,
    baseline,
    p50: percentile(deltas, 0.5),
    p95: percentile(deltas, 0.95),
    worst: deltas[deltas.length - 1] ?? 0,
    dropped: scrolling.deltas.filter((it) => it > ceiling).length,
    restWork,
    workP50: percentile(work, 0.5),
    workP95: percentile(work, 0.95),
    workWorst: work[work.length - 1] ?? 0,
  };
}

/**
 * Transmet une note de diagnostic à la coquille.
 *
 * Le banc tourne dans un webview sans console lisible : sans ce canal, « le chargement s'est
 * arrêté à 100 lignes » n'a aucune explication attachée.
 */
export async function note(message: string): Promise<void> {
  if (embedded()) {
    try {
      await shell("diag", { message });
    } catch {
      /* rien à faire */
    }
    return;
  }
  console.info(`MESURE-UI diag ${message}`);
}

/** Transmet le relevé de défilement. */
export async function reportFrames(it: Frames): Promise<void> {
  if (embedded()) {
    try {
      await shell("bench_report", { frames: it });
    } catch {
      /* rien à faire : le relevé est perdu, l'application continue */
    }
    return;
  }
  console.info(
    `MESURE-UI critere=2 lignes=${it.rows} images=${it.frames} repos=${it.baseline.toFixed(2)} ` +
      `p50=${it.p50.toFixed(2)} p95=${it.p95.toFixed(2)} pire=${it.worst.toFixed(2)} ` +
      `perdues=${it.dropped} travail_repos=${it.restWork.toFixed(2)} ` +
      `travail_p50=${it.workP50.toFixed(2)} travail_p95=${it.workP95.toFixed(2)} ` +
      `travail_pire=${it.workWorst.toFixed(2)}`,
  );
}
