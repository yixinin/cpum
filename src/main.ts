import "vuetify/styles";
import "@mdi/font/css/materialdesignicons.css";
import { createApp } from "vue";
import { createVuetify } from "vuetify";
import { aliases, mdi } from "vuetify/iconsets/mdi";
import App from "./App.vue";

// 暗色主题，参考 Process Lasso 的工程化风格
const vuetify = createVuetify({
  theme: {
    defaultTheme: "dark",
    themes: {
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
    },
  },
  icons: {
    defaultSet: "mdi",
    aliases,
    sets: { mdi },
  },
});

// Vue 挂载后移除静态骨架 (index.html 中的 #app-skeleton)
// 用 try-finally: 即使 Vue 挂载抛异常, 也要移除骨架, 否则用户永远看到 mock UI
try {
  createApp(App).use(vuetify).mount("#app");
} finally {
  const skeleton = document.getElementById("app-skeleton");
  if (skeleton) skeleton.remove();
}
