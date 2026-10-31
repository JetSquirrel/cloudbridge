// Build the community Insights rules into `dist/`: one ES module per rule
// and `index.json`, the catalog the desktop app installs from.
//
// For each `rules/<id>/rule.ts` this compiles the rule to `dist/<id>.js`,
// checks the catalog metadata its `rule` export carries, and runs every
// `rules/<id>/fixtures/*.json` case against the compiled file. Any failure
// fails the build, so a rule that is published has passed its own cases.
//
//   node build.mjs           build and test
//   node build.mjs <id>...   only these rules (no index is written)

import { createHash } from "node:crypto";
import { mkdir, readdir, readFile, rm, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { pathToFileURL } from "node:url";
import { build } from "esbuild";

/** The `Input` version this tree is written against; see `types/`. */
const API_VERSION = 1;
/** What the app accepts as a rule id, and so as a file name. */
const ID = /^[a-z0-9][a-z0-9-]{0,63}$/;
const SEMVER = /^\d+\.\d+\.\d+$/;
/** The app refuses larger downloads; a rule should be nowhere near it. */
const MAX_BYTES = 256 * 1024;

const root = new URL(".", import.meta.url).pathname.replace(/^\/([A-Za-z]:)/, "$1");
const rulesDir = join(root, "rules");
const dist = join(root, "dist");

const only = process.argv.slice(2);
const ids = (await readdir(rulesDir, { withFileTypes: true }))
  .filter((e) => e.isDirectory())
  .map((e) => e.name)
  .filter((id) => only.length === 0 || only.includes(id))
  .sort();

if (only.length === 0) await rm(dist, { recursive: true, force: true });
await mkdir(dist, { recursive: true });

const failures = [];
const entries = [];
for (const id of ids) {
  try {
    entries.push(await buildRule(id));
    console.log(`ok    ${id}`);
  } catch (e) {
    failures.push(id);
    console.error(`FAIL  ${id}\n      ${String(e.message).replaceAll("\n", "\n      ")}`);
  }
}

if (failures.length > 0) {
  console.error(`\n${failures.length} of ${ids.length} rules failed`);
  process.exit(1);
}
if (only.length === 0) {
  const index = { apiVersion: API_VERSION, rules: entries };
  await writeFile(join(dist, "index.json"), JSON.stringify(index, null, 2) + "\n");
  console.log(`\n${entries.length} rules -> dist/index.json`);
}

async function buildRule(id) {
  if (!ID.test(id)) throw new Error(`"${id}" is not a rule id: lower case letters, digits and dashes`);
  const outfile = join(dist, `${id}.js`);
  await build({
    entryPoints: [join(rulesDir, id, "rule.ts")],
    outfile,
    bundle: true,
    format: "esm",
    platform: "neutral",
    target: "es2023",
    logLevel: "silent",
  });
  const bytes = await readFile(outfile);
  if (bytes.length > MAX_BYTES) throw new Error(`compiles to ${bytes.length} bytes; the limit is ${MAX_BYTES}`);
  if (/\bimport\s*[({"'*\w]/.test(bytes.toString("utf8").replace(/\/\/.*$/gm, ""))) {
    throw new Error("still imports something after bundling; the app has no module loader");
  }

  const mod = await import(`${pathToFileURL(outfile).href}?${Date.now()}`);
  const rule = mod.rule ?? {};
  const problems = [];
  for (const key of ["title", "explanation", "author"]) {
    if (typeof rule[key] !== "string" || rule[key].trim() === "") problems.push(`rule.${key} is required`);
  }
  if (!SEMVER.test(rule.version ?? "")) problems.push("rule.version must be MAJOR.MINOR.PATCH");
  if (rule.apiVersion !== API_VERSION) problems.push(`rule.apiVersion must be ${API_VERSION}`);
  if (!Array.isArray(rule.providers) || rule.providers.length === 0) problems.push("rule.providers is required");
  if (typeof mod.evaluate !== "function") problems.push("no evaluate() export");
  if (problems.length > 0) throw new Error(problems.join("\n"));

  const fixtureDir = join(rulesDir, id, "fixtures");
  const cases = (await readdir(fixtureDir).catch(() => [])).filter((f) => f.endsWith(".json")).sort();
  if (cases.length === 0) throw new Error("has no fixtures/*.json cases");
  for (const name of cases) {
    const fixture = JSON.parse(await readFile(join(fixtureDir, name), "utf8"));
    const found = await mod.evaluate(structuredClone(fixture.input));
    checkCase(name, found, fixture.expect);
  }

  return {
    id,
    version: rule.version,
    title: rule.title,
    label: rule.label ?? rule.title.toUpperCase(),
    explanation: rule.explanation,
    author: rule.author,
    providers: rule.providers,
    apiVersion: rule.apiVersion,
    file: `${id}.js`,
    size: bytes.length,
    sha256: createHash("sha256").update(bytes).digest("hex"),
  };
}

function checkCase(name, found, expect) {
  if (!Array.isArray(found)) throw new Error(`${name}: evaluate() returned ${typeof found}, not an array`);
  for (const f of found) {
    if (typeof f?.id !== "string" || typeof f?.evidence !== "string") {
      throw new Error(`${name}: every finding needs a string id and evidence: ${JSON.stringify(f)}`);
    }
  }
  const got = found.map((f) => f.id).sort();
  const want = expect.map((e) => e.id).sort();
  if (JSON.stringify(got) !== JSON.stringify(want)) {
    throw new Error(`${name}: found ${JSON.stringify(got)}\n      expected ${JSON.stringify(want)}`);
  }
  for (const e of expect.filter((e) => e.evidence !== undefined)) {
    const f = found.find((f) => f.id === e.id);
    if (f.evidence !== e.evidence) {
      throw new Error(`${name}: ${e.id}\n      evidence ${JSON.stringify(f.evidence)}\n      expected ${JSON.stringify(e.evidence)}`);
    }
  }
}
