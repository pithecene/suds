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
