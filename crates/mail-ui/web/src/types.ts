/**
 * Les types du contrat, recopiés depuis `mailapi::dto`.
 *
 * ## Une recopie, et c'est le prix du choix du front
 *
 * `mailapi` porte ces types en Rust, et un client Rust les relit avec le même code — un
 * champ renommé casse alors à la compilation. Ici, en TypeScript, ils sont retapés : rien ne
 * garantit que ce fichier et le Rust restent d'accord.
 *
 * C'est le vrai coût d'un front web, et il était connu quand Tauri a été retenu. Le
 * contrepoids est `PROTOCOL` : le démon annonce sa version de contrat dans `server.hello`, et
 * le front refuse de parler à un démon dont le numéro ne correspond pas — voir `api.ts`. Ça
 * ne rattrape pas un champ silencieusement renommé sans changement de version, mais ça
 * rattrape tout changement incompatible fait dans les règles.
 *
 * Génération automatique depuis les types Rust : envisageable, pas faite. Elle ajouterait une
 * étape de build et une dépendance pour un fichier de 120 lignes qui change rarement. À
 * reconsidérer le jour où il en fera 600.
 */

/** La version de contrat que ce front sait parler. Doit correspondre à `mailapi::PROTOCOL`. */
export const PROTOCOL = 1;

/** Ce que le démon dit de lui-même. */
export interface Hello {
  server: string;
  version: string;
  protocol: number;
  search_available: boolean;
  revision: string;
  messages: number;
}

/** Un dossier et ses compteurs. */
export interface Folder {
  id: number;
  account: string;
  path: string;
  kind: string;
  total: number;
  unread: number;
}

/** Une ligne de liste. */
export interface Row {
  id: number;
  /** Secondes Unix. `0` quand l'en-tête `Date` manquait. */
  date: number;
  from: string;
  from_name: string | null;
  subject: string;
  has_attachments: boolean;
  unread: boolean;
  flagged: boolean;
  score?: number;
}

/** Une page de liste. */
export interface Page {
  rows: Row[];
  /** Curseur opaque de la page suivante. Absent = fin de la liste. */
  next?: string;
  revision: string;
}

/** Une pièce jointe, décrite et jamais ouverte en phase 1. */
export interface Attachment {
  name: string | null;
  mime: string;
  size: number;
}

/** Un dossier qui référence un message. */
export interface Location {
  path: string;
  unread: boolean;
}

/** Pourquoi une ressource distante est signalée. */
export type TrackerKind = "pixel" | "known_domain" | "correlated_id";

/** Un traceur relevé. L'hôte, jamais l'URL. */
export interface Tracker {
  kind: TrackerKind;
  host: string;
}

/** Le corps HTML assaini, avec de quoi le confiner. */
export interface Html {
  html: string;
  truncated: boolean;
  /** À poser en `Content-Security-Policy` sur l'`<iframe>`. Vient du démon, source unique. */
  csp: string;
  /** À poser en `sandbox` sur l'`<iframe>`. */
  sandbox: string;
  blocked_images: number;
  remote_resources: number;
  trackers: Tracker[];
}

/** Un message ouvert. */
export interface Message {
  row: Row;
  message_id: string | null;
  to: string[];
  body: string;
  body_truncated: boolean;
  html?: Html;
  attachments: Attachment[];
  folders: Location[];
  thread: number | null;
}

/** Un résultat de recherche. */
export interface Results {
  rows: Row[];
  count: number;
  truncated: boolean;
  search_available: boolean;
}

/** L'état du store. */
export interface Stats {
  accounts: number;
  folders: number;
  messages: number;
  refs: number;
  unthreaded: number;
  raw_bytes: number;
  search_available: boolean;
  revision: string;
}

/** La réponse à `store.wait`. */
export interface Change {
  revision: string;
  changed: boolean;
}

/** Une tâche de fond. */
export interface Job {
  id: number;
  kind: string;
  state: "queued" | "running" | "done" | "cancelled" | "failed";
  done: number;
  total: number;
  fraction?: number;
  message?: string;
  queued_at: number;
  finished_at?: number;
}
