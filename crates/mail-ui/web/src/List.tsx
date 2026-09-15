/**
 * La liste de messages, virtualisée à la main. **Critère 2 : 60 fps sur 100 000 messages.**
 *
 * ## Le principe
 *
 * Un conteneur défilant, une cale de la hauteur totale — `total × ROW_HEIGHT` — et une
 * fenêtre de quelques dizaines de lignes positionnée dedans. Rien d'autre n'existe dans le
 * DOM. La hauteur totale est connue sans avoir chargé une seule ligne, parce que
 * `folders.list` rend déjà le compte de chaque dossier : la barre de défilement est donc juste
 * dès la première image.
 *
 * ## `<Index>` et pas `<For>` — c'est ici que le choix de Solid se paie
 *
 * `<For>` est **clé par identité** : quand la fenêtre change, il crée, déplace et détruit des
 * nœuds. `<Index>` est **clé par position** : la ligne 0 reste la même ligne 0 du DOM, et son
 * contenu change par signal.
 *
 * En défilant, on veut exactement ça : garder les quarante lignes vivantes et ne changer que
 * leurs nœuds texte. Aucun nœud créé, aucun nœud détruit, aucune réconciliation — juste des
 * écritures de texte sur des nœuds déjà en place. C'est le motif qu'un VDOM oblige à
 * contourner et que les signaux donnent par défaut.
 *
 * ## Ce que l'implémentation a imposé de savoir
 *
 * - **`transform` et pas `top`.** Déplacer la fenêtre par `top` provoque un recalcul de mise
 *   en page ; par `translateY`, c'est une composition. Sur une liste de 22 px de ligne, la
 *   différence décide du critère.
 * - **Hauteur de ligne fixe.** Des hauteurs variables demanderaient de mesurer chaque ligne,
 *   donc de la rendre, donc de renoncer à la virtualisation. Le thème de `docs/PHASE-1.md`
 *   fixe la ligne à ~22 px : c'est une contrainte de design assumée, pas une limite technique
 *   qu'on subit.
 * - **Le défilement ne déclenche pas de requête.** Il pose un signal ; c'est un effet séparé
 *   qui décide de charger, et il ne bloque jamais le rendu.
 *
 * ## La limite connue : le saut arbitraire
 *
 * La pagination du démon est **par clé** (`docs/ARCHITECTURE.md` : jamais d'`OFFSET`), donc
 * les pages ne s'obtiennent qu'en séquence. Tirer la barre de défilement au milieu d'un
 * dossier de 20 000 messages affiche donc des lignes vides le temps que les pages
 * intermédiaires arrivent.
 *
 * Ce n'est pas un défaut de virtualisation — le défilement reste à 60 fps, les lignes vides se
 * dessinent aussi vite que les autres — c'est une conséquence du choix de pagination, qui
 * était le bon pour le coût par page. La sortie propre demandera une méthode « curseur à la
 * position N » côté démon, payée une fois par saut au lieu d'à chaque page. Pas encore écrite.
 */

import { Index, createEffect, createMemo, createSignal, onCleanup } from "solid-js";
import type { Row } from "./types";

/** Hauteur d'une ligne, en pixels. Fixée par le thème de `docs/PHASE-1.md`. */
export const ROW_HEIGHT = 22;

/**
 * Lignes rendues en plus de la hauteur visible, de chaque côté.
 *
 * 8 : de quoi qu'un défilement rapide ne montre pas de vide avant que l'image suivante
 * arrive, sans rendre trois écrans pour rien.
 */
const OVERSCAN = 8;

/** Ce que la liste a besoin de savoir. */
export interface ListProps {
  /** Nombre total de lignes du dossier, connu d'avance. */
  total: number;
  /** Les lignes chargées, indexées par position. Creux : `undefined` = pas encore là. */
  loaded: (Row | undefined)[];
  /** L'identifiant sélectionné, ou `null`. */
  selected: number | null;
  /** Appelé quand une position devient visible et n'est pas chargée. */
  onNeed: (from: number, to: number) => void;
  /** Appelé au clic ou au clavier. */
  onSelect: (row: Row) => void;
}

/** Formate une date pour une ligne de liste, à la locale du système. */
function shortDate(unix: number): string {
  if (unix <= 0) {
    // Pas de date exploitable dans l'en-tête : 102 messages du corpus réel sont dans ce cas.
    // Un tiret est plus honnête qu'une date inventée à partir de zéro.
    return "—";
  }
  const date = new Date(unix * 1000);
  const now = new Date();
  // Dans l'année en cours, le jour et le mois suffisent et laissent de la place au sujet.
  const sameYear = date.getFullYear() === now.getFullYear();
  return date.toLocaleDateString(undefined, {
    day: "2-digit",
    month: "2-digit",
    ...(sameYear ? {} : { year: "2-digit" }),
  });
}

/** Le nom à afficher : celui de l'expéditeur, à défaut son adresse. */
function who(row: Row): string {
  const name = row.from_name?.trim();
  return name !== undefined && name.length > 0 ? name : row.from;
}

export function List(props: ListProps) {
  let viewport: HTMLDivElement | undefined;
  const [scrollTop, setScrollTop] = createSignal(0);
  const [height, setHeight] = createSignal(600);

  /** Combien de lignes tiennent à l'écran, plus la marge. */
  const windowSize = createMemo(() =>
    Math.min(props.total, Math.ceil(height() / ROW_HEIGHT) + OVERSCAN * 2),
  );

  /** La première position rendue. */
  const first = createMemo(() => {
    const raw = Math.floor(scrollTop() / ROW_HEIGHT) - OVERSCAN;
    return Math.max(0, Math.min(raw, Math.max(0, props.total - windowSize())));
  });

  /**
   * La fenêtre : un tableau de longueur **stable** dans le cas courant.
   *
   * C'est ce qui permet à `<Index>` de ne rien créer ni détruire en défilant. La longueur ne
   * change qu'au redimensionnement de la fenêtre ou au changement de dossier.
   */
  const windowRows = createMemo(() => {
    const start = first();
    const size = windowSize();
    const out: (Row | undefined)[] = new Array(size);
    for (let offset = 0; offset < size; offset += 1) {
      out[offset] = props.loaded[start + offset];
    }
    return out;
  });

  // Réclamer ce qui manque. Un effet séparé du rendu : le défilement pose un signal et rend
  // la main, le chargement se décide ici, et une requête lente n'a jamais retenu une image.
  createEffect(() => {
    const start = first();
    const size = windowSize();
    if (size === 0) return;
    let missing = false;
    for (let offset = 0; offset < size; offset += 1) {
      if (props.loaded[start + offset] === undefined) {
        missing = true;
        break;
      }
    }
    if (missing) {
      props.onNeed(start, start + size);
    }
  });

  /** Suit la hauteur du conteneur : la taille de fenêtre en dépend. */
  const observe = (element: HTMLDivElement) => {
    viewport = element;
    setHeight(element.clientHeight);
    const observer = new ResizeObserver(() => setHeight(element.clientHeight));
    observer.observe(element);
    onCleanup(() => observer.disconnect());
  };

  /** Amène une position dans le champ visible, sans la centrer si elle y est déjà. */
  const reveal = (index: number) => {
    if (viewport === undefined) return;
    const top = index * ROW_HEIGHT;
    const bottom = top + ROW_HEIGHT;
    if (top < viewport.scrollTop) {
      viewport.scrollTop = top;
    } else if (bottom > viewport.scrollTop + viewport.clientHeight) {
      viewport.scrollTop = bottom - viewport.clientHeight;
    }
  };

  /** La position de la ligne sélectionnée, si on la connaît. */
  const selectedIndex = createMemo(() => {
    const id = props.selected;
    if (id === null) return -1;
    return props.loaded.findIndex((row) => row?.id === id);
  });

  /** Déplace la sélection de `delta` lignes. */
  const move = (delta: number) => {
    const from = selectedIndex();
    const next = Math.max(0, Math.min(props.total - 1, (from < 0 ? first() : from) + delta));
    const row = props.loaded[next];
    reveal(next);
    if (row !== undefined) {
      props.onSelect(row);
    }
    // Ligne pas encore chargée : `reveal` l'a rendue visible, l'effet la réclamera, et la
    // sélection suivra au prochain passage de l'utilisateur. Mieux que de ne rien bouger.
  };

  const onKeyDown = (event: KeyboardEvent) => {
    const step = Math.max(1, Math.floor(height() / ROW_HEIGHT) - 1);
    switch (event.key) {
      case "ArrowDown":
      case "j":
        move(1);
        break;
      case "ArrowUp":
      case "k":
        move(-1);
        break;
      case "PageDown":
        move(step);
        break;
      case "PageUp":
        move(-step);
        break;
      case "Home":
        move(-props.total);
        break;
      case "End":
        move(props.total);
        break;
      default:
        // Tout le reste appartient à qui écoute plus haut — la recherche, par exemple.
        return;
    }
    event.preventDefault();
  };

  return (
    <div
      class="list"
      ref={observe}
      tabindex="0"
      role="listbox"
      aria-label="Messages"
      onScroll={(event) => setScrollTop(event.currentTarget.scrollTop)}
      onKeyDown={onKeyDown}
    >
      {/* La cale : elle donne à la barre de défilement sa course réelle, sans rien rendre. */}
      <div class="list-spacer" style={{ height: `${props.total * ROW_HEIGHT}px` }}>
        <div
          class="list-window"
          style={{ transform: `translateY(${first() * ROW_HEIGHT}px)` }}
        >
          <Index each={windowRows()}>
            {(row) => {
              // `row` est un accesseur : le lire ici, dans le JSX, est ce qui rend la ligne
              // réactive sans que la fonction de composant soit rejouée.
              const isSelected = () => {
                const it = row();
                return it !== undefined && it.id === props.selected;
              };
              return (
                <div
                  class="row"
                  classList={{
                    selected: isSelected(),
                    unread: row()?.unread === true,
                    pending: row() === undefined,
                  }}
                  role="option"
                  aria-selected={isSelected()}
                  onClick={() => {
                    const it = row();
                    if (it !== undefined) props.onSelect(it);
                  }}
                >
                  <span class="cell-date">{row() === undefined ? "" : shortDate(row()!.date)}</span>
                  <span class="cell-from">{row() === undefined ? "" : who(row()!)}</span>
                  <span class="cell-subject">
                    {/* Une ligne pas encore chargée ne montre pas « chargement… » : un mot qui
                        défile est plus agité qu'un vide, et le vide dit la même chose. */}
                    {row() === undefined ? "" : row()!.subject}
                  </span>
                  <span class="cell-marks">
                    {row()?.has_attachments === true ? "📎" : ""}
                    {row()?.flagged === true ? "★" : ""}
                  </span>
                </div>
              );
            }}
          </Index>
        </div>
      </div>
    </div>
  );
}

/** Le bandeau du haut de la liste : ce qu'on regarde, et combien. */
export function ListHeader(props: { title: string; total: number; loaded: number }) {
  // Le compteur est écrit en clair. Le premier jet affichait `100 / 9702`, ce qui ne dit pas
  // de quoi on parle — c'était lisible pour qui venait d'écrire le code, et pour personne
  // d'autre.
  const count = () => {
    const total = props.total.toLocaleString();
    if (props.loaded < props.total) {
      return `${props.loaded.toLocaleString()} chargés sur ${total}`;
    }
    return `${total} message${props.total === 1 ? "" : "s"}`;
  };
  return (
    <div class="list-header">
      <span class="list-title">{props.title}</span>
      <span class="list-count">{count()}</span>
    </div>
  );
}
