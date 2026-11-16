"use strict";

(() => {
  const POLL_INTERVAL = 700;
  const MAX_MESSAGE = 2000;
  const $ = (id) => document.getElementById(id);
  const el = {
    sidebar: $("sidebar"), backdrop: $("sidebar-backdrop"), sidebarToggle: $("sidebar-toggle"), sidebarClose: $("sidebar-close"),
    workspace: $("workspace"), newConversation: $("new-conversation"), conversationList: $("conversation-list"), conversationCount: $("conversation-count"),
    conversationsEmpty: $("conversations-empty"), title: $("conversation-title"), settings: $("settings-button"), settingsHint: $("settings-hint"),
    modelButton: $("model-button"), model: $("current-model"), offlineBadge: $("offline-badge"), connection: $("connection-status"), connectionText: $("connection-text"),
    connectionError: $("connection-error"), connectionMessage: $("connection-error-message"), retry: $("retry-button"),
    scroll: $("chat-scroll"), timeline: $("chat-timeline"), welcome: $("welcome"), welcomeTitle: $("welcome-title"), welcomeCopy: $("welcome-copy"),
    welcomeNote: $("welcome-note"), configure: $("configure-button"), suggestions: $("suggestions"), jumpBottom: $("jump-bottom"),
    form: $("message-form"), prompt: $("prompt"), send: $("send-button"), sendIcon: $("send-icon"), stop: $("stop-button"), stopLabel: $("stop-label"),
    composerStatus: $("composer-status"), count: $("input-count"), actionError: $("action-error"), modeNote: $("mode-note"), announcer: $("announcer"),
    dialog: $("settings-dialog"), settingsForm: $("settings-form"), settingsClose: $("settings-close"), settingsCancel: $("settings-cancel"),
    mode: $("provider-mode"), apiSettings: $("api-settings"), offlineSettings: $("offline-settings-note"), endpoint: $("api-endpoint"), modelInput: $("api-model"),
    key: $("api-key"), keyStatus: $("key-status"), keyHint: $("key-hint"), clearKey: $("clear-api-key"), systemPrompt: $("system-prompt"),
    temperature: $("temperature"), maxTokens: $("max-tokens"), timeout: $("timeout-secs"), feedback: $("settings-feedback"),
    save: $("save-settings"), saveLabel: $("save-label"), test: $("test-connection"), testLabel: $("test-label")
  };
  const states = { running: "执行中", completed: "已完成", cancelled: "已停止", failed: "执行失败" };
  const drafts = new Map();
  const conversationViews = new Map();
  const turnViews = new Map();
  const announcedTurns = new Set();
  const pendingTurns = new Map();
  const mobile = window.matchMedia("(max-width: 760px)");
  let state = null;
  let provider = null;
  let selectedId = null;
  let renderedConversation = undefined;
  let selectedUntil = 0;
  let initialSelectionDone = false;
  let connected = false;
  let pollPromise = null;
  let pollTimer = null;
  let newPending = false;
  let submitting = false;
  let stopping = null;
  let composing = false;
  let settingsBusy = false;
  let settingsLoading = false;
  let settingsLoadSequence = 0;
  let settingsReturnFocus = null;

  function node(tag, className, text) {
    const element = document.createElement(tag);
    if (className) element.className = className;
    if (text !== undefined) element.textContent = String(text);
    return element;
  }
  function text(element, value) {
    const next = String(value ?? "");
    if (element.textContent !== next) element.textContent = next;
  }
  function pretty(value) {
    try { return JSON.stringify(value ?? null, null, 2); } catch (_) { return "无法显示的数据"; }
  }
  function asText(value) {
    if (value === null || value === undefined) return "";
    if (typeof value === "string") return value;
    if (typeof value === "number" || typeof value === "boolean") return String(value);
    if (typeof value.text === "string") return value.text;
    if (typeof value.content === "string") return value.content;
    return pretty(value);
  }
  function brief(value, max = 160) {
    const content = asText(value).replace(/\s+/g, " ").trim();
    return content.length > max ? `${content.slice(0, max)}…` : content;
  }
  function messageLength(value) { return Array.from(value).length; }
  function draftKey(id = selectedId) { return id === null ? "new" : String(id); }
  function currentConversation() { return state?.conversations.find((entry) => entry.id === selectedId) || null; }
  function activeTurn(conversation = currentConversation()) {
    if (conversation) return conversation.turns.find((turn) => turn.task === conversation.active_task && turn.state === "running")
      || conversation.turns.findLast((turn) => turn.state === "running") || pendingTurns.get(conversation.id) || null;
    return selectedId === null ? null : pendingTurns.get(selectedId) || null;
  }
  function ready() { return provider?.mode === "offline" || provider?.configured === true; }
  function announce(message) { text(el.announcer, message); }
  function showActionError(message) { text(el.actionError, message); el.actionError.hidden = !message; }
  function errorText(error) { return error instanceof Error ? error.message : "操作失败，请稍后重试。"; }
  function hideSecret(message, secret) { return secret ? String(message).split(secret).join("[密钥已隐藏]") : String(message); }
  function dateLabel(milliseconds) {
    if (!Number.isFinite(milliseconds) || milliseconds < 1000000000000) return "";
    const value = new Date(milliseconds);
    return Number.isNaN(value.getTime()) ? "" : value.toLocaleTimeString("zh-CN", { hour: "2-digit", minute: "2-digit", hour12: false });
  }
  function normalizeProvider(value) {
    if (!value || typeof value !== "object") return { mode: "api", configured: false, api_key_set: false };
    return {
      mode: value.mode === "offline" ? "offline" : "api", endpoint: typeof value.endpoint === "string" ? value.endpoint : "",
      model: typeof value.model === "string" ? value.model : "", api_key_set: value.api_key_set === true,
      system_prompt: typeof value.system_prompt === "string" ? value.system_prompt : "", temperature: typeof value.temperature === "number" ? value.temperature : null,
      max_tokens: Number.isInteger(value.max_tokens) ? value.max_tokens : 4096,
      timeout_secs: Number.isInteger(value.timeout_secs) ? value.timeout_secs : 60, configured: value.configured === true
    };
  }
  async function request(path, options = {}) {
    const controller = new AbortController();
    const timeout = window.setTimeout(() => controller.abort(), options.timeout ?? 10000);
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
        if (path === "/api/settings" && response.status === 409 && (detail?.code === "busy" || /running|运行|任务/i.test(message))) message = "还有任务正在运行。请先停止或等待任务完成，再保存模型设置。";
        const error = new Error(`${message}${typeof detail?.code === "string" ? ` [${detail.code}]` : ""}`);
        error.code = detail?.code; error.status = response.status;
        throw error;
      }
      return data;
    } catch (error) {
      if (error?.name === "AbortError") throw new Error("请求超时。服务可能仍在执行，请先查看当前状态，不要重复提交。");
      if (error instanceof TypeError) throw new Error("无法连接本地服务，请确认服务正在运行。");
      throw error;
    } finally { window.clearTimeout(timeout); }
  }
  function validateState(value) {
    if (!value || !Array.isArray(value.conversations) || !["api", "offline"].includes(value.mode)) throw new Error("前后端接口版本不一致：需要支持会话的 Agent 服务。");
    for (const conversation of value.conversations) {
      if (!Number.isSafeInteger(conversation.id) || !Array.isArray(conversation.turns)) throw new Error("会话数据格式错误，保留上次内容。");
      for (const turn of conversation.turns) if (!Number.isSafeInteger(turn.task) || typeof turn.prompt !== "string" || !Object.hasOwn(states, turn.state)) throw new Error("消息数据格式错误，保留上次内容。");
    }
    return value;
  }
  function connectionStatus(success, error) {
    connected = success;
    el.connection.className = `connection-status ${success ? "is-connected" : "is-disconnected"}`;
    text(el.connectionText, success ? "已连接" : "连接中断");
    el.connectionError.hidden = success;
    if (!success) text(el.connectionMessage, `${errorText(error)} 正在自动重试。`);
    updateComposer();
  }
  function refreshState() {
    if (pollPromise) return pollPromise;
    window.clearTimeout(pollTimer);
    el.retry.disabled = true;
    pollPromise = (async () => {
      try {
        const next = validateState(await request("/api/state"));
        const wasDisconnected = !!state && !connected;
        state = next;
        for (const conversation of next.conversations) {
          const pending = pendingTurns.get(conversation.id);
          if (pending && conversation.turns.some((turn) => turn.task === pending.task)) pendingTurns.delete(conversation.id);
        }
        provider = normalizeProvider(next.provider || { mode: next.mode });
        if (!initialSelectionDone) {
          initialSelectionDone = true;
          if (selectedId === null && next.conversations.length) selectConversation(sortedConversations()[0].id, false);
        }
        if (selectedId !== null && !next.conversations.some((conversation) => conversation.id === selectedId) && Date.now() > selectedUntil) {
          saveDraft();
          selectedId = null;
          restoreDraft();
        }
        connectionStatus(true);
        render();
        if (wasDisconnected) announce("连接已恢复。");
      } catch (error) { connectionStatus(false, error); }
      finally { pollPromise = null; el.retry.disabled = false; pollTimer = window.setTimeout(refreshState, POLL_INTERVAL); }
    })();
    return pollPromise;
  }
  function sortedConversations() { return state ? state.conversations.slice().sort((a, b) => (b.updated_at_ms || b.created_at_ms || 0) - (a.updated_at_ms || a.created_at_ms || 0) || b.id - a.id) : []; }

  function saveDraft() { drafts.set(draftKey(), el.prompt.value); }
  function restoreDraft() { el.prompt.value = drafts.get(draftKey()) || ""; updateComposer(); }
  function selectConversation(id, focus = true) {
    if (selectedId !== id) { saveDraft(); selectedId = id; selectedUntil = Date.now() + 6000; restoreDraft(); showActionError(""); }
    render();
    closeSidebar(false);
    if (focus) el.prompt.focus();
  }
  async function newConversation() {
    if (newPending) return;
    saveDraft();
    const originId = selectedId;
    const originDraft = el.prompt.value;
    newPending = true; el.newConversation.disabled = true; showActionError("");
    try {
      const response = await request("/api/conversations", { method: "POST", body: {} });
      if (!Number.isSafeInteger(response?.conversation)) throw new Error("服务没有返回会话编号，请刷新后检查。");
      // Keep the old draft and carry a copy into the fresh conversation. If the
      // user changed chats while POST was pending, do not hijack that selection.
      drafts.set(String(response.conversation), selectedId === originId ? el.prompt.value : originDraft);
      if (selectedId === originId) selectConversation(response.conversation);
      await refreshState();
      announce("新对话已创建，草稿已保留。");
    } catch (error) { showActionError(errorText(error)); }
    finally { newPending = false; el.newConversation.disabled = false; }
  }
  function render() {
    renderProvider();
    renderConversations();
    renderTimeline();
    updateComposer();
  }
  function renderProvider() {
    const offline = provider?.mode === "offline";
    text(el.model, offline ? "离线假模型" : provider?.model || "配置模型");
    text(el.settingsHint, offline ? "离线演示" : ready() ? provider?.model || "已配置" : "尚未配置");
    el.offlineBadge.hidden = !offline;
    text(el.modeNote, offline ? "离线演示 · 这是本地假模型，不是真实大模型。" : "回答由所选模型生成，请核实重要信息。");
  }
  function conversationView(id) {
    const item = node("li"); const button = node("button", "conversation-button");
    button.type = "button"; button.dataset.conversationId = String(id);
    const icon = node("span", "conversation-icon", "◌"); icon.setAttribute("aria-hidden", "true");
    const label = node("span", "conversation-label"); const draft = node("span", "conversation-draft", "草稿");
    const running = node("span", "conversation-running"); running.setAttribute("aria-label", "正在运行");
    button.append(icon, label, draft, running); item.append(button);
    button.addEventListener("click", () => selectConversation(id));
    return { item, button, label, draft, running };
  }
  function renderConversations() {
    const conversations = sortedConversations();
    text(el.conversationCount, conversations.length);
    el.conversationsEmpty.hidden = conversations.length > 0;
    const active = new Set(conversations.map((conversation) => conversation.id));
    for (const [id, view] of conversationViews) if (!active.has(id)) { view.item.remove(); conversationViews.delete(id); }
    conversations.forEach((conversation, index) => {
      let view = conversationViews.get(conversation.id);
      if (!view) { view = conversationView(conversation.id); conversationViews.set(conversation.id, view); }
      const title = conversation.title || "新对话";
      text(view.label, title); view.button.title = title;
      view.button.className = `conversation-button${conversation.id === selectedId ? " is-selected" : ""}`;
      if (conversation.id === selectedId) view.button.setAttribute("aria-current", "page"); else view.button.removeAttribute("aria-current");
      view.running.hidden = !activeTurn(conversation);
      view.draft.hidden = !(drafts.get(String(conversation.id)) || "").trim() || conversation.id === selectedId;
      if (el.conversationList.children[index] !== view.item) el.conversationList.insertBefore(view.item, el.conversationList.children[index] || null);
    });
  }
  function nearBottom() { return el.scroll.scrollHeight - el.scroll.scrollTop - el.scroll.clientHeight < 110; }
  function scrollBottom() { el.scroll.scrollTop = el.scroll.scrollHeight; el.jumpBottom.hidden = true; }
  function renderTimeline() {
    const conversation = currentConversation();
    const changed = renderedConversation !== selectedId;
    const follow = changed || nearBottom();
    if (changed) { el.timeline.replaceChildren(); renderedConversation = selectedId; }
    text(el.title, conversation?.title || "新对话");
    const turns = conversation?.turns || [];
    el.welcome.hidden = turns.length > 0;
    if (!turns.length) {
      const offline = provider?.mode === "offline";
      text(el.welcomeTitle, !ready() ? "先连接一个模型" : "你在想什么？");
      text(el.welcomeCopy, !ready() ? "配置 API 地址和模型，让这个工作空间真正开始工作。" : offline ? "先试一段工作流。当前运行的是离线假模型。" : "把问题留在这里，让 Agent 帮你一起完成。");
      el.configure.hidden = ready();
      el.suggestions.hidden = !ready();
      text(el.welcomeNote, !ready() ? "API Key 只保存在服务内存中，重启后需要重新填写。" : offline ? "演示支持整数加法；其他输入仅作接口回显。" : "输入消息开始对话。工具调用过程默认折叠显示。");
    }
    const visible = new Set();
    turns.forEach((turn, index) => {
      visible.add(turn.task);
      let view = turnViews.get(turn.task);
      if (!view) { view = createTurnView(turn.task); turnViews.set(turn.task, view); }
      updateTurn(view, turn);
      if (el.timeline.children[index] !== view.wrapper) el.timeline.insertBefore(view.wrapper, el.timeline.children[index] || null);
      if (turn.state !== "running" && !announcedTurns.has(turn.task)) {
        announcedTurns.add(turn.task);
        if (!changed) announce(`Agent ${states[turn.state]}。`);
      }
    });
    for (const child of Array.from(el.timeline.children)) if (!visible.has(Number(child.dataset.taskId))) child.remove();
    if (follow) window.requestAnimationFrame(scrollBottom);
    else el.jumpBottom.hidden = turns.length === 0 || nearBottom();
  }
  function createTurnView(task) {
    const wrapper = node("article", "chat-turn"); wrapper.dataset.taskId = String(task);
    const user = node("div", "message message-user"); const userBody = node("div", "message-body"); const userText = node("div", "message-text");
    userBody.append(userText); user.append(userBody);
    const assistant = node("div", "message message-assistant"); const avatar = node("span", "message-avatar", "i"); avatar.setAttribute("aria-hidden", "true");
    const body = node("div", "message-body"); const label = node("div", "message-label"); const stateLabel = node("span", "message-state");
    label.append(node("strong", "", "俺寻思"), stateLabel);
    const content = node("div", "message-content"); content.setAttribute("aria-live", "polite");
    const reasoning = node("details", "reasoning-details"); const reasoningSummary = node("summary"); reasoningSummary.append(node("span", "details-chevron", "›"), node("span", "", "推理内容"));
    const reasoningBody = node("div", "reasoning-body"); reasoning.append(reasoningSummary, reasoningBody);
    const process = node("details", "process-details"); process.dataset.taskId = String(task);
    const processSummary = node("summary"); const processLabel = node("span", "process-label"); const processCount = node("span", "process-count");
    processSummary.append(node("span", "details-chevron", "›"), processLabel, processCount); const processList = node("ol", "process-list"); process.append(processSummary, processList);
    const meta = node("div", "message-meta");
    const context = node("details", "context-details"); const contextSummary = node("summary"); contextSummary.append(node("span", "details-chevron", "›"), node("span", "", "查看上下文"));
    const contextPre = node("pre"); context.append(contextSummary, contextPre);
    body.append(label, content, reasoning, process, meta, context); assistant.append(avatar, body); wrapper.append(user, assistant);
    return { wrapper, userText, stateLabel, content, reasoning, reasoningBody, process, processLabel, processCount, processList, meta, context, contextPre, signature: "", eventSignature: "", contextSignature: "" };
  }
  function displayContent(content) {
    if (typeof content === "string") return { text: content, reasoning: "", json: false };
    if (content && typeof content === "object") {
      if (typeof content.text === "string") return { text: content.text, reasoning: typeof content.reasoning_content === "string" ? content.reasoning_content : "", json: false };
      const answer = content.answer;
      if (answer?.status === "ok" && typeof answer.payload?.sum === "number") return { text: String(answer.payload.sum), reasoning: "", json: false };
      if (typeof content.sum === "number") return { text: String(content.sum), reasoning: "", json: false };
    }
    return { text: content === null || content === undefined ? "" : asText(content), reasoning: "", json: true };
  }
  function stageText(turn) {
    const events = Array.isArray(turn.events) ? turn.events : [];
    const last = events.at(-1)?.event;
    if (last?.kind === "needs_compaction") return "上下文接近上限，等待压缩…";
    if (last?.kind === "needs_intervention") return "需要人工处理，执行已暂停。";
    if (last?.kind === "model_output" && last.reply?.tool_calls?.length) return `正在调用 ${last.reply.tool_calls.map((call) => call.name).join("、")}…`;
    if (last?.kind === "tool_finished") return "工具结果已收到，正在继续…";
    if (last?.kind === "model_output") return "正在提交回答…";
    return typeof turn.stage === "string" && turn.stage ? turn.stage : "正在调用模型…";
  }
  function updateTurn(view, turn) {
    text(view.userText, turn.prompt);
    const signature = JSON.stringify([turn.state, turn.stage, turn.result, turn.events?.at(-1)]);
    if (signature !== view.signature) {
      view.signature = signature;
      text(view.stateLabel, turn.state === "running" ? "工作中" : states[turn.state]);
      view.stateLabel.className = `message-state state-${turn.state}`;
      view.content.replaceChildren(); view.content.className = "message-content"; view.reasoning.hidden = true;
      if (turn.state === "running") {
        view.content.classList.add("is-pending");
        const dot = node("span", "pending-dot"); dot.setAttribute("aria-hidden", "true");
        view.content.append(dot, node("span", "", stageText(turn)));
      } else if (turn.state === "failed") {
        const block = node("div", "message-error"); const error = turn.result?.error;
        block.append(node("p", "message-text", typeof error?.message === "string" ? error.message : "任务未能完成，请查看执行过程。"));
        if (error?.code) block.append(node("code", "error-code", error.code));
        view.content.append(block);
      } else if (turn.state === "cancelled") view.content.append(node("p", "message-muted", "已停止本次执行。已发生的外部操作不保证撤回。"));
      else {
        const content = turn.result && Object.hasOwn(turn.result, "content") ? turn.result.content : turn.result;
        const display = displayContent(content);
        const answer = node(display.json ? "pre" : "div", display.json ? "message-json" : "message-text assistant-answer", display.text || "（模型返回了空内容）");
        view.content.append(answer);
        text(view.reasoningBody, display.reasoning); view.reasoning.hidden = !display.reasoning;
      }
      view.meta.replaceChildren();
      const created = dateLabel(turn.created_at_ms);
      if (created) view.meta.append(node("span", "", created));
      if (Number.isFinite(turn.result?.steps)) view.meta.append(node("span", "", `${turn.result.steps} 次模型请求`));
      if (Number.isFinite(turn.used) && Number.isFinite(turn.max_input)) view.meta.append(node("span", "", `上下文 ${turn.used} / ${turn.max_input}`));
    }
    const events = Array.isArray(turn.events) ? turn.events : [];
    const eventSignature = pretty(events);
    if (eventSignature !== view.eventSignature) {
      view.eventSignature = eventSignature; view.processList.replaceChildren();
      const tools = events.filter((record) => record.event?.kind === "tool_finished");
      const steps = events.filter((record) => record.event?.kind === "step_started").length;
      text(view.processLabel, tools.length ? `工具与执行过程 · ${tools.length} 个工具结果` : "执行过程");
      text(view.processCount, steps ? `${steps} 步` : `${events.length} 条`);
      events.forEach((record) => {
        const info = eventInfo(record.event || {});
        const item = node("li", `process-item${info.error ? " is-error" : ""}`);
        const heading = node("div", "process-item-heading"); heading.append(node("span", "process-kind", info.tag), node("span", "", info.title));
        item.append(heading); if (info.body) item.append(node("div", "process-item-body", info.body));
        view.processList.append(item);
      });
    }
    view.process.hidden = events.length === 0;
    const context = Array.isArray(turn.context) ? turn.context : [];
    view.context.hidden = context.length === 0;
    const contextSignature = pretty(context);
    if (contextSignature !== view.contextSignature) { view.contextSignature = contextSignature; text(view.contextPre, contextSignature); }
  }
  function eventInfo(event) {
    switch (event.kind) {
      case "step_started": return { tag: "MODEL", title: `第 ${event.step ?? "?"} 步 · 请求模型`, body: "发送当前上下文，等待模型返回。" };
      case "model_output": {
        const calls = Array.isArray(event.reply?.tool_calls) ? event.reply.tool_calls : [];
        return { tag: "MODEL", title: calls.length ? `请求工具：${calls.map((call) => call.name).join("、")}` : "模型已返回回答", body: calls.length ? calls.map((call) => `${call.name} ${brief(call.arguments, 250)}`).join("\n") : "完整回答显示在对话中。" };
      }
      case "tool_finished": return { tag: "TOOL", title: `${event.call?.name || "工具"} · ${event.result?.status === "ok" ? "执行完成" : "返回错误"}`, body: brief(event.result?.payload, 800), error: event.result?.status !== "ok" };
      case "needs_compaction": return { tag: "PAUSE", title: "等待上下文压缩", body: "循环已暂停，没有自动丢弃历史内容。" };
      case "needs_intervention": return { tag: "PAUSE", title: "需要人工处理", body: event.error?.message || "暂时无法确认任务状态。", error: true };
      case "ended": return { tag: "END", title: event.outcome?.state === "completed" ? "本次执行完成" : event.outcome?.state === "cancelled" ? "本次执行已停止" : "本次执行失败", body: event.outcome?.error?.message || "", error: event.outcome?.state === "failed" };
      case "large_event": return { tag: "INFO", title: "详细事件超出展示上限", body: "后端仅提供概要。" };
      default: return { tag: "EVENT", title: typeof event.kind === "string" ? event.kind : "执行事件", body: "" };
    }
  }

  function updateComposer() {
    const length = messageLength(el.prompt.value);
    const running = activeTurn();
    text(el.count, `${length} / ${MAX_MESSAGE}`);
    el.count.classList.toggle("is-over", length > MAX_MESSAGE);
    el.prompt.rows = Math.max(3, Math.min(7, el.prompt.value.split("\n").length + 1));
    el.send.hidden = !!running;
    el.stop.hidden = !running;
    el.stop.disabled = stopping === running?.task;
    text(el.stopLabel, stopping === running?.task ? "停止中" : "停止");
    el.send.disabled = submitting || !connected || !ready() || !el.prompt.value.trim() || length > MAX_MESSAGE || !!running;
    el.send.classList.toggle("is-loading", submitting);
    text(el.sendIcon, submitting ? "…" : "↑");
    text(el.composerStatus, !connected ? "等待连接" : !ready() ? "请先配置模型" : running ? "Agent 正在工作" : provider?.mode === "offline" ? "离线演示" : "准备就绪");
    el.prompt.setCustomValidity("");
  }
  async function sendMessage(event) {
    event?.preventDefault();
    if (submitting || activeTurn()) return;
    const content = el.prompt.value;
    if (!content.trim()) return;
    if (messageLength(content) > MAX_MESSAGE) { showActionError(`消息不能超过 ${MAX_MESSAGE} 字。`); return; }
    if (!ready()) { showActionError("请先在设置中连接模型，或手动选择离线演示。"); openSettings(); return; }
    if (!connected) { showActionError("服务连接已中断，请稍后重试。"); return; }
    submitting = true; showActionError(""); saveDraft(); updateComposer();
    let id = selectedId;
    const sourceKey = draftKey(id);
    try {
      if (id === null) {
        const created = await request("/api/conversations", { method: "POST", body: {} });
        if (!Number.isSafeInteger(created?.conversation)) throw new Error("服务没有返回会话编号。");
        id = created.conversation;
        drafts.set(String(id), drafts.get(sourceKey) || content);
        if (selectedId === null) { selectedId = id; selectedUntil = Date.now() + 6000; renderedConversation = undefined; }
      }
      const response = await request(`/api/conversations/${id}/messages`, { method: "POST", body: { content } });
      if (!Number.isSafeInteger(response?.task)) throw new Error("服务没有返回执行编号，请先刷新会话检查是否已发送。");
      pendingTurns.set(id, { task: response.task, state: "running" });
      if (drafts.get(String(id)) === content) drafts.set(String(id), "");
      if (sourceKey === "new" && drafts.get("new") === content) drafts.set("new", "");
      if (selectedId === id && el.prompt.value === content) { el.prompt.value = ""; drafts.set(String(id), ""); }
      if (selectedId === id) { selectedUntil = Date.now() + 6000; el.prompt.focus(); scrollBottom(); }
      announce("消息已发送。");
      await refreshState();
    } catch (error) { showActionError(errorText(error)); }
    finally { submitting = false; updateComposer(); }
  }
  async function stopTask() {
    const turn = activeTurn();
    if (!turn || stopping === turn.task) return;
    stopping = turn.task; updateComposer(); showActionError("");
    try { await request(`/api/tasks/${turn.task}/cancel`, { method: "POST", body: {} }); announce("停止请求已提交。"); await refreshState(); }
    catch (error) { showActionError(errorText(error)); }
    finally { stopping = null; updateComposer(); }
  }

  function setSettingsFeedback(message, kind = "error") {
    text(el.feedback, message); el.feedback.hidden = !message; el.feedback.className = `settings-feedback is-${kind}`;
  }
  function toggleSettingsMode() {
    const offline = el.mode.value === "offline";
    el.apiSettings.hidden = offline; el.offlineSettings.hidden = !offline;
    el.key.disabled = settingsBusy || settingsLoading || el.clearKey.checked;
  }
  function setSettingsBusy(busy, action = "") {
    settingsBusy = busy;
    const disabled = busy || settingsLoading;
    for (const input of [el.mode, el.endpoint, el.modelInput, el.key, el.clearKey, el.systemPrompt, el.temperature, el.maxTokens, el.timeout]) input.disabled = disabled;
    el.save.disabled = disabled; el.test.disabled = disabled;
    el.settingsClose.disabled = busy; el.settingsCancel.disabled = busy;
    el.save.classList.toggle("is-loading", busy && action === "save"); el.test.classList.toggle("is-loading", busy && action === "test");
    text(el.saveLabel, busy && action === "save" ? "正在应用…" : "保存并应用"); text(el.testLabel, busy && action === "test" ? "正在测试…" : "测试连接");
    toggleSettingsMode();
  }
  function fillSettings(value) {
    const config = normalizeProvider(value);
    el.mode.value = config.mode; el.endpoint.value = config.endpoint; el.modelInput.value = config.model;
    el.key.value = ""; el.clearKey.checked = false;
    el.key.placeholder = config.api_key_set ? "已设置 · 留空保留，输入新值以替换" : "输入 API Key（仅保存在服务内存）";
    text(el.keyStatus, config.api_key_set ? "当前内存中已设置" : "尚未设置");
    text(el.keyHint, config.api_key_set ? "留空会保留当前密钥。重启服务后需要重新填写。" : "密钥只保存在当前服务内存，重启后需要重新填写。");
    el.systemPrompt.value = config.system_prompt;
    el.temperature.value = config.temperature === null ? "" : String(config.temperature);
    el.maxTokens.value = String(config.max_tokens); el.timeout.value = String(config.timeout_secs);
    toggleSettingsMode();
  }
  async function openSettings() {
    settingsReturnFocus = mobile.matches && el.sidebar.contains(document.activeElement) ? el.sidebarToggle : document.activeElement;
    closeSidebar(false);
    if (!el.dialog.open) el.dialog.showModal();
    if (settingsBusy) return;
    const sequence = ++settingsLoadSequence;
    settingsLoading = true;
    fillSettings(provider || { mode: "api" });
    setSettingsBusy(false);
    setSettingsFeedback("正在读取模型设置…", "info");
    try {
      const result = await request("/api/settings");
      if (sequence !== settingsLoadSequence || !el.dialog.open) return;
      provider = normalizeProvider(result); fillSettings(provider); setSettingsFeedback(""); renderProvider(); updateComposer();
    } catch (error) { if (sequence === settingsLoadSequence) setSettingsFeedback(errorText(error)); }
    finally { if (sequence === settingsLoadSequence) { settingsLoading = false; setSettingsBusy(false); } }
  }
  function settingsBody() {
    const mode = el.mode.value;
    const endpoint = el.endpoint.value.trim(); const model = el.modelInput.value.trim();
    if (mode === "api") {
      if (!endpoint || !model) throw new Error("请填写 API Endpoint 和模型名称。");
      let url;
      try { url = new URL(endpoint); } catch (_) { throw new Error("API Endpoint 不是有效地址。"); }
      if (!["http:", "https:"].includes(url.protocol) || url.username || url.password) throw new Error("Endpoint 只支持不含用户名和密码的 HTTP/HTTPS 地址。");
    }
    const temperature = el.temperature.value.trim() === "" ? null : Number(el.temperature.value);
    const maxTokens = Number(el.maxTokens.value); const timeout = Number(el.timeout.value);
    if (temperature !== null && (!Number.isFinite(temperature) || temperature < 0 || temperature > 2)) throw new Error("Temperature 需要在 0–2 之间，或留空。");
    if (!Number.isSafeInteger(maxTokens) || maxTokens < 1 || maxTokens > 65536) throw new Error("最大输出 Tokens 需要是 1–65536 的整数。");
    if (!Number.isSafeInteger(timeout) || timeout < 1 || timeout > 120) throw new Error("超时需要是 1–120 秒的整数。");
    const body = { mode, endpoint, model, system_prompt: el.systemPrompt.value, temperature, max_tokens: maxTokens, timeout_secs: timeout, clear_api_key: el.clearKey.checked };
    if (!el.clearKey.checked && el.key.value.trim()) body.api_key = el.key.value.trim();
    return body;
  }
  async function saveSettings(event) {
    event.preventDefault();
    if (settingsBusy || settingsLoading) return;
    let body;
    try { body = settingsBody(); } catch (error) { setSettingsFeedback(errorText(error)); return; }
    const secret = body.api_key || "";
    setSettingsBusy(true, "save"); setSettingsFeedback("正在应用模型设置…", "info");
    // The secret is sent once, never stored in application state/browser storage,
    // never echoed as a fake masked input, and cleared even on failed requests.
    const operation = request("/api/settings", { method: "POST", body, timeout: 30000 });
    el.key.value = "";
    try {
      const result = await operation;
      provider = normalizeProvider(result); fillSettings(provider); renderProvider(); updateComposer();
      setSettingsFeedback("设置已保存并应用。API Key 只保存在当前服务内存。", "success"); announce("模型设置已应用。");
      await refreshState();
    } catch (error) {
      let message = hideSecret(errorText(error), secret);
      if (secret) message += "\n新输入的密钥已清空；若重试，请重新填写。";
      setSettingsFeedback(message);
    } finally { el.key.value = ""; setSettingsBusy(false); }
  }
  async function testConnection() {
    if (settingsBusy || settingsLoading) return;
    let body;
    try { body = settingsBody(); } catch (error) { setSettingsFeedback(errorText(error)); return; }
    const secret = body.api_key || "";
    setSettingsBusy(true, "test"); setSettingsFeedback(body.mode === "api" ? "正在向模型发送最小测试请求…" : "正在检查离线演示…", "info");
    const operation = request("/api/settings/test", { method: "POST", body, timeout: 20000 });
    el.key.value = "";
    try {
      const result = await operation;
      if (result?.ok !== true) throw new Error("测试没有返回成功状态。");
      const latency = Number.isFinite(result.latency_ms) ? ` · ${Math.round(result.latency_ms)} ms` : "";
      const model = typeof result.model === "string" && result.model ? ` · ${result.model}` : "";
      let message = `连接测试成功${latency}${model}。${typeof result.message === "string" ? `\n${result.message}` : ""}\n测试不会保存设置。`;
      if (secret) message += "新输入的密钥已清空，保存前请重新填写。";
      setSettingsFeedback(hideSecret(message, secret), "success");
    } catch (error) {
      let message = hideSecret(errorText(error), secret);
      if (secret) message += "\n新输入的密钥已清空，请重新填写后再试。";
      setSettingsFeedback(message);
    } finally { el.key.value = ""; setSettingsBusy(false); }
  }
  function closeSettings() { if (settingsBusy) return; el.key.value = ""; el.dialog.close(); }

  function openSidebar() {
    if (!mobile.matches) return;
    document.body.classList.add("sidebar-open"); el.sidebar.inert = false; el.workspace.inert = true; el.backdrop.hidden = false;
    el.sidebarToggle.setAttribute("aria-expanded", "true"); el.sidebarClose.focus();
  }
  function closeSidebar(restoreFocus = true) {
    const opened = document.body.classList.contains("sidebar-open");
    document.body.classList.remove("sidebar-open"); el.backdrop.hidden = true; el.workspace.inert = false;
    el.sidebar.inert = mobile.matches; el.sidebarToggle.setAttribute("aria-expanded", "false");
    if (opened && restoreFocus) el.sidebarToggle.focus();
  }
  el.form.addEventListener("submit", sendMessage);
  el.prompt.addEventListener("input", () => { saveDraft(); updateComposer(); });
  el.prompt.addEventListener("compositionstart", () => { composing = true; });
  el.prompt.addEventListener("compositionend", () => { composing = false; });
  el.prompt.addEventListener("keydown", (event) => {
    if (event.key !== "Enter" || composing || event.isComposing || event.keyCode === 229) return;
    if (event.shiftKey && !event.ctrlKey && !event.metaKey) return;
    event.preventDefault();
    if (!submitting && !activeTurn()) el.form.requestSubmit();
  });
  el.newConversation.addEventListener("click", newConversation);
  el.stop.addEventListener("click", stopTask);
  el.retry.addEventListener("click", refreshState);
  el.scroll.addEventListener("scroll", () => { el.jumpBottom.hidden = nearBottom() || !currentConversation()?.turns.length; }, { passive: true });
  el.jumpBottom.addEventListener("click", scrollBottom);
  [el.settings, el.modelButton, el.configure].forEach((button) => button.addEventListener("click", openSettings));
  el.settingsForm.addEventListener("submit", saveSettings);
  el.test.addEventListener("click", testConnection);
  el.mode.addEventListener("change", toggleSettingsMode);
  el.clearKey.addEventListener("change", () => { if (el.clearKey.checked) el.key.value = ""; toggleSettingsMode(); });
  el.settingsClose.addEventListener("click", closeSettings); el.settingsCancel.addEventListener("click", closeSettings);
  el.dialog.addEventListener("cancel", (event) => { if (settingsBusy) event.preventDefault(); });
  el.dialog.addEventListener("close", () => { ++settingsLoadSequence; el.key.value = ""; settingsLoading = false; if (settingsReturnFocus?.isConnected) settingsReturnFocus.focus(); });
  el.dialog.addEventListener("click", (event) => {
    if (event.target !== el.dialog) return;
    const rect = el.dialog.getBoundingClientRect();
    if (event.clientX < rect.left || event.clientX > rect.right || event.clientY < rect.top || event.clientY > rect.bottom) closeSettings();
  });
  el.sidebarToggle.addEventListener("click", openSidebar); el.sidebarClose.addEventListener("click", () => closeSidebar()); el.backdrop.addEventListener("click", () => closeSidebar());
  mobile.addEventListener("change", () => closeSidebar(false));
  document.addEventListener("keydown", (event) => {
    if ((event.ctrlKey || event.metaKey) && event.key.toLowerCase() === "k" && !el.dialog.open && !event.isComposing) { event.preventDefault(); newConversation(); }
    if (!document.body.classList.contains("sidebar-open") || el.dialog.open) return;
    if (event.key === "Escape") { event.preventDefault(); closeSidebar(); }
    if (event.key === "Tab") {
      const focusable = Array.from(el.sidebar.querySelectorAll("button:not([disabled]), a[href]")).filter((element) => element.getClientRects().length);
      const first = focusable[0], last = focusable.at(-1);
      if (event.shiftKey && document.activeElement === first) { event.preventDefault(); last?.focus(); }
      else if (!event.shiftKey && document.activeElement === last) { event.preventDefault(); first?.focus(); }
    }
  });
  document.querySelectorAll("[data-prompt]").forEach((button) => button.addEventListener("click", () => {
    el.prompt.value = button.dataset.prompt || ""; saveDraft(); updateComposer(); el.prompt.focus();
  }));
  document.addEventListener("visibilitychange", () => { if (document.visibilityState === "visible") refreshState(); });
  closeSidebar(false); updateComposer(); refreshState();
})();
