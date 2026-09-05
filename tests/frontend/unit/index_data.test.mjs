import assert from "node:assert/strict";
import test from "node:test";
import { parseIndexData } from "../../../clients/web/modules/shared/index_data.js";

const session = { authenticated: true, user_id: "admin", username: "admin", role: "admin", csrf_token: "A".repeat(43) };
const metadata = { href: "/folder/文件 & name", dir_exists: true };

test("business metadata and restored platform session become a fresh frozen record", () => {
  const parsed = parseIndexData(metadata, session);
  assert.deepEqual(parsed, { ...metadata, session });
  assert.notEqual(parsed, metadata);
  assert.notEqual(parsed.session, session);
  assert.ok(Object.isFrozen(parsed));
  assert.ok(Object.isFrozen(parsed.session));
  assert.equal(parseIndexData(Object.assign(Object.create(null), metadata), session).href, metadata.href);
});

test("page metadata rejects embedded credentials, extra keys and accessors without reading them", () => {
  for (const input of [null, undefined, false, 0, "", [], new Date(), Object.create(metadata)]) {
    assert.throws(() => parseIndexData(input, session), /expected a plain object/);
  }
  for (const input of [{ href: "/" }, { dir_exists: true }, { ...metadata, session }, { ...metadata, extra: true }, { ...metadata, [Symbol()]: true }]) {
    assert.throws(() => parseIndexData(input, session), /expected exactly href and dir_exists/);
  }
  let reads = 0;
  const accessor = { ...metadata, get href() { reads++; return "/"; } };
  assert.throws(() => parseIndexData(accessor, session), /data property/);
  assert.equal(reads, 0);
});

test("only canonical absolute business paths and a boolean directory flag are admitted", () => {
  for (const href of ["/", "/folder", "/目录/with space/100% literal", "/back\\slash/is-valid-on-linux"]) {
    assert.equal(parseIndexData({ ...metadata, href }, session).href, href);
  }
  for (const href of ["", "relative", "./relative", "../relative", "//", "//folder", "/folder/", "/folder//child", "/./folder", "/folder/.", "/../folder", "/folder/..", "/nul\0byte", null, 0, false, []]) {
    assert.throws(() => parseIndexData({ ...metadata, href }, session), /canonical absolute logical path/);
  }
  for (const dir_exists of [0, 1, "true", null, undefined]) {
    assert.throws(() => parseIndexData({ ...metadata, dir_exists }, session), /boolean/);
  }
});

test("session validation is owned by the Foundation contract", () => {
  for (const key of Object.keys(session)) {
    const missing = { ...session }; delete missing[key];
    assert.throws(() => parseIndexData(metadata, missing), /Foundation contract/);
  }
  for (const invalid of [null, { ...session, extra: true }, { ...session, authenticated: false }, { ...session, role: "viewer" }, { ...session, username: "Admin" }, { ...session, csrf_token: "B".repeat(43) }]) {
    assert.throws(() => parseIndexData(metadata, invalid), /Foundation contract/);
  }
});
