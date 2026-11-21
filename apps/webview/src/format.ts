// Formatting that matches the command-line table: binary units with one
// decimal place above 1 KiB.

const UNITS = ["KiB", "MiB", "GiB", "TiB", "PiB", "EiB"];

export function formatBytes(bytes: number): string {
  if (bytes < 1024) {
    return `${bytes} B`;
  }
  let value = bytes / 1024;
  let unit = 0;
  while (value >= 1024 && unit < UNITS.length - 1) {
    value /= 1024;
    unit += 1;
  }
  return `${value.toFixed(1)} ${UNITS[unit]}`;
}

export function formatElapsed(milliseconds: number): string {
  return milliseconds < 1000 ? `${milliseconds} ms` : `${(milliseconds / 1000).toFixed(1)} s`;
}

export function formatCount(count: number): string {
  return count.toLocaleString("en-US");
}

/** The calendar date of an RFC 3339 timestamp in UTC. */
export function formatDate(timestamp: string | null): string {
  return timestamp === null ? "–" : timestamp.slice(0, 10);
}
