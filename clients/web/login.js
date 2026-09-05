import { isAdministratorLoginRequest, isAdministratorPassword } from "./dist/platform.js";
import { administratorApi, authenticationErrorMessage } from "./modules/platform-session.js";

const form = /** @type {HTMLFormElement} */ (document.querySelector(".login-card"));
const username = /** @type {HTMLInputElement} */ (document.querySelector("#username"));
const password = /** @type {HTMLInputElement} */ (document.querySelector("#password"));
const submit = /** @type {HTMLButtonElement} */ (document.querySelector('button[type="submit"]'));
const errorRow = /** @type {HTMLElement} */ (document.querySelector(".error-row"));
const errorText = /** @type {HTMLElement} */ (document.querySelector(".login-error"));
let pending = false;

form.addEventListener("submit", event => {
  event.preventDefault();
  if (pending) return;
  if (!form.checkValidity()) {
    form.reportValidity();
    return;
  }
  if (!isAdministratorLoginRequest({ username: username.value, password: password.value }) || !isAdministratorPassword(password.value)) {
    password.value = "";
    setError("Enter a valid administrator username and password.");
    password.focus();
    return;
  }
  void login();
});

async function login() {
  setError("");
  pending = true;
  submit.disabled = true;
  username.readOnly = true;
  password.readOnly = true;
  form.setAttribute("aria-busy", "true");
  let failed = false;
  try {
    await administratorApi.login(username.value, password.value);
    location.replace("/");
  } catch (error) {
    failed = true;
    setError(authenticationErrorMessage(error));
  } finally {
    password.value = "";
    pending = false;
    submit.disabled = false;
    username.readOnly = false;
    password.readOnly = false;
    form.removeAttribute("aria-busy");
    if (failed) password.focus();
  }
}

/** @param {string} message */
function setError(message) {
  errorText.textContent = message;
  errorRow.classList.toggle("hidden", message.length === 0);
}
