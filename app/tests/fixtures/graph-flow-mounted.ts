import { mount } from "svelte";
import "@xyflow/svelte/dist/style.css";
import "../../src/app.css";
import GraphFlowMounted from "./GraphFlowMounted.svelte";

mount(GraphFlowMounted, { target: document.getElementById("app")! });