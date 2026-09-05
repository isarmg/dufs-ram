import { createAdministratorApiClient, ApiClientError } from "../dist/platform.js";

// One in-memory platform client per document. No credential storage in HTML or storage APIs.
export const administratorApi = createAdministratorApiClient();

/** @param {unknown} error @returns {string} */
export function authenticationErrorMessage(error) {
  const requestId = error instanceof ApiClientError ? error.requestId : undefined;
  return "Authentication request could not be completed. Please try again." +
    (requestId ? ` Request ID: ${requestId}` : "");
}
