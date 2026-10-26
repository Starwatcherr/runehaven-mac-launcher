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
  }
  navButtons.forEach(b => b.addEventListener("click", () => show(b.dataset.page)));

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
