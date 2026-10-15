// SPDX-License-Identifier: MPL-2.0
// The serveurs & onglets screen.
//
// The page holds one secret: the KIOSK token, from `?token=`. The remotes'
// tokens are set here and kept by the Kiosk; they are never read back, so a
// script on this origin cannot walk off with the fleet's credentials.

const TOKEN = new URLSearchParams(location.search).get("token") || "";
const authHeaders = () => (TOKEN ? { Authorization: `Bearer ${TOKEN}` } : {});

const $ = (id) => document.getElementById(id);

/** Says what just happened, in the topbar. */
function status(message, kind = "") {
  const el = $("status");
  el.textContent = message;
  el.className = `status ${kind}`;
}

/** One call to the Kiosk's own API, with the pane token. */
async function api(path, { method = "GET", body } = {}) {
  const response = await fetch(path, {
    method,
    headers: { ...authHeaders(), ...(body ? { "content-type": "application/json" } : {}) },
    body: body ? JSON.stringify(body) : undefined,
  });
  const text = await response.text();
  const data = text ? JSON.parse(text) : {};
  if (!response.ok) {
    throw new Error(data.error || `HTTP ${response.status}`);
  }
  return data;
}

let selected = null; // the selected remote id

/** The servers, each with a select and a forget button. */
async function loadRemotes() {
  const { remotes } = await api("/kiosk/remotes");
  const list = $("remotes");
  list.replaceChildren();
  if (remotes.length === 0) {
    const li = document.createElement("li");
    li.className = "empty";
    li.textContent = "Aucun serveur. Ajoute-en un ci-dessous.";
    list.append(li);
    $("tabs-panel").hidden = true;
    return;
  }
  for (const remote of remotes) {
    const li = document.createElement("li");

    const label = document.createElement("span");
    label.className = "label";
    label.textContent = remote.label;

    const url = document.createElement("span");
    url.className = "url";
    url.textContent = remote.url;

    const token = document.createElement("span");
    token.className = `badge${remote.has_token ? " ok" : ""}`;
    token.textContent = remote.has_token ? "jeton" : "sans jeton";

    const select = document.createElement("button");
    select.className = "link spacer";
    select.textContent = selected === remote.id ? "sélectionné" : "onglets";
    select.disabled = selected === remote.id;
    select.addEventListener("click", () => {
      selected = remote.id;
      loadRemotes().catch((err) => status(err.message, "err"));
      loadTabs();
    });

    const forget = document.createElement("button");
    forget.className = "link";
    forget.textContent = "supprimer";
    forget.addEventListener("click", async () => {
      try {
        await api(`/kiosk/remotes/${encodeURIComponent(remote.id)}/remove`, { method: "POST" });
        if (selected === remote.id) selected = null;
        status(`supprimé : ${remote.label}`, "ok");
        await loadRemotes();
      } catch (err) {
        status(err.message, "err");
      }
    });

    li.append(label, url, token, select, forget);
    list.append(li);
  }
}

/** The selected remote's tabs, with a form to open one. */
async function loadTabs() {
  if (!selected) {
    $("tabs-panel").hidden = true;
    return;
  }
  $("tabs-panel").hidden = false;
  try {
    const { tabs } = await api(`/kiosk/remotes/${encodeURIComponent(selected)}/tabs`);
    const list = $("tabs");
    list.replaceChildren();
    if (!tabs || tabs.length === 0) {
      const li = document.createElement("li");
      li.className = "empty";
      li.textContent = "Aucun onglet sur ce serveur.";
      list.append(li);
    } else {
      for (const tab of tabs) {
        const li = document.createElement("li");
        const name = document.createElement("span");
        name.className = "name";
        name.textContent = tab.name || tab.id || tab.key || "(sans nom)";
        const meta = document.createElement("span");
        meta.className = "meta";
        meta.textContent = [tab.cwd, tab.state].filter(Boolean).join(" · ");
        li.append(name, meta);
        list.append(li);
      }
    }
  } catch (err) {
    // A remote that is down is named as such, not hidden.
    const list = $("tabs");
    list.replaceChildren();
    const li = document.createElement("li");
    li.className = "empty";
    li.textContent = `Serveur injoignable : ${err.message}`;
    list.append(li);
  }
}

$("add-remote").addEventListener("submit", async (event) => {
  event.preventDefault();
  const form = event.target;
  const field = (name) => form.elements[name].value;
  try {
    const { remote } = await api("/kiosk/remotes", {
      method: "POST",
      body: { label: field("label"), url: field("url"), token: field("token") },
    });
    form.reset();
    status(`ajouté : ${remote.label}`, "ok");
    await loadRemotes();
  } catch (err) {
    status(err.message, "err");
  }
});

$("add-tab").addEventListener("submit", async (event) => {
  event.preventDefault();
  if (!selected) return;
  const form = event.target;
  const cwd = form.elements["cwd"].value.trim();
  try {
    // `sync` makes the daemon wait and hand back the new tab's uuid, so the
    // click reports the tab that was made rather than only that one was asked
    // for. `cwd` is sent only when given, or the tab inherits the active one.
    const body = { sync: true };
    if (cwd) body.cwd = cwd;
    const created = await api(`/kiosk/remotes/${encodeURIComponent(selected)}/tabs`, {
      method: "POST",
      body,
    });
    form.reset();
    status(created.id ? `onglet ${created.id.slice(0, 8)} créé` : "onglet créé", "ok");
    await loadTabs();
  } catch (err) {
    status(err.message, "err");
  }
});

// Show the selected remote's name in the tabs panel header.
const nameWatcher = () => {
  const header = $("remote-name");
  header.textContent = selected ? "" : "(choisis un serveur)";
};
nameWatcher();

loadRemotes().catch((err) => status(err.message, "err"));
