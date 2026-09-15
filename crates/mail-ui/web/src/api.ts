/**
 * Le client JSON-RPC du front, et son seul point de bascule de transport.
 *
 * ## Deux transports, un seul contrat
 *
 * | Mode | Transport | Jeton |
 * |---|---|---|
 * | **application** (Tauri) | IPC vers le service embarqué | aucun |
 * | onglet de navigateur | `POST /api` vers le démon | oui |
 *
 * Le premier est le défaut de l'application packagée, et c'est **le mode le plus sûr** : rien
 * n'écoute sur la machine, donc il n'y a pas de canal à authentifier. Le second sert quand un
 * démon tourne déjà — sur cette machine ou au bout du réseau.
 *
 * Tout le reste du front ignore lequel est actif. C'est ce qui rend le choix réversible :
 * `call()` est le seul endroit à connaître la différence, et il fait dix lignes.
 *
 * ## Le jeton, quand il y en a un
 *
 * `docs/PRIVACY.md` §7 veut le jeton dans le trousseau du système. Un onglet de navigateur n'y
 * a pas accès : le mieux qu'il puisse faire est `sessionStorage`, effacé à la fermeture de
 * l'onglet. C'est un cran en dessous de la règle, et c'est assumé — le mode onglet est le
 * chemin de secours et de débogage, pas le déploiement recommandé.
 *
 * En mode application il n'y a **pas** de jeton du tout, ce qui règle la question plutôt que
 * de la déplacer.
 *
 * `sessionStorage` plutôt que `localStorage` : un jeton qui survit à la fermeture de l'onglet
 * survit aussi à quelqu'un qui s'assied devant la machine.
 *
 * ## Le contrôle de version du contrat
 *
 * Les types de `types.ts` sont une recopie de ceux du démon. `hello()` vérifie donc que le
 * démon annonce le même `PROTOCOL` et refuse de continuer sinon : mieux vaut un message clair
 * au démarrage qu'un `undefined` trois écrans plus loin.
 */

import { PROTOCOL } from "./types";
import type {
  Change,
  Folder,
  Hello,
  Job,
  Message,
  Page,
  Results,
  Stats,
} from "./types";

/** La clé de stockage du jeton pour cette session d'onglet. */
const TOKEN_KEY = "mailcore.token";

/** Une erreur d'appel, avec de quoi distinguer les cas que l'UI doit traiter à part. */
export class ApiError extends Error {
  constructor(
    message: string,
    /** `unauthorized` : jeton absent ou faux. `offline` : démon injoignable. */
    readonly kind: "unauthorized" | "offline" | "protocol" | "rpc",
  ) {
    super(message);
    this.name = "ApiError";
  }
}

let nextId = 1;

/**
 * Vrai si on tourne dans la coquille Tauri.
 *
 * Détecté sur la présence de l'objet d'IPC posé par le runtime, et non sur l'agent
 * utilisateur : c'est la capacité réellement disponible qui décide, pas une chaîne qu'on
 * interprète.
 */
export function embedded(): boolean {
  return typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;
}

/**
 * Appelle une commande de la coquille par IPC.
 *
 * **N'appeler qu'après avoir vérifié `embedded()`** : hors de la coquille, il n'y a pas de
 * runtime à trouver et l'import échoue.
 *
 * Import dynamique, précisément pour ça : en mode onglet, `@tauri-apps/api` n'a aucune raison
 * d'être chargé, et le charger coûterait du temps d'analyse au démarrage — critère 1.
 */
export async function shell<T>(
  command: string,
  args: Record<string, unknown> = {},
): Promise<T> {
  const { invoke } = await import("@tauri-apps/api/core");
  return await invoke<T>(command, args);
}

/** Envoie un message JSON-RPC au service embarqué. */
async function viaIpc(body: string): Promise<string | null> {
  return await shell<string | null>("api", { message: body });
}

/** Poste un message au démon par HTTP. */
async function viaHttp(body: string): Promise<string | null> {
  const bearer = token();
  if (bearer === null) {
    throw new ApiError("aucun jeton", "unauthorized");
  }

  let response: Response;
  try {
    response = await fetch("/api", {
      method: "POST",
      headers: {
        "Content-Type": "application/json",
        Authorization: `Bearer ${bearer}`,
      },
      body,
    });
  } catch (cause) {
    // `fetch` ne rejette que sur une panne réseau, pas sur un code d'erreur HTTP. C'est donc
    // ici, et seulement ici, que « le démon est injoignable » se décide — critère 9.
    throw new ApiError(`démon injoignable : ${String(cause)}`, "offline");
  }

  if (response.status === 401) {
    throw new ApiError("jeton refusé par le démon", "unauthorized");
  }
  if (response.status === 204) {
    return null;
  }
  if (!response.ok) {
    throw new ApiError(`le démon a répondu ${response.status}`, "rpc");
  }
  return await response.text();
}

/** Le jeton de la session, s'il y en a un. */
export function token(): string | null {
  try {
    return sessionStorage.getItem(TOKEN_KEY);
  } catch {
    // Onglet en navigation privée avec stockage bloqué : on n'a pas de jeton, c'est tout.
    return null;
  }
}

/** Enregistre le jeton pour la durée de l'onglet. */
export function setToken(value: string): void {
  try {
    sessionStorage.setItem(TOKEN_KEY, value);
  } catch {
    // Sans stockage, le jeton ne survivra pas à un rechargement. Ça marche quand même pour
    // la session en cours, ce qui vaut mieux que refuser de démarrer.
  }
}

/** Oublie le jeton. */
export function clearToken(): void {
  try {
    sessionStorage.removeItem(TOKEN_KEY);
  } catch {
    /* rien à faire */
  }
}

/**
 * Appelle une méthode et rend son résultat.
 *
 * **Le seul endroit du front qui sait par où passe un message.** Le reste du code appelle
 * `api.folders()` sans savoir s'il y a un réseau au bout.
 */
async function call<T>(method: string, params: unknown = null): Promise<T> {
  const body = JSON.stringify({
    jsonrpc: "2.0",
    id: nextId++,
    method,
    params,
  });

  let text: string | null;
  if (embedded()) {
    try {
      text = await viaIpc(body);
    } catch (cause) {
      // Une erreur d'IPC n'est pas une panne réseau : le service est dans le même processus.
      // La classer `rpc` évite d'afficher « démon injoignable » pour un bug local.
      throw new ApiError(`appel interne en échec : ${String(cause)}`, "rpc");
    }
  } else {
    text = await viaHttp(body);
  }

  if (text === null || text.length === 0) {
    // Une notification n'a pas de réponse. Aucune méthode utilisée ici n'en est une, donc
    // c'est un cas anormal — mais rendre `undefined` typé vaut mieux que planter.
    throw new ApiError(`${method} n'a rien répondu`, "rpc");
  }

  const payload = JSON.parse(text) as {
    result?: T;
    error?: { code: number; message: string };
  };
  if (payload.error) {
    throw new ApiError(payload.error.message, "rpc");
  }
  return payload.result as T;
}

/**
 * Se présente au démon et vérifie qu'on parle la même version du contrat.
 *
 * Refuse plutôt que de continuer : les types de `types.ts` sont une recopie, et un contrat
 * qui a bougé produirait des champs manquants sans message d'erreur.
 */
export async function hello(): Promise<Hello> {
  const it = await call<Hello>("server.hello");
  if (it.protocol !== PROTOCOL) {
    throw new ApiError(
      `ce front parle le protocole ${PROTOCOL}, le démon parle ${it.protocol}. ` +
        `Mettre les deux à jour ensemble.`,
      "protocol",
    );
  }
  return it;
}

export const api = {
  hello,
  folders: () => call<Folder[]>("folders.list"),
  page: (folder: number, after?: string, limit = 100) =>
    call<Page>("messages.page", after === undefined ? { folder, limit } : { folder, after, limit }),
  /** Ouvre un message. `html` demande le corps assaini en plus du texte. */
  message: (id: number, html: boolean, remoteImages = false) =>
    call<Message | null>("messages.get", {
      id,
      body: html ? "html" : "text",
      remote_images: remoteImages,
    }),
  thread: (id: number) => call<{ rows: import("./types").Row[]; count: number }>("messages.thread", { id }),
  search: (query: string, limit = 50) => call<Results>("search.query", { query, limit }),
  stats: () => call<Stats>("store.stats"),
  revision: () => call<{ revision: string }>("store.revision"),
  /** Long-poll : rend la main quand la révision change, ou à l'expiration du délai. */
  wait: (revision: string, timeoutMs = 30000) =>
    call<Change>("store.wait", { revision, timeout_ms: timeoutMs }),
  jobs: () => call<Job[]>("jobs.list"),
};
