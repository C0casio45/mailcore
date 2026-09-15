/**
 * Le panneau des dossiers.
 *
 * ## Ce que le corpus réel impose
 *
 * 96 dossiers sur **11 comptes**, et `INBOX` existe dans presque chacun. Une liste plate de
 * chemins complets donne donc plusieurs `INBOX` indiscernables, et des entrées comme
 * `[Gmail]/Messages envoyés` ou `Auto-entreprise/AUDIOCAMP` tronquées à l'endroit qui les
 * distingue. C'était le premier jet, et c'était illisible.
 *
 * Deux corrections :
 *
 * - **grouper par compte**, avec un intitulé — sans quoi on ne sait pas de quelle boîte on
 *   regarde la réception ;
 * - **n'afficher que le dernier segment**, indenté selon la profondeur. `Auto-entreprise` puis
 *   `AUDIOCAMP` en retrait dit la même chose que le chemin complet, en tenant dans la largeur.
 *
 * L'ordre vient du démon, déjà trié par compte puis par chemin : c'est exactement l'ordre d'un
 * arbre parcouru en profondeur, donc il n'y a rien à retrier ici.
 *
 * ## Le piège de mise en page qui cassait l'affichage
 *
 * Un élément de `flex` a `min-width: auto` par défaut : il **refuse de se réduire** sous la
 * largeur de son contenu. Un `text-overflow: ellipsis` posé dessus ne se déclenche donc jamais,
 * et le texte débordait du bouton au lieu d'être coupé — ce qui poussait le compteur de non-lus
 * hors du panneau. Il faut `min-width: 0` explicite. Voir `theme.css`.
 */

import { For, Show, createMemo } from "solid-js";
import type { Folder } from "./types";

/** Un compte et ses dossiers, dans l'ordre rendu par le démon. */
interface Account {
  name: string;
  folders: Folder[];
}

export interface FoldersProps {
  folders: Folder[];
  current: number | null;
  onOpen: (id: number) => void;
}

/** Le dernier segment d'un chemin, et sa profondeur. */
function segment(path: string): { label: string; depth: number } {
  const parts = path.split("/");
  return {
    label: parts[parts.length - 1] ?? path,
    depth: parts.length - 1,
  };
}

export function Folders(props: FoldersProps) {
  /** Regroupe en conservant l'ordre d'arrivée. */
  const accounts = createMemo<Account[]>(() => {
    const out: Account[] = [];
    for (const folder of props.folders) {
      const last = out[out.length - 1];
      if (last !== undefined && last.name === folder.account) {
        last.folders.push(folder);
      } else {
        out.push({ name: folder.account, folders: [folder] });
      }
    }
    return out;
  });

  return (
    <nav class="folders" aria-label="Dossiers">
      <For each={accounts()}>
        {(account) => (
          <section class="account">
            <h2 class="account-name" title={account.name}>
              {account.name}
            </h2>
            <For each={account.folders}>
              {(folder) => {
                const it = segment(folder.path);
                return (
                  <button
                    type="button"
                    class="folder"
                    classList={{ current: folder.id === props.current }}
                    style={{ "padding-left": `${10 + it.depth * 12}px` }}
                    onClick={() => props.onOpen(folder.id)}
                    /* Le chemin complet et les compteurs en infobulle : le libellé court suffit
                       à naviguer, l'infobulle lève le doute quand deux dossiers de comptes
                       différents portent le même nom. */
                    title={`${account.name} — ${folder.path}\n${folder.total} message(s), ${folder.unread} non lu(s)`}
                  >
                    <span class="folder-name">{it.label}</span>
                    <Show when={folder.unread > 0}>
                      <span class="folder-unread">{folder.unread}</span>
                    </Show>
                  </button>
                );
              }}
            </For>
          </section>
        )}
      </For>
    </nav>
  );
}
