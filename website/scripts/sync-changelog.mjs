// Generate `docs/release-notes.md` from the repo's CHANGELOG.md.
//
// The changelog is the single source of truth, and duplicating it into the site
// by hand is how a docs page ends up a release behind. This runs from npm's
// `prebuild` / `prestart`, so `npm run build` (which is what CI runs) and the
// dev server both regenerate it — the generated file is gitignored.
//
// Rewrites two things for Docusaurus:
//   * the `# Changelog` H1 becomes frontmatter, since the page title comes from
//     there and a second H1 would duplicate it;
//   * bare `[0.2.1]` link-reference labels are left as-is — the definitions at
//     the bottom of the changelog resolve them, and they point at GitHub compare
//     views that work unchanged on the site.

import { readFileSync, writeFileSync, mkdirSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const source = resolve(here, "../../CHANGELOG.md");
const target = resolve(here, "../docs/release-notes.md");

const raw = readFileSync(source, "utf8");

// Drop the H1 and the intro paragraph that follows it; the frontmatter below
// carries the title and the page needs its own framing.
const afterH1 = raw.replace(/^#\s+Changelog\s*\n+/, "");
const body = afterH1.replace(
  /^All notable changes[\s\S]*?breaking changes\.\s*\n+/,
  "",
);

const frontmatter = `---
id: release-notes
title: Release notes
sidebar_position: 99
description: Version history for oxihipo, generated from CHANGELOG.md.
---

{/* GENERATED FILE — do not edit.
    Source: CHANGELOG.md at the repo root, copied by website/scripts/sync-changelog.mjs
    (npm prebuild/prestart). Edit the changelog, not this file. */}

# Release notes

[![PyPI](https://img.shields.io/pypi/v/oxihipo)](https://pypi.org/project/oxihipo/)
[![Python](https://img.shields.io/pypi/pyversions/oxihipo)](https://pypi.org/project/oxihipo/)

\`pip install oxihipo\` always gets the latest release; the badges above are live.
Every version is also a
[GitHub release](https://github.com/mathieuouillon/oxihipo/releases).

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and
the project follows [SemVer](https://semver.org/spec/v2.0.0.html) — while the
version is below \`1.0.0\`, **minor releases may contain breaking changes**.

`;

mkdirSync(dirname(target), { recursive: true });
writeFileSync(target, frontmatter + body);

const version = body.match(/^## \[(\d[^\]]*)\]/m)?.[1] ?? "unknown";
console.log(
  `sync-changelog: docs/release-notes.md written (latest documented: ${version})`,
);
