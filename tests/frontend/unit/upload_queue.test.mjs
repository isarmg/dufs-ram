import assert from "node:assert/strict";
import test from "node:test";

import {
  createBoundedHistory,
  createUploadQueue,
} from "../../../clients/web/modules/upload/queue.js";

test("upload queue dequeues and cancels in constant-time order", () => {
  const queue = createUploadQueue();
  const first = queue.enqueue("first");
  const second = queue.enqueue("second");
  queue.enqueue("third");
  assert.equal(queue.size, 3);
  assert.equal(queue.cancel(second), true);
  assert.equal(queue.cancel(second), false);
  assert.equal(queue.dequeue(), "first");
  assert.equal(queue.dequeue(), "third");
  assert.equal(queue.dequeue(), null);
  assert.equal(queue.cancel(first), false);
  assert.equal(queue.size, 0);
});

test("bounded upload history evicts the oldest terminal entries", () => {
  const evicted = [];
  const history = createBoundedHistory(2, entry => evicted.push(entry));
  const first = { id: 1 };
  const second = { id: 2 };
  const third = { id: 3 };
  history.add(first);
  history.add(second);
  history.add(third);
  assert.deepEqual(evicted, [first]);
  assert.equal(history.size, 2);
  assert.equal(history.evicted, 1);
  assert.equal(history.remove(second), true);
  assert.equal(history.remove(second), false);
  assert.equal(history.size, 1);
});
