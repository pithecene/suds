import { type ReactNode, useState } from "react";

import type { Location, RootResult } from "./api";
import { StatusBadge } from "./StatusBadge";
import { formatBytes, formatCount, formatDate, formatElapsed } from "./format";

/** The number of children that the table lists before the user asks for all. */
const INITIAL_ROWS = 200;

interface Props {
  location: Location;
  outcome: { result: RootResult; cancelled: boolean } | undefined;
  scanning: boolean;
  busy: boolean;
  onInspect: () => void;
}

export function RootDetail({ location, outcome, scanning, busy, onInspect }: Props) {
  const [showAll, setShowAll] = useState(false);
  const result = outcome?.result;
  const measured = result !== undefined && (result.status === "complete" || result.status === "partial");
  const children = result?.children ?? [];
  const shown = showAll ? children : children.slice(0, INITIAL_ROWS);
  const largest = children[0]?.allocated_bytes ?? 0;

  return (
    <article>
      <header className="detail-header">
        <div>
          <h2>{location.label}</h2>
          <p className="path">{location.display_path}</p>
        </div>
        <button type="button" disabled={busy} onClick={onInspect}>
          {outcome === undefined ? "Inspect" : "Refresh"}
        </button>
      </header>

      {scanning && <p className="muted">Scanning…</p>}
      {!scanning && outcome === undefined && (
        <p className="muted">Suds has not inspected this location. Inspection reads metadata only and changes nothing.</p>
      )}

      {result !== undefined && outcome !== undefined && (
        <>
          <dl className="stats">
            <Stat label="Status" value={<StatusBadge outcome={outcome} />} />
            <Stat label="Allocated" value={measured ? formatBytes(result.allocated_bytes) : "–"} />
            <Stat label="Logical" value={measured ? formatBytes(result.logical_bytes) : "–"} />
            <Stat label="Files" value={measured ? formatCount(result.files) : "–"} />
            <Stat label="Directories" value={measured ? formatCount(result.directories) : "–"} />
            <Stat label="Symbolic Links" value={measured ? formatCount(result.symlinks) : "–"} />
            <Stat
              label="Modified"
              wide
              value={measured ? `${formatDate(result.oldest_modified)} to ${formatDate(result.newest_modified)}` : "–"}
            />
            <Stat label="Elapsed" value={formatElapsed(result.elapsed_ms)} />
          </dl>

          {result.status === "absent" && <p className="muted">This location does not exist.</p>}

          {measured && children.length === 0 && <p className="muted">This location holds no entries.</p>}
          {measured && children.length > 0 && (
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
                  <th scope="col" className="number">
                    Files
                  </th>
                  <th scope="col" className="number">
                    Directories
                  </th>
                </tr>
              </thead>
              <tbody>
                {shown.map((child, index) => (
                  <tr key={`${index}:${child.name}`}>
                    <td className="name">
                      <span className="share">
                        <meter min={0} max={largest || 1} value={child.allocated_bytes} />
                      </span>
                      {child.name}
                    </td>
                    <td>{child.kind}</td>
                    <td className="number">{formatBytes(child.allocated_bytes)}</td>
                    <td className="number">{formatBytes(child.logical_bytes)}</td>
                    <td className="number">{formatCount(child.files)}</td>
                    <td className="number">{formatCount(child.directories)}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          )}
          {!showAll && children.length > INITIAL_ROWS && (
            <button type="button" className="link" onClick={() => setShowAll(true)}>
              Show all {formatCount(children.length)} entries
            </button>
          )}

          {result.skipped_mount_count > 0 && (
            <section>
              <h3>Mount Points Left Unscanned ({formatCount(result.skipped_mount_count)})</h3>
              <ul className="paths">
                {result.skipped_mounts.map((path) => (
                  <li key={path}>{path}</li>
                ))}
              </ul>
            </section>
          )}
          {result.error_count > 0 && (
            <section>
              <h3>Traversal Errors ({formatCount(result.error_count)})</h3>
              <ul className="paths">
                {result.errors.map((error, index) => (
                  <li key={`${index}:${error.path}`}>
                    <span>{error.path}</span>
                    <span className="muted"> {error.message}</span>
                  </li>
                ))}
              </ul>
              {result.error_count > result.errors.length && (
                <p className="muted">The report lists the first {result.errors.length} errors.</p>
              )}
            </section>
          )}
          <p className="note">
            Each allocated size estimates the disk space of an entry's blocks. On APFS, clones, compression,
            snapshots, and purgeable storage can make the recoverable space differ from the allocated size.
          </p>
        </>
      )}
    </article>
  );
}

function Stat({ label, value, wide = false }: { label: string; value: ReactNode; wide?: boolean }) {
  return (
    <div className={wide ? "wide" : undefined}>
      <dt>{label}</dt>
      <dd>{value}</dd>
    </div>
  );
}
