// Web panel for mirror-every-repo. No dependencies, nothing loaded from elsewhere.
//
// Everything shown on screen is inserted as text (textContent), never as
// HTML. Repository paths and audit details come from upstream archives and
// from users, so they must never be interpreted as markup.
"use strict";

const state = { me: null, tab: "overview", timer: null, flash: null, range: "7d" };
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

// The same in UTC, where a schedule's rule is in UTC.
function whenUtc(ms) {
  if (!ms) return "never";
  return `${new Date(ms).toISOString().slice(0, 16).replace("T", " ")} UTC`;
}

// "3 days ago", for how old the content of a repository is.
function ago(ms) {
  if (!ms) return "never";
  const hours = Math.floor((Date.now() - ms) / 3600000);
  if (hours < 1) return "less than an hour ago";
  if (hours < 48) return `${count(hours, "hour", "hours")} ago`;
  return `${Math.floor(hours / 24)} days ago`;
}

function num(n) {
  return Number(n).toLocaleString("en-US");
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
    incomplete: "warn", waiting_base: "warn", receiving: "warn", quota_blocked: "bad", diverged: "bad", failed: "bad",
    queued: "warn", running: "warn", succeeded: "good", cancelled: "", interrupted: "bad" };
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

// ----------------------------------------------------------------- charts
//
// One series per chart, drawn as a line over a light wash, always from zero.
// The drawing is an SVG stretched to the card; every label is ordinary text
// around it, so nothing is distorted and nothing is read from colour alone.
// Hovering (or focusing the chart and using the arrow keys) reads out one
// step; "Show as table" lists them all.

const SVG_NS = "http://www.w3.org/2000/svg";
function svg(tag, attrs) {
  const el = document.createElementNS(SVG_NS, tag);
  for (const [k, v] of Object.entries(attrs || {})) el.setAttribute(k, v);
  return el;
}

const RANGES = [["24h", "24 hours"], ["7d", "7 days"], ["30d", "30 days"], ["1y", "1 year"]];

function rangePicker() {
  return h("div", { class: "ranges", role: "group", "aria-label": "Time range" },
    RANGES.map(([id, label]) => h("button", {
      class: `btn ${state.range === id ? "on" : ""}`, "aria-pressed": state.range === id ? "true" : "false",
      onclick: () => { state.range = id; render(); },
    }, label)),
    h("span", { class: "muted" }, "Times are in UTC."));
}

// A round number at or above v, for the top of the axis.
function axisTop(v, binary) {
  if (!(v > 0)) return binary ? 1024 : 4;
  let unit = 1;
  if (binary) while (v / unit >= 1024) unit *= 1024;
  const x = v / unit;
  const p = 10 ** Math.floor(Math.log10(x));
  const m = x / p;
  const top = (m <= 1 ? 1 : m <= 2 ? 2 : m <= 4 ? 4 : m <= 5 ? 5 : 10) * p * unit;
  return binary ? top : Math.max(4, top);
}

function compact(n) {
  return n >= 10000 ? new Intl.NumberFormat("en-US", { notation: "compact", maximumFractionDigits: 1 }).format(n) : num(n);
}

function stepLabel(hist, i) {
  const pad = (x) => String(x).padStart(2, "0");
  const d = new Date(hist.start_ms + i * hist.step_hours * 3600000);
  const day = `${d.getUTCFullYear()}-${pad(d.getUTCMonth() + 1)}-${pad(d.getUTCDate())}`;
  if (hist.step_hours >= 168) return `week from ${day}`;
  if (hist.step_hours >= 24) return day;
  const from = `${day} ${pad(d.getUTCHours())}:00`;
  if (hist.step_hours === 1) return from;
  return `${from} to ${pad((d.getUTCHours() + hist.step_hours) % 24)}:00`;
}

// Points of a line in a 100 x 100 box; a gap in the data breaks the line.
function linePaths(values, top) {
  const n = values.length;
  const runs = [];
  let run = [];
  values.forEach((v, i) => {
    if (v === null || v === undefined) { if (run.length) runs.push(run); run = []; return; }
    run.push([n > 1 ? (i / (n - 1)) * 100 : 50, 100 - (Math.min(v, top) / top) * 100]);
  });
  if (run.length) runs.push(run);
  return runs;
}

function drawSeries(values, top, withGrid) {
  const el = svg("svg", { viewBox: "0 0 100 100", preserveAspectRatio: "none", "aria-hidden": "true" });
  if (withGrid) for (const y of [0, 50, 100]) el.append(svg("line", { class: "grid", x1: 0, x2: 100, y1: y, y2: y }));
  for (const run of linePaths(values, top)) {
    const pts = run.length === 1 ? [[Math.max(0, run[0][0] - 1), run[0][1]], [Math.min(100, run[0][0] + 1), run[0][1]]] : run;
    const line = pts.map(([x, y], i) => `${i ? "L" : "M"}${x.toFixed(2)} ${y.toFixed(2)}`).join(" ");
    if (withGrid) el.append(svg("path", { class: "area", d: `${line} L${pts[pts.length - 1][0].toFixed(2)} 100 L${pts[0][0].toFixed(2)} 100 Z` }));
    el.append(svg("path", { class: "line", d: line }));
  }
  return el;
}

// A small trend line for a table cell. The numbers are in the columns beside
// it. Every row is drawn to the same scale (`peak`), so rows can be compared.
function sparkline(values, peak) {
  return h("div", { class: "spark" }, drawSeries(values, axisTop(peak, false), false));
}

// kind "sum": the values are counts per step and the headline is their total.
// kind "size": the values are sizes and the headline is the latest one.
function chartCard(title, values, hist, kind, fmt) {
  const known = values.filter((v) => v !== null && v !== undefined);
  const n = values.length;
  const headline = kind === "sum" ? known.reduce((a, b) => a + b, 0) : (known.length ? known[known.length - 1] : null);
  const rangeLabel = RANGES.find((r) => r[0] === hist.range)[1];
  const sub = kind === "sum" ? `in the last ${rangeLabel}` : "now";
  const head = h("div", { class: "head" }, h("h3", {}, title), h("span", { class: "spacer" }),
    h("span", { class: "big" }, headline === null ? "no data" : fmt(headline)), h("span", { class: "muted" }, sub));
  if (!known.length) return h("div", { class: "card chart" }, head, h("div", { class: "empty" }, "Nothing has been recorded for this period yet."));

  // Byte amounts get an axis in binary units; counts a compact decimal one.
  const top = axisTop(Math.max(...known), fmt === bytes);
  const tick = (v) => (fmt === bytes ? fmt(v) : compact(v));
  const cross = h("div", { class: "cross" });
  const dot = h("div", { class: "dot" });
  const tip = h("div", { class: "tip" });
  const plot = h("div", { class: "plot", tabindex: "0", role: "img",
    "aria-label": `${title}: ${fmt(headline)} ${sub}. Use the arrow keys to read each step, or open the table below.` },
    drawSeries(values, top, true), cross, dot, tip);
  let at = -1;
  const show = (i) => {
    at = Math.max(0, Math.min(n - 1, i));
    const v = values[at];
    const x = n > 1 ? (at / (n - 1)) * 100 : 50;
    cross.style.left = `${x}%`;
    cross.style.display = "block";
    const has = v !== null && v !== undefined;
    dot.style.display = has ? "block" : "none";
    if (has) { dot.style.left = `${x}%`; dot.style.top = `${100 - (Math.min(v, top) / top) * 100}%`; }
    tip.replaceChildren(h("strong", {}, has ? fmt(v) : "no data"), h("span", {}, stepLabel(hist, at)));
    tip.style.display = "block";
    tip.style.left = x <= 50 ? `calc(${x}% + 10px)` : "auto";
    tip.style.right = x > 50 ? `calc(${100 - x}% + 10px)` : "auto";
  };
  const hide = () => { at = -1; for (const el of [cross, dot, tip]) el.style.display = "none"; };
  plot.addEventListener("pointermove", (e) => {
    const r = plot.getBoundingClientRect();
    show(Math.round(((e.clientX - r.left) / Math.max(1, r.width)) * (n - 1)));
  });
  plot.addEventListener("pointerleave", () => { if (document.activeElement !== plot) hide(); });
  plot.addEventListener("focus", () => { if (at < 0) show(n - 1); });
  plot.addEventListener("blur", hide);
  plot.addEventListener("keydown", (e) => {
    if (e.key === "ArrowLeft") { show((at < 0 ? n : at) - 1); e.preventDefault(); }
    if (e.key === "ArrowRight") { show(at + 1); e.preventDefault(); }
  });

  return h("div", { class: "card chart" }, head,
    h("div", { class: "frame" },
      h("div", { class: "yaxis" }, [top, top / 2, 0].map((v) => h("span", {}, tick(v)))),
      h("div", {}, plot,
        h("div", { class: "xaxis" }, [0, Math.floor((n - 1) / 2), n - 1].map((i) => h("span", {}, stepLabel(hist, i).replace(/^week from /, "").replace(/ to .*/, "")))))),
    h("details", {}, h("summary", {}, "Show as table"),
      table(["Period", { label: title, num: true }], values.map((v, i) => h("tr", {},
        td(stepLabel(hist, i), "nowrap"), td(v === null || v === undefined ? "no data" : fmt(v), "num"))))));
}

// --------------------------------------------------------------- overview

async function overviewView() {
  const [o, hist] = await Promise.all([api("overview"), api(`history?range=${state.range}`)]);
  const old = o.repos.filter((r) => r.stale);
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
      tile(old.length, "repositories with content older than their limit", old.length ? "alert" : ""),
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
    rangePicker(),
    h("div", { class: "charts" },
      chartCard("Downloads", hist.requests.total, hist, "sum", num),
      chartCard("Data sent", hist.bytes.total, hist, "sum", bytes),
      chartCard("Requests for missing items", hist.misses.total, hist, "sum", num),
      chartCard("Storage used", hist.storage.total, hist, "size", bytes)),
    h("div", { class: "card" },
      h("div", { class: "head" }, h("h3", {}, "Repositories")),
      table(["Name", "Format", "Access", "Published", "Content from", "Newest", { label: "Size", num: true }, "Quota"],
        o.repos.map((r) => h("tr", {},
          td(r.name), td(r.format),
          td(r.private ? badge("token required", "warn") : badge("open")),
          td(r.live ? badge(`snapshot #${r.live}`, "live") : badge("nothing published")),
          td(r.live ? h("span", { title: when(r.live_at) }, ago(r.live_at), " ", r.stale && badge(`older than ${r.stale_after_days} days`, "warn")) : ""),
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
      h("button", { class: "btn", onclick: () => browseDialog(r) }, "Browse"),
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
  } else if (r.format === "cargo") {
    add("Use the vault instead of crates.io, in .cargo/config.toml", [
      "[source.crates-io]", 'replace-with = "vault"', "", "[source.vault]", `registry = "sparse+${base}/"`,
    ]);
    if (r.private) add("Token: in .cargo/config.toml, use these in place of the [source.vault] table", [
      "[registries.vault]", `index = "sparse+${base}/"`, "",
      "[registry]", 'global-credential-providers = ["cargo:token"]', "",
      "# and the token, in ~/.cargo/credentials.toml (or CARGO_REGISTRIES_VAULT_TOKEN):",
      "[registries.vault]", 'token = "<token>"',
    ]);
  } else if (r.format === "go") {
    add("Use the vault as the only module source", [
      `go env -w GOPROXY=${base}/`,
      "go env -w GOSUMDB=off        # no checksum database inside the air gap; go.sum is still checked",
      "go env -w GOTOOLCHAIN=local  # do not try to download newer Go toolchains",
    ]);
    if (r.private) add(secure ? "Token, in ~/.netrc (machine is host:port)" : "Token: go sends credentials only over HTTPS, so serve this repository with TLS", [
      `machine ${host} login go password <token>`,
    ]);
  } else if (r.format === "apk") {
    add("Use only the vault, in /etc/apk/repositories (one line per repository mirrored)", [
      `${base}/main`, `${base}/community`,
    ]);
    add("Then", ["apk update", "apk add <package>"]);
    blocks.push(h("p", { class: "muted" }, "The index is Alpine's own, still signed by Alpine, so the keys a client already has are all it needs. It lists packages that are not mirrored too: installing one of those fails with 404 and puts it on the wishlist."));
    if (r.private) add("Token, in the repository line (any user name)", [`${base.replace("://", "://any:<token>@")}/main`]);
  } else if (r.format === "rpm") {
    add("Add the vault as a repository, in /etc/yum.repos.d/vault.repo", [
      "[vault]", "name=vault", `baseurl=${base}/`, "gpgcheck=1",
      "gpgkey=file:///etc/pki/rpm-gpg/RPM-GPG-KEY-<distribution>",
    ].concat(r.private ? ["username=any", "password=<token>"] : []));
    add("Install only from the vault", ["dnf --disablerepo='*' --enablerepo=vault install <package>"]);
  } else if (r.format === "maven") {
    add("Use the vault for everything, in ~/.m2/settings.xml", [
      "<settings>", "  <mirrors>",
      `    <mirror><id>vault</id><mirrorOf>*</mirrorOf><url>${base}/</url></mirror>`,
      "  </mirrors>",
    ].concat(r.private ? ["  <servers>", "    <server><id>vault</id><username>any</username><password>&lt;token&gt;</password></server>", "  </servers>"] : []).concat(["</settings>"]));
  } else if (r.format === "helm") {
    add("Add the vault as a chart repository", [`helm repo add vault ${base}/`, "helm search repo vault/"]);
    if (r.private) add("Token", [`helm repo add vault ${base}/ --username any --password <token>`]);
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

// What a repository holds: its packages and versions, or its files, with a
// search box. Shows what is published; if nothing is, the newest snapshot.
function browseDialog(r) {
  const hasPackages = r.format !== "raw";
  let mode = hasPackages ? "packages" : "files";
  const search = h("input", { type: "search", placeholder: "Search", "aria-label": "Search", autofocus: "" });
  const summary = h("p", { class: "muted" });
  const results = h("div", {});
  const modeButtons = h("div", { class: "ranges", role: "group", "aria-label": "What to list" });
  let latest = 0;
  const load = async () => {
    const mine = ++latest;
    const q = search.value.trim();
    try {
      const data = await api(`repos/${encodeURIComponent(r.name)}/${mode}?limit=500${q ? `&q=${encodeURIComponent(q)}` : ""}`);
      if (mine !== latest) return; // a newer search has been started
      if (data.snapshot === null) {
        summary.textContent = "This repository has no snapshot yet.";
        results.replaceChildren();
        return;
      }
      const where = `Snapshot #${data.snapshot}${r.live === data.snapshot ? " (published)" : " (not published)"}`;
      if (mode === "packages") {
        summary.textContent = `${where}: ${count(data.total_packages, "package", "packages")} in ${count(data.files, "file", "files")}, ${bytes(data.bytes)}.`
          + (data.packages.length < data.total_packages ? ` Showing ${data.packages.length}.` : "");
        results.replaceChildren(table(["Package", "Versions", { label: "Files", num: true }, { label: "Size", num: true }],
          data.packages.map((p) => h("tr", {},
            td(p.name, "path"),
            td(p.versions.length > 8 ? `${p.versions.slice(-8).join(", ")} and ${p.versions.length - 8} older` : p.versions.join(", ")),
            td(p.files, "num"), td(bytes(p.bytes), "num"))),
          q ? "No package matches." : "No packages in this snapshot."));
      } else {
        summary.textContent = `${where}: ${count(data.files, "file", "files")}`
          + (q ? `, ${data.matching} matching.` : ".") + (data.items.length < data.matching ? ` Showing the first ${data.items.length}.` : "");
        results.replaceChildren(table(["Path", { label: "Size", num: true }, "SHA-256"],
          data.items.map((f) => h("tr", {}, td(f.path, "path"), td(bytes(f.size), "num"), td(h("span", { title: f.sha256 }, f.sha256.slice(0, 16)), "mono"))),
          q ? "No file matches." : "No files in this snapshot."));
      }
    } catch (err) {
      if (mine === latest) results.replaceChildren(h("div", { class: "banner bad" }, err.message));
    }
  };
  const drawModes = () => modeButtons.replaceChildren(...[["packages", "Packages"], ["files", "Files"]].filter(([id]) => hasPackages || id === "files").map(([id, label]) =>
    h("button", { class: `btn ${mode === id ? "on" : ""}`, "aria-pressed": mode === id ? "true" : "false", onclick: () => { mode = id; drawModes(); load(); } }, label)));
  let typing = null;
  search.addEventListener("input", () => { clearTimeout(typing); typing = setTimeout(load, 250); });
  drawModes();
  load();
  confirmDialog(`Contents of ${r.name}`, [h("div", { class: "browse" }, modeButtons, search), summary, results], null, false);
}

async function reviewDialog(repo, seq) {
  let c;
  try { c = await api(`repos/${encodeURIComponent(repo)}/snapshots/${seq}/changes?limit=1000`); }
  catch (err) { return flash("bad", err.message); }
  const symbol = { add: "+", change: "~", remove: "−" };
  const summary = c.total === 0
    ? "Identical to what is published now."
    : `${count(c.added, "file", "files")} added, ${c.changed} changed, ${c.removed} removed. ${bytes(c.bytes)} of new content.`;
  // The same changes as packages: what is new, what moves to another version, what goes.
  const change = (p) => {
    if (p.added.length && p.removed.length) return [badge("updated", "warn"), ` ${p.removed.join(", ")} to ${p.added.join(", ")}`];
    if (p.added.length) return [badge("added", "good"), ` ${p.added.join(", ")}`];
    return [badge("removed", "bad"), ` ${p.removed.join(", ")}`];
  };
  const files = [
    table(["", "Path", { label: "Size", num: true }], c.items.map((i) => h("tr", {},
      td(h("span", { class: `op-${i.op}`, title: i.op }, symbol[i.op])), td(i.path, "path"),
      td(i.op === "remove" ? "" : bytes(i.size), "num"))), "No differences."),
    c.total > c.items.length && h("p", { class: "muted" }, `Showing the first ${c.items.length} of ${c.total} changes.`),
  ];
  const body = [
    h("p", {}, c.base ? `Compared with the published snapshot #${c.base}: ` : "Nothing is published yet, so everything is new: ", summary),
    c.total_packages > 0 && h("h3", {}, count(c.total_packages, "package changes", "packages change")),
    c.total_packages > 0 && table(["Package", "Change"], c.packages.map((p) => h("tr", {}, td(p.name, "path"), td(change(p)))), ""),
    c.total_packages > c.packages.length && h("p", { class: "muted" }, `Showing the first ${c.packages.length} of ${c.total_packages} packages.`),
    c.total_packages > 0 ? h("details", { class: "files" }, h("summary", {}, `Every changed file (${c.total})`), files) : files,
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
  const stale = h("input", { type: "number", min: "1", max: "3650", step: "1", value: r.stale_after_days || "" });
  const ok = await confirmDialog(`Settings for ${r.name}`, [
    h("p", {}, h("label", {}, priv, " Require an access token to download from this repository")),
    h("label", { class: "f" }, "Size limit in MiB (empty for no limit)", quota),
    h("label", { class: "f" }, "Report as out of date when the published content is older than this many days (empty for never)", stale),
  ], "Save", false);
  if (!ok) return;
  const mib = Number(quota.value);
  const days = Math.floor(Number(stale.value));
  try {
    await api(`repos/${encodeURIComponent(r.name)}/settings`, {
      private: priv.checked, quota_bytes: mib > 0 ? mib * 1048576 : null, stale_after_days: days > 0 ? days : null,
    });
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
  const rows = await api("wishlist?limit=2000");
  const act = async (path, body, done) => {
    try { await api(path, body); flash("good", done); } catch (err) { flash("bad", err.message); }
  };
  return h("div", {}, h("h2", {}, "Wishlist"),
    h("p", { class: "muted" }, "What clients asked for and this vault does not have. The last column is what to add on the collector, to the repository's source in the Jobs tab or to its sync command, so that the next bundle brings it."),
    h("div", { class: "card" },
      h("div", { class: "head" }, h("h3", {}, count(rows.length, "item", "items")), h("span", { class: "spacer" }),
        rows.length > 0 && h("a", { class: "btn", href: "/api/wishlist/export", download: "wishlist.txt" }, "Download as text"),
        rows.length > 0 && can("operator") && h("button", { class: "btn danger", onclick: async () => {
          if (!(await confirmDialog("Clear the wishlist", [h("p", {}, "Every item is forgotten. Items come back when clients ask for them again.")], "Clear", true))) return;
          act("wishlist/clear", {}, "Wishlist cleared.");
        } }, "Clear all")),
      table(["Repository", "Item", { label: "Requests", num: true }, "Last asked", "Add on the collector", ""],
        rows.map((m) => h("tr", {}, td(m.repo), td(m.path, "path"), td(num(m.count), "num"), td(when(m.last_at), "nowrap"),
          td(m.hint || "", "mono nowrap"),
          td(h("div", { class: "actions" }, can("operator") && h("button", { class: "btn", title: "Forget this item",
            onclick: () => act("wishlist/dismiss", { repo: m.repo, path: m.path }, `${m.path} dismissed.`) }, "Dismiss"))))),
        "No missing items have been requested.")));
}

// ---------------------------------------------------------------- traffic

async function trafficView() {
  const [t, hist] = await Promise.all([api("traffic"), api(`history?range=${state.range}`)]);
  const max = Math.max(1, ...t.repos.map((r) => r.bytes));
  const sum = (a) => (a || []).reduce((x, y) => x + y, 0);
  const names = [...new Set([...Object.keys(hist.requests.by_repo), ...Object.keys(hist.misses.by_repo)])]
    .sort((a, b) => sum(hist.requests.by_repo[b]) - sum(hist.requests.by_repo[a]) || a.localeCompare(b));
  const rangeLabel = RANGES.find((r) => r[0] === hist.range)[1];
  const peak = Math.max(0, ...Object.values(hist.requests.by_repo).flat());
  return h("div", {}, h("h2", {}, "Traffic"),
    rangePicker(),
    h("div", { class: "charts" },
      chartCard("Downloads", hist.requests.total, hist, "sum", num),
      chartCard("Data sent", hist.bytes.total, hist, "sum", bytes)),
    h("div", { class: "card" }, h("div", { class: "head" }, h("h3", {}, `By repository, last ${rangeLabel}`)),
      table(["Repository", "Downloads over time", { label: "Downloads", num: true }, { label: "Sent", num: true }, { label: "Missing items asked for", num: true }],
        names.map((name) => h("tr", {},
          td(name), td(sparkline(hist.requests.by_repo[name] || new Array(hist.points).fill(0), peak)),
          td(num(sum(hist.requests.by_repo[name])), "num"), td(bytes(sum(hist.bytes.by_repo[name])), "num"),
          td(num(sum(hist.misses.by_repo[name])), "num"))), "Nothing was downloaded in this period.")),
    h("div", { class: "card" }, h("div", { class: "head" }, h("h3", {}, "By repository, since the beginning")),
      table(["Repository", { label: "Downloads", num: true }, { label: "Sent", num: true }, ""], t.repos.map((r) => {
        const fill = h("i", {});
        fill.style.width = `${Math.round((r.bytes / max) * 100)}%`;
        return h("tr", {}, td(r.repo), td(num(r.hits), "num"), td(bytes(r.bytes), "num"), td(h("div", { class: "bar" }, fill)));
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
      // The role of a directory user is decided by the groups in the directory, at each sign-in.
      const role = u.source === "directory" ? h("span", {}, u.role, " ", badge("from the directory")) : sel;
      return h("tr", {}, td(u.name), td(role), td(u.disabled ? badge("disabled", "bad") : badge("active", "good")), td(when(u.created_at), "nowrap"),
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

// ------------------------------------------------------------------- jobs

// Syncs, seals, exports and imports run by the server (mer serve --jobs).
// Forms are built from the field list the server publishes, so they always
// match what the server accepts.

function sourceSummary(spec) {
  if (!spec) return h("span", { class: "muted" }, "no source yet");
  const parts = Object.entries(spec).map(([k, v]) => `${k}: ${Array.isArray(v) ? v.join(", ") : v}`);
  return h("span", { class: "mono" }, parts.join(" · "));
}

function fieldInput(f, value) {
  if (f.kind === "bool") return h("input", { type: "checkbox", checked: value === true, "data-field": f.name });
  if (f.kind === "choice") {
    const sel = h("select", { "data-field": f.name }, h("option", { value: "" }, "(default)"), f.choices.map((c) => h("option", { value: c }, c)));
    sel.value = value || "";
    return sel;
  }
  if (f.kind.endsWith("list")) {
    const ta = h("textarea", { rows: "3", "data-field": f.name, placeholder: "one per line" });
    ta.value = (value || []).join("\n");
    return ta;
  }
  const input = h("input", { type: "text", "data-field": f.name, placeholder: f.kind === "path" ? "relative to the files directory" : "" });
  input.value = value || "";
  return input;
}

function readFields(form, fields) {
  const spec = {};
  for (const f of fields) {
    const el = form.querySelector(`[data-field="${f.name}"]`);
    if (f.kind === "bool") { if (el.checked) spec[f.name] = true; continue; }
    if (f.kind.endsWith("list")) {
      const items = el.value.split("\n").map((x) => x.trim()).filter(Boolean);
      if (items.length) spec[f.name] = items;
      continue;
    }
    if (el.value.trim()) spec[f.name] = el.value.trim();
  }
  return spec;
}

async function sourceDialog(repo, format, current, info) {
  const fields = info.forms[format];
  if (!fields) { flash("bad", `${format} repositories cannot be synced from the panel.`); return; }
  const form = h("div", { class: "form" }, fields.map((f) => h("label", { class: "f" },
    `${f.name.replaceAll("_", " ")}${f.required ? " *" : ""}`, fieldInput(f, current && current[f.name]), h("small", { class: "muted" }, f.help))));
  if (!(await confirmDialog(`Source of ${repo} (${format})`, [form], "Save", false))) return;
  try {
    await api(`repos/${encodeURIComponent(repo)}/source`, { spec: readFields(form, fields) });
    flash("good", `Source of ${repo} saved.`);
  } catch (err) { flash("bad", err.message); }
  render();
}

async function newRepoDialog(info) {
  const name = h("input", { type: "text", placeholder: "for example pypi" });
  const format = h("select", {}, Object.keys(info.forms).map((f) => h("option", { value: f }, f)));
  if (!(await confirmDialog("New repository", [h("label", { class: "f" }, "Name", name), h("label", { class: "f" }, "Format", format)], "Create", false))) return;
  try {
    await api("repos", { name: name.value.trim(), format: format.value });
    flash("good", `Repository ${name.value.trim()} created. Set its source next.`);
  } catch (err) { flash("bad", err.message); }
  render();
}

async function jobDialog(id) {
  const pre = h("pre", { class: "log" }, "Loading…");
  const status = h("p", {});
  const cancel = h("button", { class: "btn danger", onclick: async () => {
    try { await api(`jobs/${id}/cancel`, {}); } catch (err) { status.textContent = err.message; }
  } }, "Cancel job");
  let timer = null;
  const refresh = async () => {
    try {
      const j = await api(`jobs/${id}`);
      status.replaceChildren(stateBadge(j.state), ` ${j.kind} ${j.target}, by ${j.actor}`, j.exit_code !== null ? `, exit code ${j.exit_code}` : "");
      pre.textContent = j.log || "(no output yet)";
      pre.scrollTop = pre.scrollHeight;
      const live = ["queued", "running"].includes(j.state);
      cancel.hidden = !live || !can("operator");
      if (live) timer = setTimeout(refresh, 1500);
    } catch (err) { status.textContent = err.message; }
  };
  refresh();
  await confirmDialog(`Job #${id}`, [status, pre, cancel], null, false);
  clearTimeout(timer);
  render();
}

async function runJob(path, body, what) {
  try {
    const r = await api(path, body || {});
    flash("good", `${what}: job #${r.job} queued.`);
  } catch (err) { flash("bad", err.message); }
  render();
}

const WEEKDAYS = [["mon", "Mon"], ["tue", "Tue"], ["wed", "Wed"], ["thu", "Thu"], ["fri", "Fri"], ["sat", "Sat"], ["sun", "Sun"]];

function ruleText(rule) {
  if (!rule) return "unreadable rule";
  if (rule.kind === "every") return `every ${count(rule.hours, "hour", "hours")}`;
  const days = rule.days || [];
  const which = days.length === 7 ? "daily" : days.join("+") === "mon+tue+wed+thu+fri" ? "weekdays" : days.map((d) => d[0].toUpperCase() + d.slice(1)).join(", ");
  return `${which} at ${rule.at} UTC`;
}

async function scheduleDialog(current, repos, sources) {
  const rule = (current && current.rule) || { kind: "daily", at: "02:00", days: WEEKDAYS.map(([d]) => d) };
  const kind = h("select", {}, h("option", { value: "daily" }, "At a time of day"), h("option", { value: "every" }, "Every few hours"));
  kind.value = rule.kind;
  const at = h("input", { type: "time", value: rule.at || "02:00", required: true });
  const hours = h("input", { type: "number", min: "1", max: "168", step: "1", value: String(rule.hours || 6) });
  const dayBoxes = WEEKDAYS.map(([d, label]) => {
    const box = h("input", { type: "checkbox", value: d, checked: (rule.days || WEEKDAYS.map(([x]) => x)).includes(d) });
    return h("label", { class: "inline" }, box, ` ${label}`);
  });
  const hhmm = (d, utc) => [utc ? d.getUTCHours() : d.getHours(), utc ? d.getUTCMinutes() : d.getMinutes()].map((x) => String(x).padStart(2, "0")).join(":");
  const now = new Date();
  const dailyPart = h("div", {}, h("label", { class: "f" }, "Time (UTC)", at,
    h("small", { class: "muted" }, `It is ${hhmm(now, true)} UTC now (${hhmm(now, false)} on this computer).`)), h("div", { class: "f" }, "Days", h("div", { class: "days" }, dayBoxes)));
  const everyPart = h("label", { class: "f" }, "Hours between runs", hours);
  const sync = () => { dailyPart.hidden = kind.value !== "daily"; everyPart.hidden = kind.value !== "every"; };
  kind.onchange = sync;
  sync();
  const chosen = new Set((current && current.repos) || []);
  const all = h("input", { type: "checkbox", checked: chosen.size === 0 });
  const repoBoxes = repos.map((r, i) => {
    const box = h("input", { type: "checkbox", value: r.name, checked: chosen.has(r.name) });
    return h("label", { class: "inline" }, box, ` ${r.name}`, !sources[i].spec && h("span", { class: "muted" }, " (no source)"));
  });
  const repoList = h("div", { class: "days" }, repoBoxes);
  const syncAll = () => { repoList.hidden = all.checked; };
  all.onchange = syncAll;
  syncAll();
  const seal = h("input", { type: "checkbox", checked: current ? current.seal : true });
  const enabled = h("input", { type: "checkbox", checked: current ? current.enabled : true });
  const body = [h("div", { class: "form" },
    h("label", { class: "f" }, "When", kind), dailyPart, everyPart,
    h("div", { class: "f" }, "Repositories", h("label", { class: "inline" }, all, " Every repository with a source"), repoList),
    h("label", { class: "inline" }, seal, " Then seal a bundle of what changed"),
    h("label", { class: "inline" }, enabled, " On"),
    h("small", { class: "muted" }, "Each run queues a sync job per repository, after any jobs already waiting, then the seal. A repository whose sync is still waiting or running is skipped. Writing the bundle to disk stays a step you take."))];
  if (!(await confirmDialog(current ? `Schedule #${current.id}` : "New schedule", body, "Save", false))) return;
  const payload = {
    rule: kind.value === "every" ? { kind: "every", hours: Number(hours.value) } : { kind: "daily", at: at.value, days: dayBoxes.map((l) => l.firstChild).filter((b) => b.checked).map((b) => b.value) },
    repos: all.checked ? [] : repoBoxes.map((l) => l.firstChild).filter((b) => b.checked).map((b) => b.value),
    seal: seal.checked, enabled: enabled.checked,
  };
  try {
    await api(current ? `schedules/${current.id}` : "schedules", payload);
    flash("good", current ? `Schedule #${current.id} saved.` : "Schedule created.");
  } catch (err) { flash("bad", err.message); }
  render();
}

function schedulesCard(schedules, repos, sources, info) {
  const act = async (path, done) => {
    try {
      const r = await api(path, {});
      flash(r.note ? "bad" : "good", r.note ? `${done} ${r.note}.` : done);
    } catch (err) { flash("bad", err.message); }
    render();
  };
  const rows = schedules.map((s) => h("tr", {},
    td(ruleText(s.rule)),
    td(s.repos.length ? s.repos.join(", ") : h("span", { class: "muted" }, "every repository with a source")),
    td(s.seal ? (info.seal ? "seal" : h("span", { class: "muted" }, "seal (no bundle key)")) : ""),
    td(s.enabled ? whenUtc(s.next_at) : badge("off"), "nowrap"),
    td(s.last_at ? h("div", {}, whenUtc(s.last_at), " ",
      s.last_jobs.map((j) => h("a", { href: "#", onclick: (e) => { e.preventDefault(); jobDialog(j); } }, `#${j} `)),
      s.last_note && h("div", { class: "muted small" }, s.last_note)) : h("span", { class: "muted" }, "never"), "nowrap"),
    td(h("div", { class: "actions" }, can("operator") && [
      h("button", { class: "btn", onclick: () => scheduleDialog(s, repos, sources) }, "Edit"),
      h("button", { class: "btn", onclick: () => act(`schedules/${s.id}/run`, `Schedule #${s.id} queued its jobs.`) }, "Run now"),
      h("button", { class: "btn danger", onclick: async () => {
        if (!(await confirmDialog(`Delete schedule #${s.id}`, [h("p", {}, `${ruleText(s.rule)} stops. Jobs it already queued still run.`)], "Delete", true))) return;
        act(`schedules/${s.id}/delete`, `Schedule #${s.id} deleted.`);
      } }, "Delete")]))));
  return h("div", { class: "card" },
    h("div", { class: "head" }, h("h3", {}, "Schedules"), h("span", { class: "spacer" }),
      can("operator") && h("button", { class: "btn", onclick: () => scheduleDialog(null, repos, sources) }, "New schedule")),
    table(["When", "Repositories", "Then", "Next run", "Last run", ""], rows, "No schedules. Syncs run when someone starts them."));
}

async function jobsView() {
  const info = await jobsInfo();
  if (!info.enabled) {
    return h("div", {}, h("h2", {}, "Jobs"), h("div", { class: "card empty" },
      "This server does not run jobs. On a collector, start it with: mer serve --jobs --admin-listen 127.0.0.1:8081 --files <dir> --bundle-key <key>"));
  }
  const [o, jobs, schedules] = await Promise.all([api("overview"), api("jobs"), api("schedules")]);
  const sources = await Promise.all(o.repos.map((r) => api(`repos/${encodeURIComponent(r.name)}/source`).catch(() => ({ spec: null }))));
  const repoRows = o.repos.map((r, i) => h("tr", {},
    td(r.name), td(badge(r.format)), td(sourceSummary(sources[i].spec)),
    td(h("div", { class: "actions" },
      can("admin") && h("button", { class: "btn", onclick: () => sourceDialog(r.name, r.format, sources[i].spec, info) }, "Edit source"),
      can("operator") && sources[i].spec && h("button", { class: "btn primary", onclick: () => runJob(`repos/${encodeURIComponent(r.name)}/sync`, {}, `Sync of ${r.name}`) }, "Sync now")))));
  const seq = h("input", { type: "number", min: "1", step: "1", placeholder: "bundle number" });
  const out = h("input", { type: "text", placeholder: "for example usb/bundle-1" });
  const vol = h("input", { type: "text", placeholder: "fat32 (default), 4G, 25G" });
  const dir = h("input", { type: "text", placeholder: "for example usb" });
  const auto = h("input", { type: "checkbox" });
  const bundleCard = can("operator") && h("div", { class: "card" },
    h("div", { class: "head" }, h("h3", {}, "Bundles"), h("span", { class: "spacer" }),
      info.seal && h("button", { class: "btn primary", onclick: () => runJob("bundles/seal", {}, "Seal") }, "Seal a bundle of all changes")),
    info.files ? h("div", { class: "split" },
      h("form", { class: "form", onsubmit: (e) => { e.preventDefault(); runJob("bundles/export", { seq: Number(seq.value), out: out.value.trim(), volume_size: vol.value.trim() }, "Export"); } },
        h("h4", {}, "Write a bundle to disk"), h("label", { class: "f" }, "Bundle", seq), h("label", { class: "f" }, "Directory (new, under the files directory)", out),
        h("label", { class: "f" }, "Volume size", vol), h("button", { class: "btn", type: "submit" }, "Export")),
      h("form", { class: "form", onsubmit: (e) => { e.preventDefault(); runJob("bundles/import", { dir: dir.value.trim(), auto_publish: auto.checked }, "Import"); } },
        h("h4", {}, "Import bundles from disk"), h("label", { class: "f" }, "Directory (under the files directory)", dir),
        h("label", {}, auto, " Publish without review"), h("button", { class: "btn", type: "submit" }, "Import")))
      : h("p", { class: "muted" }, "Exports and imports need a files directory: start the server with --files <dir>."));
  const jobRows = jobs.map((j) => h("tr", { class: "click", onclick: () => jobDialog(j.id) },
    td(`#${j.id}`), td(j.kind), td(j.target), td(stateBadge(j.state)), td(j.actor), td(when(j.started_at || j.created_at), "nowrap"),
    td(j.finished_at && j.started_at ? `${Math.max(1, Math.round((j.finished_at - j.started_at) / 1000))} s` : "", "num")));
  if (jobs.some((j) => ["queued", "running"].includes(j.state))) {
    state.timer = setTimeout(() => { if (state.tab === "jobs" && !document.querySelector("dialog[open]")) render(); }, 3000);
  }
  return h("div", {},
    h("h2", {}, "Jobs"),
    h("div", { class: "card" },
      h("div", { class: "head" }, h("h3", {}, "Repositories and their sources"), h("span", { class: "spacer" }),
        can("admin") && h("button", { class: "btn", onclick: () => newRepoDialog(info) }, "New repository")),
      table(["Repository", "Format", "Source", ""], repoRows, "No repositories yet.")),
    schedulesCard(schedules, o.repos, sources, info),
    bundleCard,
    h("div", { class: "card" }, h("h3", {}, "Recent jobs"),
      table(["Job", "Kind", "Target", "State", "By", "Started", { label: "Took", num: true }], jobRows, "No jobs yet.")));
}

async function jobsInfo() {
  if (!state.jobs) state.jobs = await api("jobs/forms");
  return state.jobs;
}

// ------------------------------------------------------------------ shell

const TABS = [
  ["overview", "Overview", overviewView],
  ["repos", "Repositories", reposView],
  ["transfers", "Transfers", transfersView],
  ["jobs", "Jobs", jobsView],
  ["wishlist", "Wishlist", wishlistView],
  ["traffic", "Traffic", trafficView],
  ["audit", "Audit log", auditView],
  ["access", "Access", accessView],
];

async function render() {
  clearTimeout(state.timer);
  state.timer = null;
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
    // Not while a dialog is open or a chart is being read.
    const busy = () => document.querySelector("dialog[open]") || document.querySelector(".plot:hover, .plot:focus, details[open]");
    state.timer = setTimeout(function again() { if (busy()) state.timer = setTimeout(again, 5000); else render(); }, 30000);
  }
}

(async () => {
  try { state.me = await api("me"); } catch (_) { state.me = null; }
  render();
})();
