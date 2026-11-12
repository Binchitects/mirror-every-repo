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
  } else if (r.format === "rpm") {
    add(`In /etc/yum.repos.d/vault-${r.name}.repo`, [
      `[vault-${r.name}]`, `name=Vault ${r.name}`, `baseurl=${base}/`, "enabled=1",
      "# Packages keep the signature of their distribution: point gpgkey at its key.",
      "gpgcheck=1", "gpgkey=file:///etc/pki/rpm-gpg/RPM-GPG-KEY-<distribution>",
    ].concat(r.private ? ["username=token", "password=<token>"] : []));
    add("If the metadata is signed with the site key (sync with --sign-home), also", ["repo_gpgcheck=1", `# and add ${base}/keys/site.asc to gpgkey`]);
    add("Then, with the distribution's own repositories switched off", [`dnf --disablerepo='*' --enablerepo=vault-${r.name} install <package>`]);
  } else if (r.format === "maven") {
    add("In ~/.m2/settings.xml: use the vault for everything", [
      "<settings>", "  <mirrors>", "    <mirror>", "      <id>vault</id>", "      <mirrorOf>*</mirrorOf>", `      <url>${base}</url>`,
      "    </mirror>", "  </mirrors>",
    ].concat(r.private ? ["  <servers>", "    <server>", "      <id>vault</id>", "      <username>token</username>", "      <password><token></password>", "    </server>", "  </servers>"] : [], ["</settings>"]));
    add("Gradle, in settings.gradle", ["dependencyResolutionManagement {", `  repositories { maven { url = uri("${base}")${secure ? "" : "; allowInsecureProtocol = true"} } }`, "}"]);
    add("Then build as usual", ["mvn package"]);
  } else if (r.format === "go") {
    add("Point the go command at the vault (once per client)", [
      `go env -w GOPROXY=${base}`,
      "go env -w GOSUMDB=off   # the collector compared every file with the checksum database",
    ]);
    add("Then as usual", ["go get <module>", "go build ./..."]);
    if (r.private) add("Token, in ~/.netrc (mode 600)", [`machine ${window.location.hostname} login go password <token>`]);
  } else if (r.format === "cargo") {
    add("In .cargo/config.toml (of the project, or of the user: ~/.cargo/config.toml)", [
      "[source.crates-io]", 'replace-with = "vault"', "",
      "[registries.vault]", `index = "sparse+${base}/index/"`,
    ].concat(r.private ? ['credential-provider = ["cargo:token"]'] : []));
    if (r.private) add("Token, in ~/.cargo/credentials.toml (mode 600)", ["[registries.vault]", 'token = "Bearer <token>"']);
    add("Then build as usual", ["cargo build"]);
  } else if (r.format === "helm") {
    const insecureNote = secure ? [] : ["# plain HTTP: helm accepts it as is; with a private CA add --ca-file"];
    add("Add the vault as a chart repository", [`helm repo add vault ${base}`, "helm repo update"].concat(insecureNote));
    add("Install a chart", ["helm search repo vault/", "helm install <release> vault/<chart>"]);
    if (r.private) add("Token", [`helm repo add vault ${base} --username any --password <token>`]);
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
    h("p", { class: "muted" }, "What clients asked for and this vault does not have. The last column is what to add to the sync command on the collector so that the next bundle brings it."),
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
    // Not while a dialog is open or a chart is being read.
    const busy = () => document.querySelector("dialog[open]") || document.querySelector(".plot:hover, .plot:focus, details[open]");
    state.timer = setTimeout(function again() { if (busy()) state.timer = setTimeout(again, 5000); else render(); }, 30000);
  }
}

(async () => {
  try { state.me = await api("me"); } catch (_) { state.me = null; }
  render();
})();
