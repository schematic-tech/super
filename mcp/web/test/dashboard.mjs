import assert from "node:assert/strict";
import { chromium } from "@playwright/test";
import { Dashboard } from "../../lib/dashboard.mjs";
let generation = 0,
  unavailable = false;
const titles = [
  "transfers_preserve_total_balance",
  "withdrawal_respects_available_funds",
  "sort_preserves_every_element",
  "invalid_token_never_grants_access",
  "roundtrip_preserves_unicode",
  "retry_does_not_duplicate_payment",
  "empty_queue_returns_no_item",
];
const rows = titles.map((name, i) => ({
  number: 2048 - i,
  supertest: {
    name,
    path: [
      "src/accounts/transfer.rs",
      "src/accounts/withdrawal.rs",
      "src/collections/sort.rs",
      "src/auth/session.rs",
    ][i % 4],
  },
  revision: { reported_git_commit: "a62f901b7a0d2c1234567890abcdef0123456789" },
  result:
    i === 0
      ? null
      : {
          outcome: i === 1 ? "fail" : i === 3 ? "conditional" : "pass",
          assurance: "standard",
        },
  terminal: i !== 0,
  updates_pending: i === 0,
  created_at: "2026-11-16T21:30:00Z",
  updated_at: "2026-11-16T21:30:35Z",
  presentation: {
    details: [
      {
        text: "A withdrawal larger than the available balance can produce a negative balance. Input: balance = 20, amount = 25.",
        emphasized: false,
      },
    ],
  },
  ...(i === 1
    ? {
        fix: {
          summary: "Reject withdrawals that exceed the available balance.",
          diff: "--- a/src/accounts/withdrawal.rs\n+++ b/src/accounts/withdrawal.rs\n@@ -12 +12 @@\n- balance -= amount;\n+ ensure!(amount <= balance);\n+ balance -= amount;",
          instructions: "Preview the proposal before applying it.",
          validation: ["super check src/accounts/withdrawal.rs"],
        },
      }
    : {}),
}));
const bridge = {
  directory: async () => "/Users/developer/projects/ledger",
  run: async (_command, args) => {
    if (unavailable)
      return {
        isError: true,
        output: {
          events: [{ type: "error", data: { message: "Disconnected" } }],
        },
      };
    const current = structuredClone(rows);
    if (generation) {
      current[0].terminal = true;
      current[0].result = { outcome: "pass" };
      current[0].updates_pending = false;
    }
    const check = args.find((arg) => arg.startsWith("--check="));
    return {
      isError: false,
      output: {
        events: [
          {
            type: "results",
            data: {
              history: check ? [] : current,
              rows: check
                ? [
                    {
                      current: current.find(
                        (row) => String(row.number) === check.slice(8),
                      ),
                    },
                  ]
                : [],
              next_before: null,
            },
          },
        ],
      },
    };
  },
};
const dashboard = new Dashboard(bridge, { cacheMs: 0 });
const url = await dashboard.start();
const browser = await chromium.launch({ headless: true });
const page = await browser.newPage({ viewport: { width: 1440, height: 1000 } });
const errors = [];
page.on("pageerror", (error) => errors.push(error.message));
try {
  await page.goto(url);
  await page.getByRole("button", { name: titles[0], exact: true }).waitFor();
  await page.screenshot({
    path: "/tmp/super-history-desktop.png",
    fullPage: true,
  });
  assert.equal(await page.locator("tbody tr").count(), 7);
  generation = 1;
  await page.waitForFunction(() => !document.querySelector("tbody .running"));
  await page
    .getByRole("button", { name: "Needs attention", exact: true })
    .click();
  assert.equal(await page.locator("tbody tr").count(), 2);
  await page.getByRole("searchbox").fill("withdrawal");
  assert.equal(await page.locator("tbody tr").count(), 1);
  await page.getByRole("button", { name: titles[1], exact: true }).click();
  await page.getByRole("dialog").waitFor();
  assert.equal(await page.locator(".app-shell").getAttribute("inert"), "");
  assert.match(
    await page.locator(".patch").innerText(),
    /ensure!\(amount <= balance\)/,
  );
  await page.screenshot({
    path: "/tmp/super-history-detail.png",
    fullPage: true,
  });
  await page.keyboard.press("Escape");
  assert.equal(await page.getByRole("dialog").count(), 0);
  await page.getByRole("searchbox").fill("");
  await page.getByRole("button", { name: "All checks", exact: true }).click();
  await page.setViewportSize({ width: 390, height: 844 });
  await page.screenshot({
    path: "/tmp/super-history-mobile.png",
    fullPage: true,
  });
  assert.ok(
    await page.evaluate(
      () => document.documentElement.scrollWidth <= innerWidth,
    ),
  );
  await page.getByRole("button", { name: titles[1], exact: true }).click();
  await page.getByRole("button", { name: "Close check details" }).click();
  unavailable = true;
  await page.getByRole("button", { name: "Refresh", exact: true }).click();
  await page
    .getByRole("status")
    .filter({ hasText: "not up to date" })
    .waitFor();
  assert.equal(await page.locator("tbody tr").count(), 7);
  assert.deepEqual(errors, []);
  console.log(
    "Browser checks passed: live refresh, filters, search, details, focus, mobile layout.",
  );
} finally {
  await browser.close();
  await dashboard.close();
}
