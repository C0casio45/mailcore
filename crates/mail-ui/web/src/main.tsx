/**
 * Le point d'entrée. Rien d'autre que le montage : tout ce qui décide est dans `App.tsx`.
 */

import { render } from "solid-js/web";
import { App } from "./App";
import "./theme.css";

const root = document.getElementById("app");
if (root === null) {
  // Impossible avec notre propre `index.html`. Le dire quand même : un échec silencieux au
  // montage donne une page blanche sans aucune piste.
  throw new Error("élément #app absent de la page");
}
render(() => <App />, root);
