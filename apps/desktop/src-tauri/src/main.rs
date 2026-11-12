//! The Suds desktop application.
//!
//! The webview presents the inspection. The commands in this file are the
//! webview's only access to the workstation, and none of them modifies a
//! file.

mod inspection;
mod locations;

use std::sync::Mutex;

use tauri::ipc::Channel;
use tauri::{AppHandle, Builder, Runtime, State};
use tauri_plugin_dialog::DialogExt;

use crate::inspection::{InspectionEvent, InspectionId, Inspections};
use crate::locations::{LocationId, LocationList, LocationView, Locations};

#[tauri::command]
fn list_locations(locations: State<'_, Mutex<Locations>>) -> LocationList {
    lock(&locations).list()
}

/// Opens the native folder picker and adds the chosen folder. An async
/// command runs off the main thread, so the picker may block this command
/// without freezing the window.
#[tauri::command]
async fn choose_location<R: Runtime>(
    app: AppHandle<R>,
    locations: State<'_, Mutex<Locations>>,
) -> Result<Option<LocationView>, String> {
    let Some(chosen) = app.dialog().file().blocking_pick_folder() else {
        return Ok(None);
    };
    let path = chosen
        .into_path()
        .map_err(|err| format!("the chosen folder has no local path: {err}"))?;
    Ok(Some(lock(&locations).add(path)))
}

#[tauri::command]
fn start_inspection(
    locations: State<'_, Mutex<Locations>>,
    inspections: State<'_, Inspections>,
    ids: Vec<LocationId>,
    events: Channel<InspectionEvent>,
) -> Result<InspectionId, String> {
    let roots = lock(&locations)
        .specs(&ids)
        .map_err(|id| format!("no location has the identifier {id}"))?;
    Ok(inspections.start(roots, move |event| {
        // A closed window drops the channel, and the results of the rest of
        // the scan then reach no receiver.
        let _ = events.send(event);
    }))
}

#[tauri::command]
fn cancel_inspection(inspections: State<'_, Inspections>, inspection: InspectionId) {
    inspections.cancel(inspection);
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Registers the application's state, plugin, and commands. The tests run
/// the same registration on Tauri's mock runtime.
fn setup<R: Runtime>(builder: Builder<R>) -> Builder<R> {
    builder
        .plugin(tauri_plugin_dialog::init())
        .manage(Mutex::new(Locations::from_profile()))
        .manage(Inspections::default())
        .invoke_handler(tauri::generate_handler![
            list_locations,
            choose_location,
            start_inspection,
            cancel_inspection
        ])
}

/// The application's configuration, assets, and capabilities. Each
/// expansion of `generate_context!` embeds the macOS property list, so the
/// crate expands the macro once.
fn context<R: Runtime>() -> tauri::Context<R> {
    tauri::generate_context!()
}

fn main() {
    setup(Builder::default())
        .run(context())
        .expect("the Suds window failed to start");
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};
    use tauri::ipc::{CallbackFn, InvokeBody};
    use tauri::test::{INVOKE_KEY, MockRuntime, get_ipc_response, mock_builder};
    use tauri::webview::InvokeRequest;
    use tauri::{WebviewWindow, WebviewWindowBuilder};

    use super::{context, setup};

    /// The main window under the application's own configuration and
    /// capability file.
    fn window() -> WebviewWindow<MockRuntime> {
        let app = setup(mock_builder()).build(context()).unwrap();
        WebviewWindowBuilder::new(&app, "main", Default::default())
            .build()
            .unwrap()
    }

    fn invoke(window: &WebviewWindow<MockRuntime>, cmd: &str, body: Value) -> Result<Value, Value> {
        let request = InvokeRequest {
            cmd: cmd.into(),
            callback: CallbackFn(0),
            error: CallbackFn(1),
            url: "tauri://localhost".parse().unwrap(),
            body: InvokeBody::Json(body),
            headers: Default::default(),
            invoke_key: INVOKE_KEY.to_owned(),
        };
        get_ipc_response(window, request).map(|body| body.deserialize().unwrap())
    }

    #[test]
    fn the_capability_grants_the_application_commands() {
        let window = window();
        let list = invoke(&window, "list_locations", json!({})).unwrap();
        assert!(list["locations"].is_array());
        assert_eq!(
            invoke(&window, "cancel_inspection", json!({ "inspection": 1 })),
            Ok(Value::Null)
        );
        let unknown = invoke(
            &window,
            "start_inspection",
            json!({ "ids": [9999], "events": "__CHANNEL__:1" }),
        );
        assert_eq!(
            unknown,
            Err(Value::from("no location has the identifier 9999"))
        );
    }

    #[test]
    fn the_capability_denies_every_plugin_command() {
        let window = window();
        for cmd in [
            "plugin:dialog|open",
            "plugin:dialog|save",
            "plugin:event|emit",
        ] {
            let denied = invoke(&window, cmd, json!({}));
            let message = denied.expect_err(cmd).to_string();
            assert!(message.contains("not allowed"), "{cmd}: {message}");
        }
    }
}
