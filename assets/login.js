const form = /** @type {HTMLFormElement} */ (
  document.querySelector(".login-card")
);
const username = /** @type {HTMLInputElement} */ (
  document.querySelector("#username")
);
const password = /** @type {HTMLInputElement} */ (
  document.querySelector("#password")
);
const encoder = new TextEncoder();
const usernameValidator = createByteLimitValidator(username, "Username");
const passwordValidator = createByteLimitValidator(password, "Password");

username.addEventListener("input", usernameValidator);
password.addEventListener("input", passwordValidator);
form.addEventListener("submit", event => {
  usernameValidator();
  passwordValidator();
  if (form.checkValidity()) return;
  event.preventDefault();
  const invalid = form.querySelector(":invalid");
  if (invalid instanceof HTMLElement) invalid.focus();
  form.reportValidity();
});

/**
 * @param {HTMLInputElement} input
 * @param {string} fieldName
 * @returns {() => boolean}
 */
function createByteLimitValidator(input, fieldName) {
  const maximumBytes = Number(input.dataset.maxBytes);
  const scratch = new Uint8Array(maximumBytes + 1);
  const tooLongMessage =
    `${fieldName} must not exceed ${maximumBytes} UTF-8 bytes.`;
  return () => {
    const result = encoder.encodeInto(input.value, scratch);
    const fits =
      result.read === input.value.length && result.written <= maximumBytes;
    input.setCustomValidity(fits ? "" : tooLongMessage);
    return fits;
  };
}
