//! hwatu backend for the browser tool.
//!
//! hwatu (https://github.com/hongnoul/hwatu) is a daemon-based WebKitGTK
//! browser for tiling WMs. Its daemon exposes automation IPC (eval /
//! navigate / screenshot / wait_load / focus) over a Unix socket as
//! newline-delimited JSON, one request per connection. This provider
//! maps jcode browser actions onto that surface, emulating DOM-level
//! actions (click, type, fill_form, interactables, ...) with JS run
//! through `eval`, the same way the Firefox bridge's content script
//! does on the other side of its extension.

use super::{
    BrowserInput, BrowserProvider, ToolContext, ToolOutput, attach_browser_metadata,
    build_press_script,
};
use anyhow::{Context, Result};
use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Map, Value, json};
use std::path::PathBuf;
use std::sync::atomic::{AtomicI64, Ordering};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

pub(super) struct HwatuProvider;

pub(super) static HWATU_PROVIDER: HwatuProvider = HwatuProvider;

/// The hwatu window this jcode process last opened or targeted. The
/// daemon keeps its own "last target" for id-less commands, but that
/// state is global across every client: with several jcode sessions
/// sharing one daemon, another agent's open would silently redirect
/// this session's id-less eval/screenshot onto *their* page. Pinning
/// the session's own window here keeps concurrent agents isolated
/// without protocol changes. 0 means "none yet".
static SESSION_WINDOW: AtomicI64 = AtomicI64::new(0);

fn remember_window(response: &Value) {
    if let Some(id) = response
        .get("window")
        .and_then(|w| w.get("id"))
        .and_then(|v| v.as_i64())
    {
        SESSION_WINDOW.store(id, Ordering::Relaxed);
    }
}

/// The window a tool call should address: an explicit id always wins,
/// else the window this session opened, provided it is still alive.
async fn session_window_id(input: &BrowserInput) -> Option<i64> {
    if let Some(id) = input.window_id.or(input.tab_id) {
        return Some(id);
    }
    let remembered = SESSION_WINDOW.load(Ordering::Relaxed);
    if remembered == 0 {
        return None;
    }
    // Stale ids (window closed, daemon restarted) must not turn into
    // hard "no window N" errors; fall back to daemon-side resolution.
    let alive = list_windows().await.ok().is_some_and(|ws| {
        ws.iter()
            .any(|w| w.get("id").and_then(|v| v.as_i64()) == Some(remembered))
    });
    if alive {
        Some(remembered)
    } else {
        SESSION_WINDOW.store(0, Ordering::Relaxed);
        None
    }
}

/// Daemon socket: `$XDG_RUNTIME_DIR/hwatu.sock`, matching hwatu-ipc.
fn socket_path() -> Option<PathBuf> {
    std::env::var("XDG_RUNTIME_DIR")
        .ok()
        .map(|dir| PathBuf::from(dir).join("hwatu.sock"))
}

/// The hwatu client binary, used only to auto-spawn the daemon.
fn hwatu_binary() -> Option<PathBuf> {
    if let Ok(explicit) = std::env::var("JCODE_HWATU_BIN") {
        let p = PathBuf::from(explicit);
        if p.exists() {
            return Some(p);
        }
    }
    if let Some(home) = dirs::home_dir() {
        let local = home.join(".local/bin/hwatu");
        if local.exists() {
            return Some(local);
        }
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join("hwatu"))
        .find(|candidate| candidate.exists())
}

/// True when the hwatu daemon socket exists (daemon likely live) or
/// the client binary is installed (daemon can be auto-spawned).
pub(super) fn hwatu_available() -> bool {
    socket_path().is_some_and(|p| p.exists()) || hwatu_binary().is_some()
}

/// One IPC roundtrip: connect, send request line, read response line.
async fn ipc(request: Value) -> Result<Value> {
    let path = socket_path().context("XDG_RUNTIME_DIR is not set; cannot locate hwatu socket")?;
    let mut stream = UnixStream::connect(&path)
        .await
        .with_context(|| format!("hwatu daemon is not reachable at {}", path.display()))?;
    let mut payload = serde_json::to_vec(&request)?;
    payload.push(b'\n');
    stream.write_all(&payload).await?;

    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).await?;
    let response: Value =
        serde_json::from_str(line.trim()).context("bad response from hwatu daemon")?;

    if response.get("status").and_then(|v| v.as_str()) == Some("err") {
        let message = response
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown hwatu error");
        anyhow::bail!("hwatu: {}", message);
    }
    Ok(response)
}

async fn ping() -> bool {
    ipc(json!({"cmd": "ping"})).await.is_ok()
}

/// Ping; if the daemon is down, spawn it through the hwatu client
/// (which forks hwatud and waits for the socket) and ping again.
async fn ensure_daemon() -> Result<()> {
    if ping().await {
        return Ok(());
    }
    let bin = hwatu_binary().context(
        "hwatu is not installed (no hwatu binary on PATH or in ~/.local/bin). \
         Install it from https://github.com/hongnoul/hwatu",
    )?;
    let output = tokio::process::Command::new(&bin)
        .arg("ping")
        .output()
        .await
        .with_context(|| format!("failed to run {}", bin.display()))?;
    if !output.status.success() {
        anyhow::bail!(
            "could not start the hwatu daemon: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    if ping().await {
        Ok(())
    } else {
        anyhow::bail!("hwatu daemon started but its socket is not responding")
    }
}

/// Run a JS function body in the target window and return its value.
async fn eval_js(window_id: Option<i64>, js: &str, timeout_ms: Option<u64>) -> Result<Value> {
    let mut req = Map::new();
    req.insert("cmd".into(), json!("eval"));
    req.insert("js".into(), json!(js));
    if let Some(id) = window_id {
        req.insert("id".into(), json!(id));
    }
    if let Some(t) = timeout_ms {
        req.insert("timeout_ms".into(), json!(t));
    }
    let response = ipc(Value::Object(req)).await?;
    Ok(response.get("value").cloned().unwrap_or(Value::Null))
}

/// Windows double as tabs: hwatu has no tabs, the WM tiles windows.
async fn list_windows() -> Result<Vec<Value>> {
    let response = ipc(json!({"cmd": "list"})).await?;
    Ok(response
        .get("windows")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default())
}

/// JS helper prelude injected before action snippets: element lookup
/// by CSS selector or visible text, and a native-setter `setValue`
/// that React/Vue controlled inputs respect.
const JS_HELPERS: &str = r#"
const findEl = (selector, text) => {
  if (selector) {
    const el = document.querySelector(selector);
    if (!el) throw new Error('no element matches selector: ' + selector);
    return el;
  }
  if (text) {
    const walk = document.createTreeWalker(document.body, NodeFilter.SHOW_ELEMENT);
    let best = null;
    while (walk.nextNode()) {
      const el = walk.currentNode;
      const t = (el.innerText || el.value || '').trim();
      if (!t || !t.includes(text)) continue;
      if (!best || el.contains(best) === false && best.contains(el)) best = el;
      if (!best || best.innerText.length > t.length) best = el;
    }
    if (!best) throw new Error('no element contains text: ' + text);
    const clickable = best.closest('a,button,[role=button],input,select,textarea,label,[onclick]');
    return clickable || best;
  }
  throw new Error('selector or text required');
};
const setValue = (el, value) => {
  const proto = el instanceof HTMLTextAreaElement ? HTMLTextAreaElement.prototype
    : el instanceof HTMLSelectElement ? HTMLSelectElement.prototype
    : HTMLInputElement.prototype;
  const desc = Object.getOwnPropertyDescriptor(proto, 'value');
  if (desc && desc.set) desc.set.call(el, value); else el.value = value;
  el.dispatchEvent(new Event('input', { bubbles: true }));
  el.dispatchEvent(new Event('change', { bubbles: true }));
};
const describe = (el) => ({
  tag: el.tagName.toLowerCase(),
  text: (el.innerText || el.value || '').trim().slice(0, 120),
});
"#;

fn window_title(action: &str) -> String {
    format!("browser {}", action)
}

#[async_trait]
impl BrowserProvider for HwatuProvider {
    fn id(&self) -> &'static str {
        "hwatu"
    }

    fn supported_browsers(&self) -> &'static [&'static str] {
        &["hwatu"]
    }

    async fn status(&self, _ctx: &ToolContext) -> Result<ToolOutput> {
        let binary = hwatu_binary();
        let responding = ping().await;
        let metadata = json!({
            "backend": self.id(),
            "browser": "hwatu",
            "binary_installed": binary.is_some(),
            "responding": responding,
            "ready": responding,
        });
        let text = if responding {
            "hwatu daemon is running and responding.".to_string()
        } else if binary.is_some() {
            "hwatu is installed but the daemon is not running. Any browser action will auto-start it.".to_string()
        } else {
            "hwatu is not installed. Install it from https://github.com/hongnoul/hwatu or use browser='firefox'.".to_string()
        };
        Ok(attach_browser_metadata(
            ToolOutput::new(text)
                .with_title("browser status")
                .with_metadata(metadata),
            self.id(),
            "hwatu",
        ))
    }

    async fn setup(&self) -> Result<ToolOutput> {
        let result = ensure_daemon().await;
        let (text, ready) = match result {
            Ok(()) => ("hwatu daemon is running. No further setup needed.".to_string(), true),
            Err(e) => (format!("hwatu setup failed: {e:#}"), false),
        };
        Ok(attach_browser_metadata(
            ToolOutput::new(text)
                .with_title(if ready {
                    "browser setup"
                } else {
                    "browser setup (incomplete)"
                })
                .with_metadata(json!({"ready": ready, "backend": self.id(), "browser": "hwatu"})),
            self.id(),
            "hwatu",
        ))
    }

    async fn ensure_ready(&self) -> Result<Option<String>> {
        ensure_daemon().await?;
        Ok(None)
    }

    async fn execute(
        &self,
        action: &str,
        input: &BrowserInput,
        _ctx: &ToolContext,
    ) -> Result<ToolOutput> {
        let output = execute_hwatu_action(action, input).await?;
        Ok(attach_browser_metadata(output, self.id(), "hwatu"))
    }
}

/// hwatu open mode for agent-driven windows. Agents verify pages, so
/// the default is "headless": no toplevel is ever mapped, the WM never
/// sees the window, and eval/screenshot still work (hwatud realizes
/// the widget and allocates a 1024x768 viewport). An explicit
/// `focus: true` on the tool call opts into "normal" (present) mode.
/// `JCODE_HWATU_OPEN_MODE=background|normal|headless` overrides the
/// non-focused default, e.g. for users who want verification windows
/// visible-but-unfocused in their tiler.
fn open_mode(input: &BrowserInput) -> &'static str {
    if input.focus.unwrap_or(false) {
        return "normal";
    }
    match std::env::var("JCODE_HWATU_OPEN_MODE").as_deref() {
        Ok("background") => "background",
        Ok("normal") => "normal",
        _ => "headless",
    }
}

async fn execute_hwatu_action(action: &str, input: &BrowserInput) -> Result<ToolOutput> {
    // hwatu has no tabs; `window_id` and `tab_id` both address windows.
    // With neither set, target the window this session opened rather
    // than deferring to the daemon's global last-target, which another
    // concurrent jcode session may have moved to its own window.
    let window_id = session_window_id(input).await;
    let title = window_title(action);

    match action {
        "open" => {
            let url = input
                .url
                .as_deref()
                .context("url is required for open")?;
            let response = if window_id.is_none() || input.new_tab.unwrap_or(false) {
                // No window owned by this session (or a new one
                // requested): open one. Navigating an id-less target
                // here could hijack a window some *other* jcode session
                // is driving on the shared daemon. Agents default to
                // headless mode so nothing appears in the WM; pass
                // focus=true to present a visible window instead.
                let response =
                    ipc(json!({"cmd": "open", "url": url, "mode": open_mode(input)})).await?;
                remember_window(&response);
                response
            } else {
                let mut req = Map::new();
                req.insert("cmd".into(), json!("navigate"));
                req.insert("url".into(), json!(url));
                req.insert("wait".into(), json!(input.wait.unwrap_or(true)));
                if let Some(id) = window_id {
                    req.insert("id".into(), json!(id));
                }
                if let Some(t) = input.timeout_ms {
                    req.insert("timeout_ms".into(), json!(t));
                }
                let response = ipc(Value::Object(req)).await?;
                remember_window(&response);
                response
            };
            let window = response.get("window").cloned().unwrap_or(Value::Null);
            Ok(ToolOutput::new(format!("Opened {}", url))
                .with_title(title)
                .with_metadata(window))
        }
        "list_tabs" | "get_active_tab" => {
            let windows = list_windows().await?;
            if action == "get_active_tab" {
                let active = windows
                    .iter()
                    .find(|w| w.get("focused").and_then(|v| v.as_bool()).unwrap_or(false))
                    .or_else(|| windows.first());
                let body = match active {
                    Some(w) => serde_json::to_string_pretty(w)?,
                    None => "No windows open.".to_string(),
                };
                return Ok(ToolOutput::new(body)
                    .with_title(title)
                    .with_metadata(json!({"windows": windows})));
            }
            let mut lines = Vec::new();
            for w in &windows {
                lines.push(format!(
                    "{}. {} {} {}{}",
                    w.get("id").and_then(|v| v.as_u64()).unwrap_or(0),
                    w.get("title").and_then(|v| v.as_str()).unwrap_or(""),
                    w.get("url").and_then(|v| v.as_str()).unwrap_or(""),
                    if w.get("focused").and_then(|v| v.as_bool()).unwrap_or(false) {
                        "[focused]"
                    } else {
                        ""
                    },
                    if w.get("suspended").and_then(|v| v.as_bool()).unwrap_or(false) {
                        "[suspended]"
                    } else {
                        ""
                    },
                ));
            }
            let body = if lines.is_empty() {
                "No windows open. hwatu windows double as tabs (the WM tiles them).".to_string()
            } else {
                lines.join("\n")
            };
            Ok(ToolOutput::new(body)
                .with_title(title)
                .with_metadata(json!({"windows": windows})))
        }
        "new_tab" => {
            let mut req = Map::new();
            req.insert("cmd".into(), json!("open"));
            req.insert("mode".into(), json!(open_mode(input)));
            if let Some(url) = &input.url {
                req.insert("url".into(), json!(url));
            }
            let response = ipc(Value::Object(req)).await?;
            remember_window(&response);
            let window = response.get("window").cloned().unwrap_or(Value::Null);
            Ok(ToolOutput::new(serde_json::to_string_pretty(&window)?)
                .with_title(title)
                .with_metadata(window))
        }
        "select_tab" => {
            let id = input
                .tab_id
                .or(input.window_id)
                .context("tab_id is required for select_tab")?;
            ipc(json!({"cmd": "focus", "id": id})).await?;
            Ok(ToolOutput::new(format!("Focused window {}", id)).with_title(title))
        }
        "snapshot" | "get_content" => {
            let format = if action == "snapshot" {
                "text"
            } else {
                input.format.as_deref().unwrap_or("text")
            };
            let js = match format {
                "html" => "return document.documentElement.outerHTML;",
                "title" => "return document.title + '\\n' + location.href;",
                _ => {
                    "return '# ' + document.title + '\\n' + location.href + '\\n\\n' + \
                     (document.body ? document.body.innerText : '');"
                }
            };
            let value = eval_js(window_id, js, input.timeout_ms).await?;
            let body = value.as_str().map(|s| s.to_string()).unwrap_or_default();
            Ok(ToolOutput::new(body).with_title(title))
        }
        "interactables" => {
            let js = format!(
                r#"{JS_HELPERS}
const seen = new Set();
const out = [];
const els = document.querySelectorAll('a[href],button,input,select,textarea,[role=button],[role=link],[role=tab],[onclick]');
for (const el of els) {{
  if (seen.has(el)) continue;
  seen.add(el);
  const rect = el.getBoundingClientRect();
  if (rect.width === 0 && rect.height === 0) continue;
  const d = describe(el);
  let selector = el.tagName.toLowerCase();
  if (el.id) selector += '#' + CSS.escape(el.id);
  else if (el.name) selector += `[name="${{el.name}}"]`;
  else if (el.className && typeof el.className === 'string') {{
    const cls = el.className.trim().split(/\s+/).slice(0, 2).map(CSS.escape).join('.');
    if (cls) selector += '.' + cls;
  }}
  out.push({{ type: el.tagName === 'A' ? 'link' : (el.type || 'element'), tag: d.tag, text: d.text, selector }});
  if (out.length >= 150) break;
}}
return {{ elements: out }};"#
            );
            let value = eval_js(window_id, &js, input.timeout_ms).await?;
            Ok(ToolOutput::new(super::format_interactables_result(&value))
                .with_title(title)
                .with_metadata(value))
        }
        "click" => {
            if input.selector.is_none() && input.text.is_none() && input.x.is_none() {
                anyhow::bail!("click requires selector, text, or x/y coordinates");
            }
            let js = if let (Some(x), Some(y)) = (input.x, input.y) {
                format!(
                    r#"const el = document.elementFromPoint({x}, {y});
if (!el) throw new Error('no element at ({x}, {y})');
el.click();
return {{ clicked: el.tagName.toLowerCase() }};"#
                )
            } else {
                format!(
                    r#"{JS_HELPERS}
const el = findEl({}, {});
el.scrollIntoView({{ block: 'center' }});
el.click();
return {{ clicked: describe(el) }};"#,
                    serde_json::to_string(&input.selector)?,
                    serde_json::to_string(&input.text)?,
                )
            };
            let value = eval_js(window_id, &js, input.timeout_ms).await?;
            Ok(ToolOutput::new(serde_json::to_string_pretty(&value)?)
                .with_title(title)
                .with_metadata(value))
        }
        "type" => {
            let text = input.text.as_deref().context("text is required for type")?;
            let js = format!(
                r#"{JS_HELPERS}
const el = {selector} ? findEl({selector}, null) : document.activeElement;
if (!el || el === document.body) throw new Error('no target element; pass a selector');
el.focus();
const current = {clear} ? '' : (el.value || '');
setValue(el, current + {text});
if ({submit} && el.form) el.form.requestSubmit ? el.form.requestSubmit() : el.form.submit();
return {{ typed: describe(el) }};"#,
                selector = serde_json::to_string(&input.selector)?,
                text = serde_json::to_string(text)?,
                clear = input.clear.unwrap_or(false),
                submit = input.submit.unwrap_or(false),
            );
            let value = eval_js(window_id, &js, input.timeout_ms).await?;
            Ok(ToolOutput::new(serde_json::to_string_pretty(&value)?)
                .with_title(title)
                .with_metadata(value))
        }
        "fill_form" | "select" => {
            let fields: Vec<Value> = if action == "select" {
                let selector = input
                    .selector
                    .as_deref()
                    .context("selector is required for select")?;
                let value = input
                    .text
                    .as_deref()
                    .context("text is required for select (used as the option value)")?;
                vec![json!({"selector": selector, "value": value})]
            } else {
                input
                    .fields
                    .as_ref()
                    .context("fields are required for fill_form")?
                    .iter()
                    .map(|f| {
                        json!({
                            "selector": f.selector,
                            "value": f.value,
                            "checked": f.checked,
                        })
                    })
                    .collect()
            };
            let js = format!(
                r#"{JS_HELPERS}
const fields = {fields};
const results = [];
for (const f of fields) {{
  const el = findEl(f.selector, null);
  if (typeof f.checked === 'boolean') {{
    el.checked = f.checked;
    el.dispatchEvent(new Event('change', {{ bubbles: true }}));
  }} else if (f.value !== null && f.value !== undefined) {{
    setValue(el, f.value);
  }}
  results.push(describe(el));
}}
return {{ filled: results }};"#,
                fields = serde_json::to_string(&fields)?,
            );
            let value = eval_js(window_id, &js, input.timeout_ms).await?;
            Ok(ToolOutput::new(serde_json::to_string_pretty(&value)?)
                .with_title(title)
                .with_metadata(value))
        }
        "wait" => {
            if input.selector.is_none() && input.text.is_none() && input.contains.is_none() {
                anyhow::bail!("wait requires selector, text, or contains");
            }
            let timeout = input.timeout_ms.unwrap_or(10_000);
            let js = format!(
                r#"const deadline = Date.now() + {timeout};
const selector = {selector};
const needle = {needle};
while (Date.now() < deadline) {{
  if (selector && document.querySelector(selector)) return {{ found: 'selector' }};
  if (needle && document.body && document.body.innerText.includes(needle)) return {{ found: 'text' }};
  await new Promise(r => setTimeout(r, 100));
}}
throw new Error('wait timed out after {timeout} ms');"#,
                selector = serde_json::to_string(&input.selector)?,
                needle = serde_json::to_string(
                    &input.contains.clone().or_else(|| input.text.clone())
                )?,
            );
            // Give the daemon-side eval deadline headroom over the JS loop.
            let value = eval_js(window_id, &js, Some(timeout + 2_000)).await?;
            Ok(ToolOutput::new(serde_json::to_string_pretty(&value)?)
                .with_title(title)
                .with_metadata(value))
        }
        "screenshot" => {
            let target = std::env::temp_dir().join(format!(
                "jcode-hwatu-{}.png",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis())
                    .unwrap_or(0)
            ));
            let mut req = Map::new();
            req.insert("cmd".into(), json!("screenshot"));
            req.insert("path".into(), json!(target.to_string_lossy()));
            if let Some(id) = window_id {
                req.insert("id".into(), json!(id));
            }
            let response = ipc(Value::Object(req)).await?;
            let saved = response
                .get("path")
                .and_then(|v| v.as_str())
                .map(PathBuf::from)
                .unwrap_or(target);
            let mut output =
                ToolOutput::new(format!("Captured hwatu screenshot to {}.", saved.display()))
                    .with_title(title)
                    .with_metadata(response.clone());
            if let Ok(bytes) = tokio::fs::read(&saved).await {
                output = output.with_labeled_image(
                    "image/png",
                    STANDARD.encode(&bytes),
                    format!("hwatu screenshot: {}", saved.display()),
                );
                let _ = tokio::fs::remove_file(&saved).await;
            }
            Ok(output)
        }
        "eval" => {
            let script = input
                .script
                .as_deref()
                .context("script is required for eval")?;
            let value = eval_js(window_id, script, input.timeout_ms).await?;
            let body = if let Some(s) = value.as_str() {
                s.to_string()
            } else {
                serde_json::to_string_pretty(&value)?
            };
            Ok(ToolOutput::new(body)
                .with_title(title)
                .with_metadata(json!({"result": value})))
        }
        "scroll" => {
            // Every arm ends with `reportScroll(...)` so the agent
            // always learns where the page landed (x/y/max_y/
            // at_bottom) and, for selector scrolls, what matched:
            // a bare `{scrolled: ...}` forces a screenshot just to
            // find out whether the scroll hit the right thing.
            const REPORT: &str = r#"
const reportScroll = async (matched) => {
  // One macrotask lets the instant scroll settle; rAF never fires
  // in unmapped/background windows, where agent scrolls run.
  await new Promise(r => setTimeout(r, 0));
  const doc = document.documentElement;
  const maxY = Math.max(0, doc.scrollHeight - window.innerHeight);
  return {
    x: window.scrollX,
    y: window.scrollY,
    max_y: maxY,
    at_bottom: window.scrollY >= maxY - 1,
    ...(matched === undefined ? {} : { matched }),
  };
};"#;
            let js = if let Some(position) = input.position.as_deref() {
                match position {
                    "top" => format!(
                        "{REPORT}\nwindow.scrollTo({{top: 0}});\nreturn reportScroll();"
                    ),
                    "bottom" => format!(
                        "{REPORT}\nwindow.scrollTo({{top: document.documentElement.scrollHeight}});\nreturn reportScroll();"
                    ),
                    other => anyhow::bail!("unsupported scroll position: {}", other),
                }
            } else if let Some(selector) = input.selector.as_deref() {
                // `contains` filters matches by text so an agent can
                // disambiguate repeated headings ("the h3 that says
                // Manage Joule Agents") instead of blindly taking the
                // first match in document order.
                format!(
                    r#"{REPORT}
const selector = {selector};
const contains = {contains};
let els = [...document.querySelectorAll(selector)];
const total = els.length;
if (contains !== null)
  els = els.filter(e => (e.textContent || '').includes(contains));
const el = els[0];
if (!el) {{
  const filt = contains === null ? '' : ` (${{els.length}} after contains filter)`;
  throw new Error(`no match: ${{total}} element(s) for selector${{filt}}`);
}}
el.scrollIntoView({{ block: 'center' }});
return reportScroll({{
  matches: els.length,
  tag: el.tagName.toLowerCase(),
  text: (el.textContent || '').trim().slice(0, 120),
}});"#,
                    selector = serde_json::to_string(selector)?,
                    contains = input
                        .contains
                        .as_deref()
                        .map_or(Ok("null".to_string()), serde_json::to_string)?,
                )
            } else if let Some(to) = &input.scroll_to {
                format!(
                    "{REPORT}\nwindow.scrollTo({{left: {}, top: {}}});\nreturn reportScroll();",
                    to.x.unwrap_or(0.0),
                    to.y.unwrap_or(0.0)
                )
            } else if input.x.is_some() || input.y.is_some() {
                format!(
                    "{REPORT}\nwindow.scrollBy({{left: {}, top: {}}});\nreturn reportScroll();",
                    input.x.unwrap_or(0.0),
                    input.y.unwrap_or(0.0)
                )
            } else {
                anyhow::bail!("scroll requires x/y, selector, position, or scroll_to");
            };
            let value = eval_js(window_id, &js, input.timeout_ms).await?;
            Ok(ToolOutput::new(serde_json::to_string_pretty(&value)?)
                .with_title(title)
                .with_metadata(value))
        }
        "press" => {
            let script = build_press_script(input.key.as_deref(), input.selector.as_deref())?;
            let value = eval_js(window_id, &script, input.timeout_ms).await?;
            Ok(ToolOutput::new(serde_json::to_string_pretty(&value)?)
                .with_title(title)
                .with_metadata(value))
        }
        "list_frames" => {
            let js = r#"const frames = [];
document.querySelectorAll('iframe,frame').forEach((f, i) => {
  frames.push({ index: i, src: f.src || null, name: f.name || null });
});
return { frames };"#;
            let value = eval_js(window_id, js, input.timeout_ms).await?;
            Ok(ToolOutput::new(serde_json::to_string_pretty(&value)?)
                .with_title(title)
                .with_metadata(value))
        }
        "provider_command" => {
            let cmd = input
                .provider_action
                .as_deref()
                .context("provider_action is required when action='provider_command'")?;
            let mut req = input
                .params
                .clone()
                .and_then(|v| v.as_object().cloned())
                .unwrap_or_default();
            req.insert("cmd".into(), json!(cmd));
            let response = ipc(Value::Object(req)).await?;
            Ok(ToolOutput::new(serde_json::to_string_pretty(&response)?)
                .with_title(title)
                .with_metadata(response))
        }
        "upload" => {
            let path = input
                .path
                .as_deref()
                .context("path is required for upload")?;
            let selector = input.selector.as_deref().unwrap_or("input[type=file]");
            let mut req = Map::new();
            req.insert("cmd".into(), json!("upload"));
            req.insert("selector".into(), json!(selector));
            req.insert("path".into(), json!(path));
            if let Some(id) = window_id {
                req.insert("id".into(), json!(id));
            }
            if let Some(t) = input.timeout_ms {
                req.insert("timeout_ms".into(), json!(t));
            }
            let response = ipc(Value::Object(req)).await?;
            let value = response.get("value").cloned().unwrap_or(Value::Null);
            Ok(ToolOutput::new(serde_json::to_string_pretty(&value)?)
                .with_title(title)
                .with_metadata(value))
        }
        other => anyhow::bail!("Unsupported browser action for hwatu backend: {}", other),
    }
}
