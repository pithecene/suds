import { useCallback, useEffect, useRef, useState } from "react";

import {
  cancelInspection,
  chooseLocation,
  createMovePlan,
  type InspectionEvent,
  listLocations,
  type Location,
  type Plan,
  type RootResult,
  startInspection,
} from "./api";
import { HistorySheet } from "./HistorySheet";
import { MoveSheet } from "./MoveSheet";
import { RootDetail } from "./RootDetail";
import { StatusBadge } from "./StatusBadge";
import { formatBytes } from "./format";

interface Outcome {
  result: RootResult;
  cancelled: boolean;
  /** The inspection that produced the result, which a move selection names. */
  inspection: number;
}

interface Run {
  id: number;
  pending: Set<number>;
}

export function App() {
  const [locations, setLocations] = useState<Location[]>([]);
  const [profileError, setProfileError] = useState<string | null>(null);
  const [outcomes, setOutcomes] = useState<Map<number, Outcome>>(new Map());
  const [selected, setSelected] = useState<number | null>(null);
  const [run, setRun] = useState<Run | null>(null);
  const [failure, setFailure] = useState<string | null>(null);
  const [plan, setPlan] = useState<{ plan: Plan; location: number } | null>(null);
  const [historyOpen, setHistoryOpen] = useState(false);
  // Events from a replaced inspection still arrive, so each event names its
  // inspection, and the window keeps only the current one's.
  const current = useRef<number | null>(null);
  // The backend cancels a run when the next one starts, but the answers to two
  // overlapping starts can arrive in either order. Each start takes a number,
  // and only the latest start may claim the window.
  const latestRequest = useRef(0);

  useEffect(() => {
    listLocations()
      .then((list) => {
        setLocations(list.locations);
        setProfileError(list.profile_error);
        setSelected(list.locations[0]?.id ?? null);
      })
      .catch((err: unknown) => setFailure(String(err)));
  }, []);

  const onEvent = useCallback((event: InspectionEvent) => {
    if (event.inspection !== current.current) {
      return;
    }
    if (event.type === "root") {
      setOutcomes((previous) =>
        new Map(previous).set(event.location, {
          result: event.result,
          cancelled: event.cancelled,
          inspection: event.inspection,
        }),
      );
      setRun((previous) => {
        if (previous === null) {
          return null;
        }
        const pending = new Set(previous.pending);
        pending.delete(event.location);
        return { ...previous, pending };
      });
    } else {
      current.current = null;
      setRun(null);
    }
  }, []);

  const inspect = useCallback(
    (ids: number[]) => {
      setFailure(null);
      latestRequest.current += 1;
      const request = latestRequest.current;
      // The backend cancels the previous run, so the window drops its events.
      current.current = null;
      // The backend starts the scan before it answers with the identifier, so
      // a quick scan can report before the answer arrives. The run holds those
      // events and replays them once it knows its identifier.
      let started = false;
      const early: InspectionEvent[] = [];
      const receive = (event: InspectionEvent) => {
        if (started) {
          onEvent(event);
        } else {
          early.push(event);
        }
      };
      startInspection(ids, receive)
        .then((id) => {
          if (request !== latestRequest.current) {
            return;
          }
          current.current = id;
          setRun({ id, pending: new Set(ids) });
          started = true;
          for (const event of early) {
            onEvent(event);
          }
        })
        .catch((err: unknown) => {
          if (request !== latestRequest.current) {
            return;
          }
          // The previous run, if any, no longer owns the window.
          setRun(null);
          setFailure(String(err));
        });
    },
    [onEvent],
  );

  const addFolder = useCallback(() => {
    chooseLocation()
      .then((location) => {
        if (location === null) {
          return;
        }
        setLocations((previous) =>
          previous.some((entry) => entry.id === location.id) ? previous : [...previous, location],
        );
        setSelected(location.id);
        inspect([location.id]);
      })
      .catch((err: unknown) => setFailure(String(err)));
  }, [inspect]);

  const requestMove = useCallback((location: number, inspection: number, entries: number[]) => {
    setFailure(null);
    createMovePlan(inspection, location, entries)
      .then((response) => {
        if (response.type === "planned") {
          setPlan({ plan: response.plan, location });
        } else if (response.type === "refused") {
          setFailure(response.refusals.map((refusal) => refusal.message).join(" "));
        }
      })
      .catch((err: unknown) => setFailure(String(err)));
  }, []);

  // After a move or an undo, the window inspects each Downloads location
  // again, so the table and every selection match the folder.
  const refreshInboxes = useCallback(() => {
    const ids = locations.filter((location) => location.kind === "inbox").map((location) => location.id);
    if (ids.length > 0) {
      inspect(ids);
    }
  }, [locations, inspect]);

  const cancel = useCallback(() => {
    if (run !== null) {
      cancelInspection(run.id).catch((err: unknown) => setFailure(String(err)));
    }
  }, [run]);

  const selectedLocation = locations.find((location) => location.id === selected) ?? null;
  const selectedOutcome = selected === null ? undefined : outcomes.get(selected);

  return (
    <div className="app">
      <header className="toolbar">
        <h1>Suds</h1>
        <div className="actions">
          <button
            type="button"
            disabled={locations.length === 0 || run !== null}
            onClick={() => inspect(locations.map((location) => location.id))}
          >
            Inspect All
          </button>
          <button type="button" disabled={run === null} onClick={cancel}>
            Cancel
          </button>
          <button type="button" onClick={addFolder}>
            Add Folder…
          </button>
          <button type="button" onClick={() => setHistoryOpen(true)}>
            History
          </button>
        </div>
      </header>
      {failure !== null && (
        <p className="banner" role="alert">
          {failure}
        </p>
      )}
      <div className="body">
        <nav className="sidebar" aria-label="Locations">
          {profileError !== null && <p className="muted">{profileError}</p>}
          <ul>
            {locations.map((location) => {
              const outcome = outcomes.get(location.id);
              const scanning = run?.pending.has(location.id) ?? false;
              return (
                <li key={location.id}>
                  <button
                    type="button"
                    className={location.id === selected ? "location selected" : "location"}
                    aria-current={location.id === selected}
                    onClick={() => setSelected(location.id)}
                  >
                    <span className="location-label">{location.label}</span>
                    <span className="location-meta">
                      {scanning ? (
                        "Scanning…"
                      ) : outcome === undefined ? (
                        "Not inspected"
                      ) : (
                        <>
                          <StatusBadge outcome={outcome} />
                          {measured(outcome.result) && (
                            <span className="size">{formatBytes(outcome.result.allocated_bytes)}</span>
                          )}
                        </>
                      )}
                    </span>
                  </button>
                </li>
              );
            })}
          </ul>
        </nav>
        <main className="detail">
          {selectedLocation === null ? (
            <p className="muted">No location is selected.</p>
          ) : (
            // The key gives each location its own detail state, so a table that
            // one location expanded starts collapsed for the next.
            <RootDetail
              key={selectedLocation.id}
              location={selectedLocation}
              outcome={selectedOutcome}
              scanning={run?.pending.has(selectedLocation.id) ?? false}
              busy={run !== null}
              onInspect={() => inspect([selectedLocation.id])}
              selectable={selectedLocation.kind === "inbox"}
              onMove={(entries) => {
                if (selectedOutcome !== undefined) {
                  requestMove(selectedLocation.id, selectedOutcome.inspection, entries);
                }
              }}
            />
          )}
        </main>
      </div>
      {plan !== null && (
        <MoveSheet plan={plan.plan} onChanged={refreshInboxes} onClose={() => setPlan(null)} />
      )}
      {historyOpen && <HistorySheet onChanged={refreshInboxes} onClose={() => setHistoryOpen(false)} />}
    </div>
  );
}

function measured(result: RootResult): boolean {
  return result.status === "complete" || result.status === "partial";
}
