export function checksFrom(report) {
  const checks = new Map();
  for (const check of [
    ...(report.history ?? []),
    ...(report.rows ?? []).map((row) => row.current),
  ]) {
    if (check?.number !== undefined) checks.set(String(check.number), check);
  }
  return [...checks.values()].sort((a, b) =>
    BigInt(a.number) > BigInt(b.number) ? -1 : 1,
  );
}
export function category(check) {
  if (check.operational_error) return "error";
  if (check.result?.outcome === "fail") return "fail";
  if (check.result?.outcome === "conditional") return "conditional";
  if (check.result?.outcome === "pass") return "pass";
  if (!check.terminal) return "running";
  return "other";
}
export function status(check) {
  return (
    check.presentation?.status?.label ??
    {
      pass: "Passed",
      fail: "Failed",
      conditional: "Conditional",
      error: "Could not complete",
      running: "Running",
      other: "No result",
    }[category(check)]
  );
}
export function revision(check) {
  return (
    check.revision?.reported_git_commit ?? check.revision?.tree_sha256 ?? ""
  );
}
