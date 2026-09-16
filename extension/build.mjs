// Bundles the extension. One source tree, two targets.
//
// Firefox differences are structural, not cosmetic: an event page instead of a service
// worker, no tabGroups permission, and a different manifest key for the native
// messaging allowlist. See docs/07-extension.md.

import * as esbuild from "esbuild";
import { cp, mkdir, readFile, rm, writeFile } from "node:fs/promises";
import { dirname, join } from "node:path";

const targets = process.argv.includes("--target=firefox")
  ? ["firefox"]
  : process.argv.includes("--target=chrome")
    ? ["chrome"]
    : ["chrome", "firefox"];
const watch = process.argv.includes("--watch");

const ENTRIES = {
  "background.js": "src/background/index.ts",
  "pages/restore.js": "src/pages/restore.ts",
  "pages/options.js": "src/pages/options.ts",
};

async function build(target) {
  const outdir = join("dist", target);
  await rm(outdir, { recursive: true, force: true });
  await mkdir(join(outdir, "pages"), { recursive: true });

  for (const [out, entry] of Object.entries(ENTRIES)) {
    const ctx = await esbuild.context({
      entryPoints: [entry],
      outfile: join(outdir, out),
      bundle: true,
      format: "esm",
      target: target === "firefox" ? "firefox128" : "chrome116",
      sourcemap: true,
      logLevel: "info",
    });
    if (watch) await ctx.watch();
    else {
      await ctx.rebuild();
      await ctx.dispose();
    }
  }

  const manifest = JSON.parse(await readFile(`manifest.${target}.json`, "utf8"));
  await writeFile(join(outdir, "manifest.json"), JSON.stringify(manifest, null, 2));

  for (const page of ["restore.html", "options.html"]) {
    await mkdir(dirname(join(outdir, "pages", page)), { recursive: true });
    await cp(join("src/pages", page), join(outdir, "pages", page));
  }

  console.log(`built ${outdir}`);
}

for (const t of targets) await build(t);
if (watch) console.log("watching…");
