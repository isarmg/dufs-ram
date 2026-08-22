/**
 * A small FIFO with O(1) dequeue and cancellation. Cancelled entries are
 * skipped lazily and storage is compacted occasionally.
 *
 * @template T
 */
export function createUploadQueue() {
  /** @type {{ active: boolean, value: T }[]} */
  let entries = [];
  let head = 0;
  let size = 0;

  /** @param {T} value */
  function enqueue(value) {
    const entry = { active: true, value };
    entries.push(entry);
    size++;
    return entry;
  }

  /** @param {{ active: boolean } | null} entry */
  function cancel(entry) {
    if (!entry?.active) return false;
    entry.active = false;
    size--;
    compact();
    return true;
  }

  function dequeue() {
    while (head < entries.length) {
      const entry = entries[head++];
      if (!entry.active) continue;
      entry.active = false;
      size--;
      compact();
      return entry.value;
    }
    compact(true);
    return null;
  }

  function compact(force = false) {
    if (!force && (head < 256 || head * 2 < entries.length)) return;
    entries = entries.slice(head);
    head = 0;
  }

  return Object.freeze({
    enqueue,
    cancel,
    dequeue,
    get size() {
      return size;
    },
  });
}

/**
 * Keep a bounded FIFO of lightweight UI history entries.
 *
 * @template T
 * @param {number} limit
 * @param {(entry: T) => void} onEvict
 */
export function createBoundedHistory(limit, onEvict) {
  if (!Number.isSafeInteger(limit) || limit <= 0) {
    throw new TypeError("History limit must be a positive integer");
  }
  /** @type {T[]} */
  const entries = [];
  let evicted = 0;

  /** @param {T} value */
  function add(value) {
    entries.push(value);
    while (entries.length > limit) {
      const oldest = entries.shift();
      if (oldest === undefined) break;
      onEvict(oldest);
      evicted++;
    }
    return evicted;
  }

  /** @param {T} value */
  function remove(value) {
    const index = entries.indexOf(value);
    if (index < 0) return false;
    entries.splice(index, 1);
    return true;
  }

  return Object.freeze({
    add,
    remove,
    get evicted() {
      return evicted;
    },
    get size() {
      return entries.length;
    },
  });
}
