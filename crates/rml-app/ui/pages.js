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
  const actions = ["m-update", "m-verify", "m-uninstall", "m-reinstall", "m-reset", "m-remove", "m-uninstall-launcher"].map($);
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
    // Only the launcher's own Wine setup is ever reset.
    $("m-reset").disabled = busy || !status.own_prefix || !status.prefix.installed || !status.runtime.installed;
    $("m-remove").disabled = busy || status.data_bytes === 0;
    $("m-uninstall-launcher").disabled = busy;
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
    setJob(true);
    sayM(text);
    renderMaintenance();
  }

  // Home's buttons wait while a job runs (app.js).
  function setJob(on) {
    jobRunning = on;
    window.rml.render();
  }
  window.rml.jobBusy = () => jobRunning;

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

  for (const [id, what] of [["m-saves", "saves"], ["m-backups", "backups"]]) {
    $(id).addEventListener("click", () => invoke("open_saves", { what }).then(() => sayM("")).catch(e => sayM(String(e), true)));
  }

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

  // --- uninstalling the launcher itself ------------------------------------------------------------
  const unDialog = $("uninstall-dialog"), unNote = $("un-note");
  let unRunning = false;
  const choice = name => unDialog.querySelector(`[data-choice=${name}]`);

  function setChoice(name, on, disabled) {
    choice(name).setAttribute("aria-checked", String(on));
    choice(name).disabled = disabled;
  }

  function openUninstall() {
    unNote.textContent = "";
    unNote.style.color = "";
    $("un-runtime").textContent = "WineCX, DXMT, libraries and downloads" + (status.runtime.bytes ? ` · ${fmtSize(status.runtime.bytes)}` : "");
    setChoice("runtime", true, false);
    if (status.own_prefix) {
      $("un-prefix").textContent = "Your save games are packed onto the desktop first" + (status.prefix.bytes ? ` · ${fmtSize(status.prefix.bytes)}` : "");
      setChoice("prefix", true, false);
    } else {
      $("un-prefix").textContent = "Your own Wine setup is never removed.";
      setChoice("prefix", false, true);
    }
    $("un-game").textContent = status.game.installed ? `Uninstalled through Steam · ${fmtSize(status.game.bytes)}` : "Not installed.";
    setChoice("game", false, !status.game.installed);
    $("un-ok").disabled = $("un-cancel").disabled = false;
    unDialog.hidden = false;
    $("un-cancel").focus();
  }

  unDialog.querySelectorAll("[data-choice]").forEach(t => t.addEventListener("click", () => {
    if (!t.disabled && !unRunning) t.setAttribute("aria-checked", String(t.getAttribute("aria-checked") !== "true"));
  }));
  $("un-cancel").addEventListener("click", () => { if (!unRunning) unDialog.hidden = true; });
  document.addEventListener("keydown", e => { if (e.key === "Escape" && !unDialog.hidden && !unRunning) unDialog.hidden = true; });
  $("m-uninstall-launcher").addEventListener("click", openUninstall);

  $("un-ok").addEventListener("click", () => {
    const on = name => choice(name).getAttribute("aria-checked") === "true";
    unRunning = true;
    setJob(true);
    $("un-ok").disabled = $("un-cancel").disabled = true;
    unNote.textContent = "Starting…";
    invoke("uninstall_launcher", { choice: { runtime: on("runtime"), prefix: on("prefix"), logs: on("logs") }, game: on("game") });
  });

  function uninstallProgress(e) {
    if (e.kind === "working") { unNote.textContent = e.detail; return; }
    if (e.kind === "done") {
      unNote.textContent = `${e.detail} The launcher closes now.`;
      setTimeout(() => invoke("quit_and_trash"), 3000);
      return;
    }
    unRunning = false;
    setJob(false);
    unNote.textContent = e.message;
    unNote.style.color = "var(--red)";
    $("un-ok").disabled = $("un-cancel").disabled = false;
  }

  // --- install locations (Home "Custom…" and Settings) --------------------------------------------
  const locDialog = $("locations-dialog"), locNote = $("loc-note"), libSelect = $("loc-library");
  let locs = null;

  function sayLoc(text, isError = false) {
    locNote.textContent = text;
    locNote.style.color = isError ? "var(--red)" : "";
  }

  function updateMove() {
    const here = locs && locs.libraries.find(l => l.has_game);
    $("loc-move").hidden = !here || String(here.index) === libSelect.value;
  }

  async function loadLocations() {
    try {
      locs = await invoke("install_locations");
    } catch (e) {
      sayLoc(String(e), true);
      return;
    }
    libSelect.innerHTML = "";
    if (!locs.libraries.length) libSelect.add(new Option("Steam is not installed", ""));
    for (const l of locs.libraries) {
      const free = l.free_bytes == null ? "" : ` · ${fmtSize(l.free_bytes)} free`;
      const label = `${l.index === 0 ? "Main library" : l.path} · ${l.drive}${free}${l.has_game ? " · Runehaven is here" : ""}`;
      libSelect.add(new Option(label, String(l.index)));
    }
    const here = locs.libraries.find(l => l.has_game);
    libSelect.value = String(locs.chosen_library ?? (here ? here.index : 0));
    updateMove();
    for (const kind of ["runtime", "prefix"]) {
      locDialog.querySelectorAll(`[data-kind=${kind}] [data-mode]`)
        .forEach(b => b.setAttribute("aria-pressed", String(b.dataset.mode === locs[kind].mode)));
      $(`loc-${kind}`).textContent = locs[kind].path;
    }
  }

  function openLocations() {
    sayLoc("");
    locDialog.hidden = false;
    loadLocations();
    $("loc-done").focus();
  }
  function closeLocations() {
    locDialog.hidden = true;
    window.rml.refresh();
  }
  window.rml.openLocations = openLocations;
  $("open-locations").addEventListener("click", openLocations);
  $("loc-done").addEventListener("click", closeLocations);
  document.addEventListener("keydown", e => { if (e.key === "Escape" && !locDialog.hidden && confirmDialog.hidden) closeLocations(); });
  window.addEventListener("focus", () => { if (!locDialog.hidden) loadLocations(); });

  libSelect.addEventListener("change", async () => {
    updateMove();
    try {
      sayLoc(await invoke("set_install_location", { kind: "library", mode: "chosen", value: libSelect.value }));
    } catch (e) {
      sayLoc(String(e), true);
    }
  });

  $("loc-move").addEventListener("click", async () => {
    if (!await ask("Move Runehaven?", "Steam moves the game into the chosen library and checks the files afterwards.", "Move")) return;
    sayLoc("Starting Steam…");
    $("loc-move").disabled = true;
    setJob(true);
    invoke("move_game", { library: Number(libSelect.value) });
  });

  $("loc-add-library").addEventListener("click", () => {
    invoke("open_steam_settings");
    sayLoc("Add the library in Steam's Storage settings, then come back here.");
  });

  locDialog.querySelectorAll("[data-kind] [data-mode]").forEach(b => b.addEventListener("click", async () => {
    const kind = b.closest("[data-kind]").dataset.kind, mode = b.dataset.mode;
    let value = null;
    if (mode !== "default") {
      value = await invoke("pick_path", { what: kind === "runtime" && mode === "existing" ? "wine" : "folder" });
      if (!value) return;
    }
    try {
      sayLoc(await invoke("set_install_location", { kind, mode, value }));
      await loadLocations();
    } catch (e) {
      sayLoc(String(e), true);
    }
  }));

  function moveProgress(e) {
    if (e.kind === "working") { sayLoc(e.detail); return; }
    $("loc-move").disabled = false;
    setJob(false);
    if (e.kind === "done") sayLoc(e.detail); else sayLoc(e.message, true);
    loadLocations();
  }

  window.__TAURI__.event.listen("maintenance", ({ payload: e }) => {
    if (e.job === "uninstall-launcher") { uninstallProgress(e); return; }
    if (e.job === "move") { moveProgress(e); return; }
    if (e.job === "restore") { if (e.kind !== "working") loadMaintenance(); return; }
    if (e.kind === "working") { sayM(e.detail); return; }
    setJob(false);
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
  invoke("launcher_update").then(found => {
    if (!found) return;
    $("home-update").textContent = `Version ${found.version} of the launcher is available.`;
    $("about-new").textContent = found.version;
    $("home-update").hidden = $("about-update").hidden = false;
  }).catch(() => {});
  document.addEventListener("click", e => {
    const link = e.target.closest("[data-link]");
    if (link) invoke("open_link", { which: link.dataset.link });
  });
})();
