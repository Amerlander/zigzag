#!/usr/bin/env node
// Runs this workspace's ZigZag studio for Obeya's workers (see `app.run` in
// settings.json). Everything it keeps is in target/obeya/ (ignored by git).
//
//   node .obeya/app.mjs start [design.svg]   build, then start the studio
//   node .obeya/app.mjs stop                 stop this workspace's studio
//   node .obeya/app.mjs status               PID and MCP address, if running
//   node .obeya/app.mjs mcp <tool> [json]    call an MCP tool of the studio
//   node .obeya/app.mjs shot <out.png>       screenshot of its window (Windows)
//
// The studio gets a settings folder of its own (XDG_CONFIG_HOME), so it never
// reopens or overwrites the owner's own ZigZag session, and a free MCP port,
// so several workspaces run side by side.

import { spawn, spawnSync } from 'node:child_process';
import fs from 'node:fs';
import net from 'node:net';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const run = path.join(root, 'target', 'obeya');
const stateFile = path.join(run, 'app.json');
const exe = path.join(root, 'target', 'debug', process.platform === 'win32' ? 'zigzag-studio.exe' : 'zigzag-studio');

const [cmd, ...args] = process.argv.slice(2);

function state() {
  try {
    const s = JSON.parse(fs.readFileSync(stateFile, 'utf8'));
    return alive(s.pid) ? s : null;
  } catch {
    return null;
  }
}

function alive(pid) {
  try {
    process.kill(pid, 0);
    return true;
  } catch (e) {
    return e.code === 'EPERM';
  }
}

function freePort() {
  return new Promise((resolve, reject) => {
    const srv = net.createServer();
    srv.once('error', reject);
    srv.listen(0, '127.0.0.1', () => {
      const { port } = srv.address();
      srv.close(() => resolve(port));
    });
  });
}

async function rpc(port, method, params) {
  const res = await fetch(`http://127.0.0.1:${port}/mcp`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ jsonrpc: '2.0', id: 1, method, params }),
  });
  const body = await res.json();
  if (body.error) throw new Error(body.error.message);
  return body.result;
}

async function stop() {
  const s = state();
  if (s) {
    process.kill(s.pid);
    for (let i = 0; i < 50 && alive(s.pid); i++) await new Promise((r) => setTimeout(r, 100));
    console.log(`Stopped the studio (PID ${s.pid}).`);
  }
  fs.rmSync(stateFile, { force: true });
}

async function start() {
  const design = args[0] ? path.resolve(args[0]) : null;
  const build = spawnSync('cargo build --features gui,ble --bins', {
    cwd: root,
    stdio: 'inherit',
    shell: true,
  });
  if (build.status !== 0) process.exit(build.status ?? 1);
  await stop();
  fs.mkdirSync(run, { recursive: true });
  const port = await freePort();
  const log = path.join(run, 'studio.log');
  const out = fs.openSync(log, 'w');
  const env = {
    ...process.env,
    XDG_CONFIG_HOME: path.join(run, 'config'),
    // Capture mode: Ctrl+Alt+S (⌃⌥⌘S on a Mac) saves a screenshot and
    // Ctrl+Alt+R records into target/obeya/capture; it also keeps the update
    // check and crash-report offers out of the way.
    ZIGZAG_CAPTURE: path.join(run, 'capture'),
  };
  // Without HOME the studio keeps no settings folder at all.
  if (!env.HOME) env.HOME = path.join(run, 'home');
  const child = spawn(exe, [...(design ? [design] : []), '--mcp-port', String(port)], {
    cwd: root,
    env,
    detached: true,
    stdio: ['ignore', out, out],
  });
  child.unref();
  const s = { pid: child.pid, port, mcp: `http://127.0.0.1:${port}/mcp`, log, design };
  fs.writeFileSync(stateFile, JSON.stringify(s, null, 2));
  for (let i = 0; i < 600; i++) {
    if (!alive(child.pid)) {
      console.error(`The studio exited at once; see ${log}`);
      process.exit(1);
    }
    try {
      await rpc(port, 'tools/list', {});
      console.log(`ZigZag studio running: PID ${s.pid}, MCP ${s.mcp}`);
      return;
    } catch {
      await new Promise((r) => setTimeout(r, 100));
    }
  }
  console.error(`The studio started (PID ${child.pid}) but its MCP server does not answer; see ${log}`);
  process.exit(1);
}

async function mcp() {
  const s = state();
  if (!s) throw new Error('No studio runs in this workspace: node .obeya/app.mjs start');
  const [tool, json, outPng] = args;
  if (!tool) {
    const { tools } = await rpc(s.port, 'tools/list', {});
    for (const t of tools) console.log(`${t.name}: ${(t.description ?? '').split('\n')[0]}`);
    return;
  }
  const result = await rpc(s.port, 'tools/call', { name: tool, arguments: json ? JSON.parse(json) : {} });
  for (const c of result.content ?? []) {
    if (c.type === 'image') {
      const file = path.resolve(outPng ?? `${tool}.png`);
      fs.writeFileSync(file, Buffer.from(c.data, 'base64'));
      console.log(`(image written to ${file})`);
    } else console.log(c.text ?? JSON.stringify(c));
  }
  if (result.isError) process.exit(1);
}

function shot() {
  const s = state();
  if (!s) throw new Error('No studio runs in this workspace: node .obeya/app.mjs start');
  const file = path.resolve(args[0] ?? 'studio.png');
  if (process.platform !== 'win32') {
    throw new Error('shot works on Windows only; elsewhere press Ctrl+Alt+S (⌃⌥⌘S) in the studio: target/obeya/capture/');
  }
  // PrintWindow with PW_RENDERFULLCONTENT: the window as drawn, also when
  // other windows cover it.
  const ps = `
Add-Type -AssemblyName System.Drawing
Add-Type @'
using System; using System.Runtime.InteropServices;
public class ZzWin {
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int L, T, R, B; }
  [DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr h, IntPtr dc, uint f);
  [DllImport("dwmapi.dll")] public static extern int DwmGetWindowAttribute(IntPtr h, int a, out RECT r, int s);
}
'@
[ZzWin]::SetProcessDPIAware() | Out-Null
$h = (Get-Process -Id ${s.pid}).MainWindowHandle
if ($h -eq 0) { throw 'the studio has no window yet' }
$r = New-Object ZzWin+RECT
[ZzWin]::GetWindowRect($h, [ref]$r) | Out-Null
$f = New-Object ZzWin+RECT
[ZzWin]::DwmGetWindowAttribute($h, 9, [ref]$f, 16) | Out-Null
$bmp = New-Object System.Drawing.Bitmap ($r.R - $r.L), ($r.B - $r.T)
$g = [System.Drawing.Graphics]::FromImage($bmp)
$dc = $g.GetHdc(); [ZzWin]::PrintWindow($h, $dc, 2) | Out-Null; $g.ReleaseHdc($dc); $g.Dispose()
$crop = New-Object System.Drawing.Rectangle ($f.L - $r.L), ($f.T - $r.T), ($f.R - $f.L), ($f.B - $f.T)
$bmp.Clone($crop, $bmp.PixelFormat).Save('${file.replace(/'/g, "''")}', [System.Drawing.Imaging.ImageFormat]::Png)
`;
  const script = path.join(os.tmpdir(), `zigzag-shot-${process.pid}.ps1`);
  fs.writeFileSync(script, ps);
  const r = spawnSync('powershell', ['-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', script], { stdio: 'inherit' });
  fs.rmSync(script, { force: true });
  if (r.status !== 0) process.exit(r.status ?? 1);
  console.log(file);
}

try {
  if (cmd === 'start') await start();
  else if (cmd === 'stop') await stop();
  else if (cmd === 'status') console.log(state() ? JSON.stringify(state(), null, 2) : 'not running');
  else if (cmd === 'mcp') await mcp();
  else if (cmd === 'shot') shot();
  else {
    console.error('usage: node .obeya/app.mjs start [design.svg] | stop | status | mcp [tool [json-args [out.png]]] | shot [out.png]');
    process.exit(2);
  }
} catch (e) {
  console.error(e.message);
  process.exit(1);
}
