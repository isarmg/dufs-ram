import assert from "node:assert/strict";
import test from "node:test";

import {
  prepareUploadSelection,
} from "../../../assets/modules/upload/selection.js";

test("upload selection enforces file-count and UTF-8 path budgets", () => {
  const OriginalFile = globalThis.File;
  class TestFile {
    constructor(name, relativePath = "") {
      this.name = name;
      this.webkitRelativePath = relativePath;
    }
  }
  globalThis.File = TestFile;
  try {
    const files = [
      new TestFile("a.txt"),
      new TestFile("b.txt"),
      new TestFile("c.txt"),
    ];
    const overCount = prepareUploadSelection(files, {
      fileLimit: 2,
      pathBytesLimit: 100,
    });
    assert.equal(overCount.ok, false);
    assert.equal(overCount.entries.length, 0);
    assert.match(overCount.error, /no more than 2 files/);

    const unicode = new TestFile("child.txt", "目录/child.txt");
    const expectedBytes = new TextEncoder().encode(unicode.webkitRelativePath)
      .byteLength;
    const accepted = prepareUploadSelection([unicode], {
      fileLimit: 1,
      pathBytesLimit: expectedBytes,
    });
    assert.equal(accepted.ok, true);
    assert.equal(accepted.totalPathBytes, expectedBytes);
    assert.equal(accepted.entries[0].name, unicode.webkitRelativePath);

    const overBytes = prepareUploadSelection([unicode], {
      fileLimit: 1,
      pathBytesLimit: expectedBytes - 1,
    });
    assert.equal(overBytes.ok, false);
    assert.equal(overBytes.entries.length, 0);
    assert.match(overBytes.error, /byte batch limit/);
  } finally {
    if (OriginalFile === undefined) {
      delete globalThis.File;
    } else {
      globalThis.File = OriginalFile;
    }
  }
});
