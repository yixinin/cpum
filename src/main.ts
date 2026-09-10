import { createApp } from "vue";
import vuetify from "./vuetify";
import App from "./App.vue";

// Remove the static skeleton (the #app-skeleton in index.html) once Vue has mounted.
// Wrapped in try/finally so a mount exception still removes the skeleton — otherwise
// the user would be stuck on the mock UI.
try {
  createApp(App).use(vuetify).mount("#app");
} finally {
  const skeleton = document.getElementById("app-skeleton");
  if (skeleton) skeleton.remove();
}
