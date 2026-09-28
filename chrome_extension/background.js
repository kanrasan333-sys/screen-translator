// Two things live here.
//
// Right-click entries that save an image in the format you pick.  Chrome's own
// "Save image as…" writes whatever bytes the server sent, which these days is
// mostly WebP or AVIF, and the type box in its dialog can't change that.  These
// entries fetch the same image, re-encode it, and only then open the ordinary
// Save As dialog — with the right extension already in the name.
//
// And a full-page screenshot: the whole page, top to bottom, wherever the
// reader happens to be scrolled — without scrolling through it.  See
// `captureFullPage`.

const FORMATS = {
  png: { label: 'PNG…', mime: 'image/png', ext: 'png' },
  jpg: { label: 'JPG…', mime: 'image/jpeg', ext: 'jpg', quality: 0.92 },
  webp: { label: 'WebP…', mime: 'image/webp', ext: 'webp', quality: 0.92 },
};

const PARENT_ID = 'save-image-as';
const FULL_PAGE_ID = 'full-page';

chrome.runtime.onInstalled.addListener(() => {
  // Menus outlive the service worker, so they're built once per install or
  // update rather than on every wake-up.
  chrome.contextMenus.removeAll(() => {
    chrome.contextMenus.create({
      id: PARENT_ID,
      title: chrome.i18n.getMessage('menuParent'),
      contexts: ['image'],
    });
    for (const [id, format] of Object.entries(FORMATS)) {
      chrome.contextMenus.create({ id, parentId: PARENT_ID, title: format.label, contexts: ['image'] });
    }
    chrome.contextMenus.create({
      id: FULL_PAGE_ID,
      title: chrome.i18n.getMessage('actionFullPage'),
      contexts: ['page', 'selection', 'link', 'image'],
    });
  });
});

// The same screenshot from the toolbar button, the keyboard shortcut and the
// page's context menu.
chrome.action.onClicked.addListener((tab) => fullPage(tab));
chrome.commands.onCommand.addListener((command, tab) => {
  if (command === FULL_PAGE_ID) fullPage(tab);
});

chrome.contextMenus.onClicked.addListener((info, tab) => {
  if (info.menuItemId === FULL_PAGE_ID) {
    fullPage(tab);
    return;
  }
  const format = FORMATS[info.menuItemId];
  if (!format || !info.srcUrl) return;
  saveImage(info, tab, format).catch((err) => report(tab, info.frameId, err));
});

// A converted image lives as a blob: URL in the offscreen document until Chrome
// has finished writing it — or the dialog was cancelled.
chrome.downloads.onChanged.addListener(async (delta) => {
  const state = delta.state?.current;
  if (state !== 'complete' && state !== 'interrupted') return;
  const [item] = await chrome.downloads.search({ id: delta.id });
  if (item?.url.startsWith(`blob:chrome-extension://${chrome.runtime.id}/`)) release(item.url);
});

async function saveImage(info, tab, format) {
  let url;
  try {
    url = await convert(info.srcUrl, format);
  } catch (err) {
    // The extension fetches without the page's referrer and can't see the
    // page's blob: URLs, so hotlink-protected and script-made images only
    // open from inside the page itself.
    if (err.code !== 'fetch' || !(tab?.id >= 0)) throw err;
    const dataUrl = await readFromPage(tab.id, info.frameId, info.srcUrl).catch(() => {
      throw err;
    });
    url = await convert(dataUrl, format);
  }

  const download = (filename) => chrome.downloads.download({ url, filename, saveAs: true });
  try {
    await download(`${baseName(info.srcUrl)}.${format.ext}`).catch((err) =>
      // Chrome has a longer list of names it refuses than any sanitiser keeps up with.
      /filename/i.test(err.message) ? download(`image.${format.ext}`) : Promise.reject(err),
    );
  } catch (err) {
    release(url);
    throw Object.assign(err, { code: 'download', detail: err.message });
  }
}

// The offscreen document is a single shared page; conversions and the release
// that may close it take turns, so it is never closed under a conversion.
let offscreenQueue = Promise.resolve();

function withOffscreen(task) {
  const run = offscreenQueue.then(task, task);
  offscreenQueue = run.catch(() => {});
  return run;
}

async function hasOffscreen() {
  const contexts = await chrome.runtime.getContexts({ contextTypes: ['OFFSCREEN_DOCUMENT'] });
  return contexts.length > 0;
}

async function ensureOffscreen() {
  if (!(await hasOffscreen())) {
    await chrome.offscreen.createDocument({
      url: 'offscreen.html',
      reasons: ['BLOBS'],
      justification: 'Decode images, re-encode them, and join screenshot tiles into one picture.',
    });
  }
}

function convert(src, format) {
  return withOffscreen(async () => {
    await ensureOffscreen();
    const reply = await chrome.runtime.sendMessage({
      target: 'offscreen',
      type: 'convert',
      src,
      mime: format.mime,
      quality: format.quality,
    });
    if (!reply?.url) {
      throw Object.assign(new Error(reply?.detail ?? 'no reply'), {
        code: reply?.code ?? 'encode',
        detail: reply?.detail,
      });
    }
    return reply.url;
  });
}

function release(url) {
  return withOffscreen(async () => {
    if (!(await hasOffscreen())) return;
    const reply = await chrome.runtime.sendMessage({ target: 'offscreen', type: 'release', url });
    // An idle offscreen page still holds a renderer process; don't keep it.
    if (reply?.live === 0) await chrome.offscreen.closeDocument();
  }).catch(() => {});
}

async function readFromPage(tabId, frameId, src) {
  const [injection] = await chrome.scripting.executeScript({
    target: { tabId, frameIds: [frameId ?? 0] },
    args: [src],
    func: async (src) => {
      const res = await fetch(src);
      if (!res.ok) throw new Error(`HTTP ${res.status}`);
      const blob = await res.blob();
      return new Promise((resolve, reject) => {
        const reader = new FileReader();
        reader.onload = () => resolve(reader.result);
        reader.onerror = () => reject(reader.error);
        reader.readAsDataURL(blob);
      });
    },
  });
  if (typeof injection?.result !== 'string') throw new Error('page fetch failed');
  return injection.result;
}

// The last path segment without its extension, made safe for Windows.
function baseName(src) {
  let name = '';
  try {
    const url = new URL(src);
    if (['http:', 'https:', 'file:'].includes(url.protocol)) {
      name = url.pathname.split('/').pop();
      name = decodeURIComponent(name);
    }
  } catch {
    // A malformed escape leaves the segment as it was.
  }
  return safeName(name.replace(/\.[^.]*$/, '')) || 'image';
}

// A file name Windows will accept.
function safeName(name) {
  name = name
    .replace(/[\\/:*?"<>|\x00-\x1f]+/g, '_')
    .replace(/^[\s.~]+|[\s.]+$/g, '')
    .slice(0, 100);
  if (/^(con|prn|aux|nul|com\d|lpt\d)$/i.test(name)) name = `_${name}`;
  return name;
}

function report(tab, frameId, err) {
  console.error(err);
  const messages = {
    fetch: ['errFetch', [err.detail ?? err.message]],
    decode: ['errDecode', []],
    encode: ['errEncode', [err.detail ?? err.message]],
    download: ['errDownload', [err.detail ?? err.message]],
    capture: ['errCapture', [err.detail ?? err.message]],
  };
  const [key, subs] = messages[err.code] ?? ['errDownload', [err.message]];
  const text = chrome.i18n.getMessage(key, subs);
  if (!(tab?.id >= 0)) return;
  chrome.scripting
    .executeScript({ target: { tabId: tab.id, frameIds: [frameId ?? 0] }, args: [text], func: (text) => alert(text) })
    .catch(() => {});
}

// ============================================================
// Full-page screenshot
// ============================================================

// Tiles are captured this many CSS pixels tall.  One capture of a whole long
// page runs into the GPU's texture limit and comes back blank or cut off.
const TILE_H = 4000;

// A canvas can't be taller than this, in device pixels.  A page longer than
// that is saved up to here.
const MAX_CANVAS_H = 32000;

// How long to wait for images the page only loads on the way down.
const LAZY_WAIT_MS = 2500;

// Tabs with a capture already running, so a double press doesn't attach twice.
const capturing = new Set();

async function fullPage(tab) {
  if (!(tab?.id >= 0) || capturing.has(tab.id)) return;
  capturing.add(tab.id);
  try {
    const tiles = await captureFullPage(tab.id);
    const url = await withOffscreen(async () => {
      await ensureOffscreen();
      const reply = await chrome.runtime.sendMessage({ target: 'offscreen', type: 'stitch', tiles });
      if (!reply?.url) {
        throw Object.assign(new Error(reply?.detail ?? 'no reply'), { code: 'encode', detail: reply?.detail });
      }
      return reply.url;
    });
    const name = `${safeName(tab.title ?? '') || 'page'} ${timestamp()}.png`;
    try {
      // Straight into Downloads, no dialog: the point is a snapshot in one
      // keystroke.  Chrome's download bubble shows where it went.
      await chrome.downloads.download({ url, filename: name }).catch((err) =>
        /filename/i.test(err.message)
          ? chrome.downloads.download({ url, filename: `page ${timestamp()}.png` })
          : Promise.reject(err),
      );
    } catch (err) {
      release(url);
      throw Object.assign(err, { code: 'download', detail: err.message });
    }
  } catch (err) {
    report(tab, 0, err);
  } finally {
    capturing.delete(tab.id);
  }
}

// Renders the whole page through the DevTools protocol and returns it as PNG
// tiles, top to bottom.
//
// A screenshot from outside the browser can only show what is on screen, so
// the page would have to be scrolled through and the pieces stitched.  Chrome
// can instead render parts of the page that are off screen:
// `captureBeyondViewport` draws a clip of the document wherever it lies,
// without moving the view.  Two things still need the page itself:
//
// * Sticky and fixed elements are laid out for the current scroll position, so
//   a header stuck to the top of the window would show up in the middle of the
//   picture and be missing from the top.  The page is put at the top for the
//   duration — instantly, no animation — and put back afterwards.
// * Images marked `loading="lazy"` only load once scrolled near, and nothing is
//   scrolled near.  They're switched to eager and given a moment to arrive.
//
// Chrome shows its "started debugging this browser" bar while attached; that's
// the price of the protocol, and it goes away with the detach.
async function captureFullPage(tabId) {
  const target = { tabId };
  try {
    await chrome.debugger.attach(target, '1.3');
  } catch (err) {
    // chrome:// pages, the Web Store, or DevTools already attached.
    throw Object.assign(err, { code: 'capture', detail: err.message });
  }
  const send = (method, params = {}) => chrome.debugger.sendCommand(target, method, params);

  try {
    const prepared = await send('Runtime.evaluate', {
      expression: `(${preparePage})(${LAZY_WAIT_MS})`,
      awaitPromise: true,
      returnByValue: true,
    });
    const { x, y, dpr } = prepared?.result?.value ?? { x: 0, y: 0, dpr: 1 };

    try {
      const metrics = await send('Page.getLayoutMetrics');
      const size = metrics.cssContentSize ?? metrics.contentSize;
      const width = Math.ceil(size.width);
      const height = Math.min(Math.ceil(size.height), Math.floor(MAX_CANVAS_H / Math.max(dpr, 1)));

      const tiles = [];
      for (let top = 0; top < height; top += TILE_H) {
        const shot = await send('Page.captureScreenshot', {
          format: 'png',
          captureBeyondViewport: true,
          fromSurface: true,
          clip: { x: 0, y: top, width, height: Math.min(TILE_H, height - top), scale: 1 },
        });
        tiles.push(shot.data);
      }
      return tiles;
    } finally {
      await send('Runtime.evaluate', {
        expression: `window.scrollTo({ left: ${x}, top: ${y}, behavior: 'instant' })`,
      }).catch(() => {});
    }
  } catch (err) {
    throw Object.assign(err, { code: err.code ?? 'capture', detail: err.detail ?? err.message });
  } finally {
    await chrome.debugger.detach(target).catch(() => {});
  }
}

// Runs inside the page.  Remembers where the reader is, moves to the top,
// and lets lazy images load.  Returns the position to go back to, and the
// pixel ratio the tiles will come back at.
async function preparePage(waitMs) {
  // Attaching the debugger slides Chrome's "started debugging" bar in above
  // the page, which shrinks the viewport a moment later, and the page lays
  // itself out again.  A capture taken in the middle of that can catch a
  // layout for the old size, so first wait for the viewport to stop changing.
  const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
  const start = performance.now();
  let height = window.innerHeight;
  let stableSince = start;
  while (performance.now() - start < 1500) {
    await sleep(40);
    if (window.innerHeight !== height) {
      height = window.innerHeight;
      stableSince = performance.now();
    } else if (performance.now() - stableSince >= 250 && performance.now() - start >= 400) {
      break;
    }
  }

  const pos = { x: window.scrollX, y: window.scrollY, dpr: window.devicePixelRatio || 1 };
  for (const img of document.images) {
    if (img.loading === 'lazy') img.loading = 'eager';
  }
  window.scrollTo({ left: 0, top: 0, behavior: 'instant' });

  const pending = [...document.images].filter((img) => !img.complete);
  const loaded = Promise.all(
    pending.map(
      (img) =>
        new Promise((resolve) => {
          img.addEventListener('load', resolve, { once: true });
          img.addEventListener('error', resolve, { once: true });
        }),
    ),
  );
  await Promise.race([loaded, new Promise((resolve) => setTimeout(resolve, waitMs))]);
  // Two frames, so the jump to the top has been laid out and painted.
  await Promise.race([
    new Promise((resolve) => requestAnimationFrame(() => requestAnimationFrame(resolve))),
    new Promise((resolve) => setTimeout(resolve, 200)),
  ]);
  return pos;
}

// "2026-09-29 00-45-12" — sorts right and is legal in a Windows file name.
function timestamp() {
  const d = new Date();
  const p = (n) => String(n).padStart(2, '0');
  return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}-${p(d.getMinutes())}-${p(d.getSeconds())}`;
}
