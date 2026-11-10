import { createApp } from "vue";
import App from "@/App.vue";
import "@/styles/global.css";

createApp(App).mount("#app");

// Block the browser's native context menu / file drop navigation: the app
// supplies its own menus and treats drops as uploads.
window.addEventListener("contextmenu", (event) => event.preventDefault());
window.addEventListener("dragover", (event) => event.preventDefault());
window.addEventListener("drop", (event) => event.preventDefault());
