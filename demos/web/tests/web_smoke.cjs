"use strict";
// Isolated real-browser test: starts a temporary host and local compatible API.
// No external API, real credential, or running user instance is touched.
const path = require("node:path");
const fs = require("node:fs/promises");
const http = require("node:http");
const { spawn } = require("node:child_process");
const assert = require("node:assert/strict");
const { once } = require("node:events");
const ROOT = path.join(__dirname, "../../..");
const { chromium } = require(process.env.ITHINK_PLAYWRIGHT || path.join(ROOT, ".tools/web-test/node_modules/playwright-core"));
const SECRET = "browser-test-key-not-real";
const requests = [];
const reply = content => ({ choices: [{ finish_reason: "stop", message: { role: "assistant", content } }], usage: { prompt_tokens: 10, completion_tokens: 4 } });
const mock = http.createServer((request, response) => {
  let bytes = "";
  request.on("data", chunk => { bytes += chunk; if (bytes.length > 1048576) request.destroy(); });
  request.on("end", () => {
    try {
      assert.equal(request.url, "/v1/chat/completions");
      const body = JSON.parse(bytes);
      requests.push({ body, authorization: request.headers.authorization });
      const messages = body.messages;
      const last = messages.at(-1);
      const prompt = [...messages].reverse().find(message => message.role === "user")?.content || "";
      if (prompt === "保持等待") return; // cancellation must close this request
      response.setHeader("Content-Type", "application/json");
      if (prompt === "制造错误") { response.statusCode = 401; response.end(JSON.stringify({ error: { message: `private ${SECRET}` } })); return; }
      if (last.role === "tool") {
        const result = JSON.parse(last.content);
        response.end(JSON.stringify(reply(`计算结果：${result.payload.sum}`)));
      } else if (prompt.includes("17 + 25")) {
        response.end(JSON.stringify({ choices: [{ finish_reason: "tool_calls", message: { role: "assistant", content: null, tool_calls: [{ id: "call_1", type: "function", function: { name: "add", arguments: '{"a":17,"b":25}' } }] } }] }));
      } else {
        response.end(JSON.stringify(reply(`本地测试模型：${prompt}；本对话用户消息数 ${messages.filter(message => message.role === "user").length}`)));
      }
    } catch (error) { response.statusCode = 500; response.end(JSON.stringify({ error: "mock failure" })); }
  });
});

async function startHost() {
  const binary = process.env.ITHINK_WEB_BIN || path.join(ROOT, "target/debug", process.platform === "win32" ? "ithink-web.exe" : "ithink-web");
  const child = spawn(binary, ["--port", "0", "--no-persist"], { cwd: ROOT, stdio: ["ignore", "pipe", "inherit"] });
  let output = "";
  const url = await new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error("Web test host startup timeout")), 15000);
    child.once("error", error => { clearTimeout(timer); reject(error); });
    child.once("exit", code => { clearTimeout(timer); reject(new Error(`Web test host exited ${code}`)); });
    child.stdout.on("data", chunk => { output += chunk; const match = output.match(/ITHINK_WEB (http:\/\/127\.0\.0\.1:\d+)/); if (match) { clearTimeout(timer); resolve(match[1]); } });
  }).catch(error => { child.kill(); throw error; });
  return { child, url };
}

(async () => {
  mock.listen(0, "127.0.0.1"); await once(mock, "listening");
  const endpoint = `http://127.0.0.1:${mock.address().port}/v1`;
  let host, browser, page;
  const errors = [];
  try {
    host = await startHost();
    browser = await chromium.launch({ executablePath: process.env.ITHINK_BROWSER || "C:\\Program Files\\Google\\Chrome\\Application\\chrome.exe", headless: true });
    const context = await browser.newContext({ viewport: { width: 1440, height: 1000 }, locale: "zh-CN" });
    page = await context.newPage(); page.on("pageerror", error => errors.push(error.message));
    const network = await context.newCDPSession(page); await network.send("Network.enable");
    const navigation = [];
    network.on("Network.requestWillBeSentExtraInfo", event => navigation.push(Object.fromEntries(Object.entries(event.headers).map(([key, value]) => [key.toLowerCase(), value]))));
    await page.route("https://ithink-incoming-test.invalid/**", route => route.fulfill({ contentType: "text/html", body: `<a id="open" href="${host.url}/">open</a>` }));
    await page.goto("https://ithink-incoming-test.invalid/"); await page.locator("#open").click();
    await page.waitForFunction(() => !document.querySelector("#configure-button")?.hidden);
    assert.ok(navigation.some(headers => headers.host === new URL(host.url).host && headers["sec-fetch-site"] === "cross-site"));
    assert.equal(await page.locator("#send-button").isDisabled(), true);
    await page.locator("#settings-button").click(); await page.locator("#api-endpoint").fill(endpoint);
    await page.locator("#api-model").fill("local-test-model"); await page.locator("#api-key").fill(SECRET);
    await page.locator("#test-connection").click();
    await page.waitForFunction(() => !document.querySelector("#test-connection")?.disabled && !document.querySelector("#settings-feedback")?.hidden);
    assert.match(await page.locator("#settings-feedback").innerText(), /成功|连通/);
    assert.equal(await page.locator("#api-key").inputValue(), "");
    assert.equal(requests[0].authorization, `Bearer ${SECRET}`); assert.ok(requests[0].body.max_tokens <= 32);
    await page.locator("#api-key").fill(SECRET); await page.locator("#save-settings").click();
    await page.waitForFunction(() => !document.querySelector("#save-settings")?.disabled && document.querySelector("#settings-feedback")?.textContent.includes("已保存"));
    await page.locator("#settings-cancel").click();
    await page.locator("#current-model").filter({ hasText: "local-test-model" }).waitFor();

    await page.locator("#prompt").fill("帮我计算 17 + 25"); await page.locator("#prompt").press("Enter");
    await page.locator(".assistant-answer").filter({ hasText: "计算结果：42" }).waitFor();
    assert.equal(await page.locator(".process-details").first().getAttribute("open"), null);
    const firstState = await (await page.request.get(`${host.url}/api/state`)).json();
    const conversationId = firstState.conversations[0].id;
    assert.equal(firstState.conversations[0].turns[0].state, "completed");
    assert.equal(firstState.conversations[0].turns[0].context.length, 4);
    assert.equal(JSON.stringify(firstState).includes(SECRET), false);
    assert.ok(requests.some(request => request.body.messages.some(message => message.role === "tool")));
    await page.locator("#prompt").fill("继续这段对话"); await page.locator("#prompt").press("Enter");
    await page.locator(".assistant-answer").filter({ hasText: "本对话用户消息数 2" }).waitFor();
    assert.equal(await page.locator(".chat-turn").count(), 2);

    await page.locator("#prompt").fill("未发送的草稿"); await page.locator("#new-conversation").click();
    await page.waitForFunction(id => document.querySelector(`#conversation-list [data-conversation-id='${id}']`)?.getAttribute("aria-current") !== "page", conversationId);
    assert.equal(await page.locator("#prompt").inputValue(), "未发送的草稿");
    await page.locator("#prompt").fill("保持等待"); await page.locator("#prompt").press("Enter"); await page.locator("#stop-button").waitFor({ state: "visible" });
    await page.locator("#prompt").fill("草稿不会因为轮询消失"); await page.locator("#prompt").focus();
    await page.waitForResponse(response => response.url().endsWith("/api/state"));
    assert.equal(await page.locator("#prompt").inputValue(), "草稿不会因为轮询消失");
    assert.equal(await page.evaluate(() => document.activeElement?.id), "prompt");
    await page.locator("#stop-button").click(); await page.locator(".message-state").filter({ hasText: "已停止" }).waitFor();
    await page.locator("#prompt").fill("停止后正常发送"); await page.locator("#prompt").press("Enter");
    await page.locator(".assistant-answer").filter({ hasText: "本对话用户消息数 1" }).waitFor();

    await page.locator(`#conversation-list [data-conversation-id='${conversationId}']`).click();
    assert.equal(await page.locator("#prompt").inputValue(), "未发送的草稿");
    await page.locator("#prompt").fill("制造错误"); await page.locator("#prompt").press("Enter");
    await page.locator(".message-state").filter({ hasText: "执行失败" }).waitFor();
    assert.equal((await page.locator("#chat-timeline").innerText()).includes(SECRET), false);
    await page.locator("#settings-button").click(); assert.equal(await page.locator("#api-key").inputValue(), "");
    await page.locator("#settings-cancel").click();
    const storage = await page.evaluate(() => ({ local: { ...localStorage }, session: { ...sessionStorage } }));
    assert.equal(JSON.stringify(storage).includes(SECRET), false);
    await page.reload({ waitUntil: "domcontentloaded" }); await page.locator(".chat-turn").first().waitFor();
    assert.equal(await page.locator("#current-model").innerText(), "local-test-model");
    await page.route("**/api/state", route => route.abort("connectionrefused")); await page.locator("#connection-error").waitFor({ state: "visible" });
    await page.unroute("**/api/state"); await page.locator("#retry-button").click(); await page.waitForFunction(() => document.querySelector("#connection-error")?.hidden === true);

    // Check literal rendering, never HTML execution, then leave a clean
    // successful turn at the bottom for visual inspection.
    await page.locator(`#conversation-list [data-conversation-id='${conversationId}']`).click();
    await page.locator("#prompt").fill('<img src=x onerror="window.ithinkXss=true">'); await page.locator("#prompt").press("Enter");
    await page.locator(".assistant-answer").filter({hasText:'<img src=x'}).waitFor();
    assert.equal(await page.locator("#chat-timeline img").count(),0); assert.equal(await page.evaluate(()=>window.ithinkXss),undefined);
    await page.locator("#prompt").fill("帮我计算 17 + 25"); await page.locator("#prompt").press("Enter");
    await page.waitForFunction(()=>Array.from(document.querySelectorAll(".assistant-answer")).filter(element=>element.textContent==="计算结果：42").length===2);
    const output = path.join(ROOT, ".tools/web-test/screenshots"); await fs.mkdir(output, { recursive: true });
    await page.screenshot({ path: path.join(output, "chat-desktop.png"), fullPage: true });
    await page.locator("#settings-button").click(); await page.screenshot({ path: path.join(output, "chat-settings.png"), fullPage: true }); await page.locator("#settings-cancel").click();
    await page.setViewportSize({ width: 390, height: 844 }); assert.equal(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth), true);
    await page.locator("#sidebar-toggle").click(); await page.locator("#sidebar-close").click();
    await page.screenshot({ path: path.join(output, "chat-mobile.png"), fullPage: true });
    assert.deepEqual(errors, []);
    console.log(JSON.stringify({ ok: true, tests: ["external navigation", "unconfigured guidance", "real mock API probe/auth", "settings save/key privacy", "tool call/result", "multi-turn history", "new conversation/drafts", "cancel", "errors", "reload", "reconnect", "mobile"], api_calls: requests.length, screenshots: output }));
  } catch (error) {
    if (page) { console.error("PAGE",page.url(),"ERRORS",JSON.stringify(errors),await page.locator("body").innerText().catch(()=>"unavailable")); await page.screenshot({path:path.join(ROOT, ".tools/web-test/screenshots/chat-failure.png"),fullPage:true}).catch(()=>{}); }
    throw error;
  } finally {
    if (browser) await browser.close();
    if (host) {
      try { await fetch(`${host.url}/api/shutdown`, { method: "POST", headers: { Origin: host.url, "Content-Type": "application/json" }, body: "{}" }); } catch (_) {}
      if (host.child.exitCode === null) await Promise.race([once(host.child, "exit"), new Promise(resolve => setTimeout(resolve, 4000))]);
      if (host.child.exitCode === null) host.child.kill();
    }
    mock.closeAllConnections(); await new Promise(resolve => mock.close(resolve));
  }
})().catch(error => { console.error(error); process.exitCode = 1; });
