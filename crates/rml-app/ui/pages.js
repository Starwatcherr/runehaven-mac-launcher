// The pages behind the side bar: switching between them, Settings and About. Home lives in app.js.
"use strict";

(function () {
  const { invoke } = window.__TAURI__.core;
  const $ = id => document.getElementById(id);
  const pages = document.querySelectorAll(".page");
  const navButtons = document.querySelectorAll("nav button[data-page]");

  function show(name) {
    pages.forEach(p => { p.hidden = p.dataset.page !== name; });
    navButtons.forEach(b => {
      if (b.dataset.page === name) b.setAttribute("aria-current", "page");
      else b.removeAttribute("aria-current");
    });
    if (name === "settings") loadSettings();
    if (name === "maintenance") loadMaintenance();
  }
  navButtons.forEach(b => b.addEventListener("click", () => show(b.dataset.page)));

  // --- asking before anything is changed or deleted -----------------------------------------------
  const confirmDialog = $("confirm-dialog");
  let answer = null;
  function ask(title, text, okLabel, danger = false) {
    $("confirm-h").textContent = title;
    $("confirm-text").textContent = text;
    $("confirm-ok").textContent = okLabel;
    $("confirm-ok").classList.toggle("danger", danger);
    confirmDialog.hidden = false;
    $("confirm-cancel").focus();
    return new Promise(resolve => { answer = resolve; });
  }
  function settle(yes) {
    confirmDialog.hidden = true;
    if (answer) answer(yes);
    answer = null;
  }
  $("confirm-ok").addEventListener("click", () => settle(true));
  $("confirm-cancel").addEventListener("click", () => settle(false));
  confirmDialog.addEventListener("click", e => { if (e.target === confirmDialog) settle(false); });
  document.addEventListener("keydown", e => { if (e.key === "Escape" && !confirmDialog.hidden) settle(false); });

  // --- Maintenance ----------------------------------------------------------------------------------
  const mNote = $("m-note");
  const actions = ["m-update", "m-verify", "m-uninstall", "m-reinstall", "m-reset", "m-remove"].map($);
  let status = null;
  let jobRunning = false;

  const fmtSize = bytes => bytes >= 1 << 30 ? `${(bytes / (1 << 30)).toFixed(1)} GB` : `${Math.max(1, Math.round(bytes / (1 << 20)))} MB`;
  const withSize = (part) => part.installed && part.bytes > 0 ? `${part.detail} · ${fmtSize(part.bytes)}` : part.detail;

  function sayM(text, isError = false) {
    mNote.textContent = text;
    mNote.style.color = isError ? "var(--red)" : "";
  }

  function renderMaintenance() {
    const busy = jobRunning || window.rml.busy();
    if (!status) { actions.forEach(b => { b.disabled = true; }); return; }
    $("m-game").textContent = withSize(status.game);
    $("m-update").hidden = !status.update_pending;
    $("m-runtime").textContent = withSize(status.runtime);
    $("m-prefix").textContent = withSize(status.prefix);
    $("m-data").textContent = status.own_prefix
      ? `Wine runtime, Wine setup, downloads and logs (${fmtSize(status.data_bytes)}). Your save games go to the desktop first.`
      : `Wine runtime, downloads and logs (${fmtSize(status.data_bytes)}). Your own Wine setup is kept.`;
    $("m-update").disabled = busy;
    $("m-verify").disabled = busy || !status.game.installed;
    $("m-uninstall").disabled = busy || !status.game.installed;
    $("m-reinstall").disabled = busy || status.runtime.detail.startsWith("Custom");
    $("m-reset").disabled = busy || !status.prefix.installed || !status.runtime.installed;
    $("m-remove").disabled = busy || status.data_bytes === 0;
    if (window.rml.busy() && !jobRunning) sayM("Close Runehaven or wait for the installation to finish first.");
  }

  async function loadMaintenance() {
    try {
      status = await invoke("maintenance_status");
    } catch (e) {
      sayM(String(e), true);
    }
    renderMaintenance();
  }

  function startJob(text) {
    jobRunning = true;
    sayM(text);
    renderMaintenance();
  }

  $("m-update").addEventListener("click", () => { show("home"); window.rml.install(); });

  $("m-verify").addEventListener("click", async () => {
    if (!await ask("Verify game files?", "Steam checks Runehaven's files and downloads anything broken or missing. For this, Steam briefly switches to the Windows version.", "Verify")) return;
    startJob("Starting Steam…");
    invoke("verify_game");
  });

  $("m-uninstall").addEventListener("click", async () => {
    if (!await ask("Uninstall Runehaven?", "Steam removes the game files from your Mac. Your save games stay in the Wine setup, and you can install the game again on the home page.", "Uninstall", true)) return;
    startJob("Starting Steam…");
    invoke("uninstall_game");
  });

  $("m-reinstall").addEventListener("click", async () => {
    if (!await ask("Reinstall the Wine runtime?", "The launcher deletes it and downloads it again (about 376 MB).", "Reinstall")) return;
    try {
      await invoke("remove_runtime");
      show("home");
      window.rml.install();
    } catch (e) {
      sayM(String(e), true);
    }
  });

  $("m-reset").addEventListener("click", async () => {
    if (!await ask("Reset the Wine setup?", "The launcher sets Wine up again from scratch. Save games and game settings are kept, and the old setup stays as a backup.", "Reset")) return;
    startJob("Setting up Wine again, about a minute…");
    invoke("reset_prefix");
  });

  $("m-remove").addEventListener("click", async () => {
    const text = status.own_prefix
      ? "This deletes the Wine runtime, the Wine setup, downloads and logs. Your save games are packed onto your desktop first. Runehaven itself stays in Steam."
      : "This deletes the Wine runtime, downloads and logs. Your own Wine setup is not touched. Runehaven itself stays in Steam.";
    if (!await ask("Remove launcher data?", text, "Remove", true)) return;
    startJob("Removing…");
    invoke("remove_data");
  });

  window.__TAURI__.event.listen("maintenance", ({ payload: e }) => {
    if (e.kind === "working") { sayM(e.detail); return; }
    jobRunning = false;
    if (e.kind === "done") sayM(e.detail);
    else sayM(e.message, true);
    loadMaintenance();
    window.rml.refresh();
  });

  // --- Settings -----------------------------------------------------------------------------------
  const note = $("settings-note");
  const NOTE = "Changes apply the next time Runehaven starts.";
  const size = $("size");

  function tell(text, isError = false) {
    note.textContent = text;
    note.style.color = isError ? "var(--red)" : "";
  }

  async function loadSettings() {
    try {
      const s = await invoke("get_settings");
      document.querySelectorAll(".toggle[data-key]").forEach(t => t.setAttribute("aria-checked", String(s[t.dataset.key])));
      const value = `${s.width}x${s.height}`;
      if (![...size.options].some(o => o.value === value)) size.add(new Option(`${s.width} × ${s.height}`, value));
      size.value = value;
      size.disabled = s.fullscreen;
      tell(NOTE);
    } catch (e) {
      tell(String(e), true);
    }
  }

  async function save(key, value) {
    try {
      await invoke("set_setting", { key, value });
      tell(NOTE);
    } catch (e) {
      tell(String(e), true);
      loadSettings();
    }
  }

  document.querySelectorAll(".toggle[data-key]").forEach(t => t.addEventListener("click", () => {
    const on = t.getAttribute("aria-checked") !== "true";
    t.setAttribute("aria-checked", String(on));
    if (t.dataset.key === "fullscreen") size.disabled = on;
    save(t.dataset.key, String(on));
  }));
  size.addEventListener("change", () => save("size", size.value));

  $("open-logs").addEventListener("click", () => invoke("open_logs").catch(e => tell(String(e), true)));
  $("export-logs").addEventListener("click", async () => {
    tell("Packing the logs…");
    try {
      const path = await invoke("export_logs");
      tell(`Saved ${path.split("/").pop()} on your desktop.`);
    } catch (e) {
      tell(String(e), true);
    }
  });
  $("settings-force").addEventListener("click", async () => {
    tell("Closing Runehaven…");
    try {
      await invoke("force_close_game");
      tell("Runehaven was closed.");
    } catch (e) {
      tell("Runehaven is not running.");
    }
  });

  // --- About ----------------------------------------------------------------------------------------
  window.__TAURI__.app.getVersion().then(v => { $("version").textContent = v; }).catch(() => {});
  document.addEventListener("click", e => {
    const link = e.target.closest("[data-link]");
    if (link) invoke("open_link", { which: link.dataset.link });
  });
})();
