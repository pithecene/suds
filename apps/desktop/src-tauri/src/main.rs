//! The Suds desktop application.
//!
//! The webview presents the inspection and the move plans. The commands in
//! this file are the webview's only access to the workstation. Only
//! `apply_plan` and `undo_run` modify files, and each acts only on a plan
//! that Suds built and recorded.

mod inspection;
mod locations;
mod organize;

use std::path::PathBuf;
use std::sync::Mutex;

use suds_core::model::RootKind;
use suds_core::organize::journal;
use tauri::ipc::Channel;
use tauri::{AppHandle, Builder, Runtime, State};
use tauri_plugin_dialog::DialogExt;

use crate::inspection::{InspectionEvent, InspectionId, Inspections};
use crate::locations::{LocationId, LocationList, LocationView, Locations};
use crate::organize::{MoveView, Operations, PlanResponse, RunSummaryView, RunView};

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

/// Builds a plan to move the selected children of an inbox location into a
/// folder that the person chooses in the native picker. The webview names
/// each child by its position in an inspection's result, never by a path.
#[tauri::command]
async fn create_move_plan<R: Runtime>(
    app: AppHandle<R>,
    locations: State<'_, Mutex<Locations>>,
    inspections: State<'_, Inspections>,
    operations: State<'_, Operations>,
    inspection: InspectionId,
    location: LocationId,
    entries: Vec<u32>,
) -> Result<PlanResponse, String> {
    let (_, spec) = lock(&locations)
        .specs(&[location])
        .map_err(|id| format!("no location has the identifier {id}"))?
        .remove(0);
    // This release organizes the Downloads folder alone.
    if spec.kind != RootKind::Inbox {
        return Err("Suds moves entries only out of the Downloads location".to_owned());
    }
    // The selection resolves before the picker opens, so a stale selection
    // fails without asking for a folder.
    let requests = inspections.requests(inspection, location, &entries)?;
    let Some(chosen) = app
        .dialog()
        .file()
        .set_title("Move To")
        .blocking_pick_folder()
    else {
        return Ok(PlanResponse::Cancelled);
    };
    let destination = chosen
        .into_path()
        .map_err(|err| format!("the chosen folder has no local path: {err}"))?;
    operations.create_plan(&spec.path, &requests, &destination)
}

/// Applies a plan that this window built, and sends each move's outcome as
/// the move ends.
#[tauri::command]
async fn apply_plan(
    operations: State<'_, Operations>,
    plan: String,
    events: Channel<MoveView>,
) -> Result<RunView, String> {
    operations.apply(&plan, &mut |outcome| {
        // A closed window drops the channel, and the journal still records
        // each outcome.
        let _ = events.send(outcome);
    })
}

#[tauri::command]
fn list_journal(operations: State<'_, Operations>) -> Result<Vec<RunSummaryView>, String> {
    operations.list()
}

#[tauri::command]
async fn undo_run(operations: State<'_, Operations>, run: String) -> Result<RunView, String> {
    operations.undo(&run)
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Registers the application's state, plugin, and commands. The tests run
/// the same registration on Tauri's mock runtime with a temporary journal.
fn setup<R: Runtime>(builder: Builder<R>, journal: Result<PathBuf, String>) -> Builder<R> {
    builder
        .plugin(tauri_plugin_dialog::init())
        .manage(Mutex::new(Locations::from_profile()))
        .manage(Inspections::default())
        .manage(Operations::new(journal))
        .invoke_handler(tauri::generate_handler![
            list_locations,
            choose_location,
            start_inspection,
            cancel_inspection,
            create_move_plan,
            apply_plan,
            list_journal,
            undo_run
        ])
}

/// The application's configuration, assets, and capabilities. Each
/// expansion of `generate_context!` embeds the macOS property list, so the
/// crate expands the macro once.
fn context<R: Runtime>() -> tauri::Context<R> {
    tauri::generate_context!()
}

fn main() {
    let journal = journal::default_directory().map_err(|err| err.to_string());
    setup(Builder::default(), journal)
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
    /// capability file, with a journal in `journal`.
    fn window_with_journal(journal: &std::path::Path) -> WebviewWindow<MockRuntime> {
        let app = setup(mock_builder(), Ok(journal.to_path_buf()))
            .build(context())
            .unwrap();
        WebviewWindowBuilder::new(&app, "main", Default::default())
            .build()
            .unwrap()
    }

    /// A window whose journal directory does not exist and that no test
    /// creates.
    fn window() -> WebviewWindow<MockRuntime> {
        window_with_journal(std::path::Path::new("/nonexistent/suds-test-journal"))
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
    fn the_capability_grants_the_operation_commands_by_identifier_alone() {
        let journal = tempfile::TempDir::new().unwrap();
        let window = window_with_journal(journal.path());
        assert_eq!(invoke(&window, "list_journal", json!({})), Ok(json!([])));
        assert_eq!(
            invoke(
                &window,
                "apply_plan",
                json!({ "plan": "plan-forged", "events": "__CHANNEL__:1" })
            ),
            Err(Value::from(
                "this window built no plan plan-forged that awaits application"
            ))
        );
        let undo = invoke(&window, "undo_run", json!({ "run": "run-unknown" }));
        assert_eq!(
            undo,
            Err(Value::from("the journal records no run run-unknown"))
        );
        let stale = invoke(
            &window,
            "create_move_plan",
            json!({ "inspection": 1, "location": 9999, "entries": [0] }),
        );
        assert_eq!(
            stale,
            Err(Value::from("no location has the identifier 9999"))
        );
    }

    /// A command that takes a plan or a selection rejects a path, and no
    /// generic filesystem, shell, or dialog command reaches the webview.
    #[test]
    fn the_capability_denies_path_arguments_and_generic_commands() {
        let journal = tempfile::TempDir::new().unwrap();
        let window = window_with_journal(journal.path());
        let by_path = invoke(
            &window,
            "apply_plan",
            json!({
                "source": "/Users/someone/Downloads/file.pdf",
                "destination": "/Users/someone/Documents/file.pdf",
                "events": "__CHANNEL__:1"
            }),
        );
        assert!(by_path.is_err());
        let by_path = invoke(
            &window,
            "create_move_plan",
            json!({ "source": "/etc", "destination": "/tmp" }),
        );
        assert!(by_path.is_err());
        for cmd in [
            "move_path",
            "rename",
            "plugin:fs|rename",
            "plugin:fs|remove",
            "plugin:fs|write_file",
            "plugin:shell|execute",
        ] {
            assert!(invoke(&window, cmd, json!({})).is_err(), "{cmd}");
        }
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
