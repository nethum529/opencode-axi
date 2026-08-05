import assert from "node:assert/strict";

import OcaNotify from "../templates/opencode-plugin.js";

const notifyUrl = "https://notify.example.test/oca";
const notifications = [];
const originalFetch = globalThis.fetch;
const originalNtfyUrl = process.env.OCA_NTFY_URL;
const originalDesktopNotify = process.env.OCA_DESKTOP_NOTIFY;

process.env.OCA_NTFY_URL = notifyUrl;
process.env.OCA_DESKTOP_NOTIFY = "0";
globalThis.fetch = async (url, options) => {
  notifications.push({ url, options });
  return { ok: true };
};

try {
  const plugin = await OcaNotify();

  assert.equal(
    typeof plugin.event,
    "function",
    "the notify plugin must expose a callable event hook",
  );

  async function dispatch(
    type,
    command,
    { preApproved, sessionID = "matrix-session" } = {},
  ) {
    const properties = { command, sessionID };
    if (preApproved !== undefined) properties.preApproved = preApproved;
    await plugin.event({ event: { type, properties } });
  }

  async function expectNotification(
    type,
    command,
    expectedTitle,
    { preApproved, sessionID = "matrix-session" } = {},
  ) {
    const countBefore = notifications.length;
    await dispatch(type, command, { preApproved, sessionID });
    assert.equal(
      notifications.length,
      countBefore + 1,
      type + " " + command + " must publish a notification",
    );
    const notification = notifications.at(-1);
    assert.equal(notification.url, notifyUrl);
    assert.deepEqual(notification.options, {
      method: "POST",
      body: expectedTitle + ": session=" + sessionID,
    });
  }

  async function expectSilent(
    type,
    command,
    { preApproved, sessionID = "matrix-session" } = {},
  ) {
    const countBefore = notifications.length;
    await dispatch(type, command, { preApproved, sessionID });
    assert.equal(
      notifications.length,
      countBefore,
      type + " " + command + " must be silent",
    );
  }

  const destructiveCases = [
    ["rm -rf generated", "rm -rf generated"],
    ["git clean -fd", "git clean -fd"],
    ["dd if=/dev/zero of=/dev/null", "dd if=/dev/zero of=/dev/null"],
    ["rm -rf at the command boundary", "rm -rf"],
    ["single recursive flag", "rm -r generated"],
    ["single force flag", "rm -f generated"],
    ["combined git clean flags", "git clean -fdx generated"],
  ];
  for (const [label, command] of destructiveCases) {
    await expectNotification(
      "tool.execute.before",
      command,
      "oca destructive action",
      { sessionID: "destructive-" + label },
    );
  }

  const destructiveNearMisses = [
    ["command prefix", "arm -rf generated"],
    ["flag suffix", "rm -rfish generated"],
    ["different flag spelling", "rm --rf generated"],
    ["missing dd input", "dd if= of=/dev/null"],
    ["dd option near miss", "dd iff=/dev/zero of=/dev/null"],
    ["unknown git clean flag", "git clean -fz generated"],
  ];
  for (const [label, command] of destructiveNearMisses) {
    await expectSilent("tool.execute.before", command, {
      sessionID: "near-miss-" + label,
    });
  }

  const publishCommands = [
    ["git push", "git push origin main"],
    ["gh pr", "gh pr create --title publish"],
    ["gh repo", "gh repo create sample"],
    ["git remote", "git remote add origin git@example.com:team/repo"],
  ];
  for (const type of ["permission.asked", "tool.execute.before"]) {
    for (const [label, command] of publishCommands) {
      await expectNotification(type, command, "oca unapproved publish", {
        preApproved: false,
        sessionID: type + "-" + label + "-unapproved",
      });
      await expectSilent(type, command, {
        preApproved: true,
        sessionID: type + "-" + label + "-approved",
      });
    }
  }

  await expectSilent("session.idle", "rm -rf generated", {
    sessionID: "ignored-event",
  });
  console.log("plugin runtime matrix passed");
} finally {
  if (originalFetch === undefined) delete globalThis.fetch;
  else globalThis.fetch = originalFetch;

  if (originalNtfyUrl === undefined) delete process.env.OCA_NTFY_URL;
  else process.env.OCA_NTFY_URL = originalNtfyUrl;

  if (originalDesktopNotify === undefined) delete process.env.OCA_DESKTOP_NOTIFY;
  else process.env.OCA_DESKTOP_NOTIFY = originalDesktopNotify;
}
