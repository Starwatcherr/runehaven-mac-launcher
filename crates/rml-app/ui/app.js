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
let steamNote = "";      // while Steam is switched back to the Mac version


function say(text, isError = false, action = null) {
  note.textContent = text;
  note.classList.toggle("err", isError);
  if (action) {
    const button = document.createElement("button");
    button.className = "link";
    button.textContent = action.label;
    button.addEventListener("click", action.run);
    note.append(" ", button);
  }
}

// After a crash: everything a bug report needs, in one ZIP on the desktop.
const exportLogs = { label: "Export logs", run: async () => {
  say("Packing the logs…");
  try {
    const path = await invoke("export_logs");
    say(`Saved ${path.split("/").pop()} on your desktop. Attach it to your bug report.`);
  } catch (e) {
    say(String(e), true);
  }
} };

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
    const getSteam = r.id === "steam" && r.state === "missing";
    const text = w?.failed ?? w?.detail ?? r.detail;
    const kind = w?.detail !== undefined ? "working" : failed ? "failed" : r.state;
    li.querySelector(".name").textContent = r.name;
    const detail = li.querySelector(".detail");
    const shown = `${text}|${getSteam}`;
    if (detail.dataset.shown !== shown) {
      detail.textContent = text;
      if (getSteam) {
        const button = document.createElement("button");
        button.className = "link";
        button.dataset.action = "get-steam";
        button.textContent = "Get Steam";
        detail.append(button);
      }
      detail.dataset.shown = shown;
    }
    detail.classList.toggle("err", failed);
    const slot = li.querySelector(".mark-slot");
    if (slot.dataset.kind !== kind) { slot.innerHTML = MARK[kind]; slot.dataset.kind = kind; }
  }
}

function render() {
  renderList();
  const allMet = reqs.length > 0 && reqs.every(r => r.state === "ok");
  const anyFailed = Object.values(work).some(w => w.failed !== undefined);
  // Only newer versions missing: it is an update, not an installation.
  const onlyUpdates = reqs.some(r => r.state !== "ok") && reqs.every(r => r.state === "ok" || r.update);
  // A maintenance job (pages.js) changes the same things; nothing starts meanwhile.
  const jobBusy = window.rml?.jobBusy?.() ?? false;
  install.disabled = !installing && (allMet || reqs.length === 0 || jobBusy);
  install.classList.toggle("active", installing && !cancelling);
  install.style.setProperty("--fill", `${Math.round((progress ?? 0) * 24) / 24 * 100}%`);
  installState.textContent = cancelling ? "Cancelling…"
    : installing ? `Installing…${progress === null ? "" : ` ${Math.round(progress * 100)}%`}`
    : reqs.length === 0 ? "Checking…" : allMet ? "Requirements met" : anyFailed ? "Try again"
    : onlyUpdates ? "Update requirements" : "Install requirements";
  launch.disabled = installing || (!running && (!allMet || jobBusy));
  $("custom").hidden = allMet || installing || reqs.length === 0;
  launch.classList.toggle("active", running);
  launchState.textContent = !running ? "Launch Game" : loaded ? "Running" : "Starting…";
  if (!running && !note.classList.contains("err")) {
    say(steamNote || (allMet || installing || reqs.length === 0) ? steamNote : onlyUpdates ? "Update the requirements first." : "Install the requirements first.");
  }
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
    case "starting":
      if (e.first_start) say("The first start after an update can take a minute.");
      break;
    case "step":
      if (e.step === "setting_up_prefix") say("Setting up Wine, first start only (about a minute)…");
      if (e.step === "prefix_ready") say("");
      if (e.step === "foreign_dll_moved_aside") say(`Another version.dll was in the game folder. A copy is in the launcher's backups: ${e.to.split("/").pop()}`);
      break;
    case "running": loaded = true; say(""); break;
    case "crashed":
      running = loaded = false;
      closeExit();
      say("Runehaven crashed.", true, exportLogs);
      break;
    case "exited":
      running = loaded = false;
      closeExit();
      if (closing || e.code === 0 || e.code === null) say("");
      else say(`Runehaven closed unexpectedly (exit code ${e.code}).`, true, exportLogs);
      break;
    case "failed":
      running = loaded = false;
      closeExit();
      say(e.message, true);
      break;
  }
  render();
});

// Switching Steam back to the Mac version after an interrupted task (src/maintenance.rs).
listen("maintenance", ({ payload: e }) => {
  if (e.job !== "restore") return;
  steamNote = e.kind === "working" ? e.detail : "";
  if (e.kind === "working") say(e.detail);
  else if (e.kind === "failed") say(`Steam is still set to the Windows version: ${e.message}`, true);
  else if (!running) say(e.detail);
});

// Steam or the game may have changed while the window was in the background.
window.addEventListener("focus", () => { if (!running && !installing) refresh(); });

// For the other pages (pages.js): whether something is running, and the home page's own actions.
window.rml = {
  busy: () => running || installing,
  refresh,
  render,
  install: async () => { await refresh(); if (!install.disabled) install.click(); },
};

RMLFire.start($("rml-fire"));
invoke("game_running").then(r => { running = loaded = r; render(); });
refresh();
