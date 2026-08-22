import assert from "node:assert/strict";
import test from "node:test";

import {
  parseIndexData,
} from "../../../assets/modules/shared/index_data.js";

const VALID_INDEX_DATA = Object.freeze({
  href: "/folder/文件 & name",
  dir_exists: true,
  user: "browser-user",
  csrf_token: "0123456789abcdef".repeat(4),
});

test("embedded index data returns only a fresh frozen validated record", () => {
  const input = { ...VALID_INDEX_DATA };
  const parsed = parseIndexData(input);

  assert.deepEqual(parsed, VALID_INDEX_DATA);
  assert.notEqual(parsed, input);
  assert.equal(Object.isFrozen(parsed), true);
  assert.deepEqual(Object.keys(parsed), [
    "href",
    "dir_exists",
    "user",
    "csrf_token",
  ]);

  const nullPrototype = Object.assign(
    Object.create(null),
    { ...VALID_INDEX_DATA, href: "/" },
  );
  assert.deepEqual(parseIndexData(nullPrototype), {
    ...VALID_INDEX_DATA,
    href: "/",
  });
});

test("embedded index data rejects non-records and non-plain objects", () => {
  for (const value of [
    null,
    undefined,
    false,
    0,
    "",
    [],
    new Date(),
    Object.create({ ...VALID_INDEX_DATA }),
  ]) {
    assert.throws(
      () => parseIndexData(value),
      /Invalid embedded index data: expected a plain object/,
    );
  }
});

test("embedded index data requires exactly four own data properties", () => {
  for (const key of Object.keys(VALID_INDEX_DATA)) {
    const missing = { ...VALID_INDEX_DATA };
    delete missing[key];
    assert.throws(
      () => parseIndexData(missing),
      /expected exactly href, dir_exists, user, and csrf_token/,
    );
  }
  assert.throws(
    () => parseIndexData({ ...VALID_INDEX_DATA, extra: true }),
    /expected exactly href, dir_exists, user, and csrf_token/,
  );
  assert.throws(
    () => parseIndexData({
      ...VALID_INDEX_DATA,
      [Symbol("extra")]: true,
    }),
    /expected exactly href, dir_exists, user, and csrf_token/,
  );

  let hrefReads = 0;
  const accessor = { ...VALID_INDEX_DATA };
  Object.defineProperty(accessor, "href", {
    enumerable: true,
    get() {
      hrefReads++;
      return "/";
    },
  });
  assert.throws(
    () => parseIndexData(accessor),
    /href must be a data property/,
  );
  assert.equal(hrefReads, 0);
});

test("embedded index data accepts only canonical absolute logical paths", () => {
  for (const href of [
    "/",
    "/folder",
    "/folder/child.txt",
    "/目录/with space/100% literal",
    "/back\\slash/is-valid-on-linux",
  ]) {
    assert.equal(parseIndexData({ ...VALID_INDEX_DATA, href }).href, href);
  }

  for (const href of [
    "",
    "relative",
    "./relative",
    "../relative",
    "//",
    "//folder",
    "/folder/",
    "/folder//child",
    "/./folder",
    "/folder/.",
    "/../folder",
    "/folder/..",
    "/nul\0byte",
    null,
    0,
    false,
    [],
  ]) {
    assert.throws(
      () => parseIndexData({ ...VALID_INDEX_DATA, href }),
      /href must be a canonical absolute logical path/,
      href,
    );
  }
});

test("embedded index data enforces boolean and UTF-8 user fields", () => {
  assert.equal(
    parseIndexData({ ...VALID_INDEX_DATA, dir_exists: false }).dir_exists,
    false,
  );
  for (const dir_exists of [0, 1, "true", null, undefined]) {
    assert.throws(
      () => parseIndexData({ ...VALID_INDEX_DATA, dir_exists }),
      /dir_exists must be a boolean/,
    );
  }

  const exactAscii = "a".repeat(128);
  const exactUnicode = `${"界".repeat(42)}ab`;
  assert.equal(
    parseIndexData({ ...VALID_INDEX_DATA, user: exactAscii }).user,
    exactAscii,
  );
  assert.equal(
    parseIndexData({ ...VALID_INDEX_DATA, user: exactUnicode }).user,
    exactUnicode,
  );
  assert.equal(parseIndexData({ ...VALID_INDEX_DATA, user: "" }).user, "");

  for (const user of [
    "a".repeat(129),
    "界".repeat(43),
    null,
    42,
  ]) {
    assert.throws(
      () => parseIndexData({ ...VALID_INDEX_DATA, user }),
      /user must be a string no longer than 128 UTF-8 bytes/,
    );
  }
});

test("embedded index data accepts only canonical CSRF tokens", () => {
  for (const csrf_token of [
    "",
    "a".repeat(63),
    "a".repeat(65),
    "A".repeat(64),
    "g".repeat(64),
    "-".repeat(64),
    0,
    true,
    null,
  ]) {
    assert.throws(
      () => parseIndexData({ ...VALID_INDEX_DATA, csrf_token }),
      /csrf_token must be 64 lowercase hexadecimal characters/,
    );
  }
});
