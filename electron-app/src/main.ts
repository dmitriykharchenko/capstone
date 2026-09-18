import { mount } from "svelte";
import App from "./App.svelte";
import "./app.css";
import { applyTheme, resolveTheme } from "./lib/theme";

// Keep first-paint theme behavior without an inline script in the strict CSP.
try { applyTheme(resolveTheme()); } catch { /* Storage may be unavailable. */ }

const target = document.getElementById("app");
if (!target) {
  throw new Error("missing #app mount target");
}

const app = mount(App, { target });

export default app;
