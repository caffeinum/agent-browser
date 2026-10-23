//! Page commands on WebDriver sessions (webkit, safari, ios) beyond the core
//! set: typing, keys, selection, checking, scrolling, waiting, and element
//! queries. Each returns the same JSON shape as its CDP handler. Targets are
//! `@eN` refs or CSS selectors, resolved through the DOM-walk registry.

use serde_json::{json, Value};

use super::backend::WebDriverBackend;
use super::dom_refs::{element_arg, resolve_element};
use crate::native::element::RefMap;

/// Actions handled here, with their CLI command names (for the supported list).
pub const ACTIONS: &[(&str, &str)] = &[
    ("type", "type"),
    ("press", "press"),
    ("select", "select"),
    ("check", "check"),
    ("uncheck", "uncheck"),
    ("scroll", "scroll"),
    ("scrollintoview", "scrollintoview"),
    ("wait", "wait"),
    ("waitforurl", ""),
    ("waitforloadstate", ""),
    ("waitforfunction", ""),
    ("getattribute", "get attr"),
    ("innerhtml", "get html"),
    ("innertext", ""),
    ("inputvalue", "get value"),
    ("count", "get count"),
    ("isvisible", "is visible"),
    ("isenabled", "is enabled"),
    ("ischecked", "is checked"),
    ("focus", "focus"),
    ("clear", ""),
    ("hover", "hover"),
    ("dblclick", "dblclick"),
    ("keyboard", "keyboard type"),
];

const POLL: std::time::Duration = std::time::Duration::from_millis(100);

pub async fn execute(
    action: &str,
    cmd: &Value,
    wb: &WebDriverBackend,
    refs: &RefMap,
    timeout_ms: u64,
    url_matches: fn(&str, &str) -> bool,
) -> Result<Value, String> {
    match action {
        "type" => {
            let el = element(cmd, wb, refs).await?;
            let text = required(cmd, "text")?;
            if cmd.get("clear").and_then(Value::as_bool).unwrap_or(false) {
                wb.clear_element(&el).await?;
            }
            let delay = cmd
                .get("delay")
                .and_then(Value::as_u64)
                .filter(|d| *d > 0)
                .map(std::time::Duration::from_millis);
            wb.type_text(&el, text, delay).await?;
            Ok(json!({ "typed": text }))
        }
        "keyboard" => {
            // Into whatever has focus, like Chrome's keyboard type/inserttext.
            // Both use Element Send Keys: webkit-cli 0.7.0 inserts long plain
            // runs as one trusted insertText already.
            let text = required(cmd, "text")?;
            let el = wb.focused_element().await?;
            match cmd.get("subaction").and_then(Value::as_str) {
                Some("type") => {
                    wb.type_text(&el, text, None).await?;
                    Ok(json!({ "typed": text }))
                }
                Some("insertText") => {
                    wb.type_text(&el, text, None).await?;
                    Ok(json!({ "inserted": text }))
                }
                other => Err(format!(
                    "keyboard {} is not supported on WebDriver; use keyboard type or keyboard inserttext",
                    other.unwrap_or("(none)")
                )),
            }
        }
        "press" => {
            let key = required(cmd, "key")?;
            wb.perform_actions(key_chord_actions(key)?).await?;
            Ok(json!({ "pressed": key }))
        }
        "select" => {
            let el = element(cmd, wb, refs).await?;
            let values = select_values(cmd);
            let missing = page_call(
                wb,
                "var el = arguments[0], wanted = arguments[1];
                 if (el.tagName !== 'SELECT') return 'not a <select> element';
                 var opts = Array.prototype.slice.call(el.options);
                 var miss = wanted.filter(function (w) { return !opts.some(function (o) { return o.value === w || o.label === w || o.text.trim() === w; }); });
                 if (miss.length) return 'no option ' + JSON.stringify(miss);
                 if (!el.multiple && wanted.length > 1) return 'select is not multiple';
                 opts.forEach(function (o) { o.selected = wanted.some(function (w) { return o.value === w || o.label === w || o.text.trim() === w; }); });
                 el.dispatchEvent(new Event('input', { bubbles: true }));
                 el.dispatchEvent(new Event('change', { bubbles: true }));
                 return null;",
                vec![element_arg(&el), json!(values)],
            )
            .await?;
            if let Some(reason) = missing.as_str() {
                return Err(format!("select {}: {}", target(cmd)?, reason));
            }
            Ok(json!({ "selected": values }))
        }
        "check" | "uncheck" => {
            let want = action == "check";
            let el = element(cmd, wb, refs).await?;
            if checked(wb, &el).await? != want {
                wb.click_element(&el).await?;
            }
            if checked(wb, &el).await? != want {
                return Err(format!(
                    "{} {}: the element is still {}",
                    action,
                    target(cmd)?,
                    if want { "unchecked" } else { "checked" }
                ));
            }
            let key = if want { "checked" } else { "unchecked" };
            Ok(json!({ key: target(cmd)? }))
        }
        "scroll" => {
            let (mut dx, mut dy) = (
                cmd.get("x").and_then(Value::as_f64).unwrap_or(0.0),
                cmd.get("y").and_then(Value::as_f64).unwrap_or(0.0),
            );
            if let Some(direction) = cmd.get("direction").and_then(Value::as_str) {
                let amount = cmd.get("amount").and_then(Value::as_f64).unwrap_or(300.0);
                match direction {
                    "up" => dy = -amount,
                    "down" => dy = amount,
                    "left" => dx = -amount,
                    "right" => dx = amount,
                    other => return Err(format!("Unknown scroll direction '{}'", other)),
                }
            }
            let target_el = match cmd.get("selector").and_then(Value::as_str) {
                Some(sel) => element_arg(&resolve_element(wb, refs, sel).await?),
                None => Value::Null,
            };
            page_call(
                wb,
                "var el = arguments[0]; (el || window).scrollBy(arguments[1], arguments[2]); return null;",
                vec![target_el, json!(dx), json!(dy)],
            )
            .await?;
            Ok(json!({ "scrolled": true }))
        }
        "scrollintoview" => {
            let el = element(cmd, wb, refs).await?;
            page_call(
                wb,
                "arguments[0].scrollIntoView({ block: 'center', inline: 'center' }); return null;",
                vec![element_arg(&el)],
            )
            .await?;
            Ok(json!({ "scrolled": target(cmd)? }))
        }
        "wait" => wait(cmd, wb, timeout_ms, url_matches).await,
        "waitforurl" => {
            let pattern = required(cmd, "url")?;
            poll_url(wb, pattern, timeout_ms, url_matches).await?;
            Ok(json!({ "url": wb.get_url_now().await? }))
        }
        "waitforfunction" => {
            let expression = required(cmd, "expression")?;
            let single = expression.trim().trim_end_matches(';');
            let body = format!("return (\n{}\n);", single);
            let deadline =
                tokio::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
            loop {
                let value = page_call(wb, &body, vec![]).await?;
                let truthy = !matches!(value, Value::Null | Value::Bool(false))
                    && value != json!(0)
                    && value != json!("");
                if truthy {
                    return Ok(json!({ "result": value }));
                }
                if tokio::time::Instant::now() >= deadline {
                    return Err(format!(
                        "Wait gave up after {}ms: {} never returned a truthy value",
                        timeout_ms, expression
                    ));
                }
                tokio::time::sleep(POLL).await;
            }
        }
        "waitforloadstate" => {
            let load_state = cmd.get("state").and_then(Value::as_str).unwrap_or("load");
            poll_load_state(wb, load_state, timeout_ms).await?;
            Ok(json!({ "state": load_state }))
        }
        "getattribute" => {
            let el = element(cmd, wb, refs).await?;
            let attribute = required(cmd, "attribute")?;
            let value = page_call(
                wb,
                "return arguments[0].getAttribute(arguments[1]);",
                vec![element_arg(&el), json!(attribute)],
            )
            .await?;
            Ok(json!({ "value": value, "origin": wb.get_url_now().await? }))
        }
        "innerhtml" => {
            let el = element(cmd, wb, refs).await?;
            let html = page_string(wb, "return arguments[0].innerHTML;", &el).await?;
            Ok(json!({ "html": html }))
        }
        "innertext" => {
            let el = element(cmd, wb, refs).await?;
            let text = page_string(
                wb,
                "var e = arguments[0]; return e.innerText || e.textContent || '';",
                &el,
            )
            .await?;
            Ok(json!({ "text": text }))
        }
        "inputvalue" => {
            let el = element(cmd, wb, refs).await?;
            let value = page_string(
                wb,
                "var v = arguments[0].value; return v == null ? '' : String(v);",
                &el,
            )
            .await?;
            Ok(json!({ "value": value }))
        }
        "count" => {
            let selector = required(cmd, "selector")?;
            let count = page_call(
                wb,
                "return document.querySelectorAll(arguments[0]).length;",
                vec![json!(selector)],
            )
            .await?;
            let count = count
                .as_u64()
                .ok_or_else(|| format!("count returned a non-number: {}", count))?;
            Ok(json!({ "count": count, "selector": selector }))
        }
        "isvisible" | "isenabled" | "ischecked" => {
            let el = element(cmd, wb, refs).await?;
            let (key, body) = match action {
                "isvisible" => (
                    "visible",
                    "var e = arguments[0], s = getComputedStyle(e), r = e.getBoundingClientRect();
                     return s.display !== 'none' && s.visibility !== 'hidden' && parseFloat(s.opacity) !== 0 && r.width > 0 && r.height > 0;",
                ),
                "isenabled" => ("enabled", "var e = arguments[0]; return !(e.disabled === true || e.getAttribute('aria-disabled') === 'true');"),
                _ => ("checked", CHECKED_JS),
            };
            let value = page_call(wb, body, vec![element_arg(&el)]).await?;
            let flag = value
                .as_bool()
                .ok_or_else(|| format!("{} returned a non-boolean: {}", action, value))?;
            Ok(json!({ key: flag, "origin": wb.get_url_now().await? }))
        }
        "focus" => {
            let el = element(cmd, wb, refs).await?;
            page_call(
                wb,
                "arguments[0].focus(); return null;",
                vec![element_arg(&el)],
            )
            .await?;
            Ok(json!({ "focused": target(cmd)? }))
        }
        "clear" => {
            let el = element(cmd, wb, refs).await?;
            wb.clear_element(&el).await?;
            Ok(json!({ "cleared": target(cmd)? }))
        }
        "hover" => {
            let el = element(cmd, wb, refs).await?;
            wb.perform_actions(pointer_actions(&el, &[])).await?;
            Ok(json!({ "hovered": target(cmd)? }))
        }
        "dblclick" => {
            let el = element(cmd, wb, refs).await?;
            let presses = [
                json!({ "type": "pointerDown", "button": 0 }),
                json!({ "type": "pointerUp", "button": 0 }),
                json!({ "type": "pointerDown", "button": 0 }),
                json!({ "type": "pointerUp", "button": 0 }),
            ];
            wb.perform_actions(pointer_actions(&el, &presses)).await?;
            Ok(json!({ "clicked": target(cmd)? }))
        }
        other => Err(format!("{} is not a WebDriver page command", other)),
    }
}

const CHECKED_JS: &str = "var e = arguments[0];
 if (e.type === 'checkbox' || e.type === 'radio') return !!e.checked;
 var a = e.getAttribute('aria-checked'); if (a !== null) return a === 'true';
 var inner = e.querySelector && e.querySelector('input[type=checkbox], input[type=radio]');
 return inner ? !!inner.checked : false;";

async fn checked(wb: &WebDriverBackend, el: &str) -> Result<bool, String> {
    let value = page_call(wb, CHECKED_JS, vec![element_arg(el)]).await?;
    value
        .as_bool()
        .ok_or_else(|| format!("checked state is not a boolean: {}", value))
}

async fn wait(
    cmd: &Value,
    wb: &WebDriverBackend,
    timeout_ms: u64,
    url_matches: fn(&str, &str) -> bool,
) -> Result<Value, String> {
    if let Some(text) = cmd.get("text").and_then(Value::as_str) {
        poll_true(
            wb,
            "return (document.body && document.body.innerText || '').includes(arguments[0]);",
            vec![json!(text)],
            timeout_ms,
            &format!("text {:?} did not appear", text),
        )
        .await?;
        return Ok(json!({ "waited": "text", "text": text }));
    }
    if let Some(selector) = cmd.get("selector").and_then(Value::as_str) {
        let state = cmd
            .get("state")
            .and_then(Value::as_str)
            .unwrap_or("visible");
        let body = match state {
            "attached" => "return !!document.querySelector(arguments[0]);",
            "detached" => "return !document.querySelector(arguments[0]);",
            "visible" => "var e = document.querySelector(arguments[0]); if (!e) return false;
                 var s = getComputedStyle(e), r = e.getBoundingClientRect();
                 return s.display !== 'none' && s.visibility !== 'hidden' && parseFloat(s.opacity) !== 0 && r.width > 0 && r.height > 0;",
            "hidden" => "var e = document.querySelector(arguments[0]); if (!e) return true;
                 var s = getComputedStyle(e), r = e.getBoundingClientRect();
                 return s.display === 'none' || s.visibility === 'hidden' || parseFloat(s.opacity) === 0 || r.width === 0 || r.height === 0;",
            other => return Err(format!("Unknown wait state '{}'", other)),
        };
        poll_true(
            wb,
            body,
            vec![json!(selector)],
            timeout_ms,
            &format!("{} did not become {}", selector, state),
        )
        .await?;
        return Ok(json!({ "waited": "selector", "selector": selector }));
    }
    if let Some(pattern) = cmd.get("url").and_then(Value::as_str) {
        poll_url(wb, pattern, timeout_ms, url_matches).await?;
        return Ok(json!({ "waited": "url", "url": pattern }));
    }
    if let Some(function) = cmd.get("function").and_then(Value::as_str) {
        let body = format!("return !!(\n{}\n);", function.trim().trim_end_matches(';'));
        poll_true(
            wb,
            &body,
            vec![],
            timeout_ms,
            "the function did not return true",
        )
        .await?;
        return Ok(json!({ "waited": "function" }));
    }
    if let Some(load_state) = cmd.get("loadState").and_then(Value::as_str) {
        poll_load_state(wb, load_state, timeout_ms).await?;
        return Ok(json!({ "waited": "load", "state": load_state }));
    }
    tokio::time::sleep(std::time::Duration::from_millis(timeout_ms)).await;
    Ok(json!({ "waited": "timeout", "ms": timeout_ms }))
}

async fn poll_load_state(
    wb: &WebDriverBackend,
    state: &str,
    timeout_ms: u64,
) -> Result<(), String> {
    let body = match state {
        "load" => "return document.readyState === 'complete';",
        "domcontentloaded" => "return document.readyState !== 'loading';",
        other => {
            return Err(format!(
            "wait --load {} is not supported on WebDriver sessions (use load or domcontentloaded)",
            other
        ))
        }
    };
    poll_true(
        wb,
        body,
        vec![],
        timeout_ms,
        &format!("the page did not reach {}", state),
    )
    .await
}

async fn poll_url(
    wb: &WebDriverBackend,
    pattern: &str,
    timeout_ms: u64,
    url_matches: fn(&str, &str) -> bool,
) -> Result<(), String> {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
    loop {
        let url = wb.get_url_now().await?;
        if url_matches(pattern, &url) {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(format!(
                "Wait for URL {} gave up after {}ms (at {})",
                pattern, timeout_ms, url
            ));
        }
        tokio::time::sleep(POLL).await;
    }
}

/// Poll a page predicate until it returns true. The message avoids the word
/// "timeout" so generic error rewriting keeps it.
async fn poll_true(
    wb: &WebDriverBackend,
    body: &str,
    args: Vec<Value>,
    timeout_ms: u64,
    failure: &str,
) -> Result<(), String> {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
    loop {
        let value = page_call(wb, body, args.clone()).await?;
        if value.as_bool() == Some(true) {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(format!("Wait gave up after {}ms: {}", timeout_ms, failure));
        }
        tokio::time::sleep(POLL).await;
    }
}

async fn page_call(wb: &WebDriverBackend, body: &str, args: Vec<Value>) -> Result<Value, String> {
    wb.execute_function_body(body, args).await
}

async fn page_string(wb: &WebDriverBackend, body: &str, el: &str) -> Result<String, String> {
    let value = page_call(wb, body, vec![element_arg(el)]).await?;
    value
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| format!("expected text, got {}", value))
}

fn required<'a>(cmd: &'a Value, key: &str) -> Result<&'a str, String> {
    cmd.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("Missing '{}' parameter", key))
}

fn target(cmd: &Value) -> Result<&str, String> {
    required(cmd, "selector")
}

async fn element(cmd: &Value, wb: &WebDriverBackend, refs: &RefMap) -> Result<String, String> {
    resolve_element(wb, refs, target(cmd)?).await
}

fn select_values(cmd: &Value) -> Vec<String> {
    match cmd.get("values") {
        Some(Value::Array(arr)) => arr
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect(),
        Some(Value::String(s)) => vec![s.clone()],
        _ => cmd
            .get("value")
            .and_then(Value::as_str)
            .map(|s| vec![s.to_string()])
            .unwrap_or_default(),
    }
}

fn pointer_actions(element_id: &str, presses: &[Value]) -> Value {
    let mut steps = vec![json!({
        "type": "pointerMove",
        "duration": 0,
        "origin": element_arg(element_id),
        "x": 0,
        "y": 0,
    })];
    steps.extend(presses.iter().cloned());
    json!({ "actions": [{
        "type": "pointer",
        "id": "mouse",
        "parameters": { "pointerType": "mouse" },
        "actions": steps,
    }]})
}

/// WebDriver key codepoints for the named keys agent-browser accepts.
fn webdriver_key(name: &str) -> Result<String, String> {
    let code = match name {
        "Enter" | "Return" => '\u{E007}',
        "Tab" => '\u{E004}',
        "Escape" | "Esc" => '\u{E00C}',
        "Backspace" => '\u{E003}',
        "Delete" => '\u{E017}',
        "Space" | " " => ' ',
        "ArrowUp" | "Up" => '\u{E013}',
        "ArrowDown" | "Down" => '\u{E015}',
        "ArrowLeft" | "Left" => '\u{E012}',
        "ArrowRight" | "Right" => '\u{E014}',
        "Home" => '\u{E011}',
        "End" => '\u{E010}',
        "PageUp" => '\u{E00E}',
        "PageDown" => '\u{E00F}',
        "Insert" => '\u{E016}',
        "Shift" => '\u{E008}',
        "Control" | "Ctrl" => '\u{E009}',
        "Alt" => '\u{E00A}',
        "Meta" | "Command" | "Cmd" => '\u{E03D}',
        "F1" => '\u{E031}',
        "F2" => '\u{E032}',
        "F3" => '\u{E033}',
        "F4" => '\u{E034}',
        "F5" => '\u{E035}',
        "F6" => '\u{E036}',
        "F7" => '\u{E037}',
        "F8" => '\u{E038}',
        "F9" => '\u{E039}',
        "F10" => '\u{E03A}',
        "F11" => '\u{E03B}',
        "F12" => '\u{E03C}',
        single if single.chars().count() == 1 => return Ok(single.to_string()),
        other => return Err(format!("Unknown key '{}' for press on WebDriver", other)),
    };
    Ok(code.to_string())
}

/// "Control+Shift+a" -> keyDown modifiers, keyDown/keyUp key, keyUp modifiers.
fn key_chord_actions(chord: &str) -> Result<Value, String> {
    let parts: Vec<&str> = if chord == "+" {
        vec!["+"]
    } else {
        chord.split('+').collect()
    };
    let (key, modifiers) = parts.split_last().ok_or("Missing key")?;
    let modifiers = modifiers
        .iter()
        .map(|m| webdriver_key(m))
        .collect::<Result<Vec<_>, _>>()?;
    let key = webdriver_key(key)?;
    let mut steps: Vec<Value> = modifiers
        .iter()
        .map(|m| json!({ "type": "keyDown", "value": m }))
        .collect();
    steps.push(json!({ "type": "keyDown", "value": key }));
    steps.push(json!({ "type": "keyUp", "value": key }));
    steps.extend(
        modifiers
            .iter()
            .rev()
            .map(|m| json!({ "type": "keyUp", "value": m })),
    );
    Ok(json!({ "actions": [{ "type": "key", "id": "keyboard", "actions": steps }] }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_chords_map_to_webdriver_key_actions() {
        let actions = key_chord_actions("Control+Shift+a").unwrap();
        let steps = actions["actions"][0]["actions"].as_array().unwrap();
        let values: Vec<(&str, &str)> = steps
            .iter()
            .map(|s| (s["type"].as_str().unwrap(), s["value"].as_str().unwrap()))
            .collect();
        assert_eq!(
            values,
            [
                ("keyDown", "\u{E009}"),
                ("keyDown", "\u{E008}"),
                ("keyDown", "a"),
                ("keyUp", "a"),
                ("keyUp", "\u{E008}"),
                ("keyUp", "\u{E009}"),
            ]
        );
        assert_eq!(webdriver_key("Enter").unwrap(), "\u{E007}");
        assert!(key_chord_actions("Hyper+x")
            .unwrap_err()
            .contains("Unknown key 'Hyper'"));
    }

    #[test]
    fn select_values_accepts_array_string_or_value() {
        assert_eq!(select_values(&json!({ "values": ["a", "b"] })), ["a", "b"]);
        assert_eq!(select_values(&json!({ "values": "a" })), ["a"]);
        assert_eq!(select_values(&json!({ "value": "c" })), ["c"]);
    }
}
