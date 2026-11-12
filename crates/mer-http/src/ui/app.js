// Web panel for mirror-every-repo. No dependencies, nothing loaded from elsewhere.
//
// Everything shown on screen is inserted as text (textContent), never as
// HTML. Repository paths and audit details come from upstream archives and
// from users, so they must never be interpreted as markup.
"use strict";

const state = { me: null, tab: "overview", timer: null, flash: null };
const root = document.getElementById("app");

// ---------------------------------------------------------------- helpers

function h(tag, attrs, ...children) {
  const el = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs || {})) {
    if (v === null || v === undefined || v === false) continue;
    if (k === "class") el.className = v;
    else if (k.startsWith("on")) el.addEventListener(k.slice(2), v);
    else if (k === "disabled" || k === "checked" || k === "required" || k === "open") el[k] = true;
    else el.setAttribute(k, v);
  }
  for (const c of children.flat()) {
    if (c === null || c === undefined || c === false) continue;
    el.append(c instanceof Node ? c : document.createTextNode(String(c)));
  }
  return el;
}

function bytes(n) {
  if (n === null || n === undefined) return "";
  const units = ["B", "KiB", "MiB", "GiB", "TiB"];
  let v = Number(n), u = 0;
  while (v >= 1024 && u < units.length - 1) { v /= 1024; u++; }
  return u === 0 ? `${v} B` : `${v.toFixed(1)} ${units[u]}`;
}

function when(ms) {
  if (!ms) return "never";
  const d = new Date(ms);
  const pad = (x) => String(x).padStart(2, "0");
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())} ${pad(d.getHours())}:${pad(d.getMinutes())}`;
}

function count(n, one, many) {
  return `${n} ${n === 1 ? one : many}`;
}

async function api(path, body) {
  const opts = { credentials: "same-origin", headers: {} };
  if (body !== undefined) {
    opts.method = "POST";
    opts.headers["Content-Type"] = "application/json";
    if (state.me) opts.headers["X-CSRF-Token"] = state.me.csrf;
    opts.body = JSON.stringify(body);
  }
  const res = await fetch(`/api/${path}`, opts);
  let data = {};
  try { data = await res.json(); } catch (_) { /* empty body */ }
  if (res.status === 401 && path !== "login") {
    state.me = null;
    render();
    throw new Error("Your session has ended. Sign in again.");
  }
  if (!res.ok) throw new Error(data.error || `Request failed (${res.status})`);
  return data;
}

function can(role) {
  const order = { viewer: 0, operator: 1, admin: 2 };
  return state.me && order[state.me.role] >= order[role];
}

function flash(kind, text) {
  state.flash = { kind, text };
  render();
}

function badge(text, kind) {
  return h("span", { class: `badge ${kind || ""}` }, text);
}

function stateBadge(s) {
  const kinds = { approved: "good", sealed: "warn", rejected: "bad", ok: "good", imported: "good",
    incomplete: "warn", waiting_base: "warn", receiving: "warn", quota_blocked: "bad", diverged: "bad", failed: "bad" };
  const labels = { sealed: "awaiting review", waiting_base: "waiting for earlier bundle", quota_blocked: "over quota" };
  return badge(labels[s] || s, kinds[s]);
}

function table(headers, rows, empty) {
  if (!rows.length) return h("div", { class: "empty" }, empty || "Nothing here yet.");
  return h("div", { class: "scroll" },
    h("table", {},
      h("thead", {}, h("tr", {}, headers.map((x) => h("th", { class: x.num ? "num" : "" }, x.label || x)))),
      h("tbody", {}, rows)));
}

function td(content, cls) {
  return h("td", { class: cls || "" }, content);
}

// A confirmation dialog. Resolves true when the user confirms.
function confirmDialog(title, bodyNodes, confirmLabel, danger) {
  return new Promise((resolve) => {
    const dlg = h("dialog", {},
      h("div", { class: "dhead" }, h("h3", {}, title)),
      h("div", { class: "dbody" }, bodyNodes),
      h("div", { class: "dfoot" },
        h("button", { class: "btn", onclick: () => { dlg.close(); resolve(false); } }, confirmLabel ? "Cancel" : "Close"),
        confirmLabel && h("button", { class: `btn ${danger ? "danger" : "primary"}`, onclick: () => { dlg.close(); resolve(true); } }, confirmLabel)));
    dlg.addEventListener("close", () => { dlg.remove(); resolve(false); });
    document.body.append(dlg);
    dlg.showModal();
  });
}

// ------------------------------------------------------------------ login

function loginView(needsSetup) {
  const user = h("input", { name: "user", autocomplete: "username", required: true, autofocus: "" });
  const pass = h("input", { name: "password", type: "password", autocomplete: "current-password", required: true });
  const error = h("div", {});
  const form = h("form", { onsubmit: async (e) => {
    e.preventDefault();
    error.replaceChildren();
    try {
      state.me = await api("login", { user: user.value.trim(), password: pass.value });
      state.flash = null;
      render();
    } catch (err) {
      pass.value = "";
      error.replaceChildren(h("div", { class: "banner bad" }, err.message));
    }
  } },
    h("label", { class: "f" }, "User name", user),
    h("label", { class: "f" }, "Password", pass),
    h("button", { class: "btn primary", type: "submit" }, "Sign in"));
  return h("div", { class: "login" },
    h("div", { class: "card" },
      h("h2", {}, "Mirror Panel"),
      needsSetup && h("div", { class: "banner warn" },
        "No user exists yet. Create the first administrator on the server: ",
        h("code", {}, "mer user add admin --role admin")),
      error, form));
}

// --------------------------------------------------------------- overview

async function overviewView() {
  const o = await api("overview");
  const pending = o.repos.flatMap((r) => r.pending.map((seq) => ({ repo: r.name, seq })));
  const stuck = o.channels.flatMap((c) => c.repos.filter((r) => r.state !== "ok").map((r) => ({ ...r, channel: c.id })));
  const requests = Object.values(o.http.by_status).reduce((a, b) => a + b, 0);
  return h("div", {},
    h("h2", {}, "Overview"),
    !o.audit_intact && h("div", { class: "banner bad" }, "The audit log fails its integrity check. Open the Audit page for details."),
    h("div", { class: "tiles" },
      tile(o.repos.length, "repositories"),
      tile(bytes(o.store.bytes), `stored in ${count(o.store.blobs, "file", "files")}`),
      tile(pending.length, "snapshots awaiting review", pending.length ? "alert" : ""),
      tile(stuck.length, "repositories not up to date", stuck.length ? "danger" : ""),
      tile(o.wishes, "missing items requested"),
      tile(requests, `requests served, ${bytes(o.http.bytes_sent)} sent`)),
    pending.length > 0 && h("div", { class: "card" },
      h("div", { class: "head" }, h("h3", {}, "Awaiting review")),
      table(["Repository", "Snapshot", ""], pending.map((p) => h("tr", {},
        td(p.repo), td(`#${p.seq}`),
        td(h("div", { class: "actions" }, h("button", { class: "btn primary", onclick: () => reviewDialog(p.repo, p.seq) }, "Review changes"))))))),
    stuck.length > 0 && h("div", { class: "card" },
      h("div", { class: "head" }, h("h3", {}, "Needs attention")),
      table(["Repository", "State", "Why"], stuck.map((s) => h("tr", {}, td(s.repo), td(stateBadge(s.state)), td(s.detail))))),
    h("div", { class: "card" },
      h("div", { class: "head" }, h("h3", {}, "Repositories")),
      table(["Name", "Format", "Access", "Published", "Newest", { label: "Size", num: true }, "Quota"],
        o.repos.map((r) => h("tr", {},
          td(r.name), td(r.format),
          td(r.private ? badge("token required", "warn") : badge("open")),
          td(r.live ? badge(`snapshot #${r.live}`, "live") : badge("nothing published")),
          td(r.newest ? `#${r.newest}` : "none"),
          td(bytes(r.bytes), "num"),
          td(quotaBar(r)))), "No repositories yet.")));
}

function tile(n, label, cls) {
  return h("div", { class: `tile ${cls || ""}` }, h("div", { class: "n" }, n), h("div", { class: "l" }, label));
}

function quotaBar(r) {
  if (!r.quota_bytes) return h("span", { class: "muted" }, "no limit");
  const pct = Math.min(100, Math.round((r.bytes / r.quota_bytes) * 100));
  const fill = h("i", {});
  fill.style.width = `${pct}%`;
  return h("div", {}, h("div", { class: `bar ${pct >= 95 ? "over" : ""}` }, fill),
    h("span", { class: "muted" }, `${pct}% of ${bytes(r.quota_bytes)}`));
}

// ----------------------------------------------------------- repositories

async function reposView() {
  const o = await api("overview");
  const cards = [];
  for (const r of o.repos) {
    const s = await api(`repos/${encodeURIComponent(r.name)}/snapshots`);
    cards.push(repoCard(r, s));
  }
  return h("div", {}, h("h2", {}, "Repositories"),
    cards.length ? cards : h("div", { class: "card empty" }, "No repositories yet. They appear when the first bundle is imported."));
}

function repoCard(r, s) {
  const live = s.live;
  const rows = s.snapshots.map((snap) => {
    const acts = [h("button", { class: "btn", onclick: () => reviewDialog(r.name, snap.seq) }, "Changes")];
    if (can("operator")) {
      if (snap.state === "sealed") {
        acts.push(h("button", { class: "btn primary", onclick: () => decide(r.name, snap.seq, "approve") }, "Approve"));
        acts.push(h("button", { class: "btn danger", onclick: () => decide(r.name, snap.seq, "reject") }, "Reject"));
      }
      if (snap.state === "approved" && !snap.live) {
        if (!live || snap.seq > live) acts.push(h("button", { class: "btn primary", onclick: () => decide(r.name, snap.seq, "publish") }, "Publish"));
        else acts.push(h("button", { class: "btn danger", onclick: () => decide(r.name, snap.seq, "rollback") }, "Roll back to this"));
      }
    }
    return h("tr", {},
      td(`#${snap.seq}`), td(snap.live ? badge("live", "live") : stateBadge(snap.state)),
      td(snap.files, "num"), td(bytes(snap.bytes), "num"), td(when(snap.sealed_at), "nowrap"),
      td(h("div", { class: "actions" }, acts)));
  });
  return h("div", { class: "card" },
    h("div", { class: "head" },
      h("h3", {}, r.name), badge(r.format),
      r.private ? badge("token required", "warn") : badge("open"),
      h("span", { class: "muted" }, `${bytes(r.bytes)}${r.quota_bytes ? ` of ${bytes(r.quota_bytes)}` : ""}`),
      h("span", { class: "spacer" }),
      h("button", { class: "btn", onclick: () => clientSetupDialog(r) }, "Client setup"),
      can("admin") && h("button", { class: "btn", onclick: () => settingsDialog(r) }, "Settings")),
    table(["Snapshot", "State", { label: "Files", num: true }, { label: "Size", num: true }, "Sealed", ""], rows, "No snapshots yet."));
}

// How a client machine is pointed at this repository. The address shown is
// the one this page was loaded from; if package clients use another listener
// or name, substitute it.
function clientSetupDialog(r) {
  const origin = window.location.origin;
  const host = window.location.host;
  const secure = window.location.protocol === "https:";
  const base = `${origin}/${r.name}`;
  const blocks = [];
  const add = (title, lines) => blocks.push(h("h3", {}, title), h("pre", { class: "snippet" }, lines.join("\n")));
  if (r.format === "apt") {
    add("1. Trust the site key (once per client)", [
      `curl -o /etc/apt/keyrings/site.asc ${base}/keys/site.asc`,
      "# or copy the file to the client by hand",
    ]);
    add("2. Add the source (suite and components as mirrored)", [
      `deb [signed-by=/etc/apt/keyrings/site.asc] ${base} bookworm main`,
    ]);
    if (r.private) add("3. Token, in /etc/apt/auth.conf.d/vault.conf (mode 600)", [`machine ${origin} login apt password <token>`]);
  } else if (r.format === "pypi") {
    const trusted = secure ? "" : ` --trusted-host ${window.location.hostname}`;
    add("One install", [`pip install --index-url ${base}/simple/${trusted} <package>`]);
    add("Or for every install, in pip.conf", ["[global]", `index-url = ${base}/simple/`].concat(secure ? [] : [`trusted-host = ${window.location.hostname}`]));
    if (r.private) add("Token", [`index-url = ${window.location.protocol}//token:<token>@${host}/${r.name}/simple/`]);
  } else if (r.format === "npm") {
    add("Point npm at the vault", [`npm config set registry ${base}/`]);
    if (r.private) add("Token, in .npmrc", [`//${host}/${r.name}/:_authToken=<token>`]);
  } else if (r.format === "nuget") {
    const insecure = secure ? "" : " --allow-insecure-connections";
    add("Add the vault as a package source", [`dotnet nuget add source ${base}/v3/index.json --name vault${insecure}`]);
    add("Or in nuget.config, as the only source", [
      "<configuration>", "  <packageSources>", "    <clear />",
      `    <add key="vault" value="${base}/v3/index.json"${secure ? "" : ' allowInsecureConnections="true"'} />`,
      "  </packageSources>", "</configuration>",
    ]);
    if (r.private) add("Token", [`dotnet nuget add source ${base}/v3/index.json --name vault --username any --password <token> --store-password-in-clear-text${insecure}`]);
  } else if (r.format === "oci") {
    add("Pull an image", [`docker pull ${host}/${r.name}/<image>:<tag>`, `# for example: docker pull ${host}/${r.name}/library/alpine:3.20`]);
    if (!secure) add("Plain HTTP needs to be allowed on the client (or serve with TLS)", ['# /etc/docker/daemon.json', `{ "insecure-registries": ["${host}"] }`]);
    if (r.private) add("Token", [`docker login ${host}    # any user name, the token as password`]);
  } else {
    add("Download a file", [`curl -O ${base}/<path>`]);
    if (r.private) add("Token", [`curl -H "Authorization: Bearer <token>" -O ${base}/<path>`]);
  }
  if (r.private) blocks.push(h("p", { class: "muted" }, "This repository requires a token. An admin creates tokens on the Access page."));
  if (!r.live) blocks.unshift(h("div", { class: "banner warn" }, "Nothing is published in this repository yet, so clients will get 'not found' until a snapshot is published."));
  confirmDialog(`Client setup for ${r.name}`, blocks, null, false);
}

async function reviewDialog(repo, seq) {
  let c;
  try { c = await api(`repos/${encodeURIComponent(repo)}/snapshots/${seq}/changes?limit=1000`); }
  catch (err) { return flash("bad", err.message); }
  const symbol = { add: "+", change: "~", remove: "−" };
  const summary = c.total === 0
    ? "Identical to what is published now."
    : `${count(c.added, "file", "files")} added, ${c.changed} changed, ${c.removed} removed. ${bytes(c.bytes)} of new content.`;
  const body = [
    h("p", {}, c.base ? `Compared with the published snapshot #${c.base}: ` : "Nothing is published yet, so everything is new: ", summary),
    table(["", "Path", { label: "Size", num: true }], c.items.map((i) => h("tr", {},
      td(h("span", { class: `op-${i.op}`, title: i.op }, symbol[i.op])), td(i.path, "path"),
      td(i.op === "remove" ? "" : bytes(i.size), "num"))), "No differences."),
    c.total > c.items.length && h("p", { class: "muted" }, `Showing the first ${c.items.length} of ${c.total} changes.`),
  ];
  await confirmDialog(`${repo} snapshot #${seq}`, body, null, false);
}

async function decide(repo, seq, action) {
  const text = {
    approve: ["Approve snapshot", "Approving marks this snapshot as reviewed. It is not visible to clients until it is published.", "Approve", false],
    reject: ["Reject snapshot", "A rejected snapshot can never be published. This cannot be undone.", "Reject", true],
    publish: ["Publish snapshot", "Clients will see this snapshot immediately.", "Publish", false],
    rollback: ["Roll back", "Clients will immediately see this older snapshot instead of the current one.", "Roll back", true],
  }[action];
  if (!(await confirmDialog(`${text[0]} #${seq} of ${repo}`, [h("p", {}, text[1])], text[2], text[3]))) return;
  try {
    await api(`repos/${encodeURIComponent(repo)}/snapshots/${seq}/${action}`, {});
    flash("good", `${repo} #${seq}: ${action} done.`);
  } catch (err) { flash("bad", err.message); }
}

async function settingsDialog(r) {
  const priv = h("input", { type: "checkbox", checked: r.private });
  const quota = h("input", { type: "number", min: "0", step: "1", value: r.quota_bytes ? Math.round(r.quota_bytes / 1048576) : "" });
  const ok = await confirmDialog(`Settings for ${r.name}`, [
    h("p", {}, h("label", {}, priv, " Require an access token to download from this repository")),
    h("label", { class: "f" }, "Size limit in MiB (empty for no limit)", quota),
  ], "Save", false);
  if (!ok) return;
  const mib = Number(quota.value);
  try {
    await api(`repos/${encodeURIComponent(r.name)}/settings`, { private: priv.checked, quota_bytes: mib > 0 ? mib * 1048576 : null });
    flash("good", `Settings of ${r.name} saved.`);
  } catch (err) { flash("bad", err.message); }
}

// -------------------------------------------------------------- transfers

async function transfersView() {
  const o = await api("overview");
  if (!o.channels.length) return h("div", {}, h("h2", {}, "Transfers"), h("div", { class: "card empty" }, "No bundle has been sealed or imported yet."));
  return h("div", {}, h("h2", {}, "Transfers"), o.channels.map((c) => h("div", { class: "card" },
    h("div", { class: "head" }, h("h3", {}, "Channel "), h("code", {}, c.id), h("span", { class: "spacer" }),
      c.sealed_seq > 0 && badge(`sealed up to #${c.sealed_seq}`), c.imported_seq > 0 && badge(`imported up to #${c.imported_seq}`, "good")),
    c.repos.length > 0 && table(["Repository", "At bundle", "State", "Detail"], c.repos.map((r) => h("tr", {},
      td(r.repo), td(`#${r.applied_seq}`), td(stateBadge(r.state)), td(r.detail)))),
    h("h3", {}, "Bundles"),
    table(["Bundle", "State", "Id"], c.bundles.map((b) => h("tr", {},
      td(`#${b.seq}`), td(stateBadge(b.state)), td(b.id.slice(0, 16), "mono"))), "No bundles imported on this side."))));
}

// --------------------------------------------------------------- wishlist

async function wishlistView() {
  const rows = await api("wishlist");
  return h("div", {}, h("h2", {}, "Wishlist"),
    h("p", { class: "muted" }, "Files that clients asked for and this vault does not have. Use it to decide what to mirror next."),
    h("div", { class: "card" }, table(["Repository", "Path", { label: "Requests", num: true }, "Last asked"],
      rows.map((m) => h("tr", {}, td(m.repo), td(m.path, "path"), td(m.count, "num"), td(when(m.last_at), "nowrap"))),
      "No missing items have been requested.")));
}

// ---------------------------------------------------------------- traffic

async function trafficView() {
  const t = await api("traffic");
  const max = Math.max(1, ...t.repos.map((r) => r.bytes));
  return h("div", {}, h("h2", {}, "Traffic"),
    h("div", { class: "card" }, h("div", { class: "head" }, h("h3", {}, "By repository")),
      table(["Repository", { label: "Downloads", num: true }, { label: "Sent", num: true }, ""], t.repos.map((r) => {
        const fill = h("i", {});
        fill.style.width = `${Math.round((r.bytes / max) * 100)}%`;
        return h("tr", {}, td(r.repo), td(r.hits, "num"), td(bytes(r.bytes), "num"), td(h("div", { class: "bar" }, fill)));
      }), "Nothing has been downloaded yet.")),
    h("div", { class: "card" }, h("div", { class: "head" }, h("h3", {}, "Most downloaded files")),
      table(["Repository", "Path", { label: "Downloads", num: true }, { label: "Sent", num: true }, "Last"], t.top.map((r) => h("tr", {},
        td(r.repo), td(r.path, "path"), td(r.hits, "num"), td(bytes(r.bytes), "num"), td(when(r.last_at), "nowrap"))), "Nothing has been downloaded yet.")),
    h("div", { class: "card" }, h("div", { class: "head" }, h("h3", {}, "Clients since the server started")),
      table(["Address", { label: "Downloads", num: true }, { label: "Sent", num: true }, "Last seen"], t.clients.map((c) => h("tr", {},
        td(c.address, "mono"), td(c.hits, "num"), td(bytes(c.bytes), "num"), td(when(c.last_at), "nowrap"))), "No clients yet.")));
}

// ------------------------------------------------------------------ audit

async function auditView() {
  const a = await api("audit?limit=300");
  return h("div", {}, h("h2", {}, "Audit log"),
    a.intact
      ? h("div", { class: "banner good" }, `Integrity check passed: all ${a.entries} entries form an unbroken chain.`)
      : h("div", { class: "banner bad" }, `Integrity check FAILED at entry ${a.broken_at}. Entries from there on may have been altered or removed.`),
    h("div", { class: "card" }, table([{ label: "#", num: true }, "When", "Who", "Action", "Detail"], a.rows.map((r) => h("tr", {},
      td(r.seq, "num"), td(when(r.at), "nowrap"), td(r.actor), td(r.action, "mono"), td(r.detail, "path"))), "The audit log is empty.")));
}

// ----------------------------------------------------------------- access

async function accessView() {
  const parts = [h("h2", {}, "Access")];
  parts.push(passwordCard());
  if (can("admin")) {
    const [users, tokens, o] = await Promise.all([api("users"), api("tokens"), api("overview")]);
    parts.push(usersCard(users), tokensCard(tokens, o.repos.map((r) => r.name)));
  }
  return h("div", {}, parts);
}

function passwordCard() {
  const cur = h("input", { type: "password", autocomplete: "current-password", required: true });
  const next = h("input", { type: "password", autocomplete: "new-password", required: true, minlength: "10" });
  return h("div", { class: "card" }, h("div", { class: "head" }, h("h3", {}, "Change my password")),
    h("form", { class: "row", onsubmit: async (e) => {
      e.preventDefault();
      try {
        await api("me/password", { current: cur.value, new: next.value });
        state.me = null;
        flash("good", "Password changed. Sign in with the new password.");
      } catch (err) { flash("bad", err.message); }
    } },
      h("label", { class: "f" }, "Current password", cur),
      h("label", { class: "f" }, "New password (10 characters or more)", next),
      h("button", { class: "btn primary", type: "submit" }, "Change password")));
}

function usersCard(users) {
  const name = h("input", { required: true, pattern: "[a-z0-9._-]{1,64}" });
  const pass = h("input", { type: "password", required: true, minlength: "10", autocomplete: "new-password" });
  const role = h("select", {}, ["viewer", "operator", "admin"].map((r) => h("option", { value: r }, r)));
  const update = async (user, change, done) => {
    try { await api(`users/${encodeURIComponent(user)}`, change); flash("good", done); }
    catch (err) { flash("bad", err.message); }
  };
  return h("div", { class: "card" }, h("div", { class: "head" }, h("h3", {}, "Users")),
    h("p", { class: "muted" }, "Viewers can look. Operators can also approve, publish and roll back. Admins can also manage users, tokens and repository settings."),
    table(["Name", "Role", "Status", "Created", ""], users.map((u) => {
      const sel = h("select", { onchange: () => update(u.name, { role: sel.value }, `${u.name} is now ${sel.value}.`) },
        ["viewer", "operator", "admin"].map((r) => h("option", { value: r, selected: r === u.role ? "" : null }, r)));
      return h("tr", {}, td(u.name), td(sel), td(u.disabled ? badge("disabled", "bad") : badge("active", "good")), td(when(u.created_at), "nowrap"),
        td(h("div", { class: "actions" },
          h("button", { class: `btn ${u.disabled ? "" : "danger"}`, onclick: () => update(u.name, { disabled: !u.disabled }, `${u.name} ${u.disabled ? "enabled" : "disabled"}.`) }, u.disabled ? "Enable" : "Disable"))));
    })),
    h("h3", {}, "Add a user"),
    h("form", { class: "row", onsubmit: async (e) => {
      e.preventDefault();
      try { await api("users", { name: name.value.trim(), password: pass.value, role: role.value }); flash("good", `User ${name.value.trim()} created.`); }
      catch (err) { flash("bad", err.message); }
    } },
      h("label", { class: "f" }, "Name", name), h("label", { class: "f" }, "Password", pass), h("label", { class: "f" }, "Role", role),
      h("button", { class: "btn primary", type: "submit" }, "Add user")));
}

function tokensCard(tokens, repos) {
  const name = h("input", { required: true, pattern: "[a-z0-9._-]{1,64}" });
  const repo = h("select", {}, h("option", { value: "" }, "every repository"), repos.map((r) => h("option", { value: r }, r)));
  const shown = h("div", {});
  return h("div", { class: "card" }, h("div", { class: "head" }, h("h3", {}, "Client tokens")),
    h("p", { class: "muted" }, "Tokens let package clients download from repositories that require one. A client sends the token as its password."),
    table(["Name", "Repository", "Created by", "Last used", "Status", ""], tokens.map((t) => h("tr", {},
      td(t.name), td(t.repo || "every repository"), td(t.created_by), td(when(t.last_used_at), "nowrap"),
      td(t.revoked ? badge("revoked", "bad") : badge("active", "good")),
      td(h("div", { class: "actions" }, !t.revoked && h("button", { class: "btn danger", onclick: async () => {
        if (!(await confirmDialog(`Revoke token ${t.name}`, [h("p", {}, "Clients using this token will be refused immediately. This cannot be undone.")], "Revoke", true))) return;
        try { await api(`tokens/${encodeURIComponent(t.name)}/revoke`, {}); flash("good", `Token ${t.name} revoked.`); }
        catch (err) { flash("bad", err.message); }
      } }, "Revoke"))))), "No tokens yet."),
    h("h3", {}, "Create a token"),
    h("form", { class: "row", onsubmit: async (e) => {
      e.preventDefault();
      try {
        const out = await api("tokens", { name: name.value.trim(), repo: repo.value });
        shown.replaceChildren(h("div", { class: "banner warn" }, "Copy this token now. It is shown only once and cannot be recovered.",
          h("div", { class: "secret" }, out.secret)));
      } catch (err) { flash("bad", err.message); }
    } },
      h("label", { class: "f" }, "Name", name), h("label", { class: "f" }, "Valid for", repo),
      h("button", { class: "btn primary", type: "submit" }, "Create token")),
    shown);
}

// ------------------------------------------------------------------ shell

const TABS = [
  ["overview", "Overview", overviewView],
  ["repos", "Repositories", reposView],
  ["transfers", "Transfers", transfersView],
  ["wishlist", "Wishlist", wishlistView],
  ["traffic", "Traffic", trafficView],
  ["audit", "Audit log", auditView],
  ["access", "Access", accessView],
];

async function render() {
  clearTimeout(state.timer);
  if (!state.me) {
    let needsSetup = false;
    try { needsSetup = (await api("setup")).needs_setup; } catch (_) { /* shown as a normal login */ }
    const view = loginView(needsSetup);
    if (state.flash) view.prepend(h("div", { class: `banner ${state.flash.kind}` }, state.flash.text));
    root.replaceChildren(view);
    return;
  }
  const main = h("div", { class: "main" });
  root.replaceChildren(h("div", { class: "shell" },
    h("div", { class: "top" },
      h("span", { class: "brand" }, "Mirror Panel"),
      h("span", { class: "spacer" }),
      h("span", { class: "muted" }, `${state.me.user} (${state.me.role})`),
      h("button", { class: "btn", onclick: async () => {
        try { await api("logout", {}); } catch (_) { /* already signed out */ }
        state.me = null; state.flash = null; render();
      } }, "Sign out")),
    h("nav", { class: "nav" }, TABS.map(([id, label]) =>
      h("button", { class: state.tab === id ? "active" : "", onclick: () => { state.tab = id; state.flash = null; render(); } }, label))),
    main));
  if (state.flash) main.append(h("div", { class: `banner ${state.flash.kind}` }, state.flash.text));
  const tab = TABS.find((t) => t[0] === state.tab) || TABS[0];
  const loading = h("div", { class: "empty" }, "Loading…");
  main.append(loading);
  try {
    const view = await tab[2]();
    loading.replaceWith(view);
  } catch (err) {
    loading.remove();
    if (state.me) main.append(h("div", { class: "banner bad" }, err.message));
  }
  // Keep the read-only pages fresh without disturbing forms or dialogs.
  if (["overview", "transfers"].includes(state.tab)) {
    state.timer = setTimeout(() => { if (!document.querySelector("dialog[open]")) render(); }, 15000);
  }
}

(async () => {
  try { state.me = await api("me"); } catch (_) { state.me = null; }
  render();
})();
