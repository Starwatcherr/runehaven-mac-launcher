// Home page: shows what the game needs, installs it, starts the game and closes it. The work happens in Rust (src/commands.rs).
"use strict";

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const OK = '<svg class="mark ok" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-label="Ready"><path d="M5 12.5l4.5 4.5L19 7"/></svg>';
const WARN = '<svg class="mark warn" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linejoin="round" stroke-linecap="round" aria-label="Missing"><path d="M12 3.5l9.5 16.5h-19z"/><path d="M12 10v4.5M12 17.2v.3"/></svg>';
const ERR = '<svg class="mark err" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.2" stroke-linecap="round" aria-label="Failed"><path d="M6.5 6.5l11 11M17.5 6.5l-11 11"/></svg>';
const SPIN = '<span class="spin" role="img" aria-label="Working"></span>';
const MARK = { ok: OK, missing: WARN, failed: ERR, working: SPIN };

// Force close is offered after this many "Exit anyway" clicks, or when the game is still there after FORCE_AFTER_MS.
const FORCE_AFTER_TRIES = 3;
const FORCE_AFTER_MS = 10000;

const $ = id => document.getElementById(id);
const list = $("reqs"), install = $("install"), installState = $("install-state");
const launch = $("launch"), launchState = $("launch-state"), note = $("note");
const dialog = $("exit-dialog"), exitNote = $("exit-note"), force = $("force");

let reqs = [];
let running = false;     // the game is starting or running
let loaded = false;      // its main menu is up
let closing = false;     // the player asked it to close, so its end is no surprise
let checking = false;
let exitTries = 0, forceTimer = null;
let installing = false, cancelling = false;
let work = {};           // during and after setup: what a requirement is doing, or why it failed
let progress = null;     // of the current setup step, 0 to 1, null when unknown

const escape = s => s.replace(/[&<>"]/g, c => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" })[c]);

function say(text, isError = false) {
  note.textContent = text;
  note.classList.toggle("err", isError);
}

// Rows are built once and then only changed where needed, so a spinning mark keeps turning.
function renderList() {
  const ids = reqs.map(r => r.id).join();
  if (list.dataset.ids !== ids) {
    list.innerHTML = reqs.map(r => `<li data-id="${r.id}"><span><span class="name"></span>
      <span class="detail"></span></span><span class="mark-slot"></span></li>`).join("");
    list.dataset.ids = ids;
  }
  for (const r of reqs) {
    const li = list.querySelector(`li[data-id="${r.id}"]`);
    const w = work[r.id];
    const failed = w?.failed !== undefined || r.state === "failed";
    const link = r.id === "steam" && r.state === "missing" ? '<button class="link" data-action="get-steam">Get Steam</button>' : "";
    const html = escape(w?.failed ?? w?.detail ?? r.detail) + link;
    const kind = w?.detail !== undefined ? "working" : failed ? "failed" : r.state;
    li.querySelector(".name").textContent = r.name;
    const detail = li.querySelector(".detail");
    if (detail.dataset.html !== html) { detail.innerHTML = html; detail.dataset.html = html; }
    detail.classList.toggle("err", failed);
    const slot = li.querySelector(".mark-slot");
    if (slot.dataset.kind !== kind) { slot.innerHTML = MARK[kind]; slot.dataset.kind = kind; }
  }
}

function render() {
  renderList();
  const allMet = reqs.length > 0 && reqs.every(r => r.state === "ok");
  const anyFailed = Object.values(work).some(w => w.failed !== undefined);
  install.disabled = !installing && (allMet || reqs.length === 0);
  install.classList.toggle("active", installing && !cancelling);
  install.style.setProperty("--fill", `${Math.round((progress ?? 0) * 24) / 24 * 100}%`);
  installState.textContent = cancelling ? "Cancelling…"
    : installing ? `Installing…${progress === null ? "" : ` ${Math.round(progress * 100)}%`}`
    : reqs.length === 0 ? "Checking…" : allMet ? "Requirements met" : anyFailed ? "Try again" : "Install requirements";
  launch.disabled = installing || (!allMet && !running);
  $("custom").hidden = allMet || installing || reqs.length === 0;
  launch.classList.toggle("active", running);
  launchState.textContent = !running ? "Launch Game" : loaded ? "Running" : "Starting…";
  if (!running && !note.classList.contains("err")) say(allMet || installing || reqs.length === 0 ? "" : "Install the requirements first.");
}

async function refresh() {
  if (checking) return;
  checking = true;
  try {
    reqs = await invoke("requirements");
    // A requirement that is fine now no longer shows why it failed earlier.
    for (const r of reqs) if (r.state === "ok") delete work[r.id];
  } catch (e) {
    say(String(e), true);
  } finally {
    checking = false;
  }
  render();
}

// --- installing what is missing ---------------------------------------------------------------
install.addEventListener("click", async () => {
  if (installing) {
    cancelling = true;
    render();
    return invoke("cancel_install");
  }
  work = {};
  progress = null;
  installing = true;
  say("");
  render();
  try {
    await invoke("install_requirements");
  } catch (e) {
    installing = false;
    say(String(e), true);
    render();
  }
});

$("custom").addEventListener("click", () => window.rml.openLocations());

list.addEventListener("click", e => {
  if (e.target.dataset.action === "get-steam") invoke("open_steam_download");
});

listen("setup", ({ payload: e }) => {
  switch (e.kind) {
    case "working": work[e.id] = { detail: e.detail }; progress = e.progress; break;
    case "done": delete work[e.id]; progress = null; break;
    case "failed": work[e.id] = { failed: e.message }; break;
    case "finished":
      installing = cancelling = false;
      progress = null;
      for (const id in work) if (work[id].failed === undefined) delete work[id];
      refresh();
      break;
  }
  render();
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
window.addEventListener("focus", () => { if (!running && !installing) refresh(); });

// For the other pages (pages.js): whether something is running, and the home page's own actions.
window.rml = {
  busy: () => running || installing,
  refresh,
  install: async () => { await refresh(); if (!install.disabled) install.click(); },
};

RMLFire.start($("rml-fire"));
invoke("game_running").then(r => { running = loaded = r; render(); });
refresh();
