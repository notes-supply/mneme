// Renders the terminal session scripts/screenshots.sh recorded, and captures Atrium's recent
// changes, each in light and dark, into the folder given. The session's output is shown as the
// terminal gave it, colours included; only the notes between commands are the script's own.
//
//   MNEME_PLAYWRIGHT=../atrium node scripts/screenshots.mjs SESSION ATRIUM_URL OUT
//
// Playwright comes from the node_modules of the folder MNEME_PLAYWRIGHT names, and runs the
// Chromium MNEME_CHROMIUM names, or else its own.
import { createRequire } from 'node:module';
import { readdir, readFile } from 'node:fs/promises';
import { join, resolve } from 'node:path';

const [session, atrium, out] = process.argv.slice(2);
const require = createRequire(join(resolve(process.env.MNEME_PLAYWRIGHT), 'package.json'));
const { chromium } = require('playwright');
// Atrium's field is WebGL, which headless Chromium draws in software.
const GL = ['--use-gl=angle', '--use-angle=swiftshader', '--enable-unsafe-swiftshader', '--ignore-gpu-blocklist'];

const escape = (text) => text.replace(/[&<>"]/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' })[c]);

// Terminal output as HTML: SGR colours and bold become spans, other escapes and carriage
// returns are dropped.
function terminal(text) {
  let html = '';
  let style = { bold: false, colour: null };
  const open = () => {
    const classes = [style.bold && 'b', style.colour !== null && `c${style.colour}`].filter(Boolean);
    return classes.length ? `<span class="${classes.join(' ')}">` : '';
  };
  let span = '';
  const parts = text
    .replace(/\r/g, '')
    .replace(/[\n\s]+$/, '')
    .split(/(\x1b\[[0-9;?]*[A-Za-z])/);
  for (const part of parts) {
    const sgr = part.match(/^\x1b\[([0-9;]*)m$/);
    if (sgr) {
      for (const code of (sgr[1] || '0').split(';').map(Number)) {
        if (code === 0) style = { bold: false, colour: null };
        else if (code === 1) style.bold = true;
        else if (code === 22) style.bold = false;
        else if (code >= 30 && code <= 37) style.colour = code - 30;
        else if (code >= 90 && code <= 97) style.colour = code - 90;
        else if (code === 39) style.colour = null;
      }
      if (span) html += '</span>';
      span = open();
      html += span;
    } else if (!part.startsWith('\x1b')) {
      html += escape(part);
    }
  }
  if (span) html += '</span>';
  return html;
}

async function steps() {
  const lines = [];
  for (const step of (await readdir(session)).sort()) {
    const dir = join(session, step);
    const files = await readdir(dir);
    if (files.includes('note')) {
      lines.push(`<span class="note"># ${escape(await readFile(join(dir, 'note'), 'utf8'))}</span>`);
      continue;
    }
    const machine = await readFile(join(dir, 'machine'), 'utf8');
    const command = await readFile(join(dir, 'command'), 'utf8');
    const output = terminal(await readFile(join(dir, 'output'), 'utf8'));
    lines.push(`<span class="prompt ${machine}">${machine}:~$</span> ${escape(command)}`);
    if (output) lines.push(output);
  }
  return lines.join('\n');
}

const page = (body) => `<!doctype html>
<html><head><meta charset="utf-8"><style>
:root {
  --bg: #fbfaf7; --fg: #1f2328; --dim: #6e7781; --frame: #dedad2;
  --laptop: #1a7f37; --desktop: #8250df;
  --c0: #1f2328; --c1: #cf222e; --c2: #1a7f37; --c3: #9a6700; --c4: #0969da; --c5: #8250df; --c6: #1b7c83; --c7: #6e7781;
}
.dark {
  --bg: #0d1117; --fg: #e6edf3; --dim: #8b949e; --frame: #30363d;
  --laptop: #3fb950; --desktop: #bc8cff;
  --c0: #8b949e; --c1: #ff7b72; --c2: #3fb950; --c3: #d29922; --c4: #58a6ff; --c5: #bc8cff; --c6: #39c5cf; --c7: #e6edf3;
}
body { margin: 0; background: transparent; }
.window {
  display: inline-block; background: var(--bg); color: var(--fg);
  border: 1px solid var(--frame); border-radius: 10px; overflow: hidden;
  font: 14px/1.55 'JetBrains Mono', 'DejaVu Sans Mono', 'Liberation Mono', monospace;
}
.bar { display: flex; gap: 8px; padding: 12px 14px; border-bottom: 1px solid var(--frame); }
.bar i { width: 12px; height: 12px; border-radius: 50%; background: var(--frame); }
pre { margin: 0; padding: 14px 20px 18px; width: 92ch; white-space: pre-wrap; overflow-wrap: anywhere; font: inherit; }
.prompt { font-weight: 700; }
.prompt.laptop { color: var(--laptop); }
.prompt.desktop { color: var(--desktop); }
.note { color: var(--dim); font-style: italic; }
.b { font-weight: 700; }
${[0, 1, 2, 3, 4, 5, 6, 7].map((n) => `.c${n} { color: var(--c${n}); }`).join('\n')}
</style></head>
<body><div class="window"><div class="bar"><i></i><i></i><i></i></div><pre>${body}</pre></div></body></html>`;

const browser = await chromium.launch({ args: GL, executablePath: process.env.MNEME_CHROMIUM || undefined });
const body = await steps();
for (const theme of ['light', 'dark']) {
  const context = await browser.newContext({ deviceScaleFactor: 2, colorScheme: theme });
  const tab = await context.newPage();
  await tab.setContent(page(body));
  if (theme === 'dark') await tab.evaluate(() => document.documentElement.classList.add('dark'));
  await tab.evaluate(() => document.fonts.ready);
  await tab.locator('.window').screenshot({ path: join(out, `terminal-${theme}.png`), omitBackground: true });
  await context.close();
  console.log(`terminal-${theme}.png`);
}

for (const theme of ['light', 'dark']) {
  const context = await browser.newContext({
    viewport: { width: 1280, height: 800 },
    colorScheme: theme,
    reducedMotion: 'reduce',
    timezoneId: 'UTC',
  });
  const tab = await context.newPage();
  await tab.goto(`${atrium}/wiki/changes`, { waitUntil: 'networkidle' });
  await tab.evaluate(() => document.fonts.ready);
  await tab
    .waitForFunction(() => document.documentElement.classList.contains('gl-on'), null, { timeout: 10000 })
    .catch(() => {});
  await tab.waitForTimeout(1500);
  await tab.screenshot({ path: join(out, `changes-${theme}.png`) });
  await context.close();
  console.log(`changes-${theme}.png`);
}
await browser.close();
