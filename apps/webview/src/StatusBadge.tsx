import type { RootResult } from "./api";

export function StatusBadge({ outcome }: { outcome: { result: RootResult; cancelled: boolean } }) {
  const label = outcome.cancelled ? "cancelled" : outcome.result.status;
  return <span className={`badge badge-${label}`}>{label}</span>;
}
