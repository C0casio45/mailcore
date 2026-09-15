/**
 * Le cache de lecture local : les métadonnées de liste, **jamais les corps**.
 *
 * ## À quoi il sert exactement
 *
 * Deux choses, et la première n'est pas optionnelle (`docs/ARCHITECTURE.md`) :
 *
 * - **Critère 1.** Le front ouvre sur son cache et affiche une liste avant le premier
 *   aller-retour réseau. Avec un démon au bout du réseau, attendre `folders.list` puis
 *   `messages.page` mettrait deux allers-retours entre le lancement et le premier pixel.
 * - **Critère 9.** Démon injoignable, la liste déjà connue reste défilable. La recherche et
 *   l'ouverture d'un message échouent proprement, avec un état visible.
 *
 * ## Ce qu'il n'est pas
 *
 * **Jamais une seconde source de vérité.** Il est jetable, reconstruit depuis le démon, et
 * jamais consulté pour un corps de message. La règle pratique qui en découle : à chaque fois
 * que le démon répond, c'est sa réponse qui gagne, sans arbitrage ni fusion.
 *
 * ## Et c'est une donnée sensible
 *
 * Il contient les sujets et les expéditeurs de toute la boîte. Sur un poste partagé, c'est
 * presque aussi révélateur que le courrier lui-même (`docs/PRIVACY.md`, §8) — d'où
 * [`clear`], appelée à la déconnexion, et l'absence de tout corps de message ici.
 *
 * ## Pourquoi IndexedDB et pas `localStorage`
 *
 * `localStorage` est synchrone : lire 100 000 lignes y bloquerait le fil principal, ce qui
 * est exactement ce que le critère 2 interdit. Il plafonne aussi à quelques mégaoctets.
 */

import type { Folder, Row } from "./types";

const DB_NAME = "mailcore";
const DB_VERSION = 1;

/** Les dossiers, une entrée unique. */
const FOLDERS = "folders";
/** Les lignes de liste, clé `[folder, -date, id]` pour un parcours déjà trié. */
const ROWS = "rows";
/** Divers : révision connue, dernier dossier ouvert. */
const META = "meta";

let opening: Promise<IDBDatabase | null> | null = null;

/** Ouvre la base, une fois. Rend `null` si le navigateur la refuse. */
function open(): Promise<IDBDatabase | null> {
  if (opening !== null) {
    return opening;
  }
  opening = new Promise((resolve) => {
    let request: IDBOpenDBRequest;
    try {
      request = indexedDB.open(DB_NAME, DB_VERSION);
    } catch {
      // Navigation privée, stockage bloqué : on tourne sans cache. Tout marche, en plus
      // lent au démarrage — c'est une dégradation, pas une panne.
      resolve(null);
      return;
    }

    request.onupgradeneeded = () => {
      const db = request.result;
      if (!db.objectStoreNames.contains(FOLDERS)) {
        db.createObjectStore(FOLDERS, { keyPath: "id" });
      }
      if (!db.objectStoreNames.contains(ROWS)) {
        // La clé porte l'ordre d'affichage : `[dossier, -date, id]`. Un curseur sur cette
        // clé rend donc les lignes du plus récent au plus ancien sans rien trier au
        // chargement — le tri est déjà payé à l'écriture.
        db.createObjectStore(ROWS, { keyPath: ["folder", "rank", "id"] });
      }
      if (!db.objectStoreNames.contains(META)) {
        db.createObjectStore(META, { keyPath: "key" });
      }
    };
    request.onsuccess = () => resolve(request.result);
    request.onerror = () => resolve(null);
  });
  return opening;
}

/** Emballe une requête IndexedDB en promesse. */
function wrap<T>(request: IDBRequest<T>): Promise<T> {
  return new Promise((resolve, reject) => {
    request.onsuccess = () => resolve(request.result);
    request.onerror = () => reject(request.error);
  });
}

/** Enregistre les dossiers. */
export async function putFolders(folders: Folder[]): Promise<void> {
  const db = await open();
  if (db === null) return;
  const tx = db.transaction(FOLDERS, "readwrite");
  const store = tx.objectStore(FOLDERS);
  store.clear();
  for (const folder of folders) {
    store.put(folder);
  }
  await new Promise<void>((resolve) => {
    tx.oncomplete = () => resolve();
    tx.onerror = () => resolve();
  });
}

/** Relit les dossiers connus. */
export async function folders(): Promise<Folder[]> {
  const db = await open();
  if (db === null) return [];
  try {
    const all = await wrap(db.transaction(FOLDERS, "readonly").objectStore(FOLDERS).getAll());
    return all as Folder[];
  } catch {
    return [];
  }
}

/**
 * Enregistre une tranche de lignes d'un dossier.
 *
 * `rank` est `-date` : il rend l'ordre d'affichage — du plus récent au plus ancien — directement
 * exploitable par un curseur IndexedDB.
 */
export async function putRows(folder: number, rows: Row[]): Promise<void> {
  const db = await open();
  if (db === null) return;
  const tx = db.transaction(ROWS, "readwrite");
  const store = tx.objectStore(ROWS);
  for (const row of rows) {
    store.put({ folder, rank: -row.date, ...row });
  }
  await new Promise<void>((resolve) => {
    tx.oncomplete = () => resolve();
    tx.onerror = () => resolve();
  });
}

/**
 * Relit les `limit` premières lignes connues d'un dossier, dans l'ordre d'affichage.
 *
 * Une borne, et pas tout : le cache peut contenir cent mille lignes, et le critère 1 se joue
 * sur ce qu'on affiche **avant** le premier aller-retour, c'est-à-dire un écran.
 */
export async function rows(folder: number, limit: number): Promise<Row[]> {
  const db = await open();
  if (db === null) return [];
  try {
    const range = IDBKeyRange.bound([folder, -Infinity, -Infinity], [folder, Infinity, Infinity]);
    const found = await wrap(
      db.transaction(ROWS, "readonly").objectStore(ROWS).getAll(range, limit),
    );
    return (found as (Row & { folder: number; rank: number })[]).map((it) => ({
      id: it.id,
      date: it.date,
      from: it.from,
      from_name: it.from_name,
      subject: it.subject,
      has_attachments: it.has_attachments,
      unread: it.unread,
      flagged: it.flagged,
    }));
  } catch {
    return [];
  }
}

/** Lit une valeur de service. */
export async function meta(key: string): Promise<string | null> {
  const db = await open();
  if (db === null) return null;
  try {
    const found = await wrap(db.transaction(META, "readonly").objectStore(META).get(key));
    return (found as { key: string; value: string } | undefined)?.value ?? null;
  } catch {
    return null;
  }
}

/** Écrit une valeur de service. */
export async function putMeta(key: string, value: string): Promise<void> {
  const db = await open();
  if (db === null) return;
  try {
    db.transaction(META, "readwrite").objectStore(META).put({ key, value });
  } catch {
    /* sans cache, sans effet */
  }
}

/**
 * Vide tout le cache.
 *
 * À appeler à la déconnexion : les sujets et expéditeurs de toute une boîte ne doivent pas
 * survivre au jeton qui a servi à les lire (`docs/PRIVACY.md`, §8).
 */
export async function clear(): Promise<void> {
  const db = await open();
  if (db === null) return;
  const tx = db.transaction([FOLDERS, ROWS, META], "readwrite");
  tx.objectStore(FOLDERS).clear();
  tx.objectStore(ROWS).clear();
  tx.objectStore(META).clear();
  await new Promise<void>((resolve) => {
    tx.oncomplete = () => resolve();
    tx.onerror = () => resolve();
  });
}
