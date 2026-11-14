// The application commands and the shapes that they exchange. The Rust
// crate in src-tauri defines each shape. A location travels as an
// identifier, never as a path that the webview could pass back.

import { Channel, invoke } from "@tauri-apps/api/core";

export type RootKind = "cache" | "logs" | "generated" | "inbox" | "trash" | "unclassified";
export type RootStatus = "complete" | "partial" | "absent" | "unreadable";
export type EntryKind = "directory" | "file" | "symlink" | "other";

export interface Measure {
  logical_bytes: number;
  allocated_bytes: number;
  files: number;
  directories: number;
  symlinks: number;
  others: number;
  oldest_modified: string | null;
  newest_modified: string | null;
}

export interface Child extends Measure {
  name: string;
  kind: EntryKind;
}

export interface TraversalError {
  path: string;
  kind: "permission_denied" | "vanished" | "other";
  message: string;
}

export interface RootResult extends Measure {
  id: string;
  kind: RootKind;
  path: string;
  status: RootStatus;
  elapsed_ms: number;
  children: Child[];
  skipped_mount_count: number;
  skipped_mounts: string[];
  error_count: number;
  errors: TraversalError[];
}

export interface Location {
  id: number;
  label: string;
  kind: RootKind;
  display_path: string;
  explicit: boolean;
}

export interface LocationList {
  locations: Location[];
  profile_error: string | null;
}

export type InspectionEvent =
  | { type: "root"; inspection: number; location: number; cancelled: boolean; result: RootResult }
  | { type: "finished"; inspection: number; cancelled: boolean };

export function listLocations(): Promise<LocationList> {
  return invoke("list_locations");
}

export function chooseLocation(): Promise<Location | null> {
  return invoke("choose_location");
}

export function startInspection(
  ids: number[],
  onEvent: (event: InspectionEvent) => void,
): Promise<number> {
  const events = new Channel<InspectionEvent>();
  events.onmessage = onEvent;
  return invoke("start_inspection", { ids, events });
}

export function cancelInspection(inspection: number): Promise<void> {
  return invoke("cancel_inspection", { inspection });
}

// Move plans. The webview names entries by their inspection, location, and
// position, and names a plan or a run by its identifier. Every path below is
// a display string, and no command accepts one.

export type RunStatus = "complete" | "partial" | "refused";
export type MoveStatus =
  | "not_started"
  | "started"
  | "applied"
  | "refused"
  | "not_applied"
  | "unresolved"
  | "reversing"
  | "reversed";

export interface Refusal {
  code: string;
  message: string;
}

export interface PlannedMove {
  name: string;
  kind: EntryKind;
  source: string;
  destination: string;
  logical_bytes: number;
  allocated_bytes: number;
  newest_modified: string | null;
  measure_error_count: number;
}

export interface Plan {
  id: string;
  created_at: string;
  source_directory: string;
  destination_directory: string;
  moves: PlannedMove[];
  preconditions: string[];
}

export type PlanResponse =
  | { type: "cancelled" }
  | { type: "refused"; refusals: Refusal[] }
  | { type: "planned"; plan: Plan };

export interface MoveResult {
  source: string;
  destination: string;
  status: MoveStatus;
  refusal: Refusal | null;
}

export interface RunResult {
  run: string;
  status: RunStatus;
  reconciled: number;
  moves: MoveResult[];
}

export interface RunSummary {
  run: string;
  started_at: string;
  status: RunStatus | null;
  interrupted: boolean;
  undo: RunStatus | null;
  undoable: boolean;
  moves: MoveResult[];
}

export function createMovePlan(inspection: number, location: number, entries: number[]): Promise<PlanResponse> {
  return invoke("create_move_plan", { inspection, location, entries });
}

export function applyPlan(plan: string, onMove: (result: MoveResult) => void): Promise<RunResult> {
  const events = new Channel<MoveResult>();
  events.onmessage = onMove;
  return invoke("apply_plan", { plan, events });
}

export function listJournal(): Promise<RunSummary[]> {
  return invoke("list_journal");
}

export function undoRun(run: string): Promise<RunResult> {
  return invoke("undo_run", { run });
}
