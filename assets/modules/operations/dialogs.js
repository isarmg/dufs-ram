/**
 * @typedef {{
 *   showMessage: (options: DialogOptions) => Promise<undefined>,
 *   confirmAction: (options: DialogOptions) => Promise<boolean>,
 *   chooseAction: (options: DialogOptions) => Promise<DialogChoice>,
 *   requestText: (options: DialogOptions) => Promise<string | null>,
 * }} ActionDialogs
 */

/** @typedef {"confirm" | "alternate" | "cancel"} DialogChoice */

/**
 * @typedef {{
 *   title: string,
 *   message?: string,
 *   label?: string,
 *   value?: string,
 *   confirmText?: string,
 *   alternateText?: string,
 *   cancelText?: string,
 *   danger?: boolean,
 *   returnFocus?: Element | null,
 * }} DialogOptions
 */

/** @returns {ActionDialogs} */
export function createActionDialogs() {
  const dialog = requiredDialogElement(
    ".action-dialog",
    document,
    HTMLDialogElement,
  );
  const title = requiredDialogElement(
    "#action-dialog-title",
    dialog,
    HTMLElement,
  );
  const message = requiredDialogElement(
    "#action-dialog-message",
    dialog,
    HTMLElement,
  );
  const inputGroup = requiredDialogElement(
    ".action-dialog-input-group",
    dialog,
    HTMLElement,
  );
  const inputLabel = requiredDialogElement("label", inputGroup, HTMLElement);
  const input = requiredDialogElement(
    "#action-dialog-input",
    inputGroup,
    HTMLInputElement,
  );
  const cancelButton = requiredDialogElement(
    ".action-dialog-cancel",
    dialog,
    HTMLButtonElement,
  );
  const alternateButton = requiredDialogElement(
    ".action-dialog-alternate",
    dialog,
    HTMLButtonElement,
  );
  const confirmButton = requiredDialogElement(
    ".action-dialog-confirm",
    dialog,
    HTMLButtonElement,
  );
  /** @type {{
   *   kind: "alert" | "confirm" | "choice" | "prompt",
   *   resolve: (value: unknown) => void,
   *   returnFocus: Element | null,
   * } | null} */
  let active = null;
  /** @type {Promise<void>} */
  let queue = Promise.resolve();

  cancelButton.addEventListener("click", () => dialog.close("cancel"));
  dialog.addEventListener("cancel", event => {
    event.preventDefault();
    dialog.close("cancel");
  });
  dialog.addEventListener("keydown", event => {
    if (event.key !== "Tab" || !active) return;
    const controls = [
      ...(input.disabled ? [] : [input]),
      ...(cancelButton.hidden ? [] : [cancelButton]),
      ...(alternateButton.hidden ? [] : [alternateButton]),
      confirmButton,
    ];
    const currentIndex = controls.findIndex(
      control => control === document.activeElement,
    );
    const nextControl = event.shiftKey
      ? currentIndex <= 0 ? controls.at(-1) : null
      : currentIndex === -1 || currentIndex === controls.length - 1
        ? controls[0]
        : null;
    if (!nextControl) return;
    event.preventDefault();
    nextControl.focus();
  });
  dialog.addEventListener("close", () => {
    if (!active) return;
    const request = active;
    active = null;
    const confirmed = dialog.returnValue === "confirm";
    let result;
    if (request.kind === "prompt") {
      result = confirmed ? input.value : null;
    } else if (request.kind === "confirm") {
      result = confirmed;
    } else if (request.kind === "choice") {
      result = dialogChoice(dialog.returnValue);
    }
    restoreFocus(request.returnFocus);
    request.resolve(result);
  });

  /** @param {DialogOptions & { kind: "alert" | "confirm" | "choice" | "prompt" }} options */
  function enqueue(options) {
    const result = queue.then(() => show(options));
    queue = result.then(() => undefined, () => undefined);
    return result;
  }

  /** @param {DialogOptions & { kind: "alert" | "confirm" | "choice" | "prompt" }} options */
  function show(options) {
    const kind = options.kind;
    title.textContent = options.title;
    message.textContent = options.message || "";
    message.hidden = !options.message;
    if (options.message) {
      dialog.setAttribute("aria-describedby", "action-dialog-message");
    } else {
      dialog.removeAttribute("aria-describedby");
    }
    inputGroup.hidden = kind !== "prompt";
    input.disabled = kind !== "prompt";
    input.required = kind === "prompt";
    input.value = options.value || "";
    inputLabel.textContent = options.label || "";
    cancelButton.hidden = kind === "alert";
    cancelButton.textContent = options.cancelText || "Cancel";
    alternateButton.hidden = kind !== "choice";
    alternateButton.textContent = options.alternateText || "Skip";
    confirmButton.textContent = options.confirmText ||
      (kind === "alert" ? "Close" : "Confirm");
    confirmButton.classList.toggle("danger", Boolean(options.danger));
    dialog.returnValue = "";

    return new Promise(resolve => {
      active = {
        kind,
        resolve,
        returnFocus: options.returnFocus || document.activeElement,
      };
      dialog.showModal();
      (kind === "prompt" ? input : confirmButton).focus();
      if (kind === "prompt") input.select();
    });
  }

  return Object.freeze({
    /** @param {DialogOptions} options */
    showMessage(options) {
      return enqueue({ ...options, kind: "alert" }).then(() => undefined);
    },
    /** @param {DialogOptions} options */
    confirmAction(options) {
      return enqueue({ ...options, kind: "confirm" }).then(
        value => value === true,
      );
    },
    /** @param {DialogOptions} options @returns {Promise<DialogChoice>} */
    chooseAction(options) {
      return enqueue({ ...options, kind: "choice" }).then(dialogChoice);
    },
    /** @param {DialogOptions} options */
    requestText(options) {
      return enqueue({ ...options, kind: "prompt" }).then(
        value => typeof value === "string" ? value : null,
      );
    },
  });
}

/** @param {unknown} value @returns {DialogChoice} */
function dialogChoice(value) {
  return value === "confirm" || value === "alternate" ? value : "cancel";
}

/**
 * @template {Element} T
 * @param {string} selector
 * @param {ParentNode} root
 * @param {{ new (...args: never[]): T }} constructor
 * @returns {T}
 */
function requiredDialogElement(selector, root, constructor) {
  const element = root.querySelector(selector);
  if (!element) throw new Error(`Required dialog control is missing: ${selector}`);
  if (!(element instanceof constructor)) {
    throw new Error(`Required dialog control has the wrong type: ${selector}`);
  }
  return element;
}

/** @param {Element | null | undefined} element */
function restoreFocus(element) {
  if (!(element instanceof HTMLElement) || !element.isConnected) return;
  element.focus({ preventScroll: true });
}
