import { ref } from "vue";
import vuetify from "../vuetify";

export type ThemeName = "dark" | "light";

const stored =
  typeof localStorage !== "undefined" ? localStorage.getItem("cpum-theme") : null;
const theme = ref<ThemeName>(stored === "light" ? "light" : "dark");
// Apply the persisted theme at startup (Vuetify theme names match the dark/light semantics here)
vuetify.theme.global.name.value = theme.value;

/**
 * Theme switcher: dark <-> light.
 * Persisted in localStorage under `cpum-theme`, mirroring the `cpum-locale`
 * pattern used by i18n.ts. The module-level ref is auto-tracked by templates
 * and computeds, so toggling just flips the ref.
 */
export function useTheme() {
  const toggleTheme = () => {
    theme.value = theme.value === "dark" ? "light" : "dark";
    vuetify.theme.global.name.value = theme.value;
    localStorage.setItem("cpum-theme", theme.value);
  };
  return { theme, toggleTheme };
}
