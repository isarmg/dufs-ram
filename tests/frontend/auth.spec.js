const { expect, test } = require("./fixtures");

test("登录错误、会话 Cookie 与注销均由服务端生效", async ({
  browser,
  context,
  appPage: page,
}, testInfo) => {
  const anonymous = await browser.newContext({ ignoreHTTPSErrors: true });
  const loginPage = await anonymous.newPage();
  await loginPage.goto(testInfo.project.use.baseURL);
  await expect(loginPage).toHaveURL(/\/__dufs__\/login$/);

  const username = loginPage.getByLabel("账号");
  const password = loginPage.getByLabel("密码");
  const submit = loginPage.getByRole("button", { name: "登录" });
  const alert = loginPage.getByRole("alert");

  await submit.click();
  await expect(loginPage).toHaveURL(
    /\/__dufs__\/login\?login_error=[0-9a-f]{64}$/,
  );
  await expect(alert).toHaveText("请填写账号和密码");
  await loginPage.reload();
  await expect(alert).toBeHidden();

  await username.fill("frontend-test");
  await password.fill("wrong-password");
  await submit.click();
  await expect(alert).toHaveText("用户名或密码错误。");
  expect(
    (await anonymous.cookies()).find(
      cookie => cookie.name === "__Host-dufs-session",
    ),
  ).toBeUndefined();
  await anonymous.close();

  const sessionCookie = (await context.cookies()).find(
    cookie => cookie.name === "__Host-dufs-session",
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
      new URL(response.url()).pathname.endsWith("/__dufs__/logout"),
  );
  await Promise.all([
    page.waitForURL(/\/__dufs__\/login$/),
    page.getByRole("button", { name: "退出登录" }).click(),
  ]);
  expect((await logoutResponse).status()).toBe(204);
  expect(
    (await context.cookies()).find(
      cookie => cookie.name === "__Host-dufs-session",
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
  const card = page.locator(".login-card");
  const bounds = await card.boundingBox();
  expect(bounds.width / bounds.height).toBeCloseTo(1.5, 2);
  await expect(page.getByLabel("账号")).toBeFocused();
  await page.keyboard.press("Tab");
  await expect(page.getByLabel("密码")).toBeFocused();
  await page.keyboard.press("Tab");
  await expect(page.getByRole("button", { name: "登录" })).toBeFocused();
});
