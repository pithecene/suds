//! Generates the permission for each application command, so that the
//! capability file grants the webview these commands and nothing else.

fn main() {
    let manifest = tauri_build::AppManifest::new().commands(&[
        "list_locations",
        "choose_location",
        "start_inspection",
        "cancel_inspection",
        "create_move_plan",
        "apply_plan",
        "list_journal",
        "undo_run",
    ]);
    tauri_build::try_build(tauri_build::Attributes::new().app_manifest(manifest))
        .expect("the Tauri build step failed");
}
