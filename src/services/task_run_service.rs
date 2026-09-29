//! What a Task's agent is doing: the live `agent-event` stream, the run
//! snapshot behind it, and stopping a run. Re-queuing a task is
//! [`crate::services::queue_service::enqueue_task`].

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::{spawn_local, JsFuture};

use crate::models::{AgentEvent, TaskRunSnapshot};

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = ["window", "__TAURI__", "core"], js_name = invoke)]
    fn raw_invoke(cmd: &str, args: JsValue) -> js_sys::Promise;

    #[wasm_bindgen(js_namespace = ["window", "__TAURI__", "event"], js_name = "listen")]
    fn tauri_event_listen(event: &str, handler: &Closure<dyn Fn(JsValue)>) -> js_sys::Promise;
}

thread_local! {
    /// How many `agent-event` listeners are registered with Tauri right now.
    static ACTIVE_LISTENERS: Cell<i32> = const { Cell::new(0) };
}

/// Publish the live listener count on the document element as
/// `data-agent-event-listeners`, so a desktop acceptance journey can prove
/// that opening and closing the drawer never leaves listeners behind.
fn adjust_active_listeners(delta: i32) {
    let now = ACTIVE_LISTENERS.with(|count| {
        count.set(count.get() + delta);
        count.get()
    });
    if let Some(root) = web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.document_element())
    {
        let _ = root.set_attribute("data-agent-event-listeners", &now.to_string());
    }
}

struct ListenerState {
    /// Resolved once Tauri has registered the handler.
    unlisten: Option<js_sys::Function>,
    /// The listener was dropped; anything still pending must unregister.
    dropped: bool,
    /// Kept alive until Tauri has unregistered it: JavaScript holds a
    /// reference to it until then, and calling a dropped closure throws.
    handler: Option<Closure<dyn Fn(JsValue)>>,
}

/// A registered `agent-event` listener. Dropping it unregisters it.
///
/// Registration and unregistration are both asynchronous in Tauri, so this
/// covers the window between them in both directions: dropped before Tauri
/// finished registering, it unregisters as soon as registration completes,
/// and the handler never runs after the drop either way.
pub struct AgentEventListener {
    state: Rc<RefCell<ListenerState>>,
}

/// Listen to every `agent-event`. The handler is called with each event that
/// decodes; callers filter by [`AgentEvent::task_id`].
pub fn listen_agent_events(handler: impl Fn(AgentEvent) + 'static) -> AgentEventListener {
    let state = Rc::new(RefCell::new(ListenerState {
        unlisten: None,
        dropped: false,
        handler: None,
    }));

    let guard = Rc::downgrade(&state);
    let closure = Closure::wrap(Box::new(move |event: JsValue| {
        // A drop is final even if Tauri delivers an event already in flight.
        let live = guard.upgrade().is_some_and(|state| !state.borrow().dropped);
        if !live {
            return;
        }
        let payload = js_sys::Reflect::get(&event, &JsValue::from_str("payload"))
            .unwrap_or(JsValue::NULL);
        match serde_wasm_bindgen::from_value::<AgentEvent>(payload) {
            Ok(event) => handler(event),
            Err(e) => leptos::logging::warn!("[agent-event] undecodable payload: {e:?}"),
        }
    }) as Box<dyn Fn(JsValue)>);

    let registration = tauri_event_listen("agent-event", &closure);
    state.borrow_mut().handler = Some(closure);

    let pending = state.clone();
    spawn_local(async move {
        let unlisten = match JsFuture::from(registration).await {
            Ok(value) => value.dyn_into::<js_sys::Function>().ok(),
            Err(e) => {
                leptos::logging::warn!("[agent-event] listen failed: {e:?}");
                None
            }
        };
        let Some(unlisten) = unlisten else {
            pending.borrow_mut().handler = None;
            return;
        };
        adjust_active_listeners(1);
        let dropped = pending.borrow().dropped;
        if dropped {
            unregister(unlisten, pending);
        } else {
            pending.borrow_mut().unlisten = Some(unlisten);
        }
    });

    AgentEventListener { state }
}

/// Ask Tauri to unregister, and only then let the handler go.
fn unregister(unlisten: js_sys::Function, state: Rc<RefCell<ListenerState>>) {
    let result = unlisten.call0(&JsValue::NULL);
    spawn_local(async move {
        if let Ok(promise) = result.and_then(|value| value.dyn_into::<js_sys::Promise>()) {
            let _ = JsFuture::from(promise).await;
        }
        adjust_active_listeners(-1);
        state.borrow_mut().handler = None;
    });
}

impl Drop for AgentEventListener {
    fn drop(&mut self) {
        let unlisten = {
            let mut state = self.state.borrow_mut();
            state.dropped = true;
            state.unlisten.take()
        };
        // Still registering: the registration task sees `dropped` and
        // unregisters itself when it completes.
        if let Some(unlisten) = unlisten {
            unregister(unlisten, self.state.clone());
        }
    }
}

async fn invoke(cmd: &str, args: serde_json::Value) -> Result<JsValue, String> {
    let args = serde_wasm_bindgen::to_value(&args).map_err(|e| e.to_string())?;
    JsFuture::from(raw_invoke(cmd, args)).await.map_err(|value| {
        value.as_string().unwrap_or_else(|| {
            js_sys::JSON::stringify(&value)
                .ok()
                .and_then(|s| s.as_string())
                .unwrap_or_else(|| format!("{cmd} failed"))
        })
    })
}

/// Whether an agent is working on the task now, and its latest execution's
/// output in this session.
pub async fn get_task_run(task_id: String) -> Result<TaskRunSnapshot, String> {
    let value = invoke("get_task_run", serde_json::json!({ "taskId": task_id })).await?;
    serde_wasm_bindgen::from_value(value).map_err(|e| e.to_string())
}

/// End the task's live agent through the backend's own stop path. What the
/// task becomes afterwards is whatever the backend settled it to; read it
/// back rather than assuming.
pub async fn stop_task_execution(task_id: String) -> Result<(), String> {
    invoke("stop_task_execution", serde_json::json!({ "taskId": task_id })).await?;
    Ok(())
}
