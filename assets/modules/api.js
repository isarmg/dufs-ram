export const CSRF_HEADER = "X-Dufs-CSRF-Token";
export const AUTH_ERROR_HEADER = "X-Dufs-Auth-Error";
export const AUTH_REQUIRED_MESSAGE = "登录状态已失效，正在返回登录页。";
export const PAGE_EXPIRED_MESSAGE = "登录状态或当前页面已失效，请刷新页面并重新选择文件。";

class AuthenticationError extends Error {}

export function isCsrfAuthFailure(status, authError) {
  return status === 403 && authError === "csrf";
}

export function authFailureMessage(status, authError) {
  if (status === 401) return AUTH_REQUIRED_MESSAGE;
  if (isCsrfAuthFailure(status, authError)) return PAGE_EXPIRED_MESSAGE;
  return "";
}

export async function assertResponse(response, onUnauthorized) {
  if (response.ok) return;
  const authMessage = authFailureMessage(
    response.status,
    response.headers.get(AUTH_ERROR_HEADER),
  );
  if (authMessage) {
    onUnauthorized?.();
    throw new AuthenticationError(authMessage);
  }
  throw new Error(`请求失败（HTTP ${response.status}）`);
}

export function isAuthenticationError(error) {
  return error instanceof AuthenticationError;
}

export function postJson(url, csrfToken, body) {
  return fetch(url, {
    method: "POST",
    headers: {
      "Content-Type": "application/json",
      [CSRF_HEADER]: csrfToken,
    },
    body: JSON.stringify(body),
  });
}
