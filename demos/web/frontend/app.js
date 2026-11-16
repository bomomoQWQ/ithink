"use strict";
/* 单页 Agent 工作台。所有服务端文本只通过 textContent 进入 DOM。 */
(() => {
  const POLL_INTERVAL = 800;
  const MAX_MESSAGE = 2000;
  const $ = (id) => document.getElementById(id);

  const el = {
    sessions: $("sessions-button"), title: $("conversation-title"), subtitle: $("conversation-subtitle"),
    popover: $("sessions-popover"), backdrop: $("popover-backdrop"), list: $("conversation-list"),
    empty: $("conversations-empty"), count: $("conversation-count"), newConversation: $("new-conversation"),
    modelButton: $("model-button"), model: $("current-model"), badge: $("offline-badge"),
    connection: $("connection-status"), connectionText: $("connection-text"),
    connectionError: $("connection-error"), connectionMessage: $("connection-error-message"), retry: $("retry-button"),
    settings: $("settings-button"), scroll: $("chat-scroll"), timeline: $("chat-timeline"),
    welcome: $("welcome"), welcomeTitle: $("welcome-title"), welcomeCopy: $("welcome-copy"), welcomeNote: $("welcome-note"),
    configure: $("configure-button"), suggestions: $("suggestions"), jump: $("jump-bottom"),
    form: $("message-form"), prompt: $("prompt"), send: $("send-button"), sendIcon: $("send-icon"),
    stop: $("stop-button"), status: $("composer-status"), inputCount: $("input-count"),
    actionError: $("action-error"), modeNote: $("mode-note"), announcer: $("announcer"),
    dialog: $("settings-dialog"), settingsForm: $("settings-form"), close: $("settings-close"), cancel: $("settings-cancel"),
    mode: $("provider-mode"), apiSettings: $("api-settings"), offlineNote: $("offline-settings-note"),
    endpoint: $("api-endpoint"), modelInput: $("api-model"), key: $("api-key"), keyStatus: $("key-status"),
    clearKey: $("clear-api-key"), systemPrompt: $("system-prompt"), thinking: $("thinking-mode"),
    reasoning: $("reasoning-effort"), topP: $("top-p"), temperature: $("temperature"), maxTokens: $("max-tokens"), timeout: $("timeout-secs"),
    responseFormat: $("response-format"), safety: $("safety-check"), stopSequences: $("stop-sequences"),
    feedback: $("settings-feedback"), save: $("save-settings"), saveLabel: $("save-label"),
    test: $("test-connection"), testLabel: $("test-label"),
  };

  const STATE_LABEL = { running: "工作中", completed: "已完成", cancelled: "已停止", failed: "执行失败" };
  const drafts = new Map();
  const streamBuffers = new Map();
  const turnViews = new Map();
  const conversationViews = new Map();
  const pendingTurns = new Map();
  const announced = new Set();
  const mobile = window.matchMedia("(max-width: 640px)");

  let state = null;
  let provider = null;
  let selectedId = null;
  let connected = false;
  let pollPromise = null;
  let pollTimer = null;
  let submitting = false;
  let stopping = null;
  let composing = false;
  let newPending = false;
  let settingsBusy = false;
  let settingsLoading = false;
  let settingsSequence = 0;
  let settingsReturnFocus = null;
  let initialSelection = false;

  /* ── 基础工具 ─────────────────────────────────────── */
  const node = (tag, className, value) => {
    const element = document.createElement(tag);
    if (className) element.className = className;
    if (value !== undefined) element.textContent = String(value);
    return element;
  };
  const text = (element, value) => {
    const next = String(value ?? "");
    if (element.textContent !== next) element.textContent = next;
  };
  const pretty = (value) => { try { return JSON.stringify(value ?? null, null, 2); } catch (_) { return "无法显示"; } };
  const asText = (value) => {
    if (value === null || value === undefined) return "";
    if (typeof value === "string") return value;
    if (typeof value === "number" || typeof value === "boolean") return String(value);
    if (typeof value.text === "string") return value.text;
    if (typeof value.content === "string") return value.content;
    return pretty(value);
  };
  const brief = (value, max = 90) => {
    const flat = String(value ?? "").replace(/\s+/g, " ").trim();
    return flat.length > max ? `${flat.slice(0, max)}…` : flat;
  };
  const charCount = (value) => Array.from(value).length;
  const draftKey = (id = selectedId) => (id === null ? "new" : String(id));
  const announce = (message) => text(el.announcer, message);
  const actionError = (message) => { text(el.actionError, message); el.actionError.hidden = !message; };
  const errorText = (error) => (error instanceof Error ? error.message : "操作失败，请重试。");
  const hideSecret = (message, secret) => (secret ? String(message).split(secret).join("[密钥已隐藏]") : String(message));

  function clock(milliseconds) {
    if (!Number.isFinite(milliseconds) || milliseconds < 1e12) return "";
    const value = new Date(milliseconds);
    return Number.isNaN(value.getTime()) ? "" : value.toLocaleTimeString("zh-CN", { hour: "2-digit", minute: "2-digit", hour12: false });
  }

  /* ── 富文本：围栏代码 + 行内代码 + 粗体，全部走文本节点 ── */
  function inline(parent, source) {
    const pattern = /(`[^`\n]+`|\*\*[^*\n]+\*\*)/g;
    let last = 0;
    for (const match of source.matchAll(pattern)) {
      if (match.index > last) parent.append(document.createTextNode(source.slice(last, match.index)));
      const token = match[0];
      if (token.startsWith("`")) parent.append(node("code", "", token.slice(1, -1)));
      else parent.append(node("strong", "", token.slice(2, -2)));
      last = match.index + token.length;
    }
    if (last < source.length) parent.append(document.createTextNode(source.slice(last)));
  }
  function codeBlock(body, language) {
    const wrap = node("div", "code-block");
    const head = node("div", "code-head");
    head.append(node("span", "", language || "code"));
    const copy = node("button", "", "复制");
    copy.type = "button";
    copy.addEventListener("click", async () => {
      try { await navigator.clipboard.writeText(body); text(copy, "已复制"); }
      catch (_) { text(copy, "复制失败"); }
      window.setTimeout(() => text(copy, "复制"), 1400);
    });
    head.append(copy);
    const pre = node("pre");
    pre.append(node("code", "", body));
    wrap.append(head, pre);
    return wrap;
  }
  function renderRich(container, source) {
    container.replaceChildren();
    const value = String(source ?? "");
    const parts = value.split("```");
    parts.forEach((part, index) => {
      if (index % 2 === 1) {
        const cut = part.indexOf("\n");
        const language = cut >= 0 ? part.slice(0, cut).trim() : "";
        const body = cut >= 0 ? part.slice(cut + 1) : part;
        container.append(codeBlock(body.replace(/\n+$/, ""), language));
      } else if (part) {
        const paragraph = node("div", "prose");
        inline(paragraph, part);
        container.append(paragraph);
      }
    });
    if (!container.childElementCount) container.append(node("div", "prose", "（空回复）"));
  }

  /* ── API ──────────────────────────────────────────── */
  async function request(path, options = {}) {
    const controller = new AbortController();
    const timer = window.setTimeout(() => controller.abort(), options.timeout ?? 10000);
    try {
      const headers = { Accept: "application/json" };
      const config = { method: options.method || "GET", credentials: "same-origin", cache: "no-store", headers, signal: controller.signal };
      if (options.body !== undefined) { headers["Content-Type"] = "application/json"; config.body = JSON.stringify(options.body); }
      const response = await fetch(path, config);
      let data;
      try { data = await response.json(); } catch (_) { throw new Error(`服务返回了无效响应（HTTP ${response.status}）。`); }
      if (!response.ok) {
        const detail = data?.error;
        let message = typeof detail?.message === "string" ? detail.message : `请求失败（HTTP ${response.status}）。`;
        if (path === "/api/settings" && response.status === 409) message = "还有任务在跑。先停止或等它结束，再切换模型设置。";
        const error = new Error(`${message}${typeof detail?.code === "string" ? ` [${detail.code}]` : ""}`);
        error.code = detail?.code;
        error.status = response.status;
        throw error;
      }
      return data;
    } catch (error) {
      if (error?.name === "AbortError") throw new Error("请求超时。服务可能还在执行，先看状态，别重复提交。");
      if (error instanceof TypeError) throw new Error("连不上本地服务，确认它还在运行。");
      throw error;
    } finally { window.clearTimeout(timer); }
  }

  function normalizeProvider(value) {
    const fallback = { mode: "api", endpoint: "", model: "", api_key_set: false, system_prompt: "", temperature: null, top_p: null, max_tokens: null, stop: [], response_format: "text", thinking: "auto", reasoning_effort: null, safety_check: false, timeout_secs: 60, configured: false };
    if (!value || typeof value !== "object") return fallback;
    return {
      ...fallback,
      mode: value.mode === "offline" ? "offline" : "api",
      endpoint: typeof value.endpoint === "string" ? value.endpoint : "",
      model: typeof value.model === "string" ? value.model : "",
      api_key_set: value.api_key_set === true,
      system_prompt: typeof value.system_prompt === "string" ? value.system_prompt : "",
      temperature: typeof value.temperature === "number" ? value.temperature : null,
      top_p: Number.isFinite(value.top_p) ? value.top_p : null,
      max_tokens: Number.isInteger(value.max_tokens) ? value.max_tokens : null,
      stop: Array.isArray(value.stop) ? value.stop.filter((item) => typeof item === "string") : [],
      response_format: value.response_format === "json_object" ? "json_object" : "text",
      thinking: ["auto", "enabled", "disabled"].includes(value.thinking) ? value.thinking : "auto",
      reasoning_effort: ["none", "low", "high", "max"].includes(value.reasoning_effort) ? value.reasoning_effort : null,
      safety_check: value.safety_check === true,
      timeout_secs: Number.isInteger(value.timeout_secs) ? value.timeout_secs : 60,
      configured: value.configured === true,
    };
  }
  const ready = () => provider?.mode === "offline" || provider?.configured === true;

  function validateState(value) {
    if (!value || !Array.isArray(value.conversations) || !["api", "offline"].includes(value.mode)) throw new Error("前后端版本不一致：需要支持会话的 Agent 服务。");
    for (const conversation of value.conversations) {
      if (!Number.isSafeInteger(conversation.id) || !Array.isArray(conversation.turns)) throw new Error("会话数据格式错误，保留上次内容。");
      for (const turn of conversation.turns) {
        if (!Number.isSafeInteger(turn.task) || typeof turn.prompt !== "string" || !Object.hasOwn(STATE_LABEL, turn.state)) throw new Error("消息数据格式错误，保留上次内容。");
      }
    }
    return value;
  }

  /* ── 连接与轮询 ───────────────────────────────────── */
  function setConnection(ok, error) {
    connected = ok;
    el.connection.className = `conn ${ok ? "is-connected" : "is-disconnected"}`;
    text(el.connectionText, ok ? "已连接" : "连接中断");
    el.connectionError.hidden = ok;
    if (!ok) text(el.connectionMessage, `${errorText(error)} 稍后自动重试。`);
    updateComposer();
  }
  function refreshState() {
    if (pollPromise) return pollPromise;
    window.clearTimeout(pollTimer);
    pollPromise = (async () => {
      try {
        const next = validateState(await request("/api/state"));
        const recovered = !!state && !connected;
        state = next;
        for (const conversation of next.conversations) {
          const pending = pendingTurns.get(conversation.id);
          if (pending && conversation.turns.some((turn) => turn.task === pending.task)) pendingTurns.delete(conversation.id);
        }
        provider = normalizeProvider(next.provider || { mode: next.mode });
        if (!initialSelection) {
          initialSelection = true;
          if (selectedId === null && next.conversations.length) selectConversation(sortedConversations()[0].id, false);
        }
        if (selectedId !== null && !next.conversations.some((item) => item.id === selectedId)) {
          saveDraft(); selectedId = null; restoreDraft();
        }
        setConnection(true);
        render();
        if (recovered) announce("连接已恢复。");
      } catch (error) { setConnection(false, error); }
      finally { pollPromise = null; pollTimer = window.setTimeout(refreshState, POLL_INTERVAL); }
    })();
    return pollPromise;
  }

  const sortedConversations = () => (state ? state.conversations.slice().sort((a, b) => (b.updated_at_ms || b.created_at_ms || 0) - (a.updated_at_ms || a.created_at_ms || 0) || b.id - a.id) : []);
  const currentConversation = () => state?.conversations.find((item) => item.id === selectedId) || null;
  function activeTurn(conversation = currentConversation()) {
    if (!conversation) return null;
    return conversation.turns.find((turn) => turn.task === conversation.active_task && turn.state === "running")
      || conversation.turns.findLast((turn) => turn.state === "running")
      || pendingTurns.get(conversation.id) || null;
  }

  /* ── 草稿与会话 ───────────────────────────────────── */
  const saveDraft = () => drafts.set(draftKey(), el.prompt.value);
  const restoreDraft = () => { el.prompt.value = drafts.get(draftKey()) || ""; updateComposer(); };
  function selectConversation(id, focus = true) {
    if (selectedId !== id) { saveDraft(); selectedId = id; restoreDraft(); actionError(""); }
    render();
    closePopover(false);
    if (focus) el.prompt.focus();
  }
  async function newConversation() {
    if (newPending) return;
    saveDraft();
    const originId = selectedId;
    const originDraft = el.prompt.value;
    newPending = true;
    el.newConversation.disabled = true;
    actionError("");
    try {
      const response = await request("/api/conversations", { method: "POST", body: {} });
      if (!Number.isSafeInteger(response?.conversation)) throw new Error("服务没返回会话编号，刷新看看。");
      drafts.set(String(response.conversation), selectedId === originId ? el.prompt.value : originDraft);
      if (selectedId === originId) selectConversation(response.conversation);
      await refreshState();
      announce("新对话已创建，草稿已保留。");
    } catch (error) { actionError(errorText(error)); }
    finally { newPending = false; el.newConversation.disabled = false; }
  }

  /* ── 会话浮层 ─────────────────────────────────────── */
  function openPopover() {
    if (!el.popover.hidden) return;
    el.popover.hidden = false;
    el.backdrop.hidden = false;
    el.sessions.setAttribute("aria-expanded", "true");
    renderConversations();
    (el.list.querySelector(".is-selected") || el.newConversation).focus();
  }
  function closePopover(restoreFocus = true) {
    if (el.popover.hidden) return;
    el.popover.hidden = true;
    el.backdrop.hidden = true;
    el.sessions.setAttribute("aria-expanded", "false");
    if (restoreFocus) el.sessions.focus();
  }

  /* ── 渲染：会话列表 ───────────────────────────────── */
  function conversationView(id) {
    const item = node("li");
    const button = node("button", "conversation-button");
    button.type = "button";
    button.dataset.conversationId = String(id);
    const label = node("span", "conversation-label");
    const draft = node("span", "conversation-draft", "草稿");
    const dot = node("span", "conversation-dot");
    dot.setAttribute("aria-label", "正在运行");
    button.append(label, draft, dot);
    button.addEventListener("click", () => selectConversation(id));
    item.append(button);
    return { item, button, label, draft, dot };
  }
  function renderConversations() {
    const conversations = sortedConversations();
    text(el.count, conversations.length);
    el.empty.hidden = conversations.length > 0;
    const alive = new Set(conversations.map((item) => item.id));
    for (const [id, view] of conversationViews) if (!alive.has(id)) { view.item.remove(); conversationViews.delete(id); }
    conversations.forEach((conversation, index) => {
      let view = conversationViews.get(conversation.id);
      if (!view) { view = conversationView(conversation.id); conversationViews.set(conversation.id, view); }
      const title = conversation.title || "新对话";
      text(view.label, title);
      view.button.title = title;
      view.button.className = `conversation-button${conversation.id === selectedId ? " is-selected" : ""}`;
      if (conversation.id === selectedId) view.button.setAttribute("aria-current", "page"); else view.button.removeAttribute("aria-current");
      view.dot.hidden = !activeTurn(conversation);
      view.draft.hidden = !(drafts.get(String(conversation.id)) || "").trim() || conversation.id === selectedId;
      if (el.list.children[index] !== view.item) el.list.insertBefore(view.item, el.list.children[index] || null);
    });
  }

  /* ── 渲染：一轮对话 ───────────────────────────────── */
  function createTurn(task) {
    const wrapper = node("article", "chat-turn");
    wrapper.dataset.taskId = String(task);
    const question = node("div", "turn-question");
    const block = node("div", "turn-answer-block");
    const head = node("div", "turn-head");
    const name = node("span", "turn-name", "俺寻思");
    const chip = node("span", "message-state");
    head.append(name, chip);

    const thinking = node("details", "thinking");
    const thinkingSummary = node("summary");
    const thinkingLabel = node("span", "", "思考");
    thinkingSummary.append(node("span", "chevron", "›"), thinkingLabel);
    const thinkingBody = node("div", "thinking-body");
    thinking.append(thinkingSummary, thinkingBody);

    const tools = node("div", "tools");
    const answer = node("div", "assistant-answer");
    const meta = node("div", "turn-meta");

    const context = node("details", "context");
    const contextSummary = node("summary");
    const contextLabel = node("span", "", "上下文");
    contextSummary.append(node("span", "chevron", "›"), contextLabel);
    const contextPre = node("pre");
    context.append(contextSummary, contextPre);

    block.append(head, thinking, tools, answer, meta, context);
    wrapper.append(question, block);
    return { wrapper, question, chip, thinking, thinkingLabel, thinkingBody, tools, answer, meta, context, contextLabel, contextPre, signature: "", toolsSignature: "", contextSignature: "" };
  }

  function toolRow(call, result) {
    const ok = !result || result.status !== "error";
    const wrap = node("div", "tool");
    const head = node("button", "tool-head");
    head.type = "button";
    head.setAttribute("aria-expanded", "false");
    const dot = node("span", `tool-dot${ok ? "" : " is-error"}`);
    const name = node("span", "tool-name", call?.name || "tool");
    const args = node("span", "tool-args", brief(pretty(call?.arguments ?? {})));
    head.append(dot, name, args, node("span", "chevron", "›"));
    const body = node("div", "tool-body");
    body.hidden = true;
    const pre = node("pre");
    const input = pretty(call?.arguments ?? {});
    const output = ok ? pretty(result?.payload ?? result ?? {}) : pretty(result?.error ?? result ?? {});
    pre.textContent = `参数\n${input}\n\n结果\n${output}`;
    body.append(pre);
    head.addEventListener("click", () => {
      const open = body.hidden;
      body.hidden = !open;
      head.setAttribute("aria-expanded", String(open));
      head.querySelector(".chevron").style.transform = open ? "rotate(90deg)" : "";
    });
    wrap.append(head, body);
    return wrap;
  }

  function reasoningOf(events) {
    let out = "";
    for (const record of events) {
      const reply = record.event?.reply;
      const content = reply?.content;
      if (content && typeof content === "object" && typeof content.reasoning_content === "string") out += content.reasoning_content;
    }
    return out;
  }

  function updateTurn(view, turn) {
    text(view.question, turn.prompt);
    const signature = JSON.stringify([turn.state, turn.stage, turn.result, turn.events?.length]);
    if (signature !== view.signature) {
      view.signature = signature;
      const label = STATE_LABEL[turn.state] || "";
      text(view.chip, label);
      view.chip.className = `message-state state-${turn.state}`;
      view.answer.className = "assistant-answer";
      if (turn.state === "running") {
        const buffer = streamBuffers.get(turn.task);
        if (buffer?.content) { text(view.answer, buffer.content); view.answer.classList.add("is-streaming"); }
        else {
          view.answer.replaceChildren();
          const pending = node("div", "message-pending");
          pending.append(node("span", "pending-dot"), node("span", "", typeof turn.stage === "string" && turn.stage ? turn.stage : "正在请求模型…"));
          view.answer.append(pending);
        }
      } else if (turn.state === "failed") {
        view.answer.replaceChildren();
        const error = turn.result?.error;
        let message = typeof error?.message === "string" ? error.message : "这次没跑完，展开执行过程看看。";
        let code = typeof error?.code === "string" ? error.code : "";
        // 宿主会把后端错误整体序列化成 message，展示时只取里面的可读文本。
        if (message.startsWith("{")) {
          try {
            const inner = JSON.parse(message);
            if (inner && typeof inner.message === "string") message = inner.message;
            if (inner && typeof inner.code === "string" && inner.code) code = inner.code;
          } catch (_) { /* 保留原样 */ }
        }
        const block = node("div", "message-error");
        block.append(node("div", "", message));
        if (code) block.append(node("code", "error-code", code));
        view.answer.append(block);
      } else if (turn.state === "cancelled") {
        view.answer.replaceChildren(node("div", "message-muted", "已停止。已经发生的外部操作不会撤销。"));
      } else {
        const content = turn.result && Object.hasOwn(turn.result, "content") ? turn.result.content : turn.result;
        renderRich(view.answer, asText(content) || "（模型没有返回内容）");
      }
      view.meta.replaceChildren();
      const when = clock(turn.created_at_ms);
      if (when) view.meta.append(node("span", "", when));
      if (Number.isFinite(turn.result?.steps)) view.meta.append(node("span", "", `${turn.result.steps} 步`));
      if (Number.isFinite(turn.used) && Number.isFinite(turn.max_input)) view.meta.append(node("span", "", `上下文 ${turn.used} / ${turn.max_input}`));
      const events = Array.isArray(turn.events) ? turn.events : [];
      const calls = events.filter((record) => record.event?.kind === "tool_finished");
      if (turn.state !== "running") view.meta.append(node("span", "", calls.length ? `${calls.length} 次工具调用` : "无工具调用"));
    }

    const events = Array.isArray(turn.events) ? turn.events : [];
    const toolsSignature = JSON.stringify(events.filter((record) => record.event?.kind === "tool_finished"));
    if (toolsSignature !== view.toolsSignature) {
      view.toolsSignature = toolsSignature;
      view.tools.replaceChildren();
      for (const record of events) {
        if (record.event?.kind !== "tool_finished") continue;
        view.tools.append(toolRow(record.event.call, record.event.result));
      }
      view.tools.hidden = view.tools.childElementCount === 0;
    }

    const buffer = streamBuffers.get(turn.task);
    const reasoning = (turn.state === "running" ? buffer?.reasoning : "") || reasoningOf(events);
    view.thinking.hidden = !reasoning;
    if (reasoning) {
      text(view.thinkingBody, reasoning);
      text(view.thinkingLabel, turn.state === "running" ? "思考中…" : `思考 · ${charCount(reasoning)} 字`);
    }

    if (turn.state === "running" && buffer?.content && view.answer.textContent !== buffer.content) {
      text(view.answer, buffer.content);
      view.answer.className = "assistant-answer is-streaming";
    }

    const contextSignature = JSON.stringify(turn.context ?? []);
    if (contextSignature !== view.contextSignature) {
      view.contextSignature = contextSignature;
      const items = Array.isArray(turn.context) ? turn.context : [];
      view.contextPre.textContent = pretty(items);
      text(view.contextLabel, `上下文 · ${items.length} 条`);
    }
  }

  function renderTimeline() {
    const conversation = currentConversation();
    const turns = conversation ? conversation.turns : [];
    el.welcome.hidden = turns.length > 0;
    const alive = new Set();
    for (const turn of turns) {
      alive.add(turn.task);
      let view = turnViews.get(turn.task);
      if (!view) { view = createTurn(turn.task); turnViews.set(turn.task, view); }
      updateTurn(view, turn);
      const index = turns.indexOf(turn);
      if (el.timeline.children[index] !== view.wrapper) el.timeline.insertBefore(view.wrapper, el.timeline.children[index] || null);
      if (turn.state !== "running" && !announced.has(turn.task)) {
        announced.add(turn.task);
        announce(`Agent ${STATE_LABEL[turn.state]}。`);
      }
    }
    for (const [task, view] of turnViews) if (!alive.has(task)) { view.wrapper.remove(); turnViews.delete(task); streamBuffers.delete(task); }
    el.jump.hidden = turns.length === 0 || nearBottom();
  }

  function renderChrome() {
    const conversation = currentConversation();
    text(el.title, conversation?.title || "新对话");
    const offline = provider?.mode === "offline";
    text(el.subtitle, offline ? "离线演示" : "俺寻思 🤔");
    el.badge.hidden = !offline;
    text(el.model, !ready() ? "未配置模型" : offline ? "离线假模型" : provider?.model || "配置模型");
    text(el.modeNote, offline ? "离线演示 · 本地假模型，不是真实大模型。" : "回答由所选模型生成，重要信息请核实。");
    const configured = ready();
    el.configure.hidden = configured;
    el.suggestions.hidden = !configured;
    text(el.welcomeTitle, configured ? "想聊点什么？" : "先把模型接上");
    text(el.welcomeCopy, configured ? "把问题丢过来，或者直接让它动手。" : "填一个 OpenAI 兼容的地址和模型，就能开始。");
    text(el.welcomeNote, !configured ? "API Key 只保存在服务内存，重启后要重填。" : offline ? "演示支持整数加法，其他输入只做回显。" : "工具过程默认收起，点开能看参数和结果。");
  }
  function render() { renderChrome(); renderConversations(); renderTimeline(); updateComposer(); }

  /* ── 输入器 ───────────────────────────────────────── */
  const nearBottom = () => el.scroll.scrollHeight - el.scroll.scrollTop - el.scroll.clientHeight < 72;
  const scrollBottom = () => { el.scroll.scrollTop = el.scroll.scrollHeight; };
  function updateComposer() {
    const length = charCount(el.prompt.value);
    const running = activeTurn();
    text(el.inputCount, `${length} / ${MAX_MESSAGE}`);
    el.inputCount.classList.toggle("is-over", length > MAX_MESSAGE);
    el.prompt.style.height = "auto";
    el.prompt.style.height = `${Math.min(el.prompt.scrollHeight, 244)}px`;
    el.stop.hidden = !running;
    el.stop.disabled = stopping === running?.task;
    el.send.hidden = !!running;
    el.send.disabled = submitting || !connected || !ready() || !el.prompt.value.trim() || length > MAX_MESSAGE || !!running;
    el.send.classList.toggle("is-loading", submitting);
    text(el.sendIcon, submitting ? "…" : "↑");
    text(el.status, !connected ? "等待连接" : !ready() ? "先配置模型" : running ? "Agent 工作中" : submitting ? "发送中" : "准备就绪");
  }

  async function sendMessage(event) {
    event?.preventDefault();
    if (submitting || activeTurn()) return;
    const content = el.prompt.value;
    if (!content.trim()) return;
    if (charCount(content) > MAX_MESSAGE) { actionError(`消息最多 ${MAX_MESSAGE} 字。`); return; }
    if (!ready()) { actionError("先在设置里接上模型，或手动切到离线演示。"); openSettings(); return; }
    if (!connected) { actionError("连接断了，稍后再发。"); return; }
    submitting = true;
    actionError("");
    saveDraft();
    updateComposer();
    let id = selectedId;
    const sourceKey = draftKey(id);
    try {
      if (id === null) {
        const created = await request("/api/conversations", { method: "POST", body: {} });
        if (!Number.isSafeInteger(created?.conversation)) throw new Error("服务没返回会话编号。");
        id = created.conversation;
        drafts.set(String(id), drafts.get(sourceKey) || content);
        if (selectedId === null) { selectedId = id; }
      }
      const response = await request(`/api/conversations/${id}/messages`, { method: "POST", body: { content } });
      if (!Number.isSafeInteger(response?.task)) throw new Error("服务没返回执行编号，刷新后确认是否已发出。");
      pendingTurns.set(id, { task: response.task, state: "running" });
      if (drafts.get(String(id)) === content) drafts.set(String(id), "");
      if (sourceKey === "new" && drafts.get("new") === content) drafts.set("new", "");
      if (selectedId === id && el.prompt.value === content) { el.prompt.value = ""; drafts.set(String(id), ""); }
      if (selectedId === id) { el.prompt.focus(); scrollBottom(); }
      announce("已发送。");
      await refreshState();
    } catch (error) { actionError(errorText(error)); }
    finally { submitting = false; updateComposer(); }
  }

  async function stopTask() {
    const turn = activeTurn();
    if (!turn || stopping === turn.task) return;
    stopping = turn.task;
    updateComposer();
    actionError("");
    try {
      await request(`/api/tasks/${turn.task}/cancel`, { method: "POST", body: {} });
      announce("停止请求已提交。");
      await refreshState();
    } catch (error) { actionError(errorText(error)); }
    finally { stopping = null; updateComposer(); }
  }

  /* ── 设置 ─────────────────────────────────────────── */
  const feedback = (message, kind = "error") => {
    text(el.feedback, message);
    el.feedback.hidden = !message;
    el.feedback.className = `settings-feedback is-${kind}`;
  };
  function toggleMode() {
    const offline = el.mode.value === "offline";
    el.apiSettings.hidden = offline;
    el.offlineNote.hidden = !offline;
  }
  function settingsControls() {
    return [el.mode, el.endpoint, el.modelInput, el.key, el.clearKey, el.systemPrompt, el.thinking, el.reasoning,
      el.topP, el.temperature, el.maxTokens, el.timeout, el.responseFormat, el.safety, el.stopSequences];
  }
  function setSettingsBusy(busy, action = "") {
    settingsBusy = busy;
    const disabled = busy || settingsLoading;
    for (const control of settingsControls()) control.disabled = disabled;
    el.save.disabled = disabled;
    el.test.disabled = disabled;
    el.close.disabled = busy;
    el.cancel.disabled = busy;
    el.save.classList.toggle("is-loading", busy && action === "save");
    el.test.classList.toggle("is-loading", busy && action === "test");
    text(el.saveLabel, busy && action === "save" ? "正在应用…" : "保存并应用");
    text(el.testLabel, busy && action === "test" ? "正在测试…" : "测试连接");
    toggleMode();
  }
  function fillSettings(value) {
    const config = normalizeProvider(value);
    el.mode.value = config.mode;
    el.endpoint.value = config.endpoint;
    el.modelInput.value = config.model;
    el.key.value = "";
    el.key.placeholder = config.api_key_set ? "已设置，留空保持不变" : "输入 API Key";
    text(el.keyStatus, config.api_key_set ? "已保存在内存" : "尚未设置");
    el.clearKey.checked = false;
    el.clearKey.disabled = !config.api_key_set;
    el.systemPrompt.value = config.system_prompt;
    el.thinking.value = config.thinking;
    el.reasoning.value = config.reasoning_effort || "";
    el.topP.value = config.top_p === null ? "" : String(config.top_p);
    el.temperature.value = config.temperature === null ? "" : String(config.temperature);
    el.maxTokens.value = config.max_tokens === null ? "" : String(config.max_tokens);
    el.timeout.value = String(config.timeout_secs);
    el.responseFormat.value = config.response_format;
    el.safety.checked = config.safety_check;
    el.stopSequences.value = config.stop.join("\n");
    toggleMode();
  }
  async function openSettings() {
    settingsReturnFocus = document.activeElement;
    if (!el.dialog.open) el.dialog.showModal();
    const sequence = ++settingsSequence;
    settingsLoading = true;
    setSettingsBusy(false);
    for (const control of settingsControls()) control.disabled = true;
    el.save.disabled = true;
    el.test.disabled = true;
    feedback("正在读取当前设置…", "info");
    try {
      const result = await request("/api/settings");
      if (sequence !== settingsSequence || !el.dialog.open) return;
      provider = normalizeProvider(result);
      fillSettings(provider);
      renderChrome();
      feedback("");
      updateComposer();
    } catch (error) { if (sequence === settingsSequence) feedback(errorText(error)); }
    finally { if (sequence === settingsSequence) { settingsLoading = false; setSettingsBusy(false); } }
  }
  function settingsBody() {
    const mode = el.mode.value;
    const endpoint = el.endpoint.value.trim();
    const model = el.modelInput.value.trim();
    if (mode === "api") {
      if (!endpoint || !model) throw new Error("Endpoint 和模型名都要填。");
      let url;
      try { url = new URL(endpoint); } catch (_) { throw new Error("Endpoint 不是合法地址。"); }
      if (!["http:", "https:"].includes(url.protocol) || url.username || url.password) throw new Error("Endpoint 只支持不含账号密码的 HTTP/HTTPS 地址。");
    }
    const optional = (value) => (value.trim() === "" ? null : Number(value));
    const temperature = optional(el.temperature.value);
    const topP = optional(el.topP.value);
    const maxTokens = optional(el.maxTokens.value);
    const timeout = Number(el.timeout.value);
    const stop = el.stopSequences.value.split(/\r?\n/).map((item) => item.trim()).filter(Boolean);
    if (temperature !== null && (!Number.isFinite(temperature) || temperature < 0 || temperature > 2)) throw new Error("Temperature 要在 0–2 之间，或留空。");
    if (topP !== null && (!Number.isFinite(topP) || topP <= 0 || topP > 1)) throw new Error("Top P 要大于 0 且不超过 1，或留空。");
    if (maxTokens !== null && (!Number.isSafeInteger(maxTokens) || maxTokens < 1 || maxTokens > 393216)) throw new Error("最大输出要在 1–393216 之间，或留空。");
    if (!Number.isSafeInteger(timeout) || timeout < 1 || timeout > 120) throw new Error("超时要是 1–120 秒的整数。");
    if (stop.length > 16) throw new Error("停止序列最多 16 个。");
    const thinking = el.thinking.value;
    const body = {
      mode, endpoint, model, system_prompt: el.systemPrompt.value,
      temperature, top_p: topP, max_tokens: maxTokens, timeout_secs: timeout,
      response_format: el.responseFormat.value, safety_check: el.safety.checked,
      thinking, reasoning_effort: thinking === "disabled" ? null : (el.reasoning.value || null),
      stop, clear_api_key: el.clearKey.checked,
    };
    if (!el.clearKey.checked && el.key.value.trim()) body.api_key = el.key.value.trim();
    return body;
  }
  async function saveSettings(event) {
    event.preventDefault();
    if (settingsBusy || settingsLoading) return;
    let body;
    try { body = settingsBody(); } catch (error) { feedback(errorText(error)); return; }
    const secret = body.api_key || "";
    setSettingsBusy(true, "save");
    feedback("正在应用…", "info");
    const operation = request("/api/settings", { method: "POST", body, timeout: 30000 });
    el.key.value = "";
    try {
      const result = await operation;
      provider = normalizeProvider(result);
      fillSettings(provider);
      renderChrome();
      updateComposer();
      feedback("已保存并应用。API Key 只在内存里。", "success");
      announce("模型设置已应用。");
      await refreshState();
    } catch (error) {
      let message = hideSecret(errorText(error), secret);
      if (secret) message += "\n新填的密钥已清空，重试要再填一次。";
      feedback(message);
    } finally { el.key.value = ""; setSettingsBusy(false); }
  }
  async function testConnection() {
    if (settingsBusy || settingsLoading) return;
    let body;
    try { body = settingsBody(); } catch (error) { feedback(errorText(error)); return; }
    const secret = body.api_key || "";
    setSettingsBusy(true, "test");
    feedback(body.mode === "api" ? "正在向模型发最小测试请求…" : "正在检查离线演示…", "info");
    const operation = request("/api/settings/test", { method: "POST", body, timeout: 20000 });
    el.key.value = "";
    try {
      const result = await operation;
      if (result?.ok !== true) throw new Error("测试没返回成功。");
      const latency = Number.isFinite(result.latency_ms) ? ` · ${Math.round(result.latency_ms)} ms` : "";
      const model = typeof result.model === "string" && result.model ? ` · ${result.model}` : "";
      let message = `连上了${latency}${model}。${typeof result.message === "string" ? `\n${result.message}` : ""}\n测试不会保存设置。`;
      if (secret) message += "\n新填的密钥已清空，保存前要再填一次。";
      feedback(hideSecret(message, secret), "success");
    } catch (error) {
      let message = hideSecret(errorText(error), secret);
      if (secret) message += "\n新填的密钥已清空，重试要再填一次。";
      feedback(message);
    } finally { el.key.value = ""; setSettingsBusy(false); }
  }
  function closeSettings() {
    if (settingsBusy) return;
    el.key.value = "";
    el.dialog.close();
  }

  /* ── 事件绑定 ─────────────────────────────────────── */
  el.form.addEventListener("submit", sendMessage);
  el.prompt.addEventListener("input", () => { saveDraft(); updateComposer(); });
  el.prompt.addEventListener("compositionstart", () => { composing = true; });
  el.prompt.addEventListener("compositionend", () => { composing = false; });
  el.prompt.addEventListener("keydown", (event) => {
    if (event.key !== "Enter" || event.shiftKey || composing || event.isComposing || event.keyCode === 229) return;
    event.preventDefault();
    if (!el.send.disabled) el.form.requestSubmit();
  });
  el.stop.addEventListener("click", stopTask);
  el.newConversation.addEventListener("click", async () => { closePopover(false); await newConversation(); });
  el.sessions.addEventListener("click", () => { if (el.popover.hidden) openPopover(); else closePopover(); });
  el.backdrop.addEventListener("click", () => closePopover());
  el.retry.addEventListener("click", refreshState);
  el.jump.addEventListener("click", () => { scrollBottom(); el.jump.hidden = true; });
  el.scroll.addEventListener("scroll", () => { el.jump.hidden = nearBottom() || !currentConversation()?.turns.length; }, { passive: true });
  [el.settings, el.modelButton, el.configure].forEach((button) => button.addEventListener("click", openSettings));
  el.settingsForm.addEventListener("submit", saveSettings);
  el.test.addEventListener("click", testConnection);
  el.mode.addEventListener("change", toggleMode);
  el.clearKey.addEventListener("change", () => { if (el.clearKey.checked) el.key.value = ""; });
  el.close.addEventListener("click", closeSettings);
  el.cancel.addEventListener("click", closeSettings);
  el.dialog.addEventListener("cancel", (event) => { if (settingsBusy) event.preventDefault(); });
  el.dialog.addEventListener("close", () => {
    ++settingsSequence;
    el.key.value = "";
    settingsLoading = false;
    if (settingsReturnFocus?.isConnected) settingsReturnFocus.focus();
  });
  document.addEventListener("keydown", (event) => {
    if ((event.ctrlKey || event.metaKey) && event.key.toLowerCase() === "k" && !el.dialog.open && !event.isComposing) {
      event.preventDefault();
      newConversation();
      return;
    }
    if (event.key === "Escape" && !el.popover.hidden) { event.preventDefault(); closePopover(); }
  });
  document.addEventListener("click", (event) => {
    if (el.popover.hidden) return;
    if (el.popover.contains(event.target) || el.sessions.contains(event.target) || el.backdrop === event.target) return;
    closePopover(false);
  });
  mobile.addEventListener("change", () => closePopover(false));
  document.querySelectorAll("[data-prompt]").forEach((button) => button.addEventListener("click", () => {
    el.prompt.value = button.dataset.prompt || "";
    saveDraft();
    updateComposer();
    el.prompt.focus();
  }));
  document.addEventListener("visibilitychange", () => { if (document.visibilityState === "visible") refreshState(); });

  /* ── SSE 增量 ─────────────────────────────────────── */
  function startStream() {
    if (!window.EventSource) return;
    const source = new EventSource("/api/stream");
    source.addEventListener("message", (event) => {
      let data;
      try { data = JSON.parse(event.data); } catch (_) { return; }
      if (data?.type !== "model_delta") return;
      const task = Number(data.task);
      const delta = typeof data.delta === "string" ? data.delta : "";
      if (!Number.isSafeInteger(task) || !delta) return;
      if (data.kind !== "content" && data.kind !== "reasoning") return;
      const step = Number.isSafeInteger(data.step) ? data.step : null;
      const buffer = streamBuffers.get(task) || { content: "", reasoning: "", step };
      if (buffer.step !== step) { buffer.content = ""; buffer.reasoning = ""; buffer.step = step; }
      if (data.kind === "content") buffer.content += delta; else buffer.reasoning += delta;
      streamBuffers.set(task, buffer);
      const view = turnViews.get(task);
      if (!view) return;
      if (data.kind === "content") {
        text(view.answer, buffer.content);
        view.answer.className = "assistant-answer is-streaming";
      } else {
        view.thinking.hidden = false;
        text(view.thinkingBody, buffer.reasoning);
        text(view.thinkingLabel, "思考中…");
      }
      if (nearBottom()) window.requestAnimationFrame(scrollBottom);
    });
  }

  /* ── 启动 ─────────────────────────────────────────── */
  updateComposer();
  refreshState();
  startStream();
})();
