import assert from "node:assert/strict";

import OcaNotify from "../templates/opencode-plugin.js";

const plugin = await OcaNotify();

assert.equal(
  typeof plugin.event,
  "function",
  "the notify plugin must expose a callable event hook",
);
