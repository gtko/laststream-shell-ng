// Serveur HTTP local à débit limité (LAST-146, mesure mémoire du tampon mpv).
// Usage : node scripts/throttled-server.mjs <fichier> <Mo/s> [port]
// Écoute sur 127.0.0.1 uniquement. Accepte Range (206) ; le corps part au débit donné, par tranches de 64 KiB.
import { createServer } from 'node:http';
import { once } from 'node:events';
import { open, stat } from 'node:fs/promises';

const [file, rateArg, portArg = '0'] = process.argv.slice(2);
if (!file || !(Number(rateArg) > 0)) {
  console.error('usage: node scripts/throttled-server.mjs <fichier> <Mo/s> [port]');
  process.exit(2);
}
const bytesPerSec = Number(rateArg) * 1024 * 1024;
const CHUNK = 64 * 1024;
const { size } = await stat(file);

async function pump(res, start, end) {
  const fh = await open(file, 'r');
  try {
    let pos = start;
    let sent = 0;
    const t0 = Date.now();
    while (pos <= end && !res.destroyed) {
      const want = Math.min(CHUNK, end - pos + 1);
      const buf = Buffer.alloc(want);
      const { bytesRead } = await fh.read(buf, 0, want, pos);
      if (bytesRead === 0) break;
      if (!res.write(buf.subarray(0, bytesRead))) {
        await Promise.race([once(res, 'drain'), once(res, 'close')]);
      }
      pos += bytesRead;
      sent += bytesRead;
      const wait = t0 + (sent / bytesPerSec) * 1000 - Date.now();
      if (wait > 0) await new Promise((resolve) => setTimeout(resolve, wait));
    }
  } finally {
    await fh.close();
    res.end();
  }
}

const server = createServer((req, res) => {
  let start = 0;
  let end = size - 1;
  let status = 200;
  const range = /^bytes=(\d*)-(\d*)$/.exec(req.headers.range ?? '');
  if (range && (range[1] || range[2])) {
    if (range[1]) {
      start = Number(range[1]);
      end = range[2] ? Math.min(Number(range[2]), size - 1) : size - 1;
    } else {
      start = Math.max(0, size - Number(range[2]));
    }
    if (start > end || start >= size) {
      res.writeHead(416, { 'Content-Range': `bytes */${size}` }).end();
      return;
    }
    status = 206;
    res.setHeader('Content-Range', `bytes ${start}-${end}/${size}`);
  }
  res.writeHead(status, {
    'Content-Type': 'video/mp4',
    'Content-Length': end - start + 1,
    'Accept-Ranges': 'bytes',
  });
  if (req.method === 'HEAD') {
    res.end();
    return;
  }
  pump(res, start, end).catch(() => res.destroy());
});

server.listen(Number(portArg), '127.0.0.1', () => console.log(`listening ${server.address().port}`));
for (const signal of ['SIGINT', 'SIGTERM']) {
  process.on(signal, () => {
    server.close();
    process.exit(0);
  });
}
