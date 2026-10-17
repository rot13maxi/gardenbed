import { APIRequestContext, expect } from "@playwright/test";

export interface RefEntry {
  name: string;
  cs: string;
}
export interface LogEntry {
  id: string;
  parents: string[];
  message: string;
  author: { name: string; email: string; time_secs: number; tz_min: number };
  committer: { name: string; email: string; time_secs: number; tz_min: number };
  changes: number;
}
export interface Change {
  op: "add" | "modify" | "delete" | "rename";
  path: string;
  src_path: string | null;
  mode: string;
  blob: string | null;
}

export async function refs(request: APIRequestContext): Promise<RefEntry[]> {
  const r = await request.get("/api/refs");
  expect(r.ok()).toBeTruthy();
  return (await r.json()).refs;
}

/** The seeded tip: the ref main's changeset. */
export async function mainTip(request: APIRequestContext): Promise<string> {
  const all = await refs(request);
  const main = all.find((r) => r.name === "refs/heads/main" || r.name === "main");
  expect(main, "seeded main ref").toBeTruthy();
  return main!.cs;
}

/** Full history from a revspec, newest first. */
export async function log(
  request: APIRequestContext,
  cs: string,
  limit = 100,
): Promise<LogEntry[]> {
  const r = await request.get(`/api/log?cs=${encodeURIComponent(cs)}&limit=${limit}`);
  expect(r.ok()).toBeTruthy();
  return (await r.json()).entries;
}

export async function changeset(
  request: APIRequestContext,
  id: string,
): Promise<{ changes: Change[]; message: string } & LogEntry> {
  const r = await request.get(`/api/changeset?id=${encodeURIComponent(id)}`);
  expect(r.ok()).toBeTruthy();
  return r.json();
}

/** Find the seeded commit index (1-based position from the end = oldest). */
export function findByMessage(entries: LogEntry[], needle: string): LogEntry {
  const hit = entries.find((e) => e.message.startsWith(needle));
  expect(hit, `seeded commit starting with ${needle}`).toBeTruthy();
  return hit!;
}
