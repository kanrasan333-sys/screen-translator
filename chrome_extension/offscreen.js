// Decodes and re-encodes images for background.js, and joins full-page
// screenshot tiles into one picture.
//
// The service worker has no <img> to decode SVG with and can't mint blob: URLs,
// and a data: URL won't carry a download: Chrome drops URLs over 2 MB, which a
// PNG of an ordinary photo already is.  So the pixels and the URL live here.

const live = new Set();

chrome.runtime.onMessage.addListener((msg, _sender, reply) => {
  if (msg?.target !== 'offscreen') return;
  if (msg.type === 'convert') {
    convert(msg).then(reply, (err) => reply({ code: err.code ?? 'encode', detail: err.message }));
    return true;
  }
  if (msg.type === 'stitch') {
    stitch(msg.tiles).then(reply, (err) => reply({ code: err.code ?? 'encode', detail: err.message }));
    return true;
  }
  if (msg.type === 'release') {
    URL.revokeObjectURL(msg.url);
    live.delete(msg.url);
    reply({ live: live.size });
  }
});

function fail(code, message) {
  return Object.assign(new Error(message), { code });
}

async function convert({ src, mime, quality }) {
  let blob;
  try {
    const res = await fetch(src, { credentials: 'include' });
    if (!res.ok) throw new Error(`HTTP ${res.status}`);
    blob = await res.blob();
  } catch (err) {
    throw fail('fetch', err.message);
  }

  // An <img> rather than createImageBitmap: only the element decodes SVG.
  // Waiting on `load`, not img.decode(): decode() waits for a frame, and an
  // offscreen document never paints one, so it never settles.
  const img = new Image();
  const objectUrl = URL.createObjectURL(blob);
  try {
    await new Promise((resolve, reject) => {
      img.onload = resolve;
      img.onerror = () => reject(new Error(blob.type || 'unknown type'));
      img.src = objectUrl;
    });
  } catch (err) {
    throw fail('decode', err.message);
  } finally {
    URL.revokeObjectURL(objectUrl);
  }

  const { naturalWidth: width, naturalHeight: height } = img;
  if (!width || !height) throw fail('decode', 'no intrinsic size');

  let out;
  try {
    const canvas = document.createElement('canvas');
    canvas.width = width;
    canvas.height = height;
    const ctx = canvas.getContext('2d');
    // JPEG has no alpha, and the encoder turns transparent pixels black.
    if (mime === 'image/jpeg') {
      ctx.fillStyle = '#fff';
      ctx.fillRect(0, 0, width, height);
    }
    ctx.drawImage(img, 0, 0, width, height);
    // toDataURL, not toBlob: the async PNG and JPEG encoders wait for idle time
    // between frames, and with no frames here they sit out a one-second timeout
    // on every image.
    const dataUrl = canvas.toDataURL(mime, quality);
    if (dataUrl === 'data:,') throw new Error(`${width}×${height} is too large`);
    // An encoder the browser lacks falls back to PNG without saying so.
    if (!dataUrl.startsWith(`data:${mime};`)) throw new Error(`${mime} is not supported`);
    out = await (await fetch(dataUrl)).blob();
  } catch (err) {
    throw fail('encode', err.message);
  }

  const url = URL.createObjectURL(out);
  live.add(url);
  return { url };
}

// Joins full-page screenshot tiles (base64 PNG, top to bottom) into one PNG.
// Tiles are stacked by their own pixel heights rather than by the CSS offsets
// they were cut at: at a fractional pixel ratio the two disagree by a pixel,
// and adding up CSS offsets would leave a seam at every join.
//
// Tiles are decoded with createImageBitmap and drawn on a CPU-backed canvas.
// An <img> on the default (GPU) canvas came out wrong here: this document never
// paints a frame, and a 4000 px tile drawn that way repeated its first screenful
// all the way down.
async function stitch(tiles) {
  const images = [];
  try {
    for (const data of tiles) {
      const blob = await (await fetch(`data:image/png;base64,${data}`)).blob();
      images.push(await createImageBitmap(blob));
    }
  } catch (err) {
    images.forEach((img) => img.close());
    throw fail('decode', err.message);
  }

  const width = Math.max(...images.map((img) => img.width));
  const height = images.reduce((sum, img) => sum + img.height, 0);
  if (!width || !height) throw fail('decode', 'empty page');

  let out;
  try {
    const canvas = document.createElement('canvas');
    canvas.width = width;
    canvas.height = height;
    const ctx = canvas.getContext('2d', { willReadFrequently: true });
    let y = 0;
    for (const img of images) {
      ctx.drawImage(img, 0, y);
      y += img.height;
      img.close();
    }
    // toDataURL for the same reason as in convert(): no frames here, so the
    // async encoders would wait out their idle timeout.
    const dataUrl = canvas.toDataURL('image/png');
    if (dataUrl === 'data:,') throw new Error(`${width}×${height} is too large`);
    out = await (await fetch(dataUrl)).blob();
  } catch (err) {
    throw fail('encode', err.message);
  }

  const url = URL.createObjectURL(out);
  live.add(url);
  return { url };
}
