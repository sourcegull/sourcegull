import { describe, expect, it } from "vitest";
import { diffLines, textFileDiff, textLineDiff } from "./diff.ts";

const lines = (n: number, f: (i: number) => string = (i) => `line ${i}`) =>
  Array.from({ length: n }, (_, i) => f(i + 1)).join("\n") + "\n";

describe("textFileDiff", () => {
  it("finds the changed lines", () => {
    const ops = textLineDiff("a\nb\nc\nd\n", "a\nB\nc\nd\ne\n");
    expect(ops.map((o) => `${o.kind[0]}${o.text}`)).toEqual([
      "ca",
      "db",
      "aB",
      "cc",
      "cd",
      "ae",
    ]);
    expect(
      textLineDiff("same\n", "same\n").every((o) => o.kind === "context"),
    ).toBe(true);
    expect(textLineDiff("", "x\n")).toEqual([{ kind: "added", text: "x" }]);
  });

  it("groups changes into hunks with context", () => {
    const oldText = lines(30);
    const newText = lines(30, (i) =>
      i === 2 || i === 25 ? `changed ${i}` : `line ${i}`,
    );
    const d = textFileDiff("f.txt", oldText, newText);
    expect(d.stat).toEqual({ added: 2, deleted: 2 });
    expect(d.hunks.map((h) => [h.oldRange, h.newRange])).toEqual([
      [
        { startLine: 1, lines: 5 },
        { startLine: 1, lines: 5 },
      ],
      [
        { startLine: 22, lines: 7 },
        { startLine: 22, lines: 7 },
      ],
    ]);
    const first = diffLines(d.hunks[0]?.body ?? '', 1, 1);
    expect(first.map((l) => l.kind)).toEqual([
      "context",
      "deleted",
      "added",
      "context",
      "context",
      "context",
    ]);
    expect(first[2]).toEqual({
      kind: "added",
      oldLine: null,
      newLine: 2,
      text: "changed 2",
    });
    // Changes 6 lines apart share one hunk.
    const near = textFileDiff(
      "f",
      oldText,
      lines(30, (i) => (i === 10 || i === 17 ? "x" : `line ${i}`)),
    );
    expect(near.hunks).toHaveLength(1);
    expect(textFileDiff("f", oldText, oldText).hunks).toEqual([]);
  });
});
