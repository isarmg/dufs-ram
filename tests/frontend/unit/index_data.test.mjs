import assert from "node:assert/strict";
import test from "node:test";

import {
  parseIndexData,
} from "../../../clients/web/modules/shared/index_data.js";

const VALID_SESSION = Object.freeze({
  authenticated: true,
  user_id: `dufs:${"a".repeat(64)}`,
  username: "admin",
  role: "admin",
  csrf_token: "A".repeat(43),
});
const VALID_INDEX_DATA = Object.freeze({
  href: "/folder/文件 & name",
  dir_exists: true,
  session: VALID_SESSION,
});

test("embedded index data returns only a fresh deeply frozen validated record", () => {
  const input = { ...VALID_INDEX_DATA, session: { ...VALID_SESSION } };
  const parsed = parseIndexData(input);

  assert.deepEqual(parsed, VALID_INDEX_DATA);
  assert.notEqual(parsed, input);
  assert.notEqual(parsed.session, input.session);
  assert.equal(Object.isFrozen(parsed), true);
  assert.equal(Object.isFrozen(parsed.session), true);
  assert.deepEqual(Object.keys(parsed), ["href", "dir_exists", "session"]);

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

test("embedded index data requires exactly three own data properties", () => {
  for (const key of Object.keys(VALID_INDEX_DATA)) {
    const missing = { ...VALID_INDEX_DATA };
    delete missing[key];
    assert.throws(
      () => parseIndexData(missing),
      /expected exactly href, dir_exists, and session/,
    );
  }
  for (const extra of [
    { extra: true },
    { [Symbol("extra")]: true },
  ]) {
    assert.throws(
      () => parseIndexData({ ...VALID_INDEX_DATA, ...extra }),
      /expected exactly href, dir_exists, and session/,
    );
  }

  let hrefReads = 0;
  const accessor = { ...VALID_INDEX_DATA };
  Object.defineProperty(accessor, "href", {
    enumerable: true,
    get() {
      hrefReads++;
      return "/";
    },
  });
  assert.throws(() => parseIndexData(accessor), /href must be a data property/);
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

test("embedded index data enforces boolean and exact administrator session fields", () => {
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

  for (const key of Object.keys(VALID_SESSION)) {
    const session = { ...VALID_SESSION };
    delete session[key];
    assert.throws(
      () => parseIndexData({ ...VALID_INDEX_DATA, session }),
      /session must contain exactly authenticated, user_id, username, role, and csrf_token/,
    );
  }
  assert.throws(
    () => parseIndexData({
      ...VALID_INDEX_DATA,
      session: { ...VALID_SESSION, extra: true },
    }),
    /session must contain exactly authenticated, user_id, username, role, and csrf_token/,
  );
});

test("embedded session admits only the current administrator identity contract", () => {
  for (const username of ["adm", "admin..ops", "a_b-c.d", "a".repeat(64)]) {
    assert.equal(
      parseIndexData({
        ...VALID_INDEX_DATA,
        session: { ...VALID_SESSION, username },
      }).session.username,
      username,
    );
  }
  for (const [field, value, message] of [
    ["authenticated", false, /session.authenticated must be true/],
    ["user_id", "space is invalid", /session.user_id must be a Foundation identifier/],
    ["username", "ad", /session.username must be a canonical administrator username/],
    ["username", "Admin", /session.username must be a canonical administrator username/],
    ["username", "admin@name", /session.username must be a canonical administrator username/],
    ["username", "-admin", /session.username must be a canonical administrator username/],
    ["username", "admin-", /session.username must be a canonical administrator username/],
    ["username", "a".repeat(65), /session.username must be a canonical administrator username/],
    ["role", "viewer", /session.role must be admin/],
  ]) {
    assert.throws(
      () => parseIndexData({
        ...VALID_INDEX_DATA,
        session: { ...VALID_SESSION, [field]: value },
      }),
      message,
    );
  }
});

test("embedded session accepts only Foundation URL-safe CSRF tokens", () => {
  for (const csrf_token of [
    "",
    "A".repeat(42),
    "A".repeat(44),
    "B".repeat(43),
    "+".repeat(43),
    "/".repeat(43),
    0,
    true,
    null,
  ]) {
    assert.throws(
      () => parseIndexData({
        ...VALID_INDEX_DATA,
        session: { ...VALID_SESSION, csrf_token },
      }),
      /session.csrf_token must be a Foundation URL-safe token/,
    );
  }
});
