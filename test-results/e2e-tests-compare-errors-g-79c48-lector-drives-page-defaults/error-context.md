# Instructions

- Following Playwright test failed.
- Explain why, be concise, respect Playwright best practices.
- Provide a snippet of code with the fix, if possible.

# Test info

- Name: e2e/tests/compare-errors.spec.ts >> global ref selector drives page defaults
- Location: e2e/tests/compare-errors.spec.ts:69:5

# Error details

```
TypeError: apiRequestContext.get: Invalid URL
```

# Test source

```ts
  1  | import { APIRequestContext, expect } from "@playwright/test";
  2  | 
  3  | export interface RefEntry {
  4  |   name: string;
  5  |   cs: string;
  6  | }
  7  | export interface LogEntry {
  8  |   id: string;
  9  |   parents: string[];
  10 |   message: string;
  11 |   author: { name: string; email: string; time_secs: number; tz_min: number };
  12 |   committer: { name: string; email: string; time_secs: number; tz_min: number };
  13 |   changes: number;
  14 | }
  15 | export interface Change {
  16 |   op: "add" | "modify" | "delete" | "rename";
  17 |   path: string;
  18 |   src_path: string | null;
  19 |   mode: string;
  20 |   blob: string | null;
  21 | }
  22 | 
  23 | export async function refs(request: APIRequestContext): Promise<RefEntry[]> {
> 24 |   const r = await request.get("/api/refs");
     |                           ^ TypeError: apiRequestContext.get: Invalid URL
  25 |   expect(r.ok()).toBeTruthy();
  26 |   return (await r.json()).refs;
  27 | }
  28 | 
  29 | /** The seeded tip: the ref main's changeset. */
  30 | export async function mainTip(request: APIRequestContext): Promise<string> {
  31 |   const all = await refs(request);
  32 |   const main = all.find((r) => r.name === "refs/heads/main" || r.name === "main");
  33 |   expect(main, "seeded main ref").toBeTruthy();
  34 |   return main!.cs;
  35 | }
  36 | 
  37 | /** Full history from a revspec, newest first. */
  38 | export async function log(
  39 |   request: APIRequestContext,
  40 |   cs: string,
  41 |   limit = 100,
  42 | ): Promise<LogEntry[]> {
  43 |   const r = await request.get(`/api/log?cs=${encodeURIComponent(cs)}&limit=${limit}`);
  44 |   expect(r.ok()).toBeTruthy();
  45 |   return (await r.json()).entries;
  46 | }
  47 | 
  48 | export async function changeset(
  49 |   request: APIRequestContext,
  50 |   id: string,
  51 | ): Promise<{ changes: Change[]; message: string } & LogEntry> {
  52 |   const r = await request.get(`/api/changeset?id=${encodeURIComponent(id)}`);
  53 |   expect(r.ok()).toBeTruthy();
  54 |   return r.json();
  55 | }
  56 | 
  57 | /** Find the seeded commit index (1-based position from the end = oldest). */
  58 | export function findByMessage(entries: LogEntry[], needle: string): LogEntry {
  59 |   const hit = entries.find((e) => e.message.startsWith(needle));
  60 |   expect(hit, `seeded commit starting with ${needle}`).toBeTruthy();
  61 |   return hit!;
  62 | }
  63 | 
```