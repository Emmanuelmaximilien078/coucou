// Settings window — the place where anything that writes to disk is confirmed.
// Stage 2 covers the Claude Code hooks and the general preferences; API keys and
// integrations land here too in a later stage.

import "./settings.css";
import { Bridge, onEvent, type HookStatus } from "../core/bridge";
import { DEFAULT_SETTINGS, type Settings } from "../core/state";
import { h, clear } from "../views/dom";

let settings: Settings = { ...DEFAULT_SETTINGS };
let version = "";

const root = document.getElementById("settings-root")!;

async function save() {
  await Bridge.saveSettings(settings);
}

// ── Reusable bits ─────────────────────────────────────────────────────────────

function toggle(on: boolean, onChange: (v: boolean) => void): HTMLElement {
  const el = h("button", { class: on ? "switch on" : "switch", "aria-pressed": on });
  el.addEventListener("click", () => {
    const next = !el.classList.contains("on");
    el.classList.toggle("on", next);
    onChange(next);
  });
  return el;
}

function statusDot(ok: boolean): HTMLElement {
  return h("i", { class: "dot", style: `background:${ok ? "#22c55e" : "#f4505e"}` });
}

function renderDiff(text: string): HTMLElement {
  const box = h("div", { class: "diff" });
  for (const line of text.split("\n")) {
    const cls = line.startsWith("+") ? "add" : line.startsWith("-") ? "del" : "ctx";
    box.append(h("div", { class: cls, text: line }));
  }
  return box;
}

// ── Claude Code section ───────────────────────────────────────────────────────

function claudeSection(status: HookStatus): HTMLElement {
  const body = h("div", { style: "display:flex;flex-direction:column;gap:12px" });
  const section = h(
    "section",
    {},
    h("h2", {}, statusDot(status.installed), h("span", { text: "Claude Code" })),
    body,
  );

  const rebuild = async () => {
    const fresh = await Bridge.hooksStatus();
    if (fresh) Object.assign(status, fresh);
    clear(body);
    draw();
    const head = section.querySelector("h2")!;
    clear(head);
    head.append(statusDot(status.installed), h("span", { text: "Claude Code" }));
  };

  function draw() {
    body.append(
      h("div", {
        class: "hint",
        text: status.installed
          ? "Coucou is hooked into your Claude Code sessions. Tool calls, questions and permission requests show up in the island, and you can answer them there."
          : "Install the hooks to see your Claude Code sessions in the island and approve permissions without leaving what you are doing.",
      }),
      h("div", { class: "row" },
        h("label", { text: "settings.json" }),
        h("span", { class: "path", text: status.settingsPath }),
      ),
      h("div", { class: "row" },
        h("label", { text: "Relay" }),
        h("span", { class: "path", text: status.hookPath }),
        statusDot(status.hookReady),
      ),
    );

    if (!status.hookReady) {
      body.append(h("div", {
        class: "notice warn",
        text: "coucou-hook.exe is not in place yet. Restart Coucou; if it still fails, build it with `cargo build -p coucou-hook`.",
      }));
    }

    const actions = h("div", { class: "row" });
    const install = h("button", {
      class: "primary",
      text: status.installed ? "Reinstall hooks…" : "Install hooks…",
      onclick: () => showPreview(true),
    });
    // Writing hook commands that point at a relay which isn't there would give
    // every Claude Code session a broken hook and nothing to show for it.
    if (!status.hookReady) {
      install.disabled = true;
      install.title = "The relay isn't installed yet.";
    }
    actions.append(install);
    if (status.installed) {
      actions.append(h("button", {
        class: "danger",
        text: "Uninstall hooks…",
        onclick: () => showPreview(false),
      }));
    }
    body.append(actions);
  }

  async function showPreview(install: boolean) {
    let preview;
    try {
      preview = await Bridge.hooksPreview(install);
    } catch (err) {
      // An unreadable or invalid settings.json stops here rather than being
      // treated as empty and written over.
      clear(body);
      body.append(
        h("div", { class: "notice err", text: String(err).replace(/^Error:\s*/, "") }),
        h("div", { class: "row" }, h("button", {
          text: "Back",
          onclick: () => { clear(body); draw(); },
        })),
      );
      return;
    }
    if (!preview) return;
    clear(body);
    body.append(
      h("div", {
        class: "hint",
        text: install
          ? "This is exactly what will change in your settings.json. Your own hooks are left untouched."
          : "This removes Coucou's entries only. Your own hooks are left untouched.",
      }),
      renderDiff(preview.diff),
      h("div", { class: "row" },
        h("span", { class: "path", text: `Backup → ${preview.backup}` }),
      ),
    );
    const confirm = h("button", {
      class: install ? "primary" : "danger",
      text: install ? "Back up and write" : "Back up and remove",
    });
    confirm.addEventListener("click", async () => {
      confirm.disabled = true;
      try {
        const backup = await Bridge.hooksApply(install, preview.fingerprint);
        clear(body);
        body.append(h("div", {
          class: "notice ok",
          text: `Done. Previous settings saved as ${backup}. Open a new Claude Code session to pick the hooks up.`,
        }));
        window.setTimeout(() => void rebuild(), 2600);
      } catch (err) {
        confirm.disabled = false;
        body.append(h("div", { class: "notice err", text: `Could not write: ${String(err)}` }));
      }
    });
    body.append(h("div", { class: "row" }, confirm, h("button", {
      text: "Cancel",
      onclick: () => { clear(body); draw(); },
    })));
  }

  draw();
  return section;
}

// ── Chat section ──────────────────────────────────────────────────────────────

interface ChatProviderDef {
  id: string;
  label: string;
  /** Credential Manager key for this provider's API key. */
  keyName: string;
  /** Placeholder for an unstored key. */
  placeholder: string;
  /** Known model ids; anything else goes through "Custom…". */
  models: [string, string][];
  /** The custom provider points at any OpenAI-compatible base URL. */
  custom?: boolean;
}

const CHAT_PROVIDERS: ChatProviderDef[] = [
  {
    id: "anthropic", label: "Claude (Anthropic)", keyName: "anthropic-api-key", placeholder: "sk-ant-...",
    models: [["claude-opus-5", "Claude Opus 5"], ["claude-sonnet-5", "Claude Sonnet 5"], ["claude-haiku-4-5", "Claude Haiku 4.5"]],
  },
  {
    id: "deepseek", label: "DeepSeek", keyName: "deepseek-api-key", placeholder: "sk-...",
    models: [["deepseek-chat", "DeepSeek Chat"], ["deepseek-reasoner", "DeepSeek Reasoner"], ["deepseek-flash", "DeepSeek Flash"]],
  },
  {
    id: "zhipu", label: "GLM (Zhipu)", keyName: "zhipu-api-key", placeholder: "Zhipu API key…",
    models: [["glm-5.3-flash", "GLM-5.3-Flash"], ["glm-5.3-flashx", "GLM-5.3-FlashX (200 tok/s)"], ["glm-4.6", "GLM-4.6"], ["glm-4.5-flash", "GLM-4.5-Flash (free)"]],
  },
  {
    id: "openrouter", label: "OpenRouter", keyName: "openrouter-api-key", placeholder: "sk-or-...",
    models: [["z-ai/glm-5.3-flash", "GLM-5.3-Flash"], ["deepseek/deepseek-chat", "DeepSeek Chat"], ["anthropic/claude-sonnet-4.5", "Claude Sonnet 4.5"], ["openai/gpt-5-mini", "GPT-5 Mini"]],
  },
  {
    id: "openai", label: "OpenAI", keyName: "openai-api-key", placeholder: "sk-...",
    models: [["gpt-5", "GPT-5"], ["gpt-5-mini", "GPT-5 Mini"], ["gpt-4o", "GPT-4o"]],
  },
  {
    id: "custom", label: "Custom (OpenAI-compatible)", keyName: "custom-api-key", placeholder: "API key (optional)", custom: true,
    models: [],
  },
];

function chatSection(): HTMLElement {
  const section = h("section", {}, h("h2", {}, statusDot(false), h("span", { text: "Chat" })));
  const dot = section.querySelector("i") as HTMLElement;
  const body = h("div", { style: "display:flex;flex-direction:column;gap:12px" });
  section.append(body);

  const def = () => CHAT_PROVIDERS.find((p) => p.id === settings.chatProvider) ?? CHAT_PROVIDERS[0];

  async function draw() {
    clear(body);
    const d = def();
    const present = (await Bridge.secretPresent(d.keyName)) ?? false;
    dot.style.background = present ? "#22c55e" : "#f4505e";

    const provider = h("select", {}) as HTMLSelectElement;
    for (const p of CHAT_PROVIDERS) provider.append(h("option", { value: p.id, text: p.label }));
    provider.value = d.id;
    provider.addEventListener("change", () => {
      settings.chatProvider = provider.value;
      const nd = def();
      // A model id only makes sense for the provider it came from.
      if (!nd.custom && !nd.models.some(([id]) => id === settings.model)) {
        settings.model = nd.models[0]?.[0] ?? "";
      }
      void save();
      void draw();
    });

    body.append(
      h("div", {
        class: "hint",
        text: "Who answers in the chat. Each provider keeps its own key in the Windows Credential Manager, never on disk.",
      }),
      h("div", { class: "row" }, h("label", { text: "Provider" }), provider),
    );

    // The key — storage, placeholder and status dot follow the provider.
    const field = h("input", {
      type: "password",
      placeholder: present ? "••••••••••••  (stored)" : d.placeholder,
      style: "flex:1 1 auto;min-width:0",
      autocomplete: "off",
      spellcheck: "false",
    }) as HTMLInputElement;
    const saveBtn = h("button", { class: "primary", text: "Save key" });
    const clearBtn = h("button", { class: "danger", text: "Remove" });
    const feedback = h("div", {});
    clearBtn.style.display = present ? "" : "none";

    saveBtn.addEventListener("click", async () => {
      const value = field.value.trim();
      if (!value) return;
      clear(feedback);
      try {
        await Bridge.secretSet(d.keyName, value);
        field.value = "";
        feedback.append(h("div", { class: "notice ok", text: "Saved. It never touches disk." }));
        await draw();
      } catch (err) {
        feedback.append(h("div", { class: "notice err", text: `Could not save: ${String(err)}` }));
      }
    });

    clearBtn.addEventListener("click", async () => {
      clear(feedback);
      try {
        await Bridge.secretClear(d.keyName);
        feedback.append(h("div", { class: "notice ok", text: "Key removed." }));
        await draw();
      } catch (err) {
        feedback.append(h("div", { class: "notice err", text: `Could not remove: ${String(err)}` }));
      }
    });

    body.append(h("div", { class: "row" }, h("label", { text: "API key" }), field, saveBtn, clearBtn));

    if (d.custom) {
      const base = h("input", {
        type: "text",
        value: settings.customBaseUrl,
        placeholder: "https://host/v1 — Ollama: http://localhost:11434/v1",
        style: "flex:1 1 auto;min-width:0",
        autocomplete: "off",
        spellcheck: "false",
      }) as HTMLInputElement;
      base.addEventListener("change", () => {
        settings.customBaseUrl = base.value.trim();
        void save();
      });
      body.append(h("div", { class: "row" }, h("label", { text: "Base URL" }), base));
    }

    // The model — a list per provider, with Custom… for any other id.
    if (d.custom) {
      const model = h("input", {
        type: "text",
        value: settings.model,
        placeholder: "model id, e.g. llama3.2",
        style: "flex:1 1 auto;min-width:0",
        autocomplete: "off",
        spellcheck: "false",
      }) as HTMLInputElement;
      model.addEventListener("change", () => {
        settings.model = model.value.trim();
        void save();
      });
      body.append(h("div", { class: "row" }, h("label", { text: "Model" }), model));
    } else {
      const model = h("select", {}) as HTMLSelectElement;
      for (const [id, label] of d.models) model.append(h("option", { value: id, text: label }));
      model.append(h("option", { value: "__custom__", text: "Custom…" }));
      const custom = h("input", {
        type: "text",
        placeholder: "model id",
        style: "flex:1 1 auto;min-width:0",
        autocomplete: "off",
        spellcheck: "false",
      }) as HTMLInputElement;
      if (d.models.some(([id]) => id === settings.model)) {
        model.value = settings.model;
        custom.style.display = "none";
      } else {
        model.value = "__custom__";
        custom.value = settings.model;
      }
      model.addEventListener("change", () => {
        if (model.value === "__custom__") {
          custom.style.display = "";
          custom.focus();
          return;
        }
        custom.style.display = "none";
        settings.model = model.value;
        void save();
      });
      custom.addEventListener("change", () => {
        settings.model = custom.value.trim();
        void save();
      });
      body.append(h("div", { class: "row" }, h("label", { text: "Model" }), model, custom));
    }

    body.append(feedback);
  }

  void draw();
  return section;
}

// ── Integrations section ──────────────────────────────────────────────────────

interface IntegrationDef {
  id: string;
  name: string;
  color: string;
  /** Credential Manager keys, in the order they are shown. */
  fields: { key: string; label: string; placeholder: string; secret: boolean }[];
}

const INTEGRATIONS: IntegrationDef[] = [
  { id: "integration_stripe", name: "Stripe", color: "#0570DE",
    fields: [{ key: "stripe-api-key", label: "Secret key", placeholder: "sk_live_…", secret: true }] },
  { id: "integration_github", name: "GitHub", color: "#F4505E",
    fields: [{ key: "github-token", label: "Token", placeholder: "ghp_…", secret: true }] },
  { id: "integration_vercel", name: "Vercel", color: "#7C5CFF",
    fields: [{ key: "vercel-token", label: "Token", placeholder: "…", secret: true }] },
  { id: "integration_n8n", name: "n8n", color: "#F29B38",
    fields: [
      { key: "n8n-url", label: "Instance URL", placeholder: "https://n8n.example.com", secret: false },
      { key: "n8n-api-key", label: "API key", placeholder: "…", secret: true },
    ] },
  { id: "integration_resend", name: "Resend", color: "#22C55E",
    fields: [{ key: "resend-api-key", label: "API key", placeholder: "re_…", secret: true }] },
  { id: "integration_notion", name: "Notion", color: "#8C8C8C",
    fields: [{ key: "notion-api-key", label: "Integration token", placeholder: "ntn_…", secret: true }] },
  { id: "integration_calcom", name: "Cal.com", color: "#C9956A",
    fields: [{ key: "calcom-api-key", label: "API key", placeholder: "cal_…", secret: true }] },
];

const MAX_ACTIVE = 4;

function integrationsSection(present: Record<string, boolean>): HTMLElement {
  const note = h("div", { class: "hint" });
  const list = h("div", { style: "display:flex;flex-direction:column;gap:14px" });

  function updateNote() {
    const used = settings.activeIntegrations.length;
    note.textContent = `Pick up to ${MAX_ACTIVE} pills to show next to Mochi — ${used}/${MAX_ACTIVE} in use. Keys are stored in the Windows Credential Manager, never on disk.`;
  }

  for (const def of INTEGRATIONS) {
    const active = settings.activeIntegrations.includes(def.id);
    const sw = h("button", { class: active ? "switch on" : "switch" });
    sw.addEventListener("click", () => {
      const on = settings.activeIntegrations.includes(def.id);
      if (on) {
        settings.activeIntegrations = settings.activeIntegrations.filter((x) => x !== def.id);
      } else {
        if (settings.activeIntegrations.length >= MAX_ACTIVE) return;
        settings.activeIntegrations = [...settings.activeIntegrations, def.id];
      }
      sw.classList.toggle("on", !on);
      updateNote();
      void save();
    });

    const rows = h("div", { style: "display:flex;flex-direction:column;gap:6px;flex:1 1 auto;min-width:0" });
    for (const field of def.fields) {
      const input = h("input", {
        type: field.secret ? "password" : "text",
        placeholder: present[field.key] ? "••••••••  (stored)" : field.placeholder,
        autocomplete: "off",
        spellcheck: "false",
        style: "flex:1 1 auto;min-width:0",
      }) as HTMLInputElement;
      const saveBtn = h("button", { text: "Save" });
      const dotEl = statusDot(present[field.key] ?? false);
      saveBtn.addEventListener("click", async () => {
        const value = input.value.trim();
        try {
          await Bridge.secretSet(field.key, value);
          present[field.key] = value.length > 0;
          input.value = "";
          input.placeholder = value ? "••••••••  (stored)" : field.placeholder;
          dotEl.style.background = value ? "#22c55e" : "#f4505e";
        } catch {
          dotEl.style.background = "#f5a524";
        }
      });
      rows.append(
        h("div", { class: "row" },
          h("label", { style: "min-width:104px", text: field.label }),
          input, saveBtn, dotEl,
        ),
      );
    }

    list.append(
      h("div", { style: "display:flex;gap:12px;align-items:flex-start" },
        h("div", { style: "display:flex;align-items:center;gap:8px;min-width:132px;padding-top:4px" },
          sw,
          h("i", { class: "dot", style: `background:${def.color}` }),
          h("span", { style: "font-size:12.5px", text: def.name }),
        ),
        rows,
      ),
    );
  }

  updateNote();
  return h("section", {}, h("h2", {}, h("span", { text: "Integrations" })), note, list);
}

// ── General section ───────────────────────────────────────────────────────────

function generalSection(): HTMLElement {
  const volume = h("input", {
    type: "range", min: "0", max: "0.2", step: "0.005",
    value: String(settings.soundVolume),
  }) as HTMLInputElement;
  volume.addEventListener("input", () => {
    settings.soundVolume = Number(volume.value);
    void save();
  });

  const autoClose = h("input", {
    type: "number", min: "5", max: "120", step: "1",
    value: String(Math.round(settings.autoCloseInterval)),
    style: "width:72px",
  }) as HTMLInputElement;
  autoClose.addEventListener("change", () => {
    settings.autoCloseInterval = Math.max(5, Math.min(120, Number(autoClose.value) || 15));
    autoClose.value = String(settings.autoCloseInterval);
    void save();
  });

  const screen = h("select", {}) as HTMLSelectElement;
  screen.append(
    h("option", { value: "primary", text: "Main display" }),
    h("option", { value: "cursor", text: "Display under the cursor" }),
  );
  screen.value = settings.screen;
  screen.addEventListener("change", () => {
    settings.screen = screen.value as Settings["screen"];
    void save();
  });

  return h(
    "section",
    {},
    h("h2", {}, h("span", { text: "General" })),
    h("div", { class: "row" },
      h("label", { text: "Sound" }),
      toggle(settings.soundEnabled, (v) => { settings.soundEnabled = v; void save(); }),
      volume,
    ),
    h("div", { class: "row" },
      h("label", { text: "Auto-close" }),
      autoClose,
      h("span", { class: "hint", text: "seconds after you leave the island" }),
    ),
    h("div", { class: "row" },
      h("label", { text: "Island lives on" }),
      screen,
    ),
    h("div", { class: "row" },
      h("label", { text: "Launch at startup" }),
      toggle(settings.autostart, (v) => { settings.autostart = v; void save(); }),
    ),
  );
}

// ── Boot ──────────────────────────────────────────────────────────────────────

async function main() {
  const boot = await Bridge.boot();
  if (boot) {
    settings = { ...settings, ...boot.settings };
    version = boot.version;
  }
  const status = (await Bridge.hooksStatus()) ?? {
    installed: false, settingsPath: "", hookPath: "", hookReady: false,
  };

  const keys = [
    "stripe-api-key", "github-token", "vercel-token",
    "n8n-url", "n8n-api-key", "resend-api-key", "notion-api-key", "calcom-api-key",
  ];
  const present: Record<string, boolean> = {};
  for (const k of keys) present[k] = (await Bridge.secretPresent(k)) ?? false;

  clear(root);
  root.append(
    h("h1", {}, h("span", { text: "Coucou" }), h("span", { class: "version", text: version })),
    claudeSection(status),
    chatSection(),
    integrationsSection(present),
    generalSection(),
    h("div", {
      class: "hint",
      text: "No telemetry. Network requests only go to the services you configure yourself.",
    }),
  );

  void onEvent<Settings>("settings-changed", (s) => {
    settings = { ...settings, ...s };
  });
}

void main();
