/**
 * L'assemblage : trois panneaux, le chargement par pages, le cache, l'abonnement.
 *
 * ## L'ordre de démarrage sert le critère 1
 *
 * < 400 ms jusqu'à une interface utilisable, avec un démon possiblement au bout du réseau.
 * L'ordre est donc : **cache d'abord, réseau ensuite**.
 *
 * 1. Lire les dossiers et un écran de lignes dans IndexedDB, et les afficher. Aucun
 *    aller-retour réseau n'a encore eu lieu.
 * 2. Appeler `server.hello`, qui répond en une fois à tout ce qu'il faut savoir.
 * 3. Rafraîchir depuis le démon. Sa réponse gagne toujours, sans arbitrage.
 *
 * Un front qui attendrait le réseau avant de dessiner paierait deux allers-retours avant le
 * premier pixel, et le critère serait hors de portée sur un réseau lent.
 *
 * ## Ce que « le démon est injoignable » veut dire ici — critère 9
 *
 * Une panne réseau met l'application en mode dégradé **visible** : la liste déjà chargée reste
 * défilable, un bandeau le dit, et les actions qui exigent le démon — ouvrir, chercher —
 * échouent avec un message. Rien ne gèle et rien ne prétend fonctionner.
 */

import { Show, createEffect, createMemo, createSignal, onMount } from "solid-js";
import { Folders } from "./Folders";
import { List, ListHeader } from "./List";
import { Reader } from "./Reader";
import { ApiError, api, clearToken, embedded, setToken, token } from "./api";
import * as bench from "./bench";
import * as cache from "./cache";
import type { Folder, Message, Row } from "./types";

/** Lignes demandées par page. Aligné sur le défaut du démon. */
const PAGE = 100;

/** Lignes relues du cache au démarrage : un écran large, pas toute la boîte. */
const CACHE_PRELOAD = 200;

/**
 * Plafond de lignes chargées par le banc du critère 2.
 *
 * Le critère parle de 100 000 messages. Le plafond est au-dessus pour que le plus gros
 * dossier du corpus soit chargé **en entier** quand il en contient moins : le rapport
 * annonce le compte réellement atteint, jamais un chiffre rond qu'on n'a pas mesuré.
 */
const BENCH_ROWS = 120_000;

export function App() {
  // En mode embarqué il n'y a pas de jeton : rien n'écoute, donc rien à authentifier.
  // L'écran de saisie ne concerne que le mode onglet.
  const [ready, setReady] = createSignal(embedded() || token() !== null);
  const [offline, setOffline] = createSignal(false);
  const [fatal, setFatal] = createSignal<string | null>(null);

  const [folders, setFolders] = createSignal<Folder[]>([]);
  const [current, setCurrent] = createSignal<number | null>(null);

  // Le tableau creux des lignes du dossier courant. Remplacé par une copie à chaque page
  // reçue : muter en place ne réveillerait pas les signaux qui en dépendent.
  const [rows, setRows] = createSignal<(Row | undefined)[]>([]);
  const [cursor, setCursor] = createSignal<string | undefined>(undefined);
  const [exhausted, setExhausted] = createSignal(false);
  const [loadingPage, setLoadingPage] = createSignal(false);

  const [selected, setSelected] = createSignal<number | null>(null);
  const [message, setMessage] = createSignal<Message | null>(null);
  const [openError, setOpenError] = createSignal<string | null>(null);
  const [opening, setOpening] = createSignal(false);
  const [imagesShown, setImagesShown] = createSignal(false);

  const [query, setQuery] = createSignal("");
  const [searching, setSearching] = createSignal(false);
  const [searchHits, setSearchHits] = createSignal<Row[] | null>(null);

  const folder = createMemo(() => folders().find((it) => it.id === current()) ?? null);

  /** Ce que la liste affiche : les résultats de recherche, ou le dossier. */
  const visible = createMemo<(Row | undefined)[]>(() => searchHits() ?? rows());
  const total = createMemo(() => searchHits()?.length ?? folder()?.total ?? 0);
  const loadedCount = createMemo(() => visible().reduce((n, it) => (it ? n + 1 : n), 0));

  /** Traduit une erreur d'appel en état d'interface. */
  const handle = (error: unknown): string => {
    if (error instanceof ApiError) {
      if (error.kind === "offline") {
        setOffline(true);
      }
      if (error.kind === "unauthorized") {
        setReady(false);
      }
      if (error.kind === "protocol") {
        setFatal(error.message);
      }
      return error.message;
    }
    return String(error);
  };

  /** Charge la page suivante du dossier courant. */
  const loadNextPage = async () => {
    const id = current();
    if (id === null || loadingPage() || exhausted()) return;
    setLoadingPage(true);
    try {
      const page = await api.page(id, cursor(), PAGE);
      setOffline(false);

      const start = rows().reduce((n, it) => (it ? n + 1 : n), 0);
      setRows((previous) => {
        const next = previous.slice();
        page.rows.forEach((row, offset) => {
          next[start + offset] = row;
        });
        return next;
      });
      setCursor(page.next);
      if (page.next === undefined) {
        setExhausted(true);
      }
      void cache.putRows(id, page.rows);
    } catch (error) {
      handle(error);
    } finally {
      setLoadingPage(false);
    }
  };

  /** Ouvre un dossier : vide la liste, relit le cache, puis demande au démon. */
  const openFolder = async (id: number) => {
    setCurrent(id);
    setSearchHits(null);
    setQuery("");
    setCursor(undefined);
    setExhausted(false);
    setSelected(null);
    setMessage(null);
    void cache.putMeta("folder", String(id));

    // Le cache d'abord : la liste a quelque chose à montrer avant le premier aller-retour.
    const cached = await cache.rows(id, CACHE_PRELOAD);
    setRows(cached.length > 0 ? cached.slice() : []);

    // Puis le démon, qui écrase. Le curseur repart de zéro : les lignes du cache et celles du
    // démon sont les mêmes dans le même ordre, donc la première page recouvre le préchargement.
    setRows([]);
    await loadNextPage();
  };

  /** Ouvre un message. */
  const open = async (row: Row, withImages = false) => {
    setSelected(row.id);
    setOpening(true);
    setOpenError(null);
    setImagesShown(withImages);
    try {
      const found = await api.message(row.id, true, withImages);
      setOffline(false);
      if (found === null) {
        setMessage(null);
        setOpenError("Ce message n'existe plus dans le store.");
      } else {
        setMessage(found);
      }
    } catch (error) {
      setMessage(null);
      setOpenError(handle(error));
    } finally {
      setOpening(false);
    }
  };

  /** Lance une recherche. */
  const runSearch = async (event: Event) => {
    event.preventDefault();
    const text = query().trim();
    if (text.length === 0) {
      setSearchHits(null);
      return;
    }
    setSearching(true);
    try {
      const found = await api.search(text, 200);
      setOffline(false);
      if (!found.search_available) {
        setOpenError("L'index plein texte est absent côté démon.");
        setSearchHits([]);
      } else {
        setSearchHits(found.rows);
      }
    } catch (error) {
      setOpenError(handle(error));
    } finally {
      setSearching(false);
    }
  };

  /** Rafraîchit les dossiers depuis le démon. */
  const refreshFolders = async () => {
    try {
      const found = await api.folders();
      setOffline(false);
      setFolders(found);
      void cache.putFolders(found);
      return found;
    } catch (error) {
      handle(error);
      return null;
    }
  };

  /**
   * Le banc du critère 2 : le plus gros dossier, chargé pour de vrai, puis défilé.
   *
   * **Les lignes sont chargées avant de mesurer.** Défiler sur des lignes vides mesurerait un
   * dessin qu'on n'affiche jamais : pas de date, pas d'expéditeur, pas de sujet à réécrire.
   * Le chiffre serait flatteur et faux.
   *
   * Le chargement est séquentiel parce que la pagination du démon est par clé — c'est aussi
   * ce qui rend visible la limite documentée dans `List.tsx`.
   */
  const runScrollBench = async () => {
    const biggest = folders().reduce<Folder | null>(
      (best, it) => (best === null || it.total > best.total ? it : best),
      null,
    );
    if (biggest === null) return;
    await openFolder(biggest.id);

    /**
     * Attend qu'aucune page ne soit en vol.
     *
     * **La liste demande des pages de son côté** : son effet appelle `onNeed` dès qu'une
     * position visible manque, sans attendre personne. `loadNextPage` refuse un appel
     * concurrent, donc sans cette attente le banc voyait son propre appel ne rien faire et
     * concluait à un chargement bloqué — alors qu'une page arrivait juste après.
     */
    const idlePages = async () => {
      for (let waited = 0; loadingPage() && waited < 5000; waited += 20) {
        await new Promise((resolve) => setTimeout(resolve, 20));
      }
    };

    let pages = 0;
    while (!exhausted() && loadedCount() < BENCH_ROWS) {
      await idlePages();
      const before = loadedCount();
      await loadNextPage();
      await idlePages();
      pages += 1;
      // Une page en échec laisse `exhausted` à faux et le compte immobile : sans cette
      // sortie, le banc tournerait indéfiniment sur une erreur.
      if (loadedCount() === before) {
        await bench.note(
          `chargement arrêté à ${before} lignes après ${pages} pages ` +
            `(curseur ${cursor() ?? "absent"}, démon ${offline() ? "injoignable" : "joignable"})`,
        );
        break;
      }
    }
    await bench.note(
      `${loadedCount()} lignes sur ${biggest.total} dans « ${biggest.path} », ${pages} pages`,
    );
    await bench.painted();

    const found = await bench.measureScroll(loadedCount());
    if (found !== null) {
      await bench.reportFrames(found);
    }
  };

  onMount(() => {
    void (async () => {
      // Étape 1 : le cache. Zéro aller-retour, et il y a déjà quelque chose à l'écran.
      const [cachedFolders, lastFolder] = await Promise.all([
        cache.folders(),
        cache.meta("folder"),
      ]);
      if (cachedFolders.length > 0) {
        setFolders(cachedFolders);
        const wanted = lastFolder === null ? null : Number(lastFolder);
        const chosen =
          cachedFolders.find((it) => it.id === wanted) ??
          cachedFolders.find((it) => it.kind === "inbox") ??
          cachedFolders[0];
        if (chosen !== undefined) {
          setCurrent(chosen.id);
          setRows((await cache.rows(chosen.id, CACHE_PRELOAD)).slice());
        }
      }

      // **Le critère 1 se relève ici.** L'interface est à l'écran et répond : les dossiers
      // sont là, la liste défile, le clavier fonctionne. Aucun aller-retour n'a eu lieu.
      await bench.painted();
      void bench.phase("paint", loadedCount());

      if (!embedded() && token() === null) {
        setReady(false);
        return;
      }

      // Étape 2 : se présenter. Un seul aller-retour pour tout savoir.
      try {
        await api.hello();
        setReady(true);
        setOffline(false);
      } catch (error) {
        handle(error);
        return;
      }

      // Étape 3 : la vérité du démon écrase le cache.
      const fresh = await refreshFolders();
      if (fresh === null) return;
      const wanted = current();
      const chosen =
        fresh.find((it) => it.id === wanted) ??
        fresh.find((it) => it.kind === "inbox") ??
        fresh[0];
      if (chosen !== undefined) {
        await openFolder(chosen.id);
      }

      // Ce que le premier aller-retour ajoute au critère 1. Relevé à part : un démarrage lent
      // par le réseau et un démarrage lent par notre code ne se corrigent pas au même endroit.
      await bench.painted();
      void bench.phase("settled", loadedCount());

      if ((await bench.requested()) === "scroll") {
        await runScrollBench();
      }
    })();
  });

  /**
   * L'abonnement aux changements : un long-poll qui se relance.
   *
   * Pas de minuterie. `store.wait` dort côté démon jusqu'à ce que la révision change, donc un
   * client au repos ne coûte rien — et il apprend un import déclenché ailleurs sans avoir à
   * interroger toutes les secondes.
   */
  createEffect(() => {
    if (!ready() || fatal() !== null) return;
    let stopped = false;

    void (async () => {
      let known: string | null = null;
      while (!stopped) {
        try {
          if (known === null) {
            known = (await api.revision()).revision;
            continue;
          }
          const change = await api.wait(known, 30000);
          setOffline(false);
          if (change.changed) {
            known = change.revision;
            const fresh = await refreshFolders();
            const id = current();
            // Le store a bougé : les compteurs et la liste courante sont à relire.
            if (fresh !== null && id !== null && !stopped) {
              await openFolder(id);
            }
          }
        } catch (error) {
          if (stopped) return;
          handle(error);
          // Démon injoignable : réessayer, sans marteler. Cinq secondes est assez court pour
          // qu'un redémarrage passe inaperçu, assez long pour ne pas saturer.
          await new Promise((resolve) => setTimeout(resolve, 5000));
          known = null;
        }
      }
    })();

    return () => {
      stopped = true;
    };
  });

  return (
    <Show when={fatal() === null} fallback={<Fatal message={fatal()!} />}>
      <Show when={ready()} fallback={<Login onDone={() => window.location.reload()} />}>
        <div class="app">
          <Show when={offline()}>
            <div class="offline" role="status">
              Démon injoignable. La liste déjà chargée reste consultable ; ouvrir un message et
              chercher ne fonctionneront pas.
            </div>
          </Show>

          <div class="panes">
            <Folders
              folders={folders()}
              current={current()}
              onOpen={(id) => void openFolder(id)}
            />

            <section class="middle">
              <form class="search" onSubmit={(event) => void runSearch(event)}>
                <input
                  type="search"
                  placeholder="Rechercher — facture, from:banque, &quot;phrase exacte&quot;"
                  value={query()}
                  onInput={(event) => setQuery(event.currentTarget.value)}
                  aria-label="Rechercher"
                />
                <Show when={searchHits() !== null}>
                  <button type="button" onClick={() => { setSearchHits(null); setQuery(""); }}>
                    Effacer
                  </button>
                </Show>
              </form>

              <ListHeader
                title={searchHits() !== null ? "Résultats" : (folder()?.path ?? "—")}
                total={total()}
                loaded={loadedCount()}
              />

              <List
                total={total()}
                loaded={visible()}
                selected={selected()}
                onNeed={() => {
                  // La pagination par clé n'obtient les pages qu'en séquence : on demande la
                  // suivante, sans tenir compte de la position réclamée. Voir la limite
                  // documentée dans `List.tsx`.
                  if (searchHits() === null) void loadNextPage();
                }}
                onSelect={(row) => void open(row)}
              />

              <Show when={searching() || loadingPage()}>
                <div class="list-footer muted">Chargement…</div>
              </Show>
            </section>

            <Reader
              message={message()}
              loading={opening()}
              error={openError()}
              imagesShown={imagesShown()}
              onShowImages={() => {
                const id = selected();
                const row = visible().find((it) => it?.id === id);
                if (row !== undefined) void open(row, true);
              }}
            />
          </div>
        </div>
      </Show>
    </Show>
  );
}

/** L'écran de saisie du jeton, en mode onglet de navigateur. */
function Login(props: { onDone: () => void }) {
  const [value, setValue] = createSignal("");
  const [error, setError] = createSignal<string | null>(null);
  const [busy, setBusy] = createSignal(false);

  const submit = async (event: Event) => {
    event.preventDefault();
    const candidate = value().trim();
    if (candidate.length === 0) return;
    setBusy(true);
    setError(null);
    setToken(candidate);
    try {
      await api.hello();
      props.onDone();
    } catch (cause) {
      clearToken();
      setError(cause instanceof ApiError ? cause.message : String(cause));
    } finally {
      setBusy(false);
    }
  };

  return (
    <form class="login" onSubmit={(event) => void submit(event)}>
      <h1>mailcore</h1>
      <p class="muted">
        Jeton d'accès au démon. Il ne vaut que pour cet onglet et disparaît à sa fermeture.
      </p>
      <input
        type="password"
        autocomplete="off"
        placeholder="Jeton"
        value={value()}
        onInput={(event) => setValue(event.currentTarget.value)}
        aria-label="Jeton"
      />
      <button type="submit" disabled={busy()}>
        {busy() ? "Vérification…" : "Ouvrir"}
      </button>
      <Show when={error() !== null}>
        <p class="error">{error()}</p>
      </Show>
    </form>
  );
}

/** L'écran d'arrêt : un désaccord de contrat n'est pas rattrapable côté client. */
function Fatal(props: { message: string }) {
  return (
    <div class="login">
      <h1>Incompatible</h1>
      <p class="error">{props.message}</p>
      <p class="muted">
        Le front et le démon doivent être mis à jour ensemble. Rien n'a été lu ni écrit.
      </p>
    </div>
  );
}
