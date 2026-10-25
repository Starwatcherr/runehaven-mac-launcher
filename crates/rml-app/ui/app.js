// Home page: shows what the game needs, starts it and closes it. The work happens in Rust (src/commands.rs).
"use strict";

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const OK = '<svg class="mark ok" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-label="Ready"><path d="M5 12.5l4.5 4.5L19 7"/></svg>';
const WARN = '<svg class="mark warn" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linejoin="round" stroke-linecap="round" aria-label="Missing"><path d="M12 3.5l9.5 16.5h-19z"/><path d="M12 10v4.5M12 17.2v.3"/></svg>';
const ERR = '<svg class="mark err" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.2" stroke-linecap="round" aria-label="Failed"><path d="M6.5 6.5l11 11M17.5 6.5l-11 11"/></svg>';
const MARK = { ok: OK, missing: WARN, failed: ERR };

// Force close is offered after this many "Exit anyway" clicks, or when the game is still there after FORCE_AFTER_MS.
const FORCE_AFTER_TRIES = 3;
const FORCE_AFTER_MS = 10000;

const $ = id => document.getElementById(id);
const list = $("reqs"), install = $("install"), launch = $("launch"), launchState = $("launch-state"), note = $("note");
const dialog = $("exit-dialog"), exitNote = $("exit-note"), force = $("force");

let reqs = [];
let running = false;     // the game is starting or running
let loaded = false;      // its main menu is up
let closing = false;     // the player asked it to close, so its end is no surprise
let checking = false;
let exitTries = 0, forceTimer = null;

const escape = s => s.replace(/[&<>"]/g, c => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" })[c]);

function say(text, isError = false) {
  note.textContent = text;
  note.classList.toggle("err", isError);
}

function render() {
  list.innerHTML = reqs.map(r => `<li><span><span class="name">${escape(r.name)}</span>
    <span class="detail${r.state === "failed" ? " err" : ""}">${escape(r.detail)}</span></span>${MARK[r.state]}</li>`).join("");
  const allMet = reqs.length > 0 && reqs.every(r => r.state === "ok");
  install.disabled = allMet || reqs.length === 0;
  install.textContent = reqs.length === 0 ? "Checking…" : allMet ? "Requirements met" : "Install requirements";
  launch.disabled = !allMet && !running;
  launch.classList.toggle("active", running);
  launchState.textContent = !running ? "Launch Game" : loaded ? "Running" : "Starting…";
  if (!running && !note.classList.contains("err")) say(allMet || reqs.length === 0 ? "" : "Install the requirements first.");
}

async function refresh() {
  if (checking) return;
  checking = true;
  try {
    reqs = await invoke("requirements");
  } catch (e) {
    say(String(e), true);
  } finally {
    checking = false;
  }
  render();
}

install.addEventListener("click", () => {
  say("Installing from the launcher comes in the next step.");
});

launch.addEventListener("click", async () => {
  if (running) return openExit();
  say("");
  running = true;
  loaded = closing = false;
  render();
  try {
    await invoke("launch_game");
  } catch (e) {
    running = false;
    say(String(e), true);
    render();
  }
});

// --- leaving the game ---------------------------------------------------------------------------
function openExit() {
  exitTries = 0;
  exitNote.textContent = "";
  force.hidden = true;
  dialog.hidden = false;
  $("exit-cancel").focus();
}

function closeExit() {
  dialog.hidden = true;
  clearTimeout(forceTimer);
}

function offerForce() {
  force.hidden = false;
}

$("exit-cancel").addEventListener("click", closeExit);
dialog.addEventListener("click", e => { if (e.target === dialog) closeExit(); });
document.addEventListener("keydown", e => { if (e.key === "Escape" && !dialog.hidden) closeExit(); });

$("exit-anyway").addEventListener("click", async () => {
  closing = true;
  exitTries += 1;
  exitNote.textContent = "Asking Runehaven to close…";
  if (exitTries >= FORCE_AFTER_TRIES) offerForce();
  clearTimeout(forceTimer);
  forceTimer = setTimeout(offerForce, FORCE_AFTER_MS);
  try {
    await invoke("exit_game");
  } catch (e) {
    exitNote.textContent = "Runehaven did not respond.";
    offerForce();
  }
});

$("force-close").addEventListener("click", async () => {
  closing = true;
  exitNote.textContent = "Closing Runehaven…";
  try {
    await invoke("force_close_game");
  } catch (e) {
    exitNote.textContent = String(e);
  }
});

// --- what the game does ---------------------------------------------------------------------------
listen("game", ({ payload: e }) => {
  switch (e.state) {
    case "step":
      if (e.step === "setting_up_prefix") say("Setting up Wine, first start only (about a minute)…");
      if (e.step === "prefix_ready") say("");
      break;
    case "running": loaded = true; break;
    case "crashed":
      running = loaded = false;
      closeExit();
      say("Runehaven crashed. The log is saved in the launcher's logs folder.", true);
      break;
    case "exited":
      running = loaded = false;
      closeExit();
      if (closing || e.code === 0 || e.code === null) say("");
      else say(`Runehaven closed unexpectedly (exit code ${e.code}).`, true);
      break;
    case "failed":
      running = loaded = false;
      closeExit();
      say(e.message, true);
      break;
  }
  render();
});

// Steam or the game may have changed while the window was in the background.
window.addEventListener("focus", () => { if (!running) refresh(); });

RMLFire.start($("rml-fire"));
invoke("game_running").then(r => { running = loaded = r; render(); });
refresh();
