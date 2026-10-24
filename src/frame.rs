use crate::cdp::CdpClient;
use serde_json::Value;
use std::thread;
use std::time::Duration;

const FRAME_SEPARATOR: &str = ">>>";

#[derive(Clone, Debug)]
pub(crate) struct FrameScope {
    session_id: Option<String>,
    context_id: Option<i64>,
    label: String,
}

impl FrameScope {
    fn top() -> Self {
        Self {
            session_id: None,
            context_id: None,
            label: "top frame".to_string(),
        }
    }

    fn child(&self, selector: &str, session_id: Option<String>, context_id: Option<i64>) -> Self {
        let path = match self.label.strip_prefix("frame ") {
            Some(path) => format!("{path} {FRAME_SEPARATOR} {selector}"),
            None => selector.to_string(),
        };
        Self {
            session_id,
            context_id,
            label: format!("frame {path}"),
        }
    }
}

struct Hop {
    scope: FrameScope,
    object_id: String,
}

pub(crate) struct Located {
    scope: FrameScope,
    object_id: String,
    hops: Vec<Hop>,
}

impl Located {
    pub(crate) fn release(&self, client: &mut CdpClient) {
        for hop in &self.hops {
            release_object(client, &hop.scope, &hop.object_id);
        }
        release_object(client, &self.scope, &self.object_id);
    }
}

fn release_object(client: &mut CdpClient, scope: &FrameScope, object_id: &str) {
    let _ = client.call_in(
        scope.session_id.as_deref(),
        serde_json::json!({
            "method": "Runtime.releaseObject",
            "params": { "objectId": object_id }
        }),
    );
}

fn exception_text(result: &Value) -> Option<String> {
    let details = result.get("exceptionDetails")?;
    let text = details
        .get("exception")
        .and_then(|exception| exception.get("description"))
        .and_then(|description| description.as_str())
        .or_else(|| details.get("text").and_then(|text| text.as_str()))
        .unwrap_or("unknown evaluation error");
    Some(format!("expression threw: {text}"))
}

fn evaluate_in(
    client: &mut CdpClient,
    scope: &FrameScope,
    expression: &str,
    by_value: bool,
) -> Result<Value, String> {
    let mut params = serde_json::json!({
        "expression": expression,
        "returnByValue": by_value
    });
    if let Some(context_id) = scope.context_id {
        params["contextId"] = serde_json::json!(context_id);
    }
    let response = client.call_in(
        scope.session_id.as_deref(),
        serde_json::json!({ "method": "Runtime.evaluate", "params": params }),
    )?;
    let result = response
        .get("result")
        .ok_or_else(|| "Runtime.evaluate returned no result".to_string())?;
    if let Some(error) = exception_text(result) {
        return Err(error);
    }
    Ok(result.get("result").cloned().unwrap_or(Value::Null))
}

pub(crate) fn call_on(
    client: &mut CdpClient,
    scope: &FrameScope,
    object_id: &str,
    function: &str,
) -> Result<Value, String> {
    let response = client.call_in(
        scope.session_id.as_deref(),
        serde_json::json!({
            "method": "Runtime.callFunctionOn",
            "params": {
                "objectId": object_id,
                "functionDeclaration": function,
                "returnByValue": true
            }
        }),
    )?;
    let result = response
        .get("result")
        .ok_or_else(|| "Runtime.callFunctionOn returned no result".to_string())?;
    if let Some(error) = exception_text(result) {
        return Err(error);
    }
    Ok(result
        .get("result")
        .and_then(|inner| inner.get("value"))
        .cloned()
        .unwrap_or(Value::Null))
}

fn query_object_id(
    client: &mut CdpClient,
    scope: &FrameScope,
    selector: &str,
) -> Result<Option<String>, String> {
    let selector_json = serde_json::json!(selector).to_string();
    let result = evaluate_in(
        client,
        scope,
        &format!("document.querySelector({selector_json})"),
        false,
    )?;
    Ok(result
        .get("objectId")
        .and_then(|object_id| object_id.as_str())
        .map(str::to_string))
}

fn frame_summary(client: &mut CdpClient, scope: &FrameScope) -> String {
    let value = evaluate_in(
        client,
        scope,
        "(() => ({ url: location.href, iframes: Array.from(document.querySelectorAll('iframe,frame')).slice(0, 8).map((f) => f.tagName.toLowerCase() + (f.id ? '#' + f.id : f.getAttribute('name') ? '[name=' + f.getAttribute('name') + ']' : '[src=' + (f.getAttribute('src') || '') + ']')) }))()",
        true,
    )
    .ok()
    .and_then(|result| result.get("value").cloned());
    let Some(value) = value else {
        return scope.label.clone();
    };
    let url = value
        .get("url")
        .and_then(|url| url.as_str())
        .unwrap_or("unknown url");
    let iframes = value
        .get("iframes")
        .and_then(|iframes| iframes.as_array())
        .map(|iframes| {
            iframes
                .iter()
                .filter_map(|iframe| iframe.as_str())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if iframes.is_empty() {
        format!("{} (url {url}, no iframes)", scope.label)
    } else {
        format!(
            "{} (url {url}, iframes here: {})",
            scope.label,
            iframes.join(", ")
        )
    }
}

fn missing_element(client: &mut CdpClient, scope: &FrameScope, selector: &str) -> String {
    format!(
        "no element matches selector: {selector} in {}",
        frame_summary(client, scope)
    )
}

pub(crate) fn describe_element(
    client: &mut CdpClient,
    scope: &FrameScope,
    object_id: &str,
) -> String {
    let state = call_on(
        client,
        scope,
        object_id,
        "function() { const style = getComputedStyle(this); const rect = this.getBoundingClientRect(); const hidden = style.display === 'none' || style.visibility === 'hidden' || (rect.width === 0 && rect.height === 0); return { tag: this.tagName.toLowerCase(), hidden, disabled: !!this.disabled, readOnly: !!this.readOnly }; }",
    )
    .unwrap_or(Value::Null);
    let tag = state
        .get("tag")
        .and_then(|tag| tag.as_str())
        .unwrap_or("element");
    let flag = |name: &str| state.get(name).and_then(|value| value.as_bool()) == Some(true);
    let reason = if flag("hidden") {
        ", hidden (display:none, visibility:hidden or zero size)"
    } else if flag("disabled") {
        ", disabled"
    } else if flag("readOnly") {
        ", read-only"
    } else {
        ""
    };
    format!("matched <{tag}>{reason}")
}

pub(crate) fn failure_context(
    client: &mut CdpClient,
    located: &Located,
    selector: &str,
    reason: &str,
) -> String {
    let element = describe_element(client, &located.scope, &located.object_id);
    format!(
        "{reason}: {selector} ({element}; in {})",
        frame_summary(client, &located.scope)
    )
}

fn enter_frame(
    client: &mut CdpClient,
    parent: &FrameScope,
    frame_id: &str,
    selector: &str,
) -> Result<FrameScope, String> {
    client.call_in(
        None,
        serde_json::json!({ "method": "Target.getTargets", "params": {} }),
    )?;
    let attached = client.call_in(
        None,
        serde_json::json!({
            "method": "Target.attachToTarget",
            "params": { "targetId": frame_id, "flatten": true }
        }),
    );
    match attached {
        Ok(response) => {
            let session_id = response
                .get("result")
                .and_then(|result| result.get("sessionId"))
                .and_then(|session_id| session_id.as_str())
                .ok_or_else(|| "Target.attachToTarget returned no sessionId".to_string())?;
            Ok(parent.child(selector, Some(session_id.to_string()), None))
        }
        Err(error) if error.contains("No target with given id found") => {
            let world = client.call_in(
                parent.session_id.as_deref(),
                serde_json::json!({
                    "method": "Page.createIsolatedWorld",
                    "params": {
                        "frameId": frame_id,
                        "worldName": "chrome-devtools",
                        "grantUniveralAccess": true
                    }
                }),
            )?;
            let context_id = world
                .get("result")
                .and_then(|result| result.get("executionContextId"))
                .and_then(|context_id| context_id.as_i64())
                .ok_or_else(|| "Page.createIsolatedWorld returned no context id".to_string())?;
            Ok(parent.child(selector, parent.session_id.clone(), Some(context_id)))
        }
        Err(error) => Err(error),
    }
}

pub(crate) fn locate(client: &mut CdpClient, selector: &str) -> Result<Located, String> {
    let parts = selector
        .split(FRAME_SEPARATOR)
        .map(str::trim)
        .collect::<Vec<_>>();
    if parts.iter().any(|part| part.is_empty()) {
        return Err(format!("empty selector segment in: {selector}"));
    }
    let (target_selector, frame_selectors) = parts
        .split_last()
        .ok_or_else(|| "selector is empty".to_string())?;
    let mut scope = FrameScope::top();
    let mut hops = Vec::new();
    for frame_selector in frame_selectors {
        let object_id = match query_object_id(client, &scope, frame_selector)? {
            Some(object_id) => object_id,
            None => {
                let message = format!(
                    "no iframe matches selector: {frame_selector} in {}",
                    frame_summary(client, &scope)
                );
                release_hops(client, &hops);
                return Err(message);
            }
        };
        let described = client.call_in(
            scope.session_id.as_deref(),
            serde_json::json!({
                "method": "DOM.describeNode",
                "params": { "objectId": object_id }
            }),
        );
        let node = described
            .as_ref()
            .ok()
            .and_then(|response| response.get("result").and_then(|result| result.get("node")));
        let frame_id = node
            .and_then(|node| node.get("frameId"))
            .and_then(|frame_id| frame_id.as_str());
        let Some(frame_id) = frame_id else {
            let tag = node
                .and_then(|node| node.get("nodeName"))
                .and_then(|name| name.as_str())
                .unwrap_or("element");
            release_object(client, &scope, &object_id);
            release_hops(client, &hops);
            return Err(format!(
                "selector {frame_selector} matches <{}>, not an iframe, in {}",
                tag.to_lowercase(),
                scope.label
            ));
        };
        let child = match enter_frame(client, &scope, frame_id, frame_selector) {
            Ok(child) => child,
            Err(error) => {
                release_object(client, &scope, &object_id);
                release_hops(client, &hops);
                return Err(format!(
                    "could not enter iframe {frame_selector} in {}: {error}",
                    scope.label
                ));
            }
        };
        hops.push(Hop {
            scope: scope.clone(),
            object_id,
        });
        scope = child;
    }
    match query_object_id(client, &scope, target_selector)? {
        Some(object_id) => Ok(Located {
            scope,
            object_id,
            hops,
        }),
        None => {
            let message = missing_element(client, &scope, target_selector);
            release_hops(client, &hops);
            Err(message)
        }
    }
}

fn release_hops(client: &mut CdpClient, hops: &[Hop]) {
    for hop in hops {
        release_object(client, &hop.scope, &hop.object_id);
    }
}

pub(crate) fn click_point(client: &mut CdpClient, located: &Located) -> Result<(f64, f64), String> {
    for hop in &located.hops {
        call_on(
            client,
            &hop.scope,
            &hop.object_id,
            "function() { this.scrollIntoView({ block: 'center', inline: 'center' }); }",
        )?;
    }
    call_on(
        client,
        &located.scope,
        &located.object_id,
        "function() { this.scrollIntoView({ block: 'center', inline: 'center' }); }",
    )?;
    let mut point = measure_point(client, located)?;
    if located.hops.is_empty() {
        return Ok(point);
    }
    for _ in 0..40 {
        thread::sleep(Duration::from_millis(50));
        let next = measure_point(client, located)?;
        let settled = (next.0 - point.0).abs() < 0.5 && (next.1 - point.1).abs() < 0.5;
        point = next;
        if settled {
            return Ok(point);
        }
    }
    Err("the element kept moving while the frames scrolled".to_string())
}

fn measure_point(client: &mut CdpClient, located: &Located) -> Result<(f64, f64), String> {
    let center = call_on(
        client,
        &located.scope,
        &located.object_id,
        "function() { const r = this.getBoundingClientRect(); return { x: r.x + r.width / 2, y: r.y + r.height / 2 }; }",
    )?;
    let mut x = center
        .get("x")
        .and_then(|x| x.as_f64())
        .ok_or_else(|| "selector center has no x".to_string())?;
    let mut y = center
        .get("y")
        .and_then(|y| y.as_f64())
        .ok_or_else(|| "selector center has no y".to_string())?;
    for hop in located.hops.iter().rev() {
        let origin = call_on(
            client,
            &hop.scope,
            &hop.object_id,
            "function() { const r = this.getBoundingClientRect(); const s = getComputedStyle(this); return { x: r.x + this.clientLeft + parseFloat(s.paddingLeft || '0'), y: r.y + this.clientTop + parseFloat(s.paddingTop || '0') }; }",
        )?;
        x += origin.get("x").and_then(|x| x.as_f64()).unwrap_or(0.0);
        y += origin.get("y").and_then(|y| y.as_f64()).unwrap_or(0.0);
    }
    Ok((x, y))
}

pub(crate) fn focus(client: &mut CdpClient, located: &Located) -> Result<bool, String> {
    let focused = call_on(
        client,
        &located.scope,
        &located.object_id,
        "function() { this.scrollIntoView({ block: 'center', inline: 'center' }); this.focus(); return this.ownerDocument.activeElement === this; }",
    )?;
    Ok(focused.as_bool() == Some(true))
}

pub(crate) fn is_file_input(client: &mut CdpClient, located: &Located) -> Result<bool, String> {
    let kind = call_on(
        client,
        &located.scope,
        &located.object_id,
        "function() { return this.tagName === 'INPUT' && this.type === 'file'; }",
    )?;
    Ok(kind.as_bool() == Some(true))
}

pub(crate) fn set_files(
    client: &mut CdpClient,
    located: &Located,
    files: &[String],
) -> Result<(), String> {
    client.call_in(
        located.scope.session_id.as_deref(),
        serde_json::json!({
            "method": "DOM.setFileInputFiles",
            "params": {
                "objectId": located.object_id,
                "files": files
            }
        }),
    )?;
    Ok(())
}

pub(crate) fn not_file_input(client: &mut CdpClient, located: &Located, selector: &str) -> String {
    let element = describe_element(client, &located.scope, &located.object_id);
    format!(
        "selector is not an input[type=file]: {selector} ({element}; in {})",
        frame_summary(client, &located.scope)
    )
}
