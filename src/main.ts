import { createApp } from "vue";
import App from "@/App.vue";
import { installConsoleCapture } from "@/lib/logbus";
import "@/styles/global.css";

// Before anything else mounts: every console line and uncaught error from here
// on also lands in the in-app 日志 view.
installConsoleCapture();

createApp(App).mount("#app");

// Block the browser's native context menu / file drop navigation: the app
// supplies its own menus and treats drops as uploads.
window.addEventListener("contextmenu", (event) => event.preventDefault());
window.addEventListener("dragover", (event) => event.preventDefault());
window.addEventListener("drop", (event) => event.preventDefault());
