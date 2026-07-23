import { mount } from "svelte";
import App from "./App.svelte";
import "./styles.css";

const target = document.getElementById("app");
if (target === null) {
  throw new Error("frontend mount target is missing");
}

mount(App, { target });
