// Opens the Photoshop-check samples (dist\ps-check, made by `just ps-samples`) in three
// independent readers, and says what each made of them (docs/photoshop-checks.md):
//
//   - GIMP 3 (3.0.4 or later), through its own Python, with no window;
//   - PhotoCraft's command line (photocraft-cli), when --photocraft names it;
//   - Photopea, in a headless Edge or Chrome, on a page served from 127.0.0.1.
//
// None of them is Photoshop: they tell whether readers written from the specification,
// and from Photoshop's own files, agree with ours. Run on Windows with Node 22 or later:
//
//   node scripts/standins/standins.mjs [--samples <dir>] [--out <dir>] [--only gimp,photocraft,photopea]
//        [--gimp <gimp-console exe>] [--photocraft <photocraft-cli exe>] [--browser <msedge or chrome exe>]
//        [--report]        only print the report of the results an earlier run left in --out
//
// Photopea is a web page: the browser loads it from photopea.com, and it reads the
// samples inside the browser. Nothing is uploaded. The exit status is 1 when a reader
// disagrees with what is expected and the disagreement is not in KNOWN below.
import { spawn } from 'node:child_process';
import { createHash } from 'node:crypto';
import { createReadStream, existsSync, mkdirSync, readFileSync, readdirSync, rmSync, statSync, writeFileSync } from 'node:fs';
import { createServer } from 'node:http';
import { basename, dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { inflateSync } from 'node:zlib';

const HERE = dirname(fileURLToPath(import.meta.url));
const REPO = resolve(HERE, '..', '..');
const MINUS = '\u2212';

// What a reader is known not to do. Anything else that differs is reported as unexpected.
const KNOWN = [
  { reader: 'gimp', file: /^bigtiff-/, what: 'layers', note: 'GIMP does not read the BigTIFF layer block: it shows the composite only' },
  { reader: 'gimp', file: /^psd-16bit-rgba_/, what: 'composite', note: 'GIMP undoes the blend against white in sRGB encoding whatever the profile, so on this linear-profile sample it does not get the top layer back; PhotoCraft does (its re-save stores our merged image)' },
  { reader: 'photocraft', file: /^bigtiff-/, what: 'open', note: 'PhotoCraft refuses it: more pixel memory than its budget' },
  { reader: 'photopea', file: /^bigtiff-/, what: 'open', note: 'not given to Photopea: it had not opened the 15,000 px BigTIFF after 15 minutes (2026-11-11)' },
  { reader: 'photopea', file: /^psd-16bit-rle_/, what: 'render', note: 'Photopea misrenders PackBits 16-bit layers (GIMP and PhotoCraft read them)' },
];

function options() {
  const out = {
    samples: join(REPO, 'dist', 'ps-check'),
    out: join(REPO, 'dist', 'ps-check-standins'),
    gimp: 'C:\\Program Files\\GIMP 3\\bin\\gimp-console-3.exe',
    photocraft: null,
    browser: ['C:\\Program Files (x86)\\Microsoft\\Edge\\Application\\msedge.exe', 'C:\\Program Files\\Microsoft\\Edge\\Application\\msedge.exe',
      'C:\\Program Files\\Google\\Chrome\\Application\\chrome.exe'].find((path) => existsSync(path)) ?? null,
    only: ['gimp', 'photocraft', 'photopea'],
    report: false,
  };
  const argv = process.argv.slice(2);
  for (let i = 0; i < argv.length; i++) {
    const key = argv[i].replace(/^--/, '');
    if (key === 'report') out.report = true;
    else if (key === 'only') out.only = argv[++i].split(',');
    else if (key in out && argv[i].startsWith('--')) out[key] = resolve(argv[++i]);
    else throw new Error(`unknown argument ${argv[i]}`);
  }
  return out;
}

/** The samples, smallest first, the BigTIFF last: it is the one a reader may not finish. */
function samplesIn(dir) {
  return readdirSync(dir)
    .filter((name) => /\.(psd|psb|tiff?)$/i.test(name))
    .map((name) => ({ name, path: join(dir, name), size: statSync(join(dir, name)).size }))
    .sort((a, b) => (/^bigtiff-/.test(a.name) - /^bigtiff-/.test(b.name)) || a.size - b.size);
}

function run(exe, argv, { env = process.env, timeoutMs = 600000 } = {}) {
  return new Promise((done) => {
    const child = spawn(exe, argv, { env, windowsHide: true });
    const out = [];
    const err = [];
    let timedOut = false;
    const timer = setTimeout(() => {
      timedOut = true;
      spawn('taskkill', ['/PID', String(child.pid), '/T', '/F'], { windowsHide: true });
    }, timeoutMs);
    child.stdout.on('data', (chunk) => out.push(chunk));
    child.stderr.on('data', (chunk) => err.push(chunk));
    child.on('error', (error) => { clearTimeout(timer); done({ code: null, stdout: '', stderr: String(error), timedOut }); });
    child.on('close', (code) => {
      clearTimeout(timer);
      done({ code, stdout: Buffer.concat(out).toString('utf8'), stderr: Buffer.concat(err).toString('utf8'), timedOut });
    });
  });
}

// --- GIMP ---

async function gimp(opts, samples) {
  const out = join(opts.out, 'gimp.jsonl');
  rmSync(out, { force: true });
  const sources = existsSync(join(opts.samples, 'sources'))
    ? readdirSync(join(opts.samples, 'sources')).map((name) => join(opts.samples, 'sources', name))
    : [];
  // The small files share one GIMP; each large one gets its own, so that one that does
  // not finish costs only itself.
  const small = samples.filter((s) => s.size < 64 << 20 && !/^bigtiff-/.test(s.name)).map((s) => s.path);
  const large = samples.filter((s) => !small.includes(s.path)).map((s) => [s.path]);
  const log = [];
  for (const batch of [small, sources, ...large].filter((b) => b.length)) {
    const result = await run(
      opts.gimp,
      ['-n', '-i', '-d', '-f', '--batch-interpreter', 'python-fu-eval', '-b',
        "import os;exec(open(os.environ['PS_GIMP_SCRIPT'],encoding='utf-8').read())", '--quit'],
      { env: { ...process.env, PS_GIMP_SCRIPT: join(HERE, 'gimp_check.py'), PS_GIMP_OUT: out, PS_GIMP_FILES: batch.join('|') }, timeoutMs: 15 * 60000 },
    );
    const what = `${batch.length} file(s) from ${basename(batch[0])}`;
    console.log(`  gimp: ${what}: ${result.timedOut ? 'not finished in 15 minutes' : `exit ${result.code}`}`);
    log.push(`== ${what}: exit ${result.code}${result.timedOut ? ' (stopped after 15 minutes)' : ''}`, result.stderr);
  }
  writeFileSync(join(opts.out, 'gimp.log'), log.join('\n'));
}

// --- PhotoCraft ---

/** A PNG's header and a digest of its unpacked scanlines: equal for equal pixels from one encoder. */
function pngDigest(path) {
  const data = readFileSync(path);
  let header = null;
  const parts = [];
  for (let at = 8; at < data.length;) {
    const length = data.readUInt32BE(at);
    const kind = data.toString('latin1', at + 4, at + 8);
    if (kind === 'IHDR') header = { width: data.readUInt32BE(at + 8), height: data.readUInt32BE(at + 12), bitDepth: data[at + 16], colourType: data[at + 17] };
    if (kind === 'IDAT') parts.push(data.subarray(at + 8, at + 8 + length));
    at += 12 + length;
  }
  return { ...header, digest: createHash('sha1').update(inflateSync(Buffer.concat(parts))).digest('hex').slice(0, 16) };
}

/** The merged image a PSD or PSB stores: its planes, big-endian, unpacked. */
function mergedImage(path) {
  const d = readFileSync(path);
  const version = d.readUInt16BE(4);
  const image = { channels: d.readUInt16BE(12), height: d.readUInt32BE(14), width: d.readUInt32BE(18), depth: d.readUInt16BE(22) };
  let at = 26;
  at += 4 + d.readUInt32BE(at); // the colour mode data
  at += 4 + d.readUInt32BE(at); // the image resources
  at += version === 1 ? 4 + d.readUInt32BE(at) : 8 + Number(d.readBigUInt64BE(at)); // the layers
  const compression = d.readUInt16BE(at);
  at += 2;
  const total = image.channels * image.height * image.width * (image.depth / 8);
  if (compression === 0) return { ...image, data: d.subarray(at, at + total) };
  if (compression !== 1) throw new Error(`merged image compression ${compression}`);
  const rows = image.channels * image.height;
  const countSize = version === 1 ? 2 : 4;
  const counts = at;
  at += rows * countSize;
  const data = Buffer.allocUnsafe(total);
  let to = 0;
  for (let row = 0; row < rows; row++) { // PackBits, a row at a time
    const end = at + (countSize === 2 ? d.readUInt16BE(counts + row * 2) : d.readUInt32BE(counts + row * 4));
    while (at < end) {
      const code = d[at++];
      if (code < 128) { d.copy(data, to, at, at + code + 1); to += code + 1; at += code + 1; }
      else if (code > 128) { data.fill(d[at++], to, to + 257 - code); to += 257 - code; }
    }
  }
  return { ...image, data };
}

/** How far PhotoCraft's re-save of a file is from the merged image the file stores.
 *  PhotoCraft merges every layer and ours is the top layer alone (spec 5.6), so on a
 *  picture with transparency the two are compared where the top layer alone decides:
 *  where it is opaque, where it is clear, and where both store the same alpha. */
function compareMerged(oursPath, theirsPath) {
  const ours = mergedImage(oursPath);
  const theirs = mergedImage(theirsPath);
  if (['channels', 'height', 'width', 'depth'].some((key) => ours[key] !== theirs[key])) {
    return { error: `it is ${theirs.width}x${theirs.height}, ${theirs.channels} channels, ${theirs.depth}-bit` };
  }
  const wide = ours.depth === 16;
  const max = wide ? 65535 : 255;
  const count = ours.width * ours.height;
  const at = (image, channel, i) => (wide ? image.data.readUInt16BE((channel * count + i) * 2) : image.data[channel * count + i]);
  const out = { opaque: 0, clear: 0, sameAlpha: 0, sameAlphaPixels: 0 };
  for (let i = 0; i < count; i++) {
    const a = ours.channels > 3 ? at(ours, 3, i) : max;
    const b = ours.channels > 3 ? at(theirs, 3, i) : max;
    const key = a === max && b === max ? 'opaque' : a === 0 && b === 0 ? 'clear' : a === b ? 'sameAlpha' : null;
    if (!key) continue;
    if (key === 'sameAlpha') out.sameAlphaPixels++;
    for (let channel = 0; channel < 3; channel++) {
      const difference = Math.abs(at(ours, channel, i) - at(theirs, channel, i));
      if (difference > out[key]) out[key] = difference;
    }
  }
  return out;
}

async function photocraft(opts, samples) {
  const pngs = join(opts.out, 'photocraft');
  mkdirSync(pngs, { recursive: true });
  const results = {};
  for (const sample of samples) {
    const record = { file: sample.name };
    const info = await run(opts.photocraft, ['info', '--compact', sample.path], { timeoutMs: 10 * 60000 });
    record.exit = info.code;
    record.stderr = info.stderr.trim().slice(0, 500);
    if (info.code === 0) {
      const doc = JSON.parse(info.stdout);
      record.info = {
        width: doc.width, height: doc.height, depth: doc.depth, mode: doc.mode, resolution: doc.resolution, warnings: doc.warnings,
        layers: doc.layers.map((l) => ({
          name: l.name, blend: l.blend, opacity: l.opacity, fill: l.fill, visible: l.visible, hasMask: l.hasMask,
          clipped: l.clipped, kind: l.kind, bounds: l.bounds,
        })),
      };
      const png = join(pngs, `${sample.name}.png`);
      rmSync(png, { force: true });
      const converted = await run(opts.photocraft, ['convert', sample.path, png], { timeoutMs: 10 * 60000 });
      if (converted.code === 0 && existsSync(png)) record.png = pngDigest(png);
      else record.pngError = converted.stderr.trim().slice(0, 300);
      const kind = /\.(psd|psb)$/i.exec(sample.name);
      if (kind && sample.size < 64 << 20) {
        // What PhotoCraft stores as the merged image when it saves the same document.
        const resaved = join(pngs, `${sample.name}.resaved.${kind[1]}`);
        rmSync(resaved, { force: true });
        const saved = await run(opts.photocraft, ['convert', sample.path, resaved], { timeoutMs: 10 * 60000 });
        try {
          record.merged = saved.code === 0 ? compareMerged(sample.path, resaved) : { error: saved.stderr.trim().slice(0, 300) };
        } catch (error) {
          record.merged = { error: String(error) };
        }
        rmSync(resaved, { force: true });
      }
    }
    console.log(`  photocraft: ${sample.name}: exit ${info.code}`);
    results[sample.name] = record;
  }
  const version = await run(opts.photocraft, ['--version']);
  writeFileSync(join(opts.out, 'photocraft.json'), JSON.stringify({ version: version.stdout.trim(), results }, null, 1));
}

// --- Photopea ---

const freePort = () => new Promise((done) => {
  const probe = createServer();
  probe.listen(0, '127.0.0.1', () => { const { port } = probe.address(); probe.close(() => done(port)); });
});
const sleep = (ms) => new Promise((done) => setTimeout(done, ms));

async function photopea(opts, samples) {
  // Only this page and the samples, by their bare names, and only to this machine.
  const server = createServer((request, response) => {
    const path = decodeURIComponent(new URL(request.url, 'http://127.0.0.1').pathname);
    const file = path === '/' ? join(HERE, 'photopea.html') : path.startsWith('/files/') ? join(opts.samples, basename(path)) : null;
    if (!file || !existsSync(file) || !statSync(file).isFile()) { response.writeHead(404).end(); return; }
    response.writeHead(200, {
      'Content-Type': path === '/' ? 'text/html; charset=utf-8' : 'application/octet-stream',
      'Content-Length': statSync(file).size, 'Cache-Control': 'no-store',
    });
    createReadStream(file).pipe(response);
  });
  await new Promise((done) => server.listen(0, '127.0.0.1', done));
  const page = `http://127.0.0.1:${server.address().port}/`;
  const debugPort = await freePort();
  const profile = join(opts.out, 'browser-profile');
  const browser = spawn(opts.browser, ['--headless=new', `--remote-debugging-port=${debugPort}`, `--user-data-dir=${profile}`,
    '--no-first-run', '--disable-extensions', '--window-size=1600,1000', page], { stdio: 'ignore', windowsHide: true });
  let socket = null;
  try {
    let target = null;
    for (let i = 0; i < 60 && !target; i++) {
      await sleep(500);
      try {
        target = (await (await fetch(`http://127.0.0.1:${debugPort}/json/list`)).json()).find((t) => t.type === 'page' && t.url.startsWith(page));
      } catch { /* the browser is still starting */ }
    }
    if (!target) throw new Error('the browser did not open the page');
    socket = new WebSocket(target.webSocketDebuggerUrl);
    await new Promise((done, failed) => { socket.addEventListener('open', done); socket.addEventListener('error', failed); });
    let next = 0;
    const waiting = new Map();
    socket.addEventListener('message', (event) => {
      const message = JSON.parse(event.data);
      if (message.id && waiting.has(message.id)) { waiting.get(message.id)(message); waiting.delete(message.id); }
    });
    const evaluate = (expression) => new Promise((done, failed) => {
      const id = ++next;
      waiting.set(id, (reply) => {
        if (reply.error || reply.result.exceptionDetails) failed(new Error(JSON.stringify(reply.error ?? reply.result.exceptionDetails).slice(0, 400)));
        else done(reply.result.result.value);
      });
      socket.send(JSON.stringify({ id, method: 'Runtime.evaluate', params: { expression, awaitPromise: true, returnByValue: true } }));
    });
    for (let i = 0; i < 120 && !(await evaluate('window.pp && window.pp.ready')); i++) await sleep(1000);
    if (!(await evaluate('window.pp && window.pp.ready'))) throw new Error('Photopea did not load (is this machine online?)');
    const given = samples.filter((s) => !/^bigtiff-/.test(s.name)); // see KNOWN
    await evaluate(`void window.pp.run(${JSON.stringify(given.map((s) => s.name))}, ${10 * 60000})`);
    let last = null;
    for (;;) {
      const running = await evaluate('window.pp.running');
      if (running === null) break;
      if (running !== last) { console.log(`  photopea: ${running}`); last = running; }
      await sleep(2000);
    }
    writeFileSync(join(opts.out, 'photopea.json'), JSON.stringify(JSON.parse(await evaluate('JSON.stringify(window.pp.results)')), null, 1));
  } finally {
    socket?.close();
    spawn('taskkill', ['/PID', String(browser.pid), '/T', '/F'], { windowsHide: true });
    server.close();
    await sleep(1500);
    try { rmSync(profile, { recursive: true, force: true }); } catch { /* the browser may still hold a file */ }
  }
}

// --- What is expected, and the report ---

function expectedLayers(name) {
  const match = /^(.*)_(\d+)\.(psd|psb|tiff?)$/i.exec(name);
  if (!match) return null;
  if (match[1] === 'DSC_0039') {
    return ['DSC_0039 (0 EV)', `DSC_0040 (${MINUS}2 EV)`, `DSC_0041 (${MINUS}1 EV)`, 'DSC_0042 (+1 EV)', 'DSC_0043 (+2 EV)'];
  }
  const count = Number(match[2]);
  return count <= 3 ? [`${match[1]} (0 EV)`, `${match[1]}-2 (${MINUS}1 EV)`, `${match[1]}-3 (+1 EV)`].slice(0, count) : null;
}

function expectedProfile(name) {
  if (/^(psd-16bit|psb-wide-16bit|tiff-16bit|bigtiff-16bit)/.test(name)) return 'Photostacker linear sRGB';
  if (/^psd-8bit-adobe-rgb/.test(name)) return 'Photostacker Adobe RGB (1998)';
  if (/^DSC_0039/.test(name)) return 'Photostacker sRGB';
  return null;
}

/** The files that hold the same pictures in different containers. */
const SAME = [
  ['psd-8bit-rle_3.psd', 'tiff-8bit_3.tif'],
  ['psd-16bit-zip_3.psd', 'psd-16bit-rle_3.psd', 'tiff-16bit_3.tif'],
  ['psd-8bit-rgba_3.psd', 'tiff-8bit-rgba_3.tif'],
  ['DSC_0039_5.psd', 'DSC_0039_5.tif'],
];
const same = (a, b) => JSON.stringify(a) === JSON.stringify(b);
const colour = (pixels) => pixels?.planes?.slice(0, pixels.planes.length >= 3 ? 3 : 1) ?? null;
const hasAlpha = (pixels) => [2, 4].includes(pixels?.planes?.length);

function report(opts, samples) {
  const read = (name) => (existsSync(join(opts.out, name)) ? readFileSync(join(opts.out, name), 'utf8') : null);
  const gimpRecords = Object.fromEntries((read('gimp.jsonl') ?? '').split('\n').filter(Boolean).map((line) => JSON.parse(line)).map((r) => [r.file, r]));
  const craft = JSON.parse(read('photocraft.json') ?? '{"results":{}}');
  const pea = JSON.parse(read('photopea.json') ?? '{}');
  const issues = [];
  const issue = (reader, file, what, detail) => issues.push({ reader, file, what, detail });
  const said = [];

  for (const { name } of samples) {
    const want = expectedLayers(name);
    const tiff = /\.tiff?$/i.test(name);
    const lines = [];

    const g = gimpRecords[name];
    if (g) {
      if (g.error) issue('gimp', name, 'open', g.error.trim().split('\n').pop());
      else {
        const names = g.layers.map((l) => l.name);
        const below = want ? names.slice(want.length) : [];
        if (want && !(same(names.slice(0, want.length), want) && same(below, tiff ? ['Background'] : []))) issue('gimp', name, 'layers', `it shows ${JSON.stringify(names)}`);
        const plain = g.layers.every((l) => l.visible === true && l.opacity === 100 && l.mode === 'normal' && l.mask === false
          && l.is_group === false && l.locks && !Object.values(l.locks).some(Boolean));
        if (!plain) issue('gimp', name, 'flags', 'a layer is hidden, locked, masked, not Normal or not at 100 %');
        if ((g.profile?.label ?? null) !== expectedProfile(name)) issue('gimp', name, 'profile', `it reads ${JSON.stringify(g.profile?.label ?? null)}, expected ${JSON.stringify(expectedProfile(name))}`);
        if (!same(g.resolution, [72, 72])) issue('gimp', name, 'resolution', JSON.stringify(g.resolution));
        const top = g.layers[0].pixels;
        if (tiff && names.length > 1 && colour(top) && !same(colour(g.layers.at(-1).pixels), colour(top))) issue('gimp', name, 'composite', "the TIFF's own image is not the top layer");
        let composite = '';
        if (g.stored_composite && colour(top)) {
          const stored = g.stored_composite.layers[0].pixels;
          if (same(colour(stored), colour(top))) composite = '; the merged image is the top layer';
          else if (hasAlpha(top) && g.stored_composite_vs_top && g.stored_composite_vs_top.worst <= 2) {
            composite = `; the merged image, unblended, is the top layer (within ${g.stored_composite_vs_top.worst})`;
          } else issue('gimp', name, 'composite', `the merged image differs from the top layer${g.stored_composite_vs_top ? ` by up to ${g.stored_composite_vs_top.worst} where it is at least half opaque` : ''}`);
        } else if (g.stored_composite_error) issue('gimp', name, 'composite', g.stored_composite_error.trim().split('\n').pop());
        // A sample stacked from files kept in `sources`: each layer against its source, as GIMP reads both.
        const stem = name.replace(/_\d+\.[a-z]+$/i, '');
        let sources = '';
        for (const [index, layer] of g.layers.slice(0, want?.length ?? 0).entries()) {
          const source = ['png', 'jpg'].map((ext) => gimpRecords[`${stem}${index ? `-${index + 1}` : ''}.${ext}`]).find((r) => r && !r.error);
          if (!source || !layer.pixels?.planes) continue;
          const theirs = source.layers[0].pixels;
          const gray = theirs.planes.length < 3;
          // A gray source must be the layer's red, green and blue; a JPEG is decoded by two
          // decoders, so its samples are compared at four points, within 3.
          const ok = gray
            ? layer.pixels.planes.slice(0, 3).every((plane) => plane === theirs.planes[0])
            : Object.keys(theirs.samples).every((at) => theirs.samples[at].slice(0, 3).every((v, c) => Math.abs(v - layer.pixels.samples[at][c]) <= 3));
          if (ok) sources = "; the layers hold their sources' samples";
          else issue('gimp', name, 'sources', `layer ${index + 1} does not hold the samples of ${source.file}`);
        }
        lines.push(`GIMP ${g.gimp}: ${names.length} layer(s), ${g.precision}, profile ${g.profile?.label ?? 'none'}${composite}${sources}`);
      }
    } else if (opts.only.includes('gimp')) issue('gimp', name, 'open', 'no result: GIMP did not finish');

    const c = craft.results[name];
    if (c) {
      if (c.exit !== 0) issue('photocraft', name, 'open', c.stderr);
      else {
        const names = c.info.layers.map((l) => l.name);
        if (want && !same(names, want)) issue('photocraft', name, 'layers', `it shows ${JSON.stringify(names)}`);
        if (c.info.warnings.length) issue('photocraft', name, 'warnings', JSON.stringify(c.info.warnings));
        const plain = c.info.layers.every((l) => l.blend === 'Normal' && l.opacity === 1 && l.fill === 1 && l.visible && !l.hasMask && !l.clipped
          && l.kind === 'Pixel' && same(l.bounds, [0, 0, c.info.width, c.info.height]));
        if (!plain) issue('photocraft', name, 'flags', 'a layer is hidden, masked, clipped, not Normal, not at 100 % or not the full canvas');
        if (c.info.depth !== (/16bit|^DSC_/.test(name) ? 16 : 8)) issue('photocraft', name, 'depth', String(c.info.depth));
        if (c.info.resolution !== 72) issue('photocraft', name, 'resolution', String(c.info.resolution));
        let merged = '';
        if (c.merged) {
          if (c.merged.error || c.merged.opaque > 0 || c.merged.clear > 0 || c.merged.sameAlpha > 1) {
            issue('photocraft', name, 'merged', `its re-save stores another merged image: ${JSON.stringify(c.merged)}`);
          } else merged = '; its re-save stores our merged image';
        }
        lines.push(`PhotoCraft: ${names.length} layer(s), ${c.info.depth}-bit, no warning${merged}`);
      }
    }

    const p = pea[name];
    if (p) {
      if (!p.opened) issue('photopea', name, 'open', p.error ?? 'it did not open');
      else if (!p.info) issue('photopea', name, 'layers', p.infoError ?? 'no answer');
      else {
        const names = p.info.layers.map((l) => l.name);
        if (want && !same(names, want)) issue('photopea', name, 'layers', `it shows ${JSON.stringify(names)}`);
        const plain = p.info.layers.every((l) => l.visible === true && l.opacity === 100 && l.fill === 100 && l.blend === 'norm' && l.allLocked === false
          && l.pixelsLocked === false && l.positionLocked === false && l.transparentPixelsLocked === false && l.background === false
          && same(l.bounds, [0, 0, p.info.width, p.info.height]));
        if (!plain) issue('photopea', name, 'flags', 'a layer is hidden, locked, a background, not Normal, not at 100 % or not the full canvas');
        if (p.info.resolution !== 72) issue('photopea', name, 'resolution', String(p.info.resolution));
        let render = '';
        const top = g && !g.error ? g.layers[0].pixels : null;
        if (!p.png) issue('photopea', name, 'render', p.pngError ?? 'no PNG');
        else if (top?.sample_bytes === 1 && (top.alpha_high_byte ?? [255])[0] === 255) {
          // An 8-bit picture with no transparency: Photopea's PNG must be GIMP's top layer, sample for sample.
          if (same(p.png.planes.slice(0, 3), colour(top))) render = "; its render is GIMP's top layer";
          else issue('photopea', name, 'render', "its render is not GIMP's top layer");
        }
        lines.push(`Photopea: ${names.length} layer(s)${render}`);
      }
    } else if (opts.only.includes('photopea') && /^bigtiff-/.test(name)) issue('photopea', name, 'open', 'not given to it');
    said.push([name, lines]);
  }

  for (const group of SAME) {
    const all = (records, pick) => group.map((name) => pick(records[name])).filter((value) => value !== undefined);
    const gimpLayers = all(gimpRecords, (r) => (r && !r.error ? r.layers.slice(0, expectedLayers(r.file).length).map((l) => l.pixels?.planes) : undefined));
    if (gimpLayers.length > 1 && !gimpLayers.every((layers) => same(layers, gimpLayers[0]))) issue('gimp', group.join(' = '), 'pixels', 'the layers differ between the containers');
    const craftPngs = all(craft.results, (r) => r?.png?.digest);
    if (craftPngs.length > 1 && new Set(craftPngs).size > 1) issue('photocraft', group.join(' = '), 'pixels', 'its renders differ between the containers');
    // Photopea: the odd one out is named, when most of a group agree.
    // A picture with transparency is compared by its coverage and a few samples: Photopea's
    // own blend of partly transparent layers rounds differently from one container to the other.
    const fingerprint = (png) => JSON.stringify(png.alpha[0] === 255 ? png.planes : [png.clear, png.solid, Math.round(png.alphaMean * 10), png.samples]);
    const renders = group.filter((name) => pea[name]?.png).map((name) => [name, fingerprint(pea[name].png)]);
    const counts = new Map();
    for (const [, planes] of renders) counts.set(planes, (counts.get(planes) ?? 0) + 1);
    const most = [...counts.entries()].sort((a, b) => b[1] - a[1])[0];
    if (counts.size > 1 && most[1] * 2 > renders.length) {
      for (const [name, planes] of renders) if (planes !== most[0]) issue('photopea', name, 'render', 'its render differs from the same picture in the other containers');
    } else if (counts.size > 1) issue('photopea', group.join(' = '), 'render', 'its renders differ between the containers');
  }

  const known = (i) => KNOWN.find((k) => k.reader === i.reader && k.file.test(i.file) && k.what === i.what);
  const merged = new Map();
  for (const i of issues) merged.set(`${i.reader}|${i.file}|${i.what}`, i); // one line per reader, file and kind
  const list = [...merged.values()];
  for (const [name, lines] of said) {
    console.log(name);
    for (const line of lines) console.log(`  ${line}`);
    for (const i of list.filter((each) => each.file === name)) console.log(`  ${known(i) ? 'known limit' : 'UNEXPECTED'} (${i.reader}, ${i.what}): ${known(i)?.note ?? i.detail}`);
  }
  for (const i of list.filter((each) => each.file.includes(' = '))) console.log(`${known(i) ? 'known limit' : 'UNEXPECTED'} (${i.reader}, ${i.what}) ${i.file}: ${i.detail}`);
  const unexpected = list.filter((i) => !known(i));
  console.log(`\n${samples.length} samples; ${list.length - unexpected.length} known limit(s) met; ${unexpected.length} unexpected difference(s).`);
  return unexpected.length === 0;
}

const opts = options();
if (!existsSync(opts.samples)) throw new Error(`no samples in ${opts.samples}: run \`just ps-samples\` first (docs/photoshop-checks.md)`);
mkdirSync(opts.out, { recursive: true });
const samples = samplesIn(opts.samples);
if (!opts.report) {
  if (opts.only.includes('gimp')) {
    if (existsSync(opts.gimp)) await gimp(opts, samples);
    else console.log(`  gimp: skipped, ${opts.gimp} is not there (--gimp <gimp-console exe>)`);
  }
  if (opts.only.includes('photocraft')) {
    if (opts.photocraft && existsSync(opts.photocraft)) await photocraft(opts, samples);
    else console.log('  photocraft: skipped, give --photocraft <photocraft-cli exe>');
  }
  if (opts.only.includes('photopea')) {
    if (opts.browser && existsSync(opts.browser)) await photopea(opts, samples);
    else console.log('  photopea: skipped, no Edge or Chrome found (--browser <exe>)');
  }
  console.log('');
}
process.exitCode = report(opts, samples) ? 0 : 1;
