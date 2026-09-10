import "vuetify/styles";
import "@mdi/font/css/materialdesignicons.css";
import { createVuetify } from "vuetify";
import { aliases, mdi } from "vuetify/iconsets/mdi";

/**
 * Vuetify instance (singleton).
 * Kept in its own module to avoid a main.ts <-> composables/useTheme.ts
 * circular dependency; theme toggling only needs to mutate
 * vuetify.theme.global.name.
 */
const vuetify = createVuetify({
  theme: {
    defaultTheme: "dark",
    themes: {
      // Dark theme — modeled after Process Lasso's engineering aesthetic
      dark: {
        colors: {
          background: "#1e1e1e",
          surface: "#252526",
          primary: "#3a7bd5",
          secondary: "#42a5f5",
          success: "#66bb6a",
          warning: "#ffa726",
          error: "#ef5350",
          info: "#29b6f6",
        },
      },
      // Light theme: same palette, deepened for sufficient contrast on a white background
      light: {
        colors: {
          background: "#f4f5f7",
          surface: "#ffffff",
          primary: "#2f6fce",
          secondary: "#42a5f5",
          success: "#43a047",
          warning: "#fb8c00",
          error: "#e53935",
          info: "#039be5",
        },
      },
    },
  },
  icons: {
    defaultSet: "mdi",
    aliases,
    sets: { mdi },
  },
});

export default vuetify;
