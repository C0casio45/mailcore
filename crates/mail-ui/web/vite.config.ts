import { defineConfig, type Plugin } from "vite";
import solid from "vite-plugin-solid";

/** L'élément `<meta>` qui porte la CSP du mode onglet. */
const META_CSP = /\s*<meta\s+http-equiv="Content-Security-Policy"[\s\S]*?\/>/;

/**
 * Retire la CSP en `<meta>` de la page empaquetée dans la coquille.
 *
 * ## Deux politiques ne s'additionnent pas, elles s'intersectent
 *
 * `index.html` porte sa propre CSP en `<meta>` — c'est ce qui protège le mode onglet, où le
 * démon ne pose aucun en-tête. Tauri, de son côté, injecte celle de `tauri.conf.json`. Quand
 * les deux sont présentes, **chaque directive est prise dans sa version la plus stricte**.
 *
 * ## Ce que ça cassait, et comment ça se voyait
 *
 * Deux fois, pour deux raisons différentes :
 *
 * 1. `connect-src 'self'` dans la page annule l'autorisation d'`ipc:` accordée par la
 *    configuration, donc plus aucun appel à la coquille ne passe ;
 * 2. et même une fois `ipc:` ajouté à la page, `script-src 'self'` reste sans le **nonce**
 *    que Tauri fabrique à chaque démarrage pour son script d'amorçage — celui qui pose
 *    `window.__TAURI_INTERNALS__`. Tauri ajoute ce nonce à la politique de
 *    `tauri.conf.json` ; il ne peut pas l'ajouter à une politique écrite à la main dans notre
 *    fichier. L'amorçage est donc bloqué, `embedded()` répond faux, et l'application se croit
 *    dans un onglet : elle réclame un jeton qu'il n'y a pas.
 *
 * Le symptôme, dans les deux cas, est une fenêtre qui s'ouvre et ne répond à rien — sans
 * message ailleurs que dans une console que le webview n'ouvre pas. C'est ce qu'a coûté la
 * première exécution réelle de la coquille, et c'est pour ça que ce greffon est écrit avec des
 * vérifications plutôt qu'avec un `replace` optimiste.
 *
 * ## Une politique par hôte, et une seule
 *
 * En mode onglet, le `<meta>` fait foi. Dans la coquille, `tauri.conf.json` fait foi — et
 * c'est la seule qui **puisse** faire foi, puisqu'elle est la seule que Tauri sait compléter.
 * Les deux disent la même chose au canal d'IPC près ; un test Rust les compare, pour qu'un
 * durcissement d'un côté ne laisse pas l'autre en arrière.
 */
function stripMetaCsp(): Plugin {
  return {
    name: "mailcore-csp-tauri",
    transformIndexHtml(html) {
      // Échouer bruyamment plutôt que produire une application sans politique : si le
      // `<meta>` a changé de forme, il faut le savoir ici et pas à l'exécution.
      if (!META_CSP.test(html)) {
        throw new Error(
          "CSP : le <meta http-equiv=\"Content-Security-Policy\"> d'index.html est " +
            "introuvable. La construction pour la coquille Tauri le retire — voir " +
            "vite.config.ts.",
        );
      }
      const stripped = html.replace(META_CSP, "");
      if (stripped.includes("Content-Security-Policy")) {
        throw new Error(
          "CSP : une politique subsiste dans index.html après le retrait. Deux politiques " +
            "s'intersectent et bloqueraient l'amorçage de Tauri — voir vite.config.ts.",
        );
      }
      return stripped;
    },
  };
}

// Vite + Solid, sans méta-framework.
//
// Pas de SolidStart : il apporte du rendu serveur et un routage dont on n'a aucun usage —
// `docs/ARCHITECTURE.md` demande que le front reste « ouvrable tel quel dans un onglet de
// navigateur, servi par le démon », c'est-à-dire une application statique et rien de plus.
//
// Deux modes, une seule base de code : `vite build` produit le front que le démon sert dans un
// onglet, `vite build --mode tauri` celui que la coquille embarque. La seule différence est la
// CSP, et elle est décrite au-dessus.
export default defineConfig(({ mode }) => ({
  plugins: [solid(), ...(mode === "tauri" ? [stripMetaCsp()] : [])],
  // Le démon sert ce répertoire par `--ui-dir`. Il est dans `dist/` du paquet, pas dans le
  // workspace Rust : rien de généré ne va dans `crates/`.
  build: {
    target: "es2022",
    // Deux répertoires distincts : la coquille embarque `dist-tauri/` au moment de la
    // compilation, le démon sert `dist/`. Un seul répertoire ferait que la dernière
    // construction lancée déciderait de la CSP des deux, ce qui est précisément le genre de
    // dépendance à l'ordre qu'on ne veut pas sur une politique de sécurité.
    outDir: mode === "tauri" ? "dist-tauri" : "dist",
    emptyOutDir: true,
    // Un seul fichier JS et un seul CSS. Le critère 1 se joue sur le temps d'analyse et
    // d'exécution, pas sur le téléchargement — tout est local — et une cascade de modules
    // coûterait des allers-retours pour rien.
    rollupOptions: {
      output: {
        manualChunks: undefined,
      },
    },
  },
  server: {
    port: 5173,
    // En développement, l'API est appelée sur le démon plutôt que sur Vite.
    proxy: {
      "/api": "http://127.0.0.1:7847",
    },
  },
}));
