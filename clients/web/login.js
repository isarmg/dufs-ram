const form = /** @type {HTMLFormElement} */ (
  document.querySelector(".login-card")
);
const username = /** @type {HTMLInputElement} */ (
  document.querySelector("#username")
);
const password = /** @type {HTMLInputElement} */ (
  document.querySelector("#password")
);
const submit = /** @type {HTMLButtonElement} */ (
  document.querySelector('button[type="submit"]')
);
const errorRow = /** @type {HTMLElement} */ (
  document.querySelector(".error-row")
);
const errorText = /** @type {HTMLElement} */ (
  document.querySelector(".login-error")
);
const encoder = new TextEncoder();
const canonicalAdministratorUsernamePattern =
  /^(?=.{3,64}$)[a-z0-9][a-z0-9._-]*[a-z0-9]$/u;
// A 32-byte unpadded base64url value has four payload bits in its final
// character, so only this canonical 16-character subset is valid there.
const authenticationTokenPattern = /^[A-Za-z0-9_-]{42}[AEIMQUYcgkosw048]$/u;
const passwordValidator = createPasswordValidator(password);

username.addEventListener("input", validateUsername);
password.addEventListener("input", passwordValidator);
form.addEventListener("submit", event => {
  event.preventDefault();
  validateUsername();
  passwordValidator();
  if (!form.checkValidity()) {
    const invalid = form.querySelector(":invalid");
    if (invalid instanceof HTMLElement) invalid.focus();
    form.reportValidity();
    return;
  }
  void login();
});

async function login() {
  setError("");
  submit.disabled = true;
  try {
    const response = await fetch("/api/v2/auth/login", {
      method: "POST",
      credentials: "same-origin",
      redirect: "error",
      headers: {
        Accept: "application/json",
        "Content-Type": "application/json",
      },
      body: JSON.stringify({ username: username.value, password: password.value }),
    });
    if (!response.ok) {
      throw new Error(await responseErrorMessage(response));
    }
    if (!isAdministratorSession(await response.json())) {
      throw new Error("Server returned an invalid administrator session.");
    }
    location.replace("/");
  } catch (error) {
    setError(error instanceof Error ? error.message : "Sign in failed.");
    password.focus();
  } finally {
    submit.disabled = false;
  }
}

function validateUsername() {
  const valid = normalizeAdministratorUsername(username.value) !== null;
  username.setCustomValidity(
    valid ? "" : "Enter a valid administrator username.",
  );
  return valid;
}

/** @param {unknown} value @returns {value is string} */
function isCanonicalAdministratorUsername(value) {
  return typeof value === "string" &&
    canonicalAdministratorUsernamePattern.test(value);
}

/** @param {unknown} value @returns {string | null} */
function normalizeAdministratorUsername(value) {
  if (
    typeof value !== "string" ||
    value.length < 1 ||
    value.length > 64 ||
    /[^\x20-\x7e]/u.test(value)
  ) {
    return null;
  }
  const normalized = value.replace(/^ +| +$/gu, "").toLowerCase();
  return isCanonicalAdministratorUsername(normalized) ? normalized : null;
}

/** @param {HTMLInputElement} input @returns {() => boolean} */
function createPasswordValidator(input) {
  const minimumBytes = Number(input.dataset.minBytes);
  const maximumBytes = Number(input.dataset.maxBytes);
  const scratch = new Uint8Array(maximumBytes + 1);
  return () => {
    const result = encoder.encodeInto(input.value, scratch);
    const hasControl = /[\u0000-\u001f\u007f]/u.test(input.value);
    const fits = result.read === input.value.length &&
      result.written >= minimumBytes &&
      result.written <= maximumBytes &&
      !hasControl;
    input.setCustomValidity(
      fits
        ? ""
        : `Password must contain ${minimumBytes} to ${maximumBytes} UTF-8 bytes and no control characters.`,
    );
    return fits;
  };
}

/** @param {Response} response */
async function responseErrorMessage(response) {
  try {
    /** @type {unknown} */
    const value = await response.json();
    if (
      isPlainRecord(value) &&
      typeof value.message === "string" &&
      value.message.length > 0
    ) {
      return value.message;
    }
  } catch {
    // The status remains authoritative when the server cannot return JSON.
  }
  return `Sign in failed (HTTP ${response.status}).`;
}

/** @param {unknown} value */
function isAdministratorSession(value) {
  if (!isPlainRecord(value)) return false;
  const keys = Reflect.ownKeys(value);
  const expected = ["authenticated", "user_id", "username", "role", "csrf_token"];
  return keys.length === expected.length &&
    keys.every(key => typeof key === "string" && expected.includes(key)) &&
    value.authenticated === true &&
    typeof value.user_id === "string" &&
    /^[A-Za-z0-9._:-]{1,128}$/u.test(value.user_id) &&
    isCanonicalAdministratorUsername(value.username) &&
    value.role === "admin" &&
    typeof value.csrf_token === "string" &&
    authenticationTokenPattern.test(value.csrf_token);
}

/** @param {unknown} value @returns {value is Record<string, unknown>} */
function isPlainRecord(value) {
  if (value === null || typeof value !== "object" || Array.isArray(value)) {
    return false;
  }
  const prototype = Object.getPrototypeOf(value);
  return prototype === Object.prototype || prototype === null;
}

/** @param {string} message */
function setError(message) {
  errorText.textContent = message;
  errorRow.classList.toggle("hidden", message.length === 0);
}
