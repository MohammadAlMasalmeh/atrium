// Renders launch.html to an MP4, one frame at a time, through headless Chrome.
//   node branding/launch-video/render.mjs [out.mp4] [--fps 60] [--frames 4.2,9.1]
// --frames writes PNG stills at those times instead of a video (for review).
import { spawn } from "node:child_process";
import { mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const args = process.argv.slice(2);
const opt = (name, def) => { const i = args.indexOf(name); return i < 0 ? def : args.splice(i, 2)[1]; };
const fps = Number(opt("--fps", 60));
const stills = opt("--frames", null);
const out = resolve(args[0] ?? join(here, "atrium-launch.mp4"));
const CHROME = "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";
const PORT = 9333;

const chrome = spawn(CHROME, [
  "--headless=new", `--remote-debugging-port=${PORT}`, `--user-data-dir=${mkdtempSync(join(tmpdir(), "atrium-render-"))}`,
  "--hide-scrollbars", "--force-color-profile=srgb", "--window-size=1920,1080", "about:blank",
], { stdio: "ignore" });

async function target() {
  for (let i = 0; i < 100; i++) {
    try {
      const list = await (await fetch(`http://127.0.0.1:${PORT}/json/list`)).json();
      const page = list.find(t => t.type === "page");
      if (page) return page.webSocketDebuggerUrl;
    } catch {}
    await new Promise(r => setTimeout(r, 100));
  }
  throw new Error("Chrome did not start");
}

const ws = new WebSocket(await target());
await new Promise(r => ws.addEventListener("open", r, { once: true }));
let id = 0;
const pending = new Map();
ws.addEventListener("message", e => {
  const m = JSON.parse(e.data);
  if (m.id && pending.has(m.id)) { pending.get(m.id)(m); pending.delete(m.id); }
});
const send = (method, params = {}) => new Promise((res, rej) => {
  const n = ++id;
  pending.set(n, m => m.error ? rej(new Error(`${method}: ${m.error.message}`)) : res(m.result));
  ws.send(JSON.stringify({ id: n, method, params }));
});
const evaluate = async expr => {
  const r = await send("Runtime.evaluate", { expression: expr, awaitPromise: true, returnByValue: true });
  if (r.exceptionDetails) throw new Error(r.exceptionDetails.exception?.description ?? r.exceptionDetails.text);
  return r.result.value;
};

await send("Emulation.setDeviceMetricsOverride", { width: 1920, height: 1080, deviceScaleFactor: 1, mobile: false });
await send("Page.enable");
await send("Page.navigate", { url: pathToFileURL(join(here, "launch.html")).href + "?t=0" });
await new Promise(r => setTimeout(r, 1200));
await evaluate("document.fonts.ready.then(() => true)");
const duration = await evaluate("DURATION");

const shot = async t => {
  await evaluate(`seek(${t}); new Promise(r => requestAnimationFrame(() => requestAnimationFrame(r)))`);
  const { data } = await send("Page.captureScreenshot", { format: "png", optimizeForSpeed: true });
  return Buffer.from(data, "base64");
};

if (stills) {
  for (const t of stills.split(",").map(Number)) {
    const file = join(dirname(out), `frame-${t.toFixed(2)}.png`);
    writeFileSync(file, await shot(t));
    console.log(file);
  }
} else {
  const ffmpeg = spawn("ffmpeg", [
    "-y", "-loglevel", "error", "-f", "image2pipe", "-framerate", String(fps), "-i", "-",
    "-c:v", "libx264", "-preset", "slow", "-crf", "14", "-pix_fmt", "yuv420p", "-profile:v", "high",
    "-color_primaries", "bt709", "-color_trc", "bt709", "-colorspace", "bt709",
    "-movflags", "+faststart", out,
  ], { stdio: ["pipe", "inherit", "inherit"] });
  const total = Math.round(duration * fps);
  for (let f = 0; f < total; f++) {
    const png = await shot(f / fps);
    if (!ffmpeg.stdin.write(png)) await new Promise(r => ffmpeg.stdin.once("drain", r));
    if (f % fps === 0) process.stdout.write(`\r${f}/${total} frames`);
  }
  ffmpeg.stdin.end();
  await new Promise(r => ffmpeg.on("close", r));
  console.log(`\r${total}/${total} frames -> ${out}`);
}

ws.close();
chrome.kill();
