import { useEffect, useRef, useState } from "react";

import { applyPlan, type MoveResult, type Plan, type RunResult, undoRun } from "./api";
import { formatBytes, formatCount, formatDate } from "./format";

interface Props {
  plan: Plan;
  /** Called after an apply or an undo changes the folders. */
  onChanged: () => void;
  onClose: () => void;
}

type Phase = "review" | "applying" | "applied" | "undoing" | "undone";

/**
 * The confirmation sheet. It applies the plan only on request, lists each
 * outcome as it arrives, and offers to undo the run.
 */
export function MoveSheet({ plan, onChanged, onClose }: Props) {
  const dialog = useRef<HTMLDialogElement>(null);
  const [phase, setPhase] = useState<Phase>("review");
  const [outcomes, setOutcomes] = useState<MoveResult[]>([]);
  const [run, setRun] = useState<RunResult | null>(null);
  const [undo, setUndo] = useState<RunResult | null>(null);
  const [failure, setFailure] = useState<string | null>(null);
  const working = phase === "applying" || phase === "undoing";

  useEffect(() => {
    dialog.current?.showModal();
  }, []);

  const apply = () => {
    setPhase("applying");
    setFailure(null);
    applyPlan(plan.id, (result) => setOutcomes((previous) => [...previous, result]))
      .then((result) => {
        setRun(result);
        setOutcomes(result.moves);
        setPhase("applied");
      })
      .catch((err: unknown) => {
        setFailure(String(err));
        setPhase("applied");
      })
      .finally(onChanged);
  };

  const reverse = () => {
    if (run === null) {
      return;
    }
    setPhase("undoing");
    setFailure(null);
    undoRun(run.run)
      .then((result) => {
        setUndo(result);
        setPhase("undone");
      })
      .catch((err: unknown) => {
        setFailure(String(err));
        setPhase("applied");
      })
      .finally(onChanged);
  };

  const totalAllocated = plan.moves.reduce((sum, planned) => sum + planned.allocated_bytes, 0);
  const applied = outcomes.filter((outcome) => outcome.status === "applied").length;

  return (
    <dialog
      ref={dialog}
      className="sheet"
      aria-labelledby="move-sheet-title"
      onCancel={(event) => {
        event.preventDefault();
        if (!working) {
          onClose();
        }
      }}
    >
      <h2 id="move-sheet-title">
        {phase === "review" ? `Move ${formatCount(plan.moves.length)} Items?` : "Move Results"}
      </h2>
      <dl className="plan-paths">
        <dt>From</dt>
        <dd className="path">{plan.source_directory}</dd>
        <dt>To</dt>
        <dd className="path">{plan.destination_directory}</dd>
      </dl>

      {phase === "review" ? (
        <>
          <table className="children">
            <thead>
              <tr>
                <th scope="col">Name</th>
                <th scope="col">Type</th>
                <th scope="col" className="number">
                  Allocated
                </th>
                <th scope="col" className="number">
                  Logical
                </th>
                <th scope="col">Modified</th>
              </tr>
            </thead>
            <tbody>
              {plan.moves.map((planned) => (
                <tr key={planned.source}>
                  <td className="name">
                    {planned.name}
                    {planned.measure_error_count > 0 && (
                      <span className="muted">
                        {" "}
                        ({formatCount(planned.measure_error_count)} entries unreadable; sizes are lower bounds)
                      </span>
                    )}
                  </td>
                  <td>{planned.kind}</td>
                  <td className="number">{formatBytes(planned.allocated_bytes)}</td>
                  <td className="number">{formatBytes(planned.logical_bytes)}</td>
                  <td>{formatDate(planned.newest_modified)}</td>
                </tr>
              ))}
            </tbody>
          </table>
          <p>
            {formatBytes(totalAllocated)} in all. Each move is one atomic rename on one filesystem. Suds overwrites
            nothing, and you can undo every move.
          </p>
          <details>
            <summary>Conditions that Suds verifies immediately before each move</summary>
            <ul className="conditions">
              {plan.preconditions.map((condition) => (
                <li key={condition}>{condition}</li>
              ))}
            </ul>
          </details>
        </>
      ) : (
        <>
          <ul className="outcomes">
            {outcomes.map((outcome) => (
              <li key={outcome.source}>
                <span className={`badge badge-${outcome.status}`}>{outcome.status.replace("_", " ")}</span>{" "}
                <span className="path">{outcome.source}</span>
                {outcome.refusal !== null && <p className="muted">{outcome.refusal.message}</p>}
              </li>
            ))}
          </ul>
          {phase === "applying" && <p className="muted">Moving…</p>}
          {run !== null && (
            <p>
              Moved {formatCount(applied)} of {formatCount(plan.moves.length)} items.
              {run.reconciled > 0 && ` Suds also reconciled ${formatCount(run.reconciled)} interrupted moves.`}
            </p>
          )}
          {undo !== null && (
            <>
              <p>
                Returned {formatCount(undo.moves.filter((outcome) => outcome.status === "reversed").length)} of{" "}
                {formatCount(undo.moves.length)} items to {plan.source_directory}.
              </p>
              <ul className="outcomes">
                {undo.moves
                  .filter((outcome) => outcome.refusal !== null)
                  .map((outcome) => (
                    <li key={outcome.source}>
                      <span className="path">{outcome.destination}</span>
                      <p className="muted">{outcome.refusal?.message}</p>
                    </li>
                  ))}
              </ul>
            </>
          )}
        </>
      )}

      {failure !== null && (
        <p className="banner" role="alert">
          {failure}
        </p>
      )}

      <div className="sheet-actions">
        {phase === "review" && (
          <>
            <button type="button" onClick={onClose}>
              Cancel
            </button>
            <button type="button" className="primary" onClick={apply}>
              Move
            </button>
          </>
        )}
        {phase !== "review" && (
          <>
            {phase === "applied" && run !== null && applied > 0 && (
              <button type="button" onClick={reverse}>
                Undo
              </button>
            )}
            <button type="button" className="primary" disabled={working} onClick={onClose}>
              Done
            </button>
          </>
        )}
      </div>
    </dialog>
  );
}
