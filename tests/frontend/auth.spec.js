const { expect, pageData, test } = require("./fixtures");

test("登录错误、会话 Cookie 与注销均由服务端生效", async ({
  browser,
  context,
  appPage: page,
}, testInfo) => {
  // This scenario intentionally performs several Argon2-backed login attempts,
  // session rotation, logout, and replay checks. Firefox can exceed the generic
  // 30-second UI-test budget on constrained builders even when every individual
  // request remains within its protocol deadline.
  test.slow();
  const usernameValue = (await pageData(page)).session.username;
  const anonymous = await browser.newContext({ ignoreHTTPSErrors: true });
  const loginPage = await anonymous.newPage();
  await loginPage.goto(testInfo.project.use.baseURL);
  await expect(loginPage).toHaveURL(/\/__dufs__\/login$/);

  const username = loginPage.getByLabel("Username");
  const password = loginPage.getByLabel("Password");
  const submit = loginPage.getByRole("button", { name: "Sign in" });
  const alert = loginPage.getByRole("alert");

  let emptyLoginPosts = 0;
  loginPage.on("request", request => {
    if (
      request.method() === "POST" &&
      new URL(request.url()).pathname === "/api/v2/auth/login"
    ) {
      emptyLoginPosts++;
    }
  });
  await submit.click();
  await expect(loginPage).toHaveURL(/\/__dufs__\/login$/);
  await expect(username).toBeFocused();
  expect(emptyLoginPosts).toBe(0);
  await expect(alert).toBeHidden();

  await username.fill(usernameValue);
  await password.fill("wrong-password");
  const rejectedLogin = loginPage.waitForResponse(response =>
    response.request().method() === "POST" &&
    new URL(response.url()).pathname === "/api/v2/auth/login"
  );
  await submit.click();
  expect((await rejectedLogin).status()).toBe(401);
  await expect(alert).toContainText("Authentication request could not be completed.");
  await expect(password).toHaveValue("");
  await expect(password).toBeFocused();
  expect(
    (await anonymous.cookies()).find(
      cookie => cookie.name === "__Host-sarmg-dufs-ram-session",
    ),
  ).toBeUndefined();
  await anonymous.close();

  const sessionCookie = (await context.cookies()).find(
    cookie => cookie.name === "__Host-sarmg-dufs-ram-session",
  );
  expect(sessionCookie).toMatchObject({
    httpOnly: true,
    secure: true,
    sameSite: "Strict",
    path: "/",
  });

  const logoutResponse = page.waitForResponse(
    response =>
      response.request().method() === "POST" &&
      new URL(response.url()).pathname === "/api/v2/auth/logout",
  );
  await Promise.all([
    page.waitForURL(/\/__dufs__\/login$/),
    page.getByRole("button", { name: "Sign out" }).click(),
  ]);
  expect((await logoutResponse).status()).toBe(204);
  expect(
    (await context.cookies()).find(
      cookie => cookie.name === "__Host-sarmg-dufs-ram-session",
    ),
  ).toBeUndefined();

  const replay = await browser.newContext({ ignoreHTTPSErrors: true });
  await replay.addCookies([sessionCookie]);
  const replayPage = await replay.newPage();
  await replayPage.goto(testInfo.project.use.baseURL);
  await expect(replayPage).toHaveURL(/\/__dufs__\/login$/);
  await replay.close();
});

test("登录卡片保持 3:2 布局和键盘可见控件", async ({ page }, testInfo) => {
  await page.goto(`${testInfo.project.use.baseURL}/__dufs__/login`);
  await expect(page.locator("html")).toHaveAttribute("lang", "en");
  await expect(page).toHaveTitle("Sign in");
  const card = page.locator(".login-card");
  const bounds = await card.boundingBox();
  expect(bounds.width / bounds.height).toBeCloseTo(1.5, 2);
  await expect(page.getByLabel("Username")).toBeFocused();
  await page.keyboard.press("Tab");
  await expect(page.getByLabel("Password")).toBeFocused();
  await page.keyboard.press("Tab");
  const submit = page.getByRole("button", { name: "Sign in" });
  await expect(submit).toBeFocused();

  await page.emulateMedia({ forcedColors: "active" });
  expect(
    await page.evaluate(() => matchMedia("(forced-colors: active)").matches),
  ).toBe(true);
  expect(
    await card.evaluate(element => {
      const style = getComputedStyle(element);
      return style.borderStyle !== "none" &&
        Number.parseFloat(style.borderWidth) >= 2;
    }),
  ).toBe(true);
  const username = page.getByLabel("Username");
  await username.focus();
  for (const control of [username, submit]) {
    expect(
      await control.evaluate(element => {
        const style = getComputedStyle(element);
        return style.borderStyle !== "none" &&
          Number.parseFloat(style.borderWidth) >= 1;
      }),
    ).toBe(true);
  }
  expect(
    await username.evaluate(element => {
      const style = getComputedStyle(element);
      return style.outlineStyle !== "none" &&
        Number.parseFloat(style.outlineWidth) >= 2;
    }),
  ).toBe(true);
});

test("登录密码按 UTF-8 字节而非字符数执行浏览器边界校验", async ({
  browser,
}, testInfo) => {
  const anonymous = await browser.newContext({ ignoreHTTPSErrors: true });
  const page = await anonymous.newPage();
  const response = await page.goto(
    `${testInfo.project.use.baseURL}/__dufs__/login`,
  );
  expect(response.headers()["content-security-policy"]).toContain(
    "script-src 'self'",
  );
  expect(response.headers()["content-security-policy"]).toContain(
    "connect-src 'self'",
  );

  const password = page.getByLabel("Password");
  await expect(password).toHaveAttribute("data-min-bytes", "12");
  await expect(password).toHaveAttribute("data-max-bytes", "1024");
  expect(await password.getAttribute("maxlength")).toBeNull();

  for (const value of ["p".repeat(1024), "é".repeat(512)]) {
    await password.fill(value);
    expect(
      await password.evaluate(input => ({
        bytes: new TextEncoder().encode(input.value).length,
        message: input.validationMessage,
      })),
    ).toEqual({ bytes: 1024, message: "" });
  }

  let loginPosts = 0;
  page.on("request", request => {
    if (
      request.method() === "POST" &&
      new URL(request.url()).pathname === "/api/v2/auth/login"
    ) {
      loginPosts += 1;
    }
  });
  await page.getByLabel("Username").fill("frontend-test-0");
  await password.fill(`a${"é".repeat(512)}`);
  expect(
    await password.evaluate(input => ({
      bytes: new TextEncoder().encode(input.value).length,
      message: input.validationMessage,
    })),
  ).toEqual({
    bytes: 1025,
    message: "",
  });
  await page.getByRole("button", { name: "Sign in" }).click();
  await expect(page).toHaveURL(/\/__dufs__\/login$/);
  expect(loginPosts).toBe(0);
  await expect(page.getByRole("alert")).toHaveText("Enter a valid administrator username and password.");
  await expect(password).toHaveValue("");
  await expect(password).toBeFocused();

  await anonymous.close();
});
