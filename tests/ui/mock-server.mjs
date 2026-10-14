#!/usr/bin/env node
// 本机模拟服务(DESIGN 第 4.6 节,U0c)。只用 Node 自带模块,只绑 127.0.0.1:0。
//
// 用法:
//   node tests/ui/mock-server.mjs --scenario <场景名> [--self-test]
//
// - 静态部分:把仓库根的 app/ui 当静态目录伺服(GET /、/css/*、/js/*、/*.svg 等)。单页应用走
//   hash 路由(#/xxx),这部分永远不会到达服务端,所以只需要原样伺服文件,不用 SPA 兜底。
// - /api/* 部分:从 tests/ui/fixtures/<场景>/ 按 URL 路径逐段映射到磁盘文件读 JSON 返回,
//   不在本文件里为任何具体接口写死路由 —— 后面各界面包只新增 fixtures/<自己的场景名>/**,
//   不需要、也不允许改这个文件。映射规则见 mapFixturePath()。
// - --self-test:起服务、请求 GET / 和 GET /api/channels 都拿到 200,就退出码 0,否则非 0。
"use strict";

import http from "node:http";
import { readFile, stat } from "node:fs/promises";
import { fileURLToPath } from "node:url";
import path from "node:path";

const HERE = path.dirname(fileURLToPath(import.meta.url));
const REPO_ROOT = path.resolve(HERE, "..", "..");
const STATIC_DIR = path.join(REPO_ROOT, "app", "ui");
const FIXTURES_ROOT = path.join(HERE, "fixtures");

const MIME_TYPES = {
  ".html": "text/html; charset=utf-8",
  ".css": "text/css; charset=utf-8",
  ".js": "text/javascript; charset=utf-8",
  ".mjs": "text/javascript; charset=utf-8",
  ".json": "application/json; charset=utf-8",
  ".svg": "image/svg+xml",
  ".png": "image/png",
  ".ico": "image/x-icon",
  ".txt": "text/plain; charset=utf-8",
};

function parseArgs(argv) {
  const out = { scenario: null, selfTest: false };
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    if (a === "--scenario") { out.scenario = argv[++i]; }
    else if (a.startsWith("--scenario=")) { out.scenario = a.slice("--scenario=".length); }
    else if (a === "--self-test") { out.selfTest = true; }
  }
  return out;
}

/** 防止 `..` 之类逃出静态目录。 */
function safeJoin(base, requestPath) {
  const decoded = decodeURIComponent(requestPath);
  const resolved = path.normalize(path.join(base, decoded));
  if (resolved !== base && !resolved.startsWith(base + path.sep)) return null;
  return resolved;
}

async function serveStatic(pathname, res) {
  const rel = pathname === "/" ? "/index.html" : pathname;
  const filePath = safeJoin(STATIC_DIR, rel);
  if (!filePath) { res.writeHead(400).end("bad path"); return; }
  try {
    const st = await stat(filePath);
    if (!st.isFile()) throw new Error("not a file");
    const body = await readFile(filePath);
    const ext = path.extname(filePath).toLowerCase();
    res.writeHead(200, { "Content-Type": MIME_TYPES[ext] || "application/octet-stream" });
    res.end(body);
  } catch {
    res.writeHead(404, { "Content-Type": "text/plain; charset=utf-8" });
    res.end(`not found: ${pathname}`);
  }
}

/**
 * URL 路径 → 夹具文件路径。逐段映射,不认识任何具体接口:
 *   /api/channels                → fixtures/<场景>/api/channels.<METHOD>.json (GET 可省 METHOD 段)
 *   /api/channels/demo1/health   → fixtures/<场景>/api/channels/demo1/health.<METHOD>.json
 *   DELETE /api/channels/x/rules/5 → fixtures/<场景>/api/channels/x/rules/5.DELETE.json
 * 查询串不参与映射。
 */
function candidateFixturePaths(scenario, pathname, method) {
  const segments = pathname.split("/").filter(Boolean); // 至少含 "api"
  if (segments.length === 0) return [];
  const last = segments.pop();
  const dir = path.join(FIXTURES_ROOT, scenario, ...segments);
  const candidates = [path.join(dir, `${last}.${method}.json`)];
  if (method === "GET") candidates.push(path.join(dir, `${last}.json`));
  return candidates;
}

async function readFixture(scenario, pathname, method) {
  for (const candidate of candidateFixturePaths(scenario, pathname, method)) {
    try {
      const raw = await readFile(candidate, "utf8");
      return { found: true, path: candidate, json: JSON.parse(raw) };
    } catch (error) {
      if (error && error.code === "ENOENT") continue;
      throw error;
    }
  }
  return { found: false };
}

/**
 * 默认情况下,夹具文件内容就是原样返回的响应体(方便直接粘贴真实接口输出),状态码 200、
 * 不延迟。需要非 200 状态码或延迟响应(`slow` 场景给 V-08 用)时,把内容包一层:
 *   {"__status__": 409, "__delay_ms__": 1200, "__body__": <真正的响应体,可以是对象也可以是数组>}
 * 用专门的 `__body__` 键判断是否是这种信封,不要求响应体本身是对象,数组端点(如
 * `/api/channels`)也能这样包。
 */
function splitHttpEnvelope(json) {
  if (json && typeof json === "object" && !Array.isArray(json) && Object.prototype.hasOwnProperty.call(json, "__body__")) {
    const status = typeof json.__status__ === "number" ? json.__status__ : 200;
    const delayMs = typeof json.__delay_ms__ === "number" ? json.__delay_ms__ : 0;
    return { status, delayMs, body: json.__body__ };
  }
  return { status: 200, delayMs: 0, body: json };
}

/**
 * 同一个夹具文件在同一次进程里按被请求的次序返回不同内容(给 `reset` 场景的「先大后小」
 * 计数下降用,O-04):把文件内容包成
 *   {"__sequence__": [<第 1 次的响应体或信封>, <第 2 次的>, ...]}
 * 每次命中这个文件,取按请求次数递增的下一项;超出数组长度后停在最后一项不再前进。数组里
 * 每一项既可以是普通响应体,也可以再套一层 `__status__`/`__delay_ms__`/`__body__` 信封。
 * 计数按夹具文件的绝对路径记(不区分场景切换,因为换场景要换进程、内存本来就会清空)。
 */
const sequenceCursors = new Map();

function isSequenceEnvelope(json) {
  return !!json && typeof json === "object" && !Array.isArray(json) && Array.isArray(json.__sequence__);
}

function nextSequenceStep(fixturePath, steps) {
  const at = sequenceCursors.get(fixturePath) ?? 0;
  sequenceCursors.set(fixturePath, at + 1);
  return steps[Math.min(at, steps.length - 1)];
}

function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

async function serveApi(scenario, pathname, method, res) {
  const result = await readFixture(scenario, pathname, method);
  if (!result.found) {
    res.writeHead(404, { "Content-Type": "application/json; charset=utf-8" });
    res.end(JSON.stringify({ detail: `没有夹具:${method} ${pathname}(场景 ${scenario})` }));
    return;
  }
  const json = isSequenceEnvelope(result.json) ? nextSequenceStep(result.path, result.json.__sequence__) : result.json;
  const { status, delayMs, body } = splitHttpEnvelope(json);
  if (delayMs > 0) await sleep(delayMs);
  res.writeHead(status, { "Content-Type": "application/json; charset=utf-8" });
  res.end(JSON.stringify(body));
}

export function createServer(scenario) {
  return http.createServer((req, res) => {
    const url = new URL(req.url, "http://127.0.0.1");
    if (url.pathname.startsWith("/api/")) {
      serveApi(scenario, url.pathname, req.method, res).catch((error) => {
        res.writeHead(500, { "Content-Type": "application/json; charset=utf-8" });
        res.end(JSON.stringify({ detail: String(error && error.message || error) }));
      });
      return;
    }
    if (req.method !== "GET" && req.method !== "HEAD") {
      res.writeHead(405).end("method not allowed");
      return;
    }
    serveStatic(url.pathname, res).catch((error) => {
      res.writeHead(500).end(String(error && error.message || error));
    });
  });
}

function listen(server) {
  return new Promise((resolve, reject) => {
    server.once("error", reject);
    server.listen(0, "127.0.0.1", () => {
      server.removeListener("error", reject);
      resolve(server.address());
    });
  });
}

function get(url) {
  return new Promise((resolve, reject) => {
    const req = http.get(url, (res) => {
      const chunks = [];
      res.on("data", (c) => chunks.push(c));
      res.on("end", () => resolve({ status: res.statusCode, body: Buffer.concat(chunks).toString("utf8") }));
    });
    req.on("error", reject);
    req.setTimeout(5000, () => req.destroy(new Error("timeout")));
  });
}

async function selfTest(scenario) {
  const server = createServer(scenario);
  const addr = await listen(server);
  const base = `http://127.0.0.1:${addr.port}`;
  try {
    const home = await get(`${base}/`);
    if (home.status !== 200) throw new Error(`GET / 状态码 ${home.status},预期 200`);
    const channels = await get(`${base}/api/channels`);
    if (channels.status !== 200) throw new Error(`GET /api/channels 状态码 ${channels.status},预期 200`);
    JSON.parse(channels.body); // 必须是合法 JSON
    console.log(`[self-test] 场景 ${scenario} 通过(端口 ${addr.port})`);
    return true;
  } finally {
    await new Promise((resolve) => server.close(resolve));
  }
}

async function main() {
  const args = parseArgs(process.argv.slice(2));
  if (!args.scenario) {
    console.error("用法: node tests/ui/mock-server.mjs --scenario <场景名> [--self-test]");
    process.exit(2);
  }

  if (args.selfTest) {
    try {
      await selfTest(args.scenario);
      process.exit(0);
    } catch (error) {
      console.error(`[self-test] 场景 ${args.scenario} 失败: ${error && error.message || error}`);
      process.exit(1);
    }
  }

  const server = createServer(args.scenario);
  const addr = await listen(server);
  console.log(`mock-server listening: http://127.0.0.1:${addr.port} (场景 ${args.scenario})`);
  const shutdown = () => server.close(() => process.exit(0));
  process.on("SIGINT", shutdown);
  process.on("SIGTERM", shutdown);
}

const isMain = process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url);
if (isMain) {
  main().catch((error) => {
    console.error(error);
    process.exit(1);
  });
}
