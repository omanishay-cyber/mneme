/**
 * B-023 (2026-05-02): projectIdForPath MUST match the Rust CLI's
 * `ProjectId::from_path` byte-for-byte. The CLI uses `dunce::canonicalize`
 * which preserves native backslashes and original case on Windows; only
 * the UNC `\\?\` long-path prefix is stripped. Any divergence here causes
 * the MCP server to look up a different shard than the CLI built (every
 * tool call returns "shard not found" — caught by the 2026-05-02 multi-
 * MCP bench where mneme scored 0/5 because of this mismatch alone).
 *
 * The pre-B-023 implementation lowercased + replaced backslashes for
 * "case-insensitive Windows matching" but the CLI never had that
 * behavior, so the case-insensitive feature actively broke real lookups.
 *
 * Run with: `bun test src/tools/__tests__/projectIdForPath.test.ts`
 */

import { describe, it, expect } from "bun:test";
import { projectIdForPath } from "../../store.ts";

describe("projectIdForPath - matches Rust CLI ProjectId::from_path", () => {
  it("returns a 64-char hex SHA-256 digest", () => {
    const id = projectIdForPath("/some/path/that/likely/does/not/exist");
    expect(id).toMatch(/^[0-9a-f]{64}$/);
  });

  it("on POSIX, case differences yield different ids (matches CLI)", () => {
    if (process.platform === "win32") return;
    const a = projectIdForPath("/Users/x");
    const b = projectIdForPath("/users/x");
    expect(a).not.toEqual(b);
  });

  it("on Windows, case + slash differences yield different ids (matches CLI dunce::canonicalize)", () => {
    if (process.platform !== "win32") return;
    // Pre-B-023 behavior would have asserted these hash IDENTICALLY.
    // Post-B-023: each variant is preserved as-given (after UNC strip),
    // so different spellings → different ids. This matches what the CLI
    // actually does, which is what the MCP MUST match to find shards.
    const upper = projectIdForPath("C:\\Users\\User\\x");
    const lower = projectIdForPath("c:\\users\\user\\x");
    const slash = projectIdForPath("C:/Users/User/x");
    expect(upper).not.toEqual(lower);
    expect(upper).not.toEqual(slash);
  });

  it("on Windows, strips UNC \\\\?\\ long-path prefix (mirrors dunce)", () => {
    if (process.platform !== "win32") return;
    const withUnc = projectIdForPath("\\\\?\\C:\\Users\\User\\x");
    const without = projectIdForPath("C:\\Users\\User\\x");
    // Both should hash identically because dunce strips the UNC prefix.
    expect(withUnc).toEqual(without);
  });
});
