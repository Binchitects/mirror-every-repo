// Generate tests/semver_truth.json from the semver library that npm itself uses.
//
//   docker run --rm -v "$PWD/crates/mer-format-npm/tests:/t" node:20-slim node /t/gen_fixture.js
//
// The Rust tests in src/truth.rs must give the same answers.
const semver = require("/usr/local/lib/node_modules/npm/node_modules/semver");
const fs = require("fs");
const path = require("path");

const VERSIONS = [
  "0.0.0", "0.0.1", "0.0.3", "0.0.4", "0.1.0", "0.2.3", "0.2.9", "0.3.0", "1.0.0", "1.0.1", "1.2.0", "1.2.2", "1.2.3",
  "1.2.4", "1.2.9", "1.3.0", "1.9.9", "1.10.0", "2.0.0", "2.3.4", "2.3.5", "2.3.9", "2.9.9", "3.0.0", "10.0.0",
  "1.0.0-alpha", "1.0.0-alpha.1", "1.0.0-alpha.beta", "1.0.0-beta", "1.0.0-beta.2", "1.0.0-beta.11", "1.0.0-rc.1",
  "1.2.3-beta.2", "1.2.3-beta.4", "1.2.4-beta.1", "1.3.0-beta", "2.0.0-rc.1", "2.0.0-0", "0.0.0-0", "3.0.0-alpha.1",
  "1.2.3+build.5", "1.2.3-beta.2+exp.sha", "v1.2.3", "=1.2.3",
  // not versions
  "", "1", "1.2", "1.2.3.4", "01.2.3", "1.02.3", "1.2.3-", "1.2.3-01", "a.b.c", "1.2.x", "1.2.3-a..b", "1.2.3+", "latest",
  " 1.2.3 ", "1.2.3-0a", "1.2.3--", "99999999999999999999.0.0",
];

const RANGES = [
  "", "*", "x", "X", "1", "1.x", "1.X", "1.*", "1.2", "1.2.x", "1.2.*", "1.2.3", "=1.2.3", "v1.2.3", "^1.2.3", "^1.2", "^1",
  "^0.2.3", "^0.2", "^0.0.3", "^0.0", "^0", "^1.2.x", "^1.x", "^0.x", "^0.0.x", "^*", "~1.2.3", "~1.2", "~1", "~0.2.3",
  "~0.2", "~0", "~>1.2.3", "~> 1.2", "~*", ">=1.2.3", ">1.2.3", "<1.2.3", "<=1.2.3", ">=1.2", ">1.2", "<1.2", "<=1.2",
  ">=1", ">1", "<1", "<=1", ">= 1.2.3", "> 1.2.3", "< 2", "<= 2.3.4", ">=1.2.3 <2", ">=1.2.3 <2.0.0", ">=1.0.0 <1.3.0",
  "1.2.3 - 2.3.4", "1.2.3 - 2.3", "1.2.3 - 2", "1 - 2", "1.2 - 2.3.4", "0.0.1 - 0.2.9", "<1.0.0 || >=2.0.0",
  "1.x || 2.x", "^1.2.3 || ^2.0.0", "1.2.3 || 2.3.4", ">1.2.3 <1.2.3", "^1.2.3-beta.2", "~1.2.3-beta.2", ">=1.2.3-beta.2",
  ">1.0.0-alpha", "<2.0.0-rc.1", "<=2.0.0-rc.1", "^2.0.0-rc.0", "^0.0.0-0", ">=0.0.0-0", "1.2.3-beta.2", "<*", ">*",
  ">=*", "<=*", "^1.2.3 <1.9", ">=1 <=2", "1.2.3 - 1.2.3", "*  ||  1.x", ">=2.0.0-0", "<3.0.0-0", "~1.2.3 >=1.2.5",
  "^1.2.3+build", "1.2.3+build", ">=1.2.3-0",
  // not ranges
  "not a range", "1.x.3", ">=", "^", "1.2.3 -", "1.2.3.4", "latest", "git+https://x/y.git", "file:../x", "- 1.2.3",
  "1.2.3 - 2.0.0 - 3.0.0", "<<1", "1.2.3-", ">=01.2.3",
];

const valid = VERSIONS.filter((v) => semver.valid(v) !== null && v === v.trim());
const order = {};
for (const a of valid) for (const b of valid) order[`${a} ${b}`] = semver.compare(a, b);
const ranges = {};
for (const r of RANGES) {
  const ok = semver.validRange(r) !== null;
  ranges[r] = ok ? valid.filter((v) => semver.satisfies(v, r)) : null;
}
const max = {};
for (const r of RANGES) if (ranges[r] !== null) max[r] = semver.maxSatisfying(valid, r);

const out = { valid, invalid: VERSIONS.filter((v) => !valid.includes(v)), order, ranges, max };
fs.writeFileSync(path.join(__dirname, "semver_truth.json"), JSON.stringify(out, null, 1) + "\n");
const matches = Object.values(ranges).filter(Boolean).reduce((n, l) => n + l.length, 0);
console.log(`semver ${semver.SEMVER_SPEC_VERSION}: ${valid.length} versions, ${Object.keys(order).length} pairs, ` +
  `${RANGES.length} ranges (${Object.values(ranges).filter((x) => x === null).length} invalid), ${matches} matches`);
