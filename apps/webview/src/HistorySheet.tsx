import { useCallback, useEffect, useRef, useState } from "react";

import { listJournal, type RunSummary, undoRun } from "./api";

interface Props {
  /** Called after an undo changes the folders. */
  onChanged: () => void;
  onClose: () => void;
}

/** The runs that the journal records, each with an undo while a move remains applied. */
export function HistorySheet({ onChanged, onClose }: Props) {
  const dialog = useRef<HTMLDialogElement>(null);
  const [runs, setRuns] = useState<RunSummary[] | null>(null);
  const [undoing, setUndoing] = useState<string | null>(null);
  const [failure, setFailure] = useState<string | null>(null);

  const load = useCallback(() => {
    listJournal()
      .then(setRuns)
      .catch((err: unknown) => setFailure(String(err)));
  }, []);

  useEffect(() => {
    dialog.current?.showModal();
    load();
  }, [load]);

  const reverse = (run: string) => {
    setUndoing(run);
    setFailure(null);
    undoRun(run)
      .then((result) => {
        if (result.status !== "complete") {
          setFailure("Suds could not return every item. The run lists each refusal.");
        }
      })
      .catch((err: unknown) => setFailure(String(err)))
      .finally(() => {
        setUndoing(null);
        load();
        onChanged();
      });
  };

  return (
    <dialog
      ref={dialog}
      className="sheet"
      aria-labelledby="history-sheet-title"
      onCancel={(event) => {
        event.preventDefault();
        if (undoing === null) {
          onClose();
        }
      }}
    >
      <h2 id="history-sheet-title">History</h2>
      {failure !== null && (
        <p className="banner" role="alert">
          {failure}
        </p>
      )}
      {runs === null && failure === null && <p className="muted">Reading the journal…</p>}
      {runs !== null && runs.length === 0 && <p className="muted">Suds has moved nothing.</p>}
      {runs !== null && runs.length > 0 && (
        <ul className="history">
          {runs.map((summary) => (
            <li key={summary.run}>
              <div className="history-header">
                <span>
                  {summary.started_at.replace("T", " ").replace("Z", " UTC")}{" "}
                  <span className={`badge badge-${summary.status ?? "partial"}`}>
                    {summary.status ?? "in progress"}
                  </span>
                  {summary.interrupted && <span className="muted"> interrupted</span>}
                  {summary.undo !== null && <span className="muted"> undo {summary.undo}</span>}
                </span>
                <button
                  type="button"
                  disabled={!summary.undoable || undoing !== null}
                  onClick={() => reverse(summary.run)}
                >
                  {undoing === summary.run ? "Undoing…" : "Undo"}
                </button>
              </div>
              <ul className="outcomes">
                {summary.moves.map((outcome) => (
                  <li key={outcome.source}>
                    <span className={`badge badge-${outcome.status}`}>{outcome.status.replace("_", " ")}</span>{" "}
                    <span className="path">
                      {outcome.source} → {outcome.destination}
                    </span>
                    {outcome.refusal !== null && <p className="muted">{outcome.refusal.message}</p>}
                  </li>
                ))}
              </ul>
            </li>
          ))}
        </ul>
      )}
      <div className="sheet-actions">
        <button type="button" className="primary" disabled={undoing !== null} onClick={onClose}>
          Done
        </button>
      </div>
    </dialog>
  );
}
