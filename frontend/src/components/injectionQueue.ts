/// Indices (into `queued`, oldest first) of the queued mid-turn messages an
/// echoed prompt carries, or null when it carries none. The engine delivers
/// a queued message either alone (drained at a tool_result boundary) or,
/// when the turn ended first, together with the ones queued before it as a
/// single prompt joined by blank lines.
export function matchQueuedInjections(
  queued: string[],
  incoming: string,
): number[] | null {
  const exact = queued.indexOf(incoming);
  if (exact >= 0) return [exact];
  for (let k = 2; k <= queued.length; k++) {
    if (queued.slice(0, k).join("\n\n") === incoming) {
      return Array.from({ length: k }, (_, i) => i);
    }
  }
  return null;
}
