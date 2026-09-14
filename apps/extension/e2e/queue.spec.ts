// End-to-end: the queue, in a real browser.
//
// The download engine has its own tests; these cover the part that only exists
// once several downloads compete — that a batch runs unattended, that the
// concurrency limit is respected, that pausing everything actually stops it, and
// that an expected digest is compared rather than merely displayed.

import { enqueue, expect, test, waitForStatus } from "./fixtures";

/** Read the settings the manager persists, through the page's own storage. */
async function setSettings(
  page: import("@playwright/test").Page,
  patch: Record<string, unknown>,
): Promise<void> {
  await page.evaluate(async (p) => {
    await (globalThis as any).__test.updateSettings(p);
  }, patch);
}

test("a batch of downloads runs to completion with no further clicks", async ({
  manager,
  serverUrl,
}) => {
  const size = 64 * 1024;
  const expected = (await (await fetch(`${serverUrl}/fixture.sha256?size=${size}`)).text()).trim();

  const ids: string[] = [];
  for (let i = 0; i < 3; i++) {
    ids.push(
      await enqueue(manager, {
        // Distinct URLs, since the job id is derived from the URL — three jobs
        // for one URL would be one job.
        url: `${serverUrl}/fixture.bin?size=${size}&n=${i}`,
        kind: "progressive",
        filename: `batch-${i}.bin`,
        size,
      }),
    );
  }

  for (const id of ids) {
    const job = await waitForStatus(manager, id, ["done", "error"]);
    expect(job.status).toBe("done");
    // Same bytes from the same generator, so the same digest — which also
    // proves the three concurrent jobs did not write into each other's chunks.
    expect(job.sha256).toBe(expected);
  }
});

test("no more than the configured number of downloads run at once", async ({
  manager,
  serverUrl,
}) => {
  await setSettings(manager, { maxConcurrentJobs: 2 });

  // Big enough that the jobs genuinely overlap rather than finishing one by one
  // before the next is even started.
  const size = 4 * 1024 * 1024;
  for (let i = 0; i < 5; i++) {
    await enqueue(manager, {
      url: `${serverUrl}/fixture.bin?size=${size}&n=${i}`,
      kind: "progressive",
      filename: `limit-${i}.bin`,
      size,
    });
  }

  // Sample the live count repeatedly while the batch drains. A single sample
  // could easily miss an overshoot.
  let peak = 0;
  for (let i = 0; i < 40; i++) {
    const active = await manager.evaluate(
      async () => (globalThis as any).__test.manager.activeCount() as number,
    );
    peak = Math.max(peak, active);
    const jobs = await manager.evaluate(async () => (globalThis as any).__test.listJobs());
    if (jobs.every((j: { status: string }) => j.status === "done" || j.status === "error")) break;
    await manager.waitForTimeout(250);
  }

  expect(peak).toBeGreaterThan(0);
  expect(peak).toBeLessThanOrEqual(2);
});

test("an expected digest that does not match is reported, not hidden", async ({
  manager,
  serverUrl,
}) => {
  const size = 32 * 1024;
  const real = (await (await fetch(`${serverUrl}/fixture.sha256?size=${size}`)).text()).trim();

  const wrong = await enqueue(
    manager,
    {
      url: `${serverUrl}/fixture.bin?size=${size}&n=wrong`,
      kind: "progressive",
      filename: "mismatch.bin",
      size,
    },
    { expectedSha256: "0".repeat(64) },
  );
  const right = await enqueue(
    manager,
    {
      url: `${serverUrl}/fixture.bin?size=${size}&n=right`,
      kind: "progressive",
      filename: "match.bin",
      size,
    },
    { expectedSha256: real },
  );

  const bad = await waitForStatus(manager, wrong, ["done", "error"]);
  // The file downloaded fine; it is the *claim about it* that failed, so the
  // job is done with a mismatch rather than errored.
  expect(bad.status).toBe("done");
  expect(bad.verification).toBe("mismatch");
  expect(bad.sha256).toBe(real);

  const good = await waitForStatus(manager, right, ["done", "error"]);
  expect(good.status).toBe("done");
  expect(good.verification).toBe("verified");

  await expect(manager.getByText("hash mismatch")).toBeVisible();
  await expect(manager.getByText("verified", { exact: true })).toBeVisible();
});

test("pause all stops the queue, and resume all restarts it", async ({ manager, serverUrl }) => {
  const size = 4 * 1024 * 1024;
  const ids: string[] = [];
  for (let i = 0; i < 3; i++) {
    ids.push(
      await enqueue(manager, {
        url: `${serverUrl}/fixture.bin?size=${size}&n=pause${i}`,
        kind: "progressive",
        filename: `pause-${i}.bin`,
        size,
      }),
    );
  }

  await manager.getByRole("button", { name: "Pause all" }).click({ timeout: 20_000 });

  // Everything must come to rest: nothing running, and nothing left queued that
  // would quietly start again.
  await expect
    .poll(
      async () =>
        manager.evaluate(async () => (globalThis as any).__test.manager.activeCount() as number),
      { timeout: 20_000 },
    )
    .toBe(0);

  const midway = await manager.evaluate(async () => (globalThis as any).__test.listJobs());
  expect(
    midway.some((j: { status: string }) => j.status === "paused" || j.status === "queued"),
  ).toBe(true);

  await manager.getByRole("button", { name: "Resume all" }).click({ timeout: 20_000 });

  for (const id of ids) {
    const job = await waitForStatus(manager, id, ["done", "error"], 90_000);
    expect(job.status).toBe("done");
  }
});

test("a host that serves the start and refuses the rest leaves nothing to resume", async ({
  manager,
  serverUrl,
}) => {
  // The shape reported in APP-60: Google's media addresses serve to about 1.1 MB and
  // answer 403 for every later offset. The download cannot be completed, and — the part
  // that was wrong — it cannot be resumed either, because the next attempt is refused at
  // the identical offset.
  const size = 4 * 1024 * 1024;
  const servable = 1_163_264;
  const id = "refused-remainder";

  await manager.evaluate(
    async (seed) => {
      await (globalThis as any).__test.putJob({
        id: seed.id,
        url: seed.url,
        filename: "refused.mp4",
        kind: "progressive",
        status: "queued",
        stateJson: "",
        totalBytes: seed.size,
        receivedBytes: 0,
        outputBytes: 0,
        sha256: null,
        error: null,
        createdAt: Date.now(),
        order: Date.now(),
        expectedSha256: null,
        verification: "unverified",
        // A host that states a request size is one that throttles with 403 rather than
        // refusing outright, and that is what makes its refusal the terminal case.
        maxChunkBytes: 1024 * 1024,
      });
      await (globalThis as any).__test.manager.sync();
    },
    { id, url: `${serverUrl}/fixture.mp4?size=${size}&serve_to=${servable}`, size },
  );

  const job = await waitForStatus(manager, id, ["done", "error"]);
  expect(job.status).toBe("error");
  expect(job.error).toContain("refused the rest");
  // Recorded on the job rather than re-derived from the message by whoever draws it.
  expect(job.terminal).toBe(true);

  // And the row offers no way to try again, because there is none.
  const card = manager.locator(".card").filter({ hasText: "refused.mp4" });
  await expect(card.getByRole("button", { name: /^(Resume|Start)$/ })).toHaveCount(0);
  await expect(card.getByRole("button", { name: "Remove" })).toBeVisible();
});

test("a download is not resumed onto a file the server has since replaced", async ({
  manager,
  serverUrl,
}) => {
  // Half of a file was fetched in an earlier session, when the server's ETag was "v0".
  // The server now says "v1": same URL, different file. Resuming would splice the new
  // file's second half onto the old file's first. `If-Range` never caught this — the
  // validator it sent was the one just probed, which always matches — and a web page
  // talking to another origin no longer sends it at all (APP-82), so the saved
  // validator is what has to be compared.
  const size = 256 * 1024;
  const half = size / 2;
  const id = "replaced-between-sessions";
  const stateJson = JSON.stringify({
    resume: {
      total: size,
      validator: '"v0"',
      accepts_ranges: true,
      completed: [{ start: 0, end: half - 1 }],
    },
    remux: false,
    remux_state: null,
    next_segment: 0,
    output_len: half,
  });

  await manager.evaluate(
    async (seed) => {
      await (globalThis as any).__test.putJob({
        id: seed.id,
        url: seed.url,
        filename: "replaced.mp4",
        kind: "progressive",
        status: "paused",
        stateJson: seed.stateJson,
        totalBytes: seed.size,
        receivedBytes: seed.half,
        outputBytes: seed.half,
        sha256: null,
        error: null,
        createdAt: Date.now(),
        order: Date.now(),
        expectedSha256: null,
        verification: "unverified",
      });
      await (globalThis as any).__test.manager.sync();
    },
    { id, url: `${serverUrl}/fixture.mp4?size=${size}`, size, half, stateJson },
  );
  await manager.locator(".card").filter({ hasText: "replaced.mp4" }).getByRole("button", { name: "Resume" }).click();

  const job = await waitForStatus(manager, id, ["done", "error"]);
  expect(job.status).toBe("error");
  expect(job.error).toContain("changed on the server");
});
